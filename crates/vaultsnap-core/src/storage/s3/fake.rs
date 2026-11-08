//! An in-process S3 for tests: the behavior of an S3-compatible service behind
//! [`ObjectApi`], without a network.
//!
//! It models what the backend depends on and nothing more: keys in a flat sorted namespace,
//! `ETag`s that change with every write, conditional writes (`If-None-Match: *`, `If-Match`),
//! ranged reads (and the `416` an empty object gives to any range), `HEAD`, idempotent
//! `DELETE`, and `ListObjectsV2` with a delimiter and **pages**, so that continuation is
//! exercised. It can also misbehave on purpose: refuse calls with a chosen error, apply a write
//! and then report failure (the answer lost on the way back), ignore the conditional headers the
//! way some S3-compatible stores do, refuse writes (read-only credentials), pretend the bucket
//! is gone, and be slow, while counting calls and the most calls ever in flight at once.
//!
//! The real service is covered by the MinIO tests (`tests/s3_minio.rs`), which run the same
//! scenarios against a real server when one is available.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use super::api::{
    ApiError, ApiErrorKind, ApiResult, Condition, GetOutput, HeadInfo, ListPage, ListedObject,
    ObjectApi,
};

#[derive(Debug, Clone)]
struct Stored {
    data: Vec<u8>,
    etag: String,
    modified: SystemTime,
}

/// A failure to inject into the next calls of one operation.
#[derive(Debug)]
struct Scripted {
    operation: &'static str,
    error: ApiError,
    /// Carry the call out before reporting the failure.
    apply: bool,
}

#[derive(Debug, Default)]
struct State {
    objects: BTreeMap<String, Stored>,
    counter: u64,
    calls: BTreeMap<&'static str, u64>,
    script: VecDeque<Scripted>,
}

/// Counts calls in flight, and how many there ever were at once.
struct Flight<'a>(&'a AtomicUsize);

impl Drop for Flight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// An S3 bucket in memory. See the module documentation.
pub struct FakeS3 {
    state: Mutex<State>,
    page_size: usize,
    honors_if_none_match: bool,
    honors_if_match: bool,
    creates_on_if_match: bool,
    writes_denied: bool,
    bucket_missing: bool,
    delay: Duration,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
}

impl fmt::Debug for FakeS3 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FakeS3")
            .field("objects", &self.lock().objects.len())
            .finish_non_exhaustive()
    }
}

impl Default for FakeS3 {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeS3 {
    /// An empty bucket that behaves like S3: conditions are honored, listings have 1000 entries
    /// per page.
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State::default()),
            page_size: 1000,
            honors_if_none_match: true,
            honors_if_match: true,
            creates_on_if_match: false,
            writes_denied: false,
            bucket_missing: false,
            delay: Duration::ZERO,
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
        }
    }

    /// Lists at most `entries` objects and prefixes per page (at least 1).
    #[must_use]
    pub fn with_page_size(mut self, entries: usize) -> Self {
        self.page_size = entries.max(1);
        self
    }

    /// Ignores `If-None-Match`, as some S3-compatible stores do: a conditional create overwrites.
    #[must_use]
    pub fn ignoring_if_none_match(mut self) -> Self {
        self.honors_if_none_match = false;
        self
    }

    /// Ignores `If-Match`: a conditional replace is unconditional.
    #[must_use]
    pub fn ignoring_if_match(mut self) -> Self {
        self.honors_if_match = false;
        self
    }

    /// Creates the object when `If-Match` names one that does not exist, instead of answering
    /// `404`: what the MinIO release of April 2025 does (found by running the storage contract
    /// against it). A stale `If-Match` on an existing object is still refused.
    #[must_use]
    pub fn creating_on_if_match(mut self) -> Self {
        self.creates_on_if_match = true;
        self
    }

    /// Refuses every write and delete with `403 AccessDenied`, like read-only credentials.
    #[must_use]
    pub fn read_only(mut self) -> Self {
        self.writes_denied = true;
        self
    }

    /// Answers every call with `404 NoSuchBucket`.
    #[must_use]
    pub fn without_bucket(mut self) -> Self {
        self.bucket_missing = true;
        self
    }

    /// Makes every call take `delay`, so that concurrent callers overlap.
    #[must_use]
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// Makes the next `count` calls of `operation` (`"head"`, `"get"`, `"put"`, `"delete"`,
    /// `"list"`, `"head_bucket"`) fail with `error` without doing anything.
    pub fn fail_next(&self, operation: &'static str, count: usize, error: &ApiError) {
        self.script(operation, count, error, false);
    }

    /// Makes the next `count` calls of `operation` carry out their effect and then fail with
    /// `error`: the request arrived and the answer was lost.
    pub fn lose_answer_next(&self, operation: &'static str, count: usize, error: &ApiError) {
        self.script(operation, count, error, true);
    }

    fn script(&self, operation: &'static str, count: usize, error: &ApiError, apply: bool) {
        let mut state = self.lock();
        for _ in 0..count {
            state.script.push_back(Scripted {
                operation,
                error: error.clone(),
                apply,
            });
        }
    }

    /// How many calls of `operation` were made, failed ones included.
    pub fn calls(&self, operation: &'static str) -> u64 {
        self.lock().calls.get(operation).copied().unwrap_or(0)
    }

    /// How many calls of any operation were made.
    pub fn total_calls(&self) -> u64 {
        self.lock().calls.values().sum()
    }

    /// The most calls that were ever in progress at the same moment.
    pub fn max_in_flight(&self) -> usize {
        self.max_in_flight.load(Ordering::SeqCst)
    }

    /// The keys of all objects, in order.
    pub fn keys(&self) -> Vec<String> {
        self.lock().objects.keys().cloned().collect()
    }

    /// The bytes stored under `key`, if any.
    pub fn object(&self, key: &str) -> Option<Vec<u8>> {
        self.lock()
            .objects
            .get(key)
            .map(|stored| stored.data.clone())
    }

    /// Stores `data` under `key` as if another client had written it (not counted as a call).
    pub fn insert(&self, key: &str, data: &[u8]) {
        let mut state = self.lock();
        Self::store(&mut state, key, data);
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn store(state: &mut State, key: &str, data: &[u8]) -> String {
        state.counter += 1;
        let etag = format!("\"{:032x}\"", state.counter);
        state.objects.insert(
            key.to_owned(),
            Stored {
                data: data.to_vec(),
                etag: etag.clone(),
                modified: SystemTime::now(),
            },
        );
        etag
    }

    /// Runs one call: counts it, lets it overlap with others, applies a scripted failure if
    /// there is one for `operation`.
    fn call<T>(
        &self,
        operation: &'static str,
        run: impl FnOnce(&mut State) -> ApiResult<T>,
    ) -> ApiResult<T> {
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        let _flight = Flight(&self.in_flight);
        self.max_in_flight.fetch_max(now, Ordering::SeqCst);
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }

        let mut state = self.lock();
        *state.calls.entry(operation).or_insert(0) += 1;
        let position = state
            .script
            .iter()
            .position(|scripted| scripted.operation == operation);
        let scripted = position.and_then(|index| state.script.remove(index));
        let state: &mut State = &mut state;
        match scripted {
            None => run(state),
            Some(scripted) if scripted.apply => {
                let _ = run(state);
                Err(scripted.error)
            }
            Some(scripted) => Err(scripted.error),
        }
    }

    fn no_such_bucket() -> ApiError {
        ApiError::response(
            ApiErrorKind::NoSuchBucket,
            404,
            "NoSuchBucket",
            "The specified bucket does not exist",
        )
    }

    fn denied() -> ApiError {
        ApiError::response(ApiErrorKind::Denied, 403, "AccessDenied", "Access Denied")
    }

    fn no_such_key() -> ApiError {
        ApiError::response(
            ApiErrorKind::NotFound,
            404,
            "NoSuchKey",
            "The specified key does not exist.",
        )
    }

    fn precondition_failed() -> ApiError {
        ApiError::response(
            ApiErrorKind::PreconditionFailed,
            412,
            "PreconditionFailed",
            "At least one of the pre-conditions you specified did not hold",
        )
    }

    fn check_bucket(&self) -> ApiResult<()> {
        if self.bucket_missing {
            Err(Self::no_such_bucket())
        } else {
            Ok(())
        }
    }

    fn check_writable(&self) -> ApiResult<()> {
        if self.writes_denied {
            Err(Self::denied())
        } else {
            Ok(())
        }
    }
}

impl ObjectApi for FakeS3 {
    fn head_bucket(&self) -> ApiResult<()> {
        self.call("head_bucket", |_| self.check_bucket())
    }

    fn head(&self, key: &str) -> ApiResult<HeadInfo> {
        self.call("head", |state| {
            self.check_bucket()?;
            match state.objects.get(key) {
                Some(stored) => Ok(HeadInfo {
                    len: stored.data.len() as u64,
                    etag: stored.etag.clone(),
                    modified: stored.modified,
                }),
                None => Err(ApiError::response(
                    ApiErrorKind::NotFound,
                    404,
                    "NotFound",
                    "Not Found",
                )),
            }
        })
    }

    fn get(&self, key: &str, range: Option<(u64, u64)>, max_len: u64) -> ApiResult<GetOutput> {
        self.call("get", |state| {
            self.check_bucket()?;
            let stored = state.objects.get(key).ok_or_else(Self::no_such_key)?;
            let total = stored.data.len() as u64;
            let data = match range {
                None => {
                    if total > max_len {
                        return Err(ApiError::new(
                            ApiErrorKind::TooLarge,
                            format!("the object is {total} bytes, more than the allowed {max_len}"),
                        ));
                    }
                    stored.data.clone()
                }
                Some((first, last)) => {
                    if first >= total {
                        return Err(ApiError::response(
                            ApiErrorKind::InvalidRange,
                            416,
                            "InvalidRange",
                            "The requested range is not satisfiable",
                        ));
                    }
                    let end = last.min(total - 1);
                    stored.data[first as usize..=end as usize].to_vec()
                }
            };
            Ok(GetOutput {
                data,
                etag: stored.etag.clone(),
                modified: stored.modified,
                total_len: total,
            })
        })
    }

    fn put(&self, key: &str, data: &[u8], condition: &Condition) -> ApiResult<String> {
        self.call("put", |state| {
            self.check_bucket()?;
            self.check_writable()?;
            match condition {
                Condition::IfNoneMatchAny
                    if self.honors_if_none_match && state.objects.contains_key(key) =>
                {
                    return Err(Self::precondition_failed());
                }
                Condition::IfMatch(etag) if self.honors_if_match => match state.objects.get(key) {
                    None if self.creates_on_if_match => {}
                    None => return Err(Self::no_such_key()),
                    Some(stored) if stored.etag != *etag => return Err(Self::precondition_failed()),
                    Some(_) => {}
                },
                _ => {}
            }
            Ok(Self::store(state, key, data))
        })
    }

    fn delete(&self, key: &str) -> ApiResult<()> {
        self.call("delete", |state| {
            self.check_bucket()?;
            self.check_writable()?;
            state.objects.remove(key);
            Ok(())
        })
    }

    fn list(&self, prefix: &str, delimiter: bool, token: Option<&str>) -> ApiResult<ListPage> {
        self.call("list", |state| {
            self.check_bucket()?;
            // A token is the last thing the previous page showed: a key, or a rolled-up prefix.
            let skipped = |key: &str| {
                token.is_some_and(|after| {
                    key <= after || (after.ends_with('/') && key.starts_with(after))
                })
            };
            let mut page = ListPage::default();
            let mut shown = 0_usize;
            let mut last: Option<String> = None;
            for (key, stored) in state.objects.range(prefix.to_owned()..) {
                if !key.starts_with(prefix) {
                    break;
                }
                if skipped(key) {
                    continue;
                }
                let rest = &key[prefix.len()..];
                let rolled_up = if delimiter {
                    rest.find('/').map(|at| format!("{prefix}{}", &rest[..=at]))
                } else {
                    None
                };
                if let Some(group) = &rolled_up {
                    if last.as_ref() == Some(group) {
                        continue;
                    }
                }
                if shown == self.page_size {
                    page.next_token = last;
                    return Ok(page);
                }
                shown += 1;
                match rolled_up {
                    Some(group) => {
                        last = Some(group.clone());
                        page.prefixes.push(group);
                    }
                    None => {
                        last = Some(key.clone());
                        page.objects.push(ListedObject {
                            key: key.clone(),
                            len: stored.data.len() as u64,
                        });
                    }
                }
            }
            Ok(page)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(s3: &FakeS3, key: &str, data: &[u8]) -> String {
        s3.put(key, data, &Condition::None).expect("put")
    }

    #[test]
    fn a_write_changes_the_etag_and_a_read_returns_what_was_written() {
        let s3 = FakeS3::new();
        let first = put(&s3, "a", b"one");
        let second = put(&s3, "a", b"one");
        assert_ne!(first, second, "every write is a new version");
        let read = s3.get("a", None, 100).expect("get");
        assert_eq!(
            (read.data.as_slice(), &read.etag, read.total_len),
            (b"one".as_slice(), &second, 3)
        );
        let head = s3.head("a").expect("head");
        assert_eq!((head.len, head.etag), (3, second));
        assert_eq!(
            s3.get("missing", None, 10).expect_err("missing").kind,
            ApiErrorKind::NotFound
        );
        assert_eq!(
            s3.head("missing").expect_err("missing").kind,
            ApiErrorKind::NotFound
        );
    }

    #[test]
    fn conditions_are_honored_the_way_s3_honors_them() {
        let s3 = FakeS3::new();
        let etag = s3
            .put("k", b"1", &Condition::IfNoneMatchAny)
            .expect("create");
        let err = s3
            .put("k", b"2", &Condition::IfNoneMatchAny)
            .expect_err("exists");
        assert_eq!(
            (err.kind, err.status),
            (ApiErrorKind::PreconditionFailed, Some(412))
        );
        assert_eq!(s3.object("k").as_deref(), Some(b"1".as_slice()));

        let stale = "\"0\"".to_owned();
        let err = s3
            .put("k", b"3", &Condition::IfMatch(stale))
            .expect_err("stale");
        assert_eq!(err.kind, ApiErrorKind::PreconditionFailed);
        s3.put("k", b"3", &Condition::IfMatch(etag))
            .expect("current version");
        assert_eq!(s3.object("k").as_deref(), Some(b"3".as_slice()));

        let err = s3
            .put("absent", b"x", &Condition::IfMatch("\"1\"".to_owned()))
            .expect_err("missing");
        assert_eq!(
            err.kind,
            ApiErrorKind::NotFound,
            "S3 answers 404 to If-Match on a missing key"
        );
    }

    #[test]
    fn a_store_that_ignores_the_conditional_headers_overwrites_silently() {
        let s3 = FakeS3::new().ignoring_if_none_match().ignoring_if_match();
        s3.put("k", b"1", &Condition::IfNoneMatchAny)
            .expect("create");
        s3.put("k", b"2", &Condition::IfNoneMatchAny)
            .expect("silently overwrites");
        s3.put("k", b"3", &Condition::IfMatch("\"stale\"".to_owned()))
            .expect("silently overwrites");
        assert_eq!(s3.object("k").as_deref(), Some(b"3".as_slice()));
    }

    #[test]
    fn ranges_return_the_bytes_and_the_total_and_an_empty_object_refuses_them() {
        let s3 = FakeS3::new();
        put(&s3, "digits", b"0123456789");
        let part = s3.get("digits", Some((0, 3)), u64::MAX).expect("range");
        assert_eq!(
            (part.data.as_slice(), part.total_len),
            (b"0123".as_slice(), 10)
        );
        let tail = s3
            .get("digits", Some((8, 100)), u64::MAX)
            .expect("range past the end is cut");
        assert_eq!(tail.data, b"89");
        let err = s3
            .get("digits", Some((10, 12)), u64::MAX)
            .expect_err("beyond the end");
        assert_eq!(
            (err.kind, err.status),
            (ApiErrorKind::InvalidRange, Some(416))
        );
        put(&s3, "empty", b"");
        let err = s3
            .get("empty", Some((0, 9)), u64::MAX)
            .expect_err("empty object");
        assert_eq!(err.kind, ApiErrorKind::InvalidRange);
        assert!(s3.get("empty", None, 0).expect("whole").data.is_empty());
    }

    #[test]
    fn a_whole_read_beyond_the_limit_is_refused_before_any_body() {
        let s3 = FakeS3::new();
        put(&s3, "ten", b"0123456789");
        assert_eq!(
            s3.get("ten", None, 10).expect("at the limit").data.len(),
            10
        );
        assert_eq!(
            s3.get("ten", None, 9).expect_err("over").kind,
            ApiErrorKind::TooLarge
        );
    }

    #[test]
    fn listings_roll_up_prefixes_and_continue_across_pages() {
        let s3 = FakeS3::new().with_page_size(2);
        for key in ["p/a/1", "p/a/2", "p/b/1", "p/c", "p/d", "p/e/9", "q/x"] {
            put(&s3, key, key.as_bytes());
        }
        let mut objects = Vec::new();
        let mut prefixes = Vec::new();
        let mut token: Option<String> = None;
        let mut pages = 0;
        loop {
            let page = s3.list("p/", true, token.as_deref()).expect("list");
            pages += 1;
            assert!(
                page.objects.len() + page.prefixes.len() <= 2,
                "pages hold at most 2 entries"
            );
            objects.extend(
                page.objects
                    .into_iter()
                    .map(|object| (object.key, object.len)),
            );
            prefixes.extend(page.prefixes);
            token = page.next_token;
            if token.is_none() {
                break;
            }
        }
        assert_eq!(prefixes, ["p/a/", "p/b/", "p/e/"]);
        assert_eq!(objects, [("p/c".to_owned(), 3), ("p/d".to_owned(), 3)]);
        assert_eq!(pages, 3, "five entries in pages of two");
    }

    #[test]
    fn a_listing_without_a_delimiter_returns_every_key_below_the_prefix() {
        let s3 = FakeS3::new().with_page_size(3);
        for key in ["p/a/1", "p/a/2", "p/b", "o"] {
            put(&s3, key, b"x");
        }
        let page = s3.list("p/", false, None).expect("list");
        let keys: Vec<&str> = page
            .objects
            .iter()
            .map(|object| object.key.as_str())
            .collect();
        assert_eq!(keys, ["p/a/1", "p/a/2", "p/b"]);
        assert!(page.prefixes.is_empty() && page.next_token.is_none());
    }

    #[test]
    fn deleting_is_idempotent() {
        let s3 = FakeS3::new();
        put(&s3, "a", b"1");
        s3.delete("a").expect("delete");
        s3.delete("a").expect("again");
        assert!(s3.keys().is_empty());
    }

    #[test]
    fn scripted_failures_hit_the_named_operation_only_and_can_apply_the_effect() {
        let s3 = FakeS3::new();
        let boom = ApiError::response(ApiErrorKind::Transient, 503, "SlowDown", "slow down");
        s3.fail_next("put", 1, &boom);
        s3.lose_answer_next("put", 1, &boom);
        assert!(
            s3.head_bucket().is_ok(),
            "other operations are not affected"
        );
        assert_eq!(
            s3.put("a", b"1", &Condition::None)
                .expect_err("refused")
                .code
                .as_deref(),
            Some("SlowDown")
        );
        assert!(s3.keys().is_empty(), "the first failure did nothing");
        assert!(
            s3.put("a", b"1", &Condition::None).is_err(),
            "the second loses its answer"
        );
        assert_eq!(s3.keys(), ["a"], "but it was carried out");
        s3.put("a", b"2", &Condition::None)
            .expect("the script is used up");
        assert_eq!(s3.calls("put"), 3);
        assert_eq!(s3.total_calls(), 4);
    }

    #[test]
    fn read_only_credentials_and_a_missing_bucket_are_refused_as_s3_does() {
        let read_only = FakeS3::new().read_only();
        read_only.insert("a", b"1");
        assert!(read_only.get("a", None, 10).is_ok());
        assert_eq!(
            read_only
                .put("b", b"1", &Condition::None)
                .expect_err("denied")
                .kind,
            ApiErrorKind::Denied
        );
        assert_eq!(
            read_only.delete("a").expect_err("denied").kind,
            ApiErrorKind::Denied
        );

        let gone = FakeS3::new().without_bucket();
        assert_eq!(
            gone.head_bucket().expect_err("no bucket").kind,
            ApiErrorKind::NoSuchBucket
        );
        assert_eq!(
            gone.head("a").expect_err("no bucket").kind,
            ApiErrorKind::NoSuchBucket
        );
        assert_eq!(
            gone.list("", true, None).expect_err("no bucket").kind,
            ApiErrorKind::NoSuchBucket
        );
    }

    #[test]
    fn overlapping_calls_are_counted() {
        let s3 = FakeS3::new().with_delay(Duration::from_millis(20));
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| s3.head_bucket().expect("head bucket"));
            }
        });
        assert!(s3.max_in_flight() >= 2, "{}", s3.max_in_flight());
        assert!(s3.max_in_flight() <= 4);
    }
}
