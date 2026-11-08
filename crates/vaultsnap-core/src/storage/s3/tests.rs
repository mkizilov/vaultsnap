//! Tests of the S3 backend against [`FakeS3`]: configuration, key mapping, listings, error
//! classification, the probe for conditional writes, retries, bounded concurrency, and the
//! contract. The real service is covered by `tests/s3_minio.rs`.

use std::time::Duration;

use super::*;
use crate::chunk_store::ChunkStore;
use crate::keys::RepositoryKeys;
use crate::repository::Repository;
use crate::storage::contract::{self, Fixture};
use crate::storage::ImmediateSleeper;
use crate::store::Store;

fn config() -> S3Config {
    S3Config {
        retry: RetryPolicy {
            max_attempts: 5,
            ..RetryPolicy::none()
        },
        ..S3Config::from_location("test-bucket/repo/one").expect("valid location")
    }
}

fn connect(fake: &Arc<FakeS3>, config: &S3Config) -> Result<Arc<dyn Backend>> {
    S3Backend::open(fake.clone(), config, Arc::new(ImmediateSleeper))
}

fn transient() -> ApiError {
    ApiError::response(
        ApiErrorKind::Transient,
        503,
        "SlowDown",
        "Please reduce your request rate.",
    )
}

// ---------------------------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------------------------

#[test]
fn a_location_is_a_bucket_and_a_folder_without_slashes_at_the_ends() {
    let cases = [
        ("bucket", "bucket", ""),
        ("bucket/", "bucket", ""),
        ("my-bucket.example/backups", "my-bucket.example", "backups"),
        ("bucket/a/b/", "bucket", "a/b"),
        ("/bucket/a", "bucket", "a"),
    ];
    for (location, bucket, prefix) in cases {
        let (got_bucket, got_prefix) = parse_location(location).expect(location);
        assert_eq!(
            (got_bucket.as_str(), got_prefix.as_str()),
            (bucket, prefix),
            "{location}"
        );
    }
    let config = S3Config::from_location("bucket/a/b").expect("valid");
    assert_eq!(
        (config.bucket.as_str(), config.prefix.as_str()),
        ("bucket", "a/b")
    );
}

#[test]
fn locations_that_cannot_name_a_bucket_and_a_folder_are_refused() {
    let bad = [
        "",
        "/",
        "ab",
        "UPPER/x",
        "bucket/../x",
        "bucket/./x",
        "bucket/a//b",
        "bucket/a b",
        "-bucket/x",
        "bucket-/x",
        "buck_et/x",
        "bucket/a?b",
        "bucket/caf\u{e9}",
    ];
    for location in bad {
        let err = parse_location(location).expect_err(location);
        assert!(matches!(err, Error::InvalidPath(_)), "{location:?}: {err}");
    }
    assert!(
        parse_location(&"a".repeat(64)).is_err(),
        "63 characters at most"
    );
    assert!(parse_location(&"a".repeat(63)).is_ok());
}

#[test]
fn endpoints_must_be_https_unless_they_are_on_this_machine_or_explicitly_allowed() {
    for ok in [
        "https://s3.example.com",
        "https://s3.example.com:9000/some/path",
        "HTTPS://S3.EXAMPLE.COM",
        "http://localhost:9000",
        "http://127.0.0.1:21100",
        "http://127.1.2.3",
        "http://[::1]:9000",
    ] {
        assert!(validate_endpoint(ok, false).is_ok(), "{ok}");
    }
    for plain in [
        "http://s3.example.com",
        "http://10.0.0.5:9000",
        "http://[2001:db8::1]:9000",
    ] {
        let err = validate_endpoint(plain, false).expect_err(plain);
        assert!(err.to_string().contains("plain HTTP"), "{plain}: {err}");
        assert!(
            validate_endpoint(plain, true).is_ok(),
            "{plain} when allowed"
        );
    }
    for bad in [
        "s3.example.com",
        "ftp://s3.example.com",
        "https://",
        "https:///path",
        "file:///x",
    ] {
        assert!(validate_endpoint(bad, true).is_err(), "{bad}");
    }
}

#[test]
fn credentials_in_an_endpoint_are_refused_and_never_echoed() {
    for url in [
        "https://user:hunter2@s3.example.com",
        "http://localhost:9000@evil.example.com",
        "https://AKIAIOSFODNN7EXAMPLE:wJalrXUtnFEMI@s3.example.com/bucket",
    ] {
        let err = validate_endpoint(url, true).expect_err(url);
        let text = err.to_string();
        assert!(
            !text.contains("hunter2") && !text.contains("wJalrXUtnFEMI"),
            "{text}"
        );
        assert!(!text.contains("AKIA"), "{text}");
    }
}

#[test]
fn an_endpoint_is_shown_without_what_stands_before_the_at_sign() {
    assert_eq!(
        redact_endpoint("https://user:pw@host:9000/x?y#z"),
        "https://host:9000/x?y#z"
    );
    assert_eq!(redact_endpoint("https://host:9000"), "https://host:9000");
    assert_eq!(redact_endpoint("not a url"), "<invalid endpoint>");
}

#[test]
fn a_configuration_shows_nothing_secret_when_printed() {
    let config = S3Config {
        endpoint: Some("https://AKIAIOSFODNN7EXAMPLE:secretpass@s3.example.com".to_owned()),
        ..config()
    };
    let shown = format!("{config:?}");
    assert!(
        !shown.contains("secretpass") && !shown.contains("AKIA"),
        "{shown}"
    );
    assert!(shown.contains("https://s3.example.com"), "{shown}");
    assert!(
        shown.contains("test-bucket") && shown.contains("repo/one"),
        "{shown}"
    );
}

#[test]
fn limits_and_timeouts_are_validated() {
    assert!(config().validate().is_ok());
    for requests in [0, MAX_REQUESTS_LIMIT + 1] {
        let bad = S3Config {
            max_concurrent_requests: requests,
            ..config()
        };
        assert!(bad.validate().is_err(), "{requests}");
    }
    let no_timeout = S3Config {
        request_timeout: Duration::ZERO,
        ..config()
    };
    assert!(no_timeout.validate().is_err());
    let plain = S3Config {
        endpoint: Some("http://s3.example.com".to_owned()),
        ..config()
    };
    assert!(plain.validate().is_err());
    assert!(S3Config {
        allow_insecure_http: true,
        ..plain
    }
    .validate()
    .is_ok());
}

#[test]
fn messages_from_the_network_lose_key_ids_control_characters_and_length() {
    let text = redact("denied for AKIAIOSFODNN7EXAMPLE and ASIAIOSFODNN7EXAMPLE\nand more\tstuff");
    assert!(!text.contains("AKIA") && !text.contains("ASIA"), "{text}");
    assert_eq!(text.matches("<access-key-id>").count(), 2, "{text}");
    assert!(!text.contains('\n') && !text.contains('\t'), "{text:?}");
    assert!(text.contains("and more stuff"), "{text}");
    // Ordinary words that merely start alike are left alone.
    assert_eq!(
        redact("AKIAWORD and AKIAIOSFODNN7EXAMPL"),
        "AKIAWORD and AKIAIOSFODNN7EXAMPL"
    );
    let long = redact(&"x ".repeat(2000));
    assert!(long.len() <= 610 && long.ends_with("..."), "{}", long.len());
    assert_eq!(redact(""), "");
}

#[test]
fn without_the_client_feature_or_with_it_a_bad_configuration_is_refused_before_connecting() {
    let bad = S3Config::new("ab");
    let err = S3Backend::connect(bad).expect_err("refused");
    assert!(matches!(err, Error::InvalidPath(_)), "{err}");
}

// ---------------------------------------------------------------------------------------------
// The contract and the mapping of the model onto S3
// ---------------------------------------------------------------------------------------------

#[test]
fn the_s3_backend_passes_the_contract_in_a_folder_with_small_listing_pages() {
    contract::run(&|| {
        let fake = Arc::new(FakeS3::new().with_page_size(2));
        Fixture::new(connect(&fake, &config()).expect("open"))
    });
}

#[test]
fn the_s3_backend_passes_the_contract_at_the_root_of_a_bucket() {
    contract::run(&|| {
        let fake = Arc::new(FakeS3::new().with_page_size(3));
        let root = S3Config {
            retry: config().retry,
            ..S3Config::new("test-bucket")
        };
        Fixture::new(connect(&fake, &root).expect("open"))
    });
}

#[test]
fn objects_live_under_the_prefix_and_neighbors_are_invisible() {
    let fake = Arc::new(FakeS3::new());
    fake.insert("repo/other/config", b"someone else's");
    fake.insert("repo/one-two/config", b"a sibling folder");
    fake.insert("elsewhere", b"outside");
    fake.insert("repo/one/", b"");
    let backend = connect(&fake, &config()).expect("open");

    backend
        .put("config", b"mine", PutMode::Create)
        .expect("create");
    backend
        .put("data/ab/obj", b"x", PutMode::Create)
        .expect("create");
    assert_eq!(
        fake.object("repo/one/config").as_deref(),
        Some(b"mine".as_slice())
    );
    assert!(fake.object("config").is_none());

    let root: Vec<(String, DirEntryKind)> = backend
        .list_dir("")
        .expect("list")
        .into_iter()
        .map(|entry| (entry.name, entry.kind))
        .collect();
    assert_eq!(
        root,
        [
            ("config".to_owned(), DirEntryKind::File),
            ("data".to_owned(), DirEntryKind::Directory)
        ],
        "neighbors and the folder marker object are not entries"
    );
    assert!(backend.exists("config").expect("exists"));
    assert!(!backend.exists("other/config").expect("exists"));
    assert_eq!(backend.root().to_str(), Some("s3://test-bucket/repo/one"));
}

#[test]
fn a_big_directory_is_listed_completely_across_pages() {
    let fake = Arc::new(FakeS3::new().with_page_size(3));
    let backend = connect(&fake, &config()).expect("open");
    for index in 0..25 {
        backend
            .put(&format!("data/ab/{index:02}"), &[b'x'; 7], PutMode::Create)
            .expect("create");
    }
    backend
        .put("data/cd/00", b"y", PutMode::Create)
        .expect("create");
    let shard = backend.list_dir("data/ab").expect("list");
    let names: Vec<&str> = shard.iter().map(|entry| entry.name.as_str()).collect();
    let expected: Vec<String> = (0..25).map(|index| format!("{index:02}")).collect();
    assert_eq!(
        names,
        expected.iter().map(String::as_str).collect::<Vec<_>>()
    );
    assert!(shard
        .iter()
        .all(|entry| entry.kind == DirEntryKind::File && entry.len == 7));
    let data: Vec<&str> = ["ab", "cd"].to_vec();
    let shards = backend.list_dir("data").expect("list");
    assert_eq!(
        shards
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        data
    );
    assert!(shards
        .iter()
        .all(|entry| entry.kind == DirEntryKind::Directory));
    assert!(
        fake.calls("list") >= 9,
        "25 entries in pages of 3 took several calls"
    );
}

#[test]
fn a_listing_that_never_advances_is_an_error_not_an_endless_loop() {
    #[derive(Debug)]
    struct Stuck;
    impl ObjectApi for Stuck {
        fn head_bucket(&self) -> ApiResult<()> {
            Ok(())
        }
        fn head(&self, _key: &str) -> ApiResult<HeadInfo> {
            Err(ApiError::new(ApiErrorKind::NotFound, "no"))
        }
        fn get(&self, _key: &str, _range: Option<(u64, u64)>, _max: u64) -> ApiResult<GetOutput> {
            Err(ApiError::new(ApiErrorKind::NotFound, "no"))
        }
        fn put(&self, _key: &str, _data: &[u8], _condition: &Condition) -> ApiResult<String> {
            Ok("\"1\"".to_owned())
        }
        fn delete(&self, _key: &str) -> ApiResult<()> {
            Ok(())
        }
        fn list(
            &self,
            _prefix: &str,
            _delimiter: bool,
            _token: Option<&str>,
        ) -> ApiResult<ListPage> {
            Ok(ListPage {
                next_token: Some("same".to_owned()),
                ..ListPage::default()
            })
        }
    }
    let backend = S3Backend::with_api(Arc::new(Stuck), &config()).expect("backend");
    let err = backend.list_dir("data").expect_err("must stop");
    assert!(err.to_string().contains("continuation token"), "{err}");
}

#[test]
fn prefix_reads_handle_empty_objects_and_the_limit_is_enforced_by_the_service_call() {
    let fake = Arc::new(FakeS3::new());
    let backend = connect(&fake, &config()).expect("open");
    backend.put("empty", b"", PutMode::Create).expect("create");
    backend
        .put("ten", b"0123456789", PutMode::Create)
        .expect("create");
    assert_eq!(
        backend.get_prefix("empty", 12).expect("prefix"),
        (Vec::new(), 0)
    );
    assert_eq!(
        backend.get_prefix("ten", 4).expect("prefix"),
        (b"0123".to_vec(), 10)
    );
    assert_eq!(
        backend.get_prefix("ten", 0).expect("prefix"),
        (Vec::new(), 10)
    );
    assert_eq!(
        backend.get_prefix("ten", 99).expect("prefix"),
        (b"0123456789".to_vec(), 10)
    );
    assert!(matches!(
        backend.get("ten", 9),
        Err(Error::TooLarge { max_len: 9, .. })
    ));
    assert_eq!(
        backend.get("ten", 10).expect("at the limit").data,
        b"0123456789"
    );
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

#[test]
fn service_errors_become_the_errors_the_repository_acts_on() {
    let fake = Arc::new(FakeS3::new());
    let backend = S3Backend::with_api(fake.clone(), &config()).expect("backend");

    fake.fail_next("head", 1, &transient());
    let err = backend.stat("x").expect_err("transient");
    assert!(err.is_transient() && !err.is_not_found(), "{err}");
    assert!(err.to_string().contains("SlowDown"), "{err}");

    let err = backend.stat("x").expect_err("missing");
    assert!(err.is_not_found() && !err.is_transient(), "{err}");

    let refused = ApiError::response(
        ApiErrorKind::Denied,
        403,
        "InvalidAccessKeyId",
        "The AWS Access Key Id you provided (AKIAIOSFODNN7EXAMPLE) does not exist in our records.",
    );
    fake.fail_next("get", 1, &refused);
    let err = backend.get("x", 10).expect_err("denied");
    assert!(
        matches!(
            err,
            Error::Backend {
                kind: BackendErrorKind::Permission,
                ..
            }
        ),
        "{err}"
    );
    let text = err.to_string();
    assert!(!text.contains("AKIAIOSFODNN7EXAMPLE"), "{text}");
    assert!(text.contains("check the AWS credentials"), "{text}");
    assert!(!err.is_transient());

    let odd = ApiError::response(ApiErrorKind::Other, 400, "InvalidArgument", "bad");
    fake.fail_next("delete", 1, &odd);
    let err = backend.delete("x").expect_err("other");
    assert!(!err.is_transient() && !err.is_not_found(), "{err}");
}

#[test]
fn a_missing_bucket_is_reported_as_such_and_not_retried() {
    let fake = Arc::new(FakeS3::new().without_bucket());
    let err = connect(&fake, &config()).expect_err("no bucket");
    let text = err.to_string();
    assert!(text.contains("bucket does not exist"), "{text}");
    assert!(text.contains("test-bucket"), "{text}");
    assert!(!err.is_transient());
    assert_eq!(
        fake.calls("head_bucket"),
        1,
        "waiting will not create a bucket"
    );
}

#[test]
fn transient_failures_while_connecting_are_retried_and_then_given_up_on() {
    let fake = Arc::new(FakeS3::new());
    fake.fail_next("head_bucket", 2, &transient());
    connect(&fake, &config()).expect("the third attempt works");
    assert_eq!(fake.calls("head_bucket"), 3);

    let down = Arc::new(FakeS3::new());
    down.fail_next("head_bucket", 100, &transient());
    let short = S3Config {
        retry: RetryPolicy {
            max_attempts: 3,
            ..RetryPolicy::none()
        },
        ..config()
    };
    let err = connect(&down, &short).expect_err("the service stays down");
    assert!(err.is_transient(), "{err}");
    assert_eq!(
        down.calls("head_bucket"),
        3,
        "exactly the allowed number of attempts"
    );
}

#[test]
fn read_only_credentials_connect_and_read_but_cannot_write_and_are_not_retried() {
    let fake = Arc::new(FakeS3::new().read_only());
    fake.insert("repo/one/config", b"hello");
    let backend = connect(&fake, &config()).expect("a read-only user can connect");
    assert_eq!(backend.get("config", 100).expect("read").data, b"hello");
    assert!(backend.capabilities().require_repository_support().is_ok());

    let puts = fake.calls("put");
    let err = backend
        .put("new", b"x", PutMode::Create)
        .expect_err("denied");
    assert!(
        matches!(
            err,
            Error::Backend {
                kind: BackendErrorKind::Permission,
                ..
            }
        ),
        "{err}"
    );
    assert_eq!(fake.calls("put"), puts + 1, "a refusal is not retried");
    assert_eq!(
        fake.keys(),
        ["repo/one/config"],
        "the probe left nothing behind either"
    );
}

// ---------------------------------------------------------------------------------------------
// Conditional writes: verified, and used
// ---------------------------------------------------------------------------------------------

#[test]
fn connecting_proves_that_conditions_work_and_leaves_no_trace() {
    let fake = Arc::new(FakeS3::new());
    let backend = connect(&fake, &config()).expect("open");
    let caps = backend.capabilities();
    assert_eq!(caps.create_if_absent, Guarantee::Global);
    assert_eq!(caps.replace_if_unchanged, Guarantee::Global);
    assert!(
        fake.keys().is_empty(),
        "the probe object was removed: {:?}",
        fake.keys()
    );
    assert!(
        fake.calls("put") >= 4,
        "the probe wrote and tried the conditions"
    );
}

#[tokio::test]
async fn a_service_that_ignores_if_none_match_cannot_hold_a_repository() {
    let fake = Arc::new(FakeS3::new().ignoring_if_none_match());
    let backend = connect(&fake, &config()).expect("connecting works; using it does not");
    assert_eq!(backend.capabilities().create_if_absent, Guarantee::None);
    assert!(fake.keys().is_empty(), "the probe object was removed");

    let repo = Repository::with_backend(backend);
    let err = crate::testing::init(&repo)
        .await
        .expect_err("must be refused");
    assert!(matches!(err, Error::UnsupportedBackend(_)), "{err}");
    assert!(err.to_string().contains("conditional create"), "{err}");
    assert!(fake.keys().is_empty(), "nothing was written");
}

#[tokio::test]
async fn a_service_that_ignores_if_match_can_hold_a_repository_but_not_transactions() {
    let fake = Arc::new(FakeS3::new().ignoring_if_match());
    let backend = connect(&fake, &config()).expect("open");
    let caps = backend.capabilities();
    assert_eq!(
        (caps.create_if_absent, caps.replace_if_unchanged),
        (Guarantee::Global, Guarantee::None)
    );

    let repo = Repository::with_backend(backend.clone());
    crate::testing::init(&repo)
        .await
        .expect("a repository needs only conditional create");
    let err = Store::new(backend)
        .update("notes", 100, |_| Ok(b"x".to_vec()))
        .expect_err("no compare-and-swap");
    assert!(matches!(err, Error::UnsupportedBackend(_)), "{err}");
}

#[tokio::test]
async fn a_service_that_creates_on_if_match_of_a_missing_object_gets_no_conditional_replace() {
    // MinIO of April 2025 does this; the storage contract found it. Nothing in a repository uses
    // a conditional replace, so the repository is still accepted, but the guarantee is not
    // declared and the probe leaves nothing behind.
    let fake = Arc::new(FakeS3::new().creating_on_if_match());
    let backend = connect(&fake, &config()).expect("open");
    let caps = backend.capabilities();
    assert_eq!(
        (caps.create_if_absent, caps.replace_if_unchanged),
        (Guarantee::Global, Guarantee::None)
    );
    assert!(
        fake.keys().is_empty(),
        "the probe objects were removed: {:?}",
        fake.keys()
    );

    let repo = Repository::with_backend(backend);
    crate::testing::init(&repo)
        .await
        .expect("a repository needs only conditional create");
}

#[test]
fn conditions_go_out_as_the_headers_s3_expects() {
    let fake = Arc::new(FakeS3::new());
    let backend = connect(&fake, &config()).expect("open");
    let first = backend
        .put("meta", b"one", PutMode::Create)
        .expect("create");
    assert!(matches!(
        backend.put("meta", b"two", PutMode::Create),
        Err(Error::AlreadyExists { .. })
    ));
    let second = backend
        .put("meta", b"two", PutMode::Update(first.clone()))
        .expect("update");
    assert_ne!(first, second);
    assert!(matches!(
        backend.put("meta", b"three", PutMode::Update(first)),
        Err(Error::PreconditionFailed { .. })
    ));
    assert!(matches!(
        backend.put("absent", b"x", PutMode::Update(second.clone())),
        Err(Error::PreconditionFailed { .. })
    ));
    assert_eq!(backend.stat("meta").expect("stat").version, second);
    assert_eq!(backend.get("meta", 100).expect("get").data, b"two");
}

// ---------------------------------------------------------------------------------------------
// Retries, lost answers, duplicates, resumption
// ---------------------------------------------------------------------------------------------

#[test]
fn a_failing_service_is_retried_with_the_attempts_the_policy_allows() {
    let fake = Arc::new(FakeS3::new());
    let backend = connect(&fake, &config()).expect("open");
    backend
        .put("obj", b"data", PutMode::Create)
        .expect("create");

    fake.fail_next("get", 3, &transient());
    let before = fake.calls("get");
    assert_eq!(
        backend
            .get("obj", 100)
            .expect("the fourth attempt works")
            .data,
        b"data"
    );
    assert_eq!(fake.calls("get") - before, 4);

    fake.fail_next("get", 100, &transient());
    let before = fake.calls("get");
    let err = backend
        .get("obj", 100)
        .expect_err("five attempts are not enough");
    assert!(err.is_transient(), "{err}");
    assert_eq!(fake.calls("get") - before, 5);
}

#[test]
fn a_create_whose_answer_was_lost_is_a_success_and_a_lost_race_is_still_a_conflict() {
    let fake = Arc::new(FakeS3::new());
    let backend = connect(&fake, &config()).expect("open");

    fake.lose_answer_next("put", 1, &transient());
    let version = backend
        .put("snapshot", b"mine", PutMode::Create)
        .expect("it landed");
    assert_eq!(backend.stat("snapshot").expect("stat").version, version);
    assert_eq!(
        fake.object("repo/one/snapshot").as_deref(),
        Some(b"mine".as_slice())
    );

    fake.insert("repo/one/taken", b"theirs");
    fake.fail_next("put", 1, &transient());
    let err = backend
        .put("taken", b"mine", PutMode::Create)
        .expect_err("lost the race");
    assert!(matches!(err, Error::AlreadyExists { .. }), "{err}");
    assert_eq!(
        fake.object("repo/one/taken").as_deref(),
        Some(b"theirs".as_slice())
    );
}

#[test]
fn a_delete_whose_answer_was_lost_is_a_success() {
    let fake = Arc::new(FakeS3::new());
    let backend = connect(&fake, &config()).expect("open");
    backend.put("gone", b"x", PutMode::Create).expect("create");
    fake.lose_answer_next("delete", 1, &transient());
    backend.delete("gone").expect("it was deleted");
    assert!(!backend.exists("gone").expect("exists"));
    backend
        .delete("never-there")
        .expect("deleting nothing is fine");
}

#[test]
fn concurrent_uploads_of_the_same_chunk_store_it_once_and_a_second_run_uploads_nothing() {
    let fake = Arc::new(FakeS3::new());
    let backend = connect(&fake, &config()).expect("open");
    let keys = RepositoryKeys::generate().expect("keys");
    let chunks = ChunkStore::new(Store::new(backend), &keys).expect("chunk store");
    let plaintext: Vec<u8> = (0..20_000_u32).map(|i| (i % 253) as u8).collect();

    let outcomes: Vec<bool> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|_| scope.spawn(|| chunks.put(&plaintext).expect("put").is_new()))
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("a worker panicked"))
            .collect()
    });
    assert_eq!(
        outcomes.iter().filter(|new| **new).count(),
        1,
        "exactly one upload counts"
    );
    let objects: Vec<String> = fake
        .keys()
        .into_iter()
        .filter(|key| key.contains("/data/"))
        .collect();
    assert_eq!(objects.len(), 1, "{objects:?}");

    // The same chunk again, as when an interrupted backup is run once more: found, not uploaded.
    let puts = fake.calls("put");
    assert!(!chunks.put(&plaintext).expect("put").is_new());
    assert_eq!(fake.calls("put"), puts, "nothing was uploaded");
}

// ---------------------------------------------------------------------------------------------
// Bounded resources
// ---------------------------------------------------------------------------------------------

#[test]
fn requests_in_flight_never_exceed_the_configured_limit() {
    let fake = Arc::new(FakeS3::new().with_delay(Duration::from_millis(5)));
    let limited = S3Config {
        max_concurrent_requests: 3,
        ..config()
    };
    let backend = S3Backend::with_api(fake.clone(), &limited).expect("backend");
    std::thread::scope(|scope| {
        for _ in 0..12 {
            scope.spawn(|| {
                for _ in 0..4 {
                    assert!(!backend.exists("absent").expect("exists"));
                }
            });
        }
    });
    assert_eq!(fake.calls("head"), 48);
    assert!(fake.max_in_flight() <= 3, "{}", fake.max_in_flight());
    assert!(
        fake.max_in_flight() >= 2,
        "the calls should have overlapped: {}",
        fake.max_in_flight()
    );
}

#[test]
fn a_limit_of_one_serializes_everything() {
    let fake = Arc::new(FakeS3::new().with_delay(Duration::from_millis(2)));
    let single = S3Config {
        max_concurrent_requests: 1,
        ..config()
    };
    let backend = S3Backend::with_api(fake.clone(), &single).expect("backend");
    std::thread::scope(|scope| {
        for _ in 0..6 {
            scope.spawn(|| backend.exists("absent").expect("exists"));
        }
    });
    assert_eq!(fake.max_in_flight(), 1);
}
