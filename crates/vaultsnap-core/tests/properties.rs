//! Property-based tests of the invariants everything else rests on, with inputs nobody chose:
//!
//! * **Crypto.** Sealed data opens only under the same key and context, and any change to it is
//!   detected; purpose keys are deterministic and independent.
//! * **Retention.** Every snapshot gets exactly one decision, the newest is always kept, the
//!   result does not depend on input order, and each rule keeps what it says it keeps.
//! * **Parsers.** The `config` parser never panics and reads back what it writes; arbitrary
//!   bytes in place of a chunk object, a snapshot file, or a key file are never accepted.
//! * **Chunk store.** What is put comes back, a second put is a duplicate, and one changed byte
//!   in a stored object is never decoded into wrong data.
//! * **Storage.** The in-memory and directory backends behave like a map under random sequences
//!   of operations, and a name that passes validation can never leave the repository.
//! * **Round trip.** Whatever files a backup is given, a restore returns the same bytes.
//!
//! Chunking has its own properties in `chunking.rs`. A failing case is shrunk to a small one and
//! saved by proptest (in `proptest-regressions/`, which is git-ignored scratch output).

use std::collections::BTreeMap;
use std::fs;
use std::future::Future;
use std::path::{Component, Path};
use std::sync::Arc;
use std::time::Duration;

use proptest::collection::vec;
use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestRunner};
use tempfile::TempDir;
use vaultsnap_core::crypto::{self, KeyPurpose, KEY_LEN, NONCE_LEN, TAG_LEN};
use vaultsnap_core::scan::Timestamp;
use vaultsnap_core::storage::{validate_name, DirEntryKind, PutMode, Version};
use vaultsnap_core::{
    Backend, BackupOptions, Candidate, ChunkId, ChunkerConfig, Error, KdfParams, KeepReason,
    LocalBackend, MemoryBackend, Passphrase, Repository, RepositoryConfig, RepositoryKeys,
    RestoreOptions, RetentionPolicy, SnapshotId, SnapshotRef,
};

fn fail(error: impl std::fmt::Display) -> TestCaseError {
    TestCaseError::fail(error.to_string())
}

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("build a runtime")
        .block_on(future)
}

fn passphrase() -> Passphrase {
    Passphrase::new(b"correct horse battery staple".to_vec()).expect("valid passphrase")
}

fn kdf() -> KdfParams {
    KdfParams {
        m_cost_kib: 64,
        t_cost: 1,
        p_cost: 1,
    }
}

/// A fresh in-memory repository, initialized and unlocked.
struct Fixture {
    backend: Arc<MemoryBackend>,
    repo: Repository,
    keys: RepositoryKeys,
}

fn fixture() -> Fixture {
    let backend = Arc::new(MemoryBackend::new());
    let repo = Repository::with_backend(backend.clone());
    let keys = block_on(async {
        repo.init_with_kdf(&passphrase(), kdf())
            .await
            .expect("init");
        repo.unlock(&passphrase()).await.expect("unlock")
    });
    Fixture {
        backend,
        repo,
        keys,
    }
}

// ---------------------------------------------------------------------------------------------
// Cryptography
// ---------------------------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn sealed_data_opens_only_under_the_same_key_and_context(
        key in any::<[u8; KEY_LEN]>(),
        other in any::<[u8; KEY_LEN]>(),
        aad in vec(any::<u8>(), 0..64),
        plaintext in vec(any::<u8>(), 0..2048),
    ) {
        let sealed = crypto::seal(&key, &aad, &plaintext).expect("seal");
        prop_assert_eq!(sealed.len(), NONCE_LEN + plaintext.len() + TAG_LEN);
        let opened = crypto::open(&key, &aad, &sealed).expect("open");
        prop_assert_eq!(opened.as_slice(), plaintext.as_slice());
        if other != key {
            prop_assert!(crypto::open(&other, &aad, &sealed).is_err());
        }
        let mut other_aad = aad.clone();
        other_aad.push(0);
        prop_assert!(crypto::open(&key, &other_aad, &sealed).is_err());
    }

    #[test]
    fn any_change_to_sealed_bytes_is_detected(
        key in any::<[u8; KEY_LEN]>(),
        plaintext in vec(any::<u8>(), 0..512),
        position in 0usize..10_000,
        flip in 1u8..=255,
        cut in 0usize..10_000,
    ) {
        let sealed = crypto::seal(&key, b"context", &plaintext).expect("seal");

        let mut flipped = sealed.clone();
        let at = position % flipped.len();
        flipped[at] ^= flip;
        prop_assert!(crypto::open(&key, b"context", &flipped).is_err());

        let mut shortened = sealed.clone();
        shortened.truncate(cut % sealed.len());
        prop_assert!(crypto::open(&key, b"context", &shortened).is_err());

        let mut extended = sealed;
        extended.push(flip);
        prop_assert!(crypto::open(&key, b"context", &extended).is_err());
    }

    #[test]
    fn two_seals_of_the_same_data_differ(
        key in any::<[u8; KEY_LEN]>(),
        plaintext in vec(any::<u8>(), 0..256),
    ) {
        let first = crypto::seal(&key, b"", &plaintext).expect("seal");
        let second = crypto::seal(&key, b"", &plaintext).expect("seal");
        prop_assert_ne!(first, second, "a nonce was reused");
    }

    #[test]
    fn purpose_keys_are_deterministic_and_independent(
        master in any::<[u8; KEY_LEN]>(),
        other in any::<[u8; KEY_LEN]>(),
    ) {
        let derive = |master: &[u8; KEY_LEN], purpose: KeyPurpose| {
            *crypto::derive_key(master, purpose).expect("derive")
        };
        for purpose in KeyPurpose::ALL {
            prop_assert_eq!(derive(&master, purpose), derive(&master, purpose));
            if other != master {
                prop_assert_ne!(derive(&master, purpose), derive(&other, purpose));
            }
            for different in KeyPurpose::ALL {
                if different != purpose {
                    prop_assert_ne!(derive(&master, purpose), derive(&master, different));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Retention
// ---------------------------------------------------------------------------------------------

fn candidates(times: &[(i64, i64)]) -> Vec<Candidate> {
    times
        .iter()
        .map(|(secs, nanos)| Candidate {
            id: SnapshotId::generate().expect("randomness"),
            created: Timestamp::new(*secs, *nanos),
        })
        .collect()
}

fn time_lists() -> impl Strategy<Value = Vec<(i64, i64)>> {
    vec((0_i64..4_000_000_000, 0_i64..1_000_000_000), 1..30)
}

fn policies() -> impl Strategy<Value = RetentionPolicy> {
    let rule = || proptest::option::of(1_u32..10);
    (rule(), rule(), rule(), rule(), rule())
        .prop_map(
            |(keep_last, keep_daily, keep_weekly, keep_monthly, keep_yearly)| RetentionPolicy {
                keep_last,
                keep_daily,
                keep_weekly,
                keep_monthly,
                keep_yearly,
            },
        )
        .prop_filter("at least one rule", |policy| policy.validate().is_ok())
}

fn newest_first(decisions: &[vaultsnap_core::Decision]) -> Vec<&vaultsnap_core::Decision> {
    let mut sorted: Vec<_> = decisions.iter().collect();
    sorted.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| b.id.cmp(&a.id)));
    sorted
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn every_snapshot_gets_one_decision_and_the_newest_is_always_kept(
        times in time_lists(),
        policy in policies(),
    ) {
        let input = candidates(&times);
        let decisions = policy.select(&input).expect("valid policy");

        prop_assert_eq!(decisions.len(), input.len());
        let mut wanted: Vec<_> = input.iter().map(|c| c.id.clone()).collect();
        let mut got: Vec<_> = decisions.iter().map(|d| d.id.clone()).collect();
        wanted.sort();
        got.sort();
        prop_assert_eq!(got, wanted);

        let ordered = decisions
            .windows(2)
            .all(|pair| (pair[0].created, &pair[0].id) <= (pair[1].created, &pair[1].id));
        prop_assert!(ordered, "decisions are listed oldest first");

        let newest = *newest_first(&decisions).first().expect("not empty");
        prop_assert!(newest.is_kept(), "the newest snapshot must never be removed");
    }

    #[test]
    fn the_decisions_do_not_depend_on_the_order_of_the_input(
        times in time_lists(),
        policy in policies(),
    ) {
        let input = candidates(&times);
        let mut reversed = input.clone();
        reversed.reverse();
        prop_assert_eq!(
            policy.select(&input).expect("valid"),
            policy.select(&reversed).expect("valid")
        );
    }

    #[test]
    fn keep_last_keeps_exactly_the_newest_n(times in time_lists(), count in 1_u32..12) {
        let input = candidates(&times);
        let policy = RetentionPolicy {
            keep_last: Some(count),
            ..RetentionPolicy::default()
        };
        let decisions = policy.select(&input).expect("valid");
        let expected = usize::try_from(count).expect("small").min(input.len());
        let kept: Vec<_> = newest_first(&decisions)
            .into_iter()
            .filter(|decision| decision.is_kept())
            .collect();
        prop_assert_eq!(kept.len(), expected);
        // Kept ones are the newest: no removed snapshot is newer than a kept one.
        let removed_newer = newest_first(&decisions)
            .into_iter()
            .take(expected)
            .any(|decision| !decision.is_kept());
        prop_assert!(!removed_newer);
    }

    #[test]
    fn a_larger_count_never_keeps_less(times in time_lists(), count in 1_u32..10) {
        let input = candidates(&times);
        let kept_with = |count: u32| -> Vec<SnapshotId> {
            let policy = RetentionPolicy {
                keep_last: Some(count),
                keep_daily: Some(count),
                ..RetentionPolicy::default()
            };
            policy
                .select(&input)
                .expect("valid")
                .into_iter()
                .filter(|decision| decision.is_kept())
                .map(|decision| decision.id)
                .collect()
        };
        let fewer = kept_with(count);
        let more = kept_with(count + 1);
        prop_assert!(fewer.iter().all(|id| more.contains(id)));
    }

    #[test]
    fn the_daily_rule_keeps_the_newest_snapshot_of_each_of_at_most_n_days(
        times in time_lists(),
        count in 1_u32..10,
    ) {
        let input = candidates(&times);
        let policy = RetentionPolicy {
            keep_daily: Some(count),
            ..RetentionPolicy::default()
        };
        let decisions = policy.select(&input).expect("valid");
        let day = |decision: &vaultsnap_core::Decision| decision.created.secs.div_euclid(86_400);
        let daily: Vec<_> = decisions
            .iter()
            .filter(|decision| decision.keep.contains(&KeepReason::Daily))
            .collect();
        prop_assert!(daily.len() <= usize::try_from(count).expect("small"));
        for kept in &daily {
            let later_same_day = decisions.iter().any(|other| {
                day(other) == day(kept) && (other.created, &other.id) > (kept.created, &kept.id)
            });
            prop_assert!(!later_same_day, "a kept day-snapshot must be the newest of its day");
        }
        let mut days = std::collections::BTreeSet::new();
        for kept in &daily {
            prop_assert!(days.insert(day(kept)), "two snapshots kept for one day");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Parsers
// ---------------------------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn the_config_parser_never_panics_and_reads_back_what_it_writes(
        bytes in vec(any::<u8>(), 0..300),
        nibbles in vec(0_u8..16, 32),
        created in any::<u64>(),
        position in 0usize..1000,
        flip in 1u8..=255,
    ) {
        let _ = RepositoryConfig::parse(&bytes);

        let id: String = nibbles
            .iter()
            .map(|nibble| char::from_digit(u32::from(*nibble), 16).expect("a hex digit"))
            .collect();
        let config = RepositoryConfig::new(id, created);
        let written = config.to_bytes();
        prop_assert_eq!(RepositoryConfig::parse(&written).expect("own output parses"), config);

        let mut damaged = written.clone();
        let at = position % damaged.len();
        damaged[at] ^= flip;
        let _ = RepositoryConfig::parse(&damaged);
    }

    #[test]
    fn validated_names_cannot_leave_the_repository(
        picks in vec(0usize..12, 0..14),
    ) {
        const ALPHABET: [&str; 12] =
            ["a", "b", ".", "..", "/", "\\", "\0", "-", "_", "é", " ", "%"];
        let name: String = picks.iter().map(|pick| ALPHABET[*pick]).collect();
        if validate_name(&name).is_ok() {
            let confined = Path::new(&name)
                .components()
                .all(|component| matches!(component, Component::Normal(_)));
            prop_assert!(confined, "{name:?} was accepted but is not a plain relative path");
            prop_assert!(!name.starts_with('.') && !name.contains("/."));
            prop_assert!(!name.contains('\0') && !name.contains('\\') && !name.is_empty());
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Chunk store, snapshot files, and key files
// ---------------------------------------------------------------------------------------------

fn object_name(id: &ChunkId) -> String {
    let hex = id.to_hex();
    format!("data/{}/{hex}", &hex[..2])
}

#[test]
fn chunks_round_trip_deduplicate_and_a_changed_byte_is_never_decoded() {
    let fixture = fixture();
    let store = fixture
        .repo
        .chunk_store(&fixture.keys)
        .expect("chunk store");
    let mut runner = TestRunner::new(Config::with_cases(96));
    let strategy = (vec(any::<u8>(), 1..20_000), 0usize..100_000, 1u8..=255);
    runner
        .run(
            &strategy,
            |(data, position, flip)| -> Result<(), TestCaseError> {
                let put = store.put(&data).map_err(fail)?;
                prop_assert_eq!(put.id, store.chunk_id(&data));
                prop_assert_eq!(store.get(&put.id).map_err(fail)?, data.clone());
                prop_assert!(
                    !store.put(&data).map_err(fail)?.is_new(),
                    "a second put is a duplicate"
                );

                let name = object_name(&put.id);
                let original = fixture.backend.get(&name, 1 << 26).map_err(fail)?.data;
                let mut damaged = original.clone();
                let at = position % damaged.len();
                damaged[at] ^= flip;
                fixture
                    .backend
                    .put(&name, &damaged, PutMode::Overwrite)
                    .map_err(fail)?;
                prop_assert!(
                    store.get(&put.id).is_err(),
                    "damage at byte {at} was decoded"
                );

                fixture
                    .backend
                    .put(&name, &original, PutMode::Overwrite)
                    .map_err(fail)?;
                prop_assert_eq!(store.get(&put.id).map_err(fail)?, data);
                Ok(())
            },
        )
        .expect("the chunk store keeps its promises");
}

#[test]
fn different_data_has_different_ids_and_the_same_data_the_same() {
    let fixture = fixture();
    let store = fixture
        .repo
        .chunk_store(&fixture.keys)
        .expect("chunk store");
    let mut runner = TestRunner::new(Config::with_cases(128));
    let strategy = (vec(any::<u8>(), 0..300), vec(any::<u8>(), 0..300));
    runner
        .run(&strategy, |(one, two)| -> Result<(), TestCaseError> {
            prop_assert_eq!(store.chunk_id(&one), store.chunk_id(&one));
            if one != two {
                prop_assert_ne!(store.chunk_id(&one), store.chunk_id(&two));
            }
            Ok(())
        })
        .expect("ids are content addresses");
}

#[test]
fn arbitrary_bytes_in_place_of_stored_objects_are_never_accepted() {
    let fixture = fixture();
    let store = fixture
        .repo
        .chunk_store(&fixture.keys)
        .expect("chunk store");
    let snapshots = block_on(fixture.repo.snapshot_store(&fixture.keys)).expect("snapshot store");
    let mut runner = TestRunner::new(Config::with_cases(128));
    let strategy = (any::<[u8; 32]>(), vec(any::<u8>(), 0..600));
    runner
        .run(
            &strategy,
            |(id_bytes, garbage)| -> Result<(), TestCaseError> {
                let id = ChunkId::from_bytes(id_bytes);
                let name = object_name(&id);
                fixture
                    .backend
                    .put(&name, &garbage, PutMode::Overwrite)
                    .map_err(fail)?;
                prop_assert!(store.get(&id).is_err(), "garbage decoded as a chunk");
                let _ = store.peek(&id);
                fixture.backend.delete(&name).map_err(fail)?;

                let snapshot = SnapshotId::generate().map_err(fail)?;
                let file = format!("snapshots/{}", snapshot.as_str());
                fixture
                    .backend
                    .put(&file, &garbage, PutMode::Overwrite)
                    .map_err(fail)?;
                prop_assert!(
                    snapshots.load(&snapshot).is_err(),
                    "garbage loaded as a snapshot"
                );
                let listing = snapshots.list().map_err(fail)?;
                let entry = listing.iter().find(|entry| entry.id == snapshot);
                prop_assert!(entry.is_some_and(|entry| entry.summary.is_err()));
                fixture.backend.delete(&file).map_err(fail)?;
                Ok(())
            },
        )
        .expect("nothing but real objects is accepted");
}

#[test]
fn arbitrary_bytes_in_place_of_the_key_file_never_unlock() {
    let fixture = fixture();
    let entries = fixture.backend.list_dir("keys").expect("list the keys");
    let name = format!("keys/{}", entries.first().expect("a key file").name);
    let original = fixture
        .backend
        .get(&name, 1 << 20)
        .expect("read the key file")
        .data;
    let mut runner = TestRunner::new(Config::with_cases(64));
    runner
        .run(
            &vec(any::<u8>(), 0..600),
            |garbage| -> Result<(), TestCaseError> {
                fixture
                    .backend
                    .put(&name, &garbage, PutMode::Overwrite)
                    .map_err(fail)?;
                prop_assert!(block_on(fixture.repo.unlock(&passphrase())).is_err());
                fixture
                    .backend
                    .put(&name, &original, PutMode::Overwrite)
                    .map_err(fail)?;
                prop_assert!(block_on(fixture.repo.unlock(&passphrase())).is_ok());
                Ok(())
            },
        )
        .expect("a key file that is not the real one never unlocks");
}

// ---------------------------------------------------------------------------------------------
// Backends behave like a map
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Op {
    Create(usize, Vec<u8>),
    Overwrite(usize, Vec<u8>),
    Update(usize, Vec<u8>),
    Delete(usize),
    Get(usize),
    List,
}

const NAMES: [&str; 4] = ["d/a", "d/b", "d/c", "d/d"];

fn op_sequences() -> impl Strategy<Value = Vec<Op>> {
    let name = 0_usize..NAMES.len();
    let data = vec(any::<u8>(), 0..48);
    let op = prop_oneof![
        (name.clone(), data.clone()).prop_map(|(name, data)| Op::Create(name, data)),
        (name.clone(), data.clone()).prop_map(|(name, data)| Op::Overwrite(name, data)),
        (name.clone(), data).prop_map(|(name, data)| Op::Update(name, data)),
        name.clone().prop_map(Op::Delete),
        name.prop_map(Op::Get),
        Just(Op::List),
    ];
    vec(op, 1..40)
}

/// Applies `ops` to `backend` and to a plain map, and checks that they always agree.
fn behaves_like_a_map(backend: &dyn Backend, ops: &[Op]) -> Result<(), TestCaseError> {
    backend.create_root().map_err(fail)?;
    backend.create_dir("d").map_err(fail)?;
    let mut model: BTreeMap<&str, Vec<u8>> = BTreeMap::new();
    for op in ops {
        match op {
            Op::Create(index, data) => {
                let name = NAMES[*index];
                let result = backend.put(name, data, PutMode::Create);
                if model.contains_key(name) {
                    prop_assert!(
                        matches!(result, Err(Error::AlreadyExists { .. })),
                        "{result:?}"
                    );
                } else {
                    result.map_err(fail)?;
                    model.insert(name, data.clone());
                }
            }
            Op::Overwrite(index, data) => {
                let name = NAMES[*index];
                backend.put(name, data, PutMode::Overwrite).map_err(fail)?;
                model.insert(name, data.clone());
            }
            Op::Update(index, data) => {
                let name = NAMES[*index];
                match (model.contains_key(name), backend.stat(name)) {
                    (true, Ok(info)) => {
                        backend
                            .put(name, data, PutMode::Update(info.version))
                            .map_err(fail)?;
                        model.insert(name, data.clone());
                    }
                    (false, Err(err)) => {
                        prop_assert!(err.is_not_found(), "{err}");
                        let stale = Version::new("\"no such version\"");
                        let result = backend.put(name, data, PutMode::Update(stale));
                        prop_assert!(
                            matches!(result, Err(Error::PreconditionFailed { .. })),
                            "{result:?}"
                        );
                    }
                    (present, stat) => {
                        return Err(fail(format!(
                            "model says present={present}, backend {stat:?}"
                        )));
                    }
                }
            }
            Op::Delete(index) => {
                let name = NAMES[*index];
                if let Err(err) = backend.delete(name) {
                    prop_assert!(err.is_not_found(), "{err}");
                }
                model.remove(name);
            }
            Op::Get(index) => {
                let name = NAMES[*index];
                match (model.get(name), backend.get(name, 1 << 20)) {
                    (Some(want), Ok(got)) => prop_assert_eq!(&got.data, want),
                    (None, Err(err)) => prop_assert!(err.is_not_found(), "{err}"),
                    (want, got) => {
                        return Err(fail(format!("model {want:?}, backend {got:?}")));
                    }
                }
            }
            Op::List => {
                let listed: Vec<(String, u64)> = backend
                    .list_dir("d")
                    .map_err(fail)?
                    .into_iter()
                    .inspect(|entry| assert_eq!(entry.kind, DirEntryKind::File))
                    .map(|entry| (entry.name, entry.len))
                    .collect();
                let wanted: Vec<(String, u64)> = model
                    .iter()
                    .map(|(name, data)| {
                        (name.trim_start_matches("d/").to_owned(), data.len() as u64)
                    })
                    .collect();
                prop_assert_eq!(listed, wanted);
            }
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn the_memory_and_directory_backends_behave_like_a_map(ops in op_sequences()) {
        behaves_like_a_map(&MemoryBackend::new(), &ops)?;
        let dir = tempfile::tempdir().expect("temporary directory");
        behaves_like_a_map(&LocalBackend::new(dir.path().join("repo")), &ops)?;
    }
}

// ---------------------------------------------------------------------------------------------
// Backup then restore returns the same bytes
// ---------------------------------------------------------------------------------------------

fn round_trip(files: &[Vec<u8>]) -> Result<(), TestCaseError> {
    block_on(async {
        let work: TempDir = tempfile::tempdir().expect("temporary directory");
        let src = work.path().join("src");
        fs::create_dir_all(&src).map_err(fail)?;
        for (index, data) in files.iter().enumerate() {
            fs::write(src.join(format!("f{index}")), data).map_err(fail)?;
        }
        let repo = Repository::with_backend(Arc::new(MemoryBackend::new()));
        repo.init_with_kdf(&passphrase(), kdf())
            .await
            .map_err(fail)?;
        let keys = repo.unlock(&passphrase()).await.map_err(fail)?;

        let options = BackupOptions {
            sources: vec![src.clone()],
            chunker: ChunkerConfig::new(1024, 4096, 16_384).map_err(fail)?,
            jobs: 2,
            racy_margin: Duration::ZERO,
            ..BackupOptions::default()
        };
        let report = repo.backup(&keys, options).await.map_err(fail)?;
        prop_assert!(!report.is_partial());

        let target = work.path().join("out");
        let restore = RestoreOptions::new(SnapshotRef::from(&report.snapshot), target);
        let restored = repo.restore(&keys, restore).await.map_err(fail)?;
        prop_assert!(restored.is_complete(), "{restored:?}");
        let canonical = fs::canonicalize(&src).map_err(fail)?;
        let root = restored
            .target
            .join(canonical.strip_prefix("/").map_err(fail)?);
        for (index, data) in files.iter().enumerate() {
            let got = fs::read(root.join(format!("f{index}"))).map_err(fail)?;
            prop_assert_eq!(&got, data, "file f{}", index);
        }
        Ok::<(), TestCaseError>(())
    })
}

#[test]
fn whatever_files_a_backup_is_given_a_restore_returns_the_same_bytes() {
    let mut runner = TestRunner::new(Config::with_cases(24));
    let strategy = vec(vec(any::<u8>(), 0..6_000), 0..6);
    runner
        .run(&strategy, |files| round_trip(&files))
        .expect("restore(backup(x)) == x");
}
