//! Retries with exponential backoff for any backend.
//!
//! Object stores fail transiently as a matter of course: timeouts, dropped connections,
//! throttling. [`RetryingBackend`] wraps a backend and repeats a call that failed with an error
//! for which [`Error::is_transient`] holds, waiting longer each time, up to a limit. Nothing else
//! is retried: a missing object, a conflict, a permission error, or a bad name is the answer, not
//! a failure to try again.
//!
//! Retrying a write is where care is needed. A timeout does not say whether the request was
//! carried out. If it was, the retry of a conditional write is refused *because the first
//! attempt worked*: a retried [`PutMode::Create`] finds the object it just created, a retried
//! [`PutMode::Update`] finds a version that is no longer the one it expected. Reporting that as a
//! conflict would make a successful write look like a lost race. So a conflict that follows an
//! earlier failed attempt is checked: if the object now holds exactly the bytes being written,
//! the earlier attempt landed and the write succeeded. (Another writer who stored identical bytes
//! in between would be mistaken for us; every writer in VaultSnap writes data that is unique to
//! it, a random lock name, a snapshot id, a freshly sealed chunk, so that cannot happen.)
//! Likewise a retried delete that finds the object gone has succeeded.

use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use super::{
    Backend, Capabilities, DirEntryInfo, LocationState, Object, ObjectInfo, PutMode, Version,
};
use crate::error::{Error, Result};

/// How many times to try and how long to wait between tries.
#[derive(Debug, Clone, PartialEq)]
pub struct RetryPolicy {
    /// The most tries per call, the first included. 1 means no retries.
    pub max_attempts: u32,
    /// The wait before the first retry; it doubles for each further retry.
    pub initial_delay: Duration,
    /// The longest wait between two tries.
    pub max_delay: Duration,
    /// Wait a random time between half and all of the computed delay, so that many clients that
    /// failed together do not retry together.
    pub jitter: bool,
}

impl RetryPolicy {
    /// One attempt per call: failures are passed on.
    pub const fn none() -> Self {
        Self {
            max_attempts: 1,
            initial_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            jitter: false,
        }
    }

    /// Five attempts, waiting about 0.1, 0.2, 0.4, and 0.8 seconds between them, with jitter.
    pub const fn standard() -> Self {
        Self {
            max_attempts: 5,
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(5),
            jitter: true,
        }
    }

    /// The wait before retry number `retry` (0 for the first retry). `unit` is a number from 0
    /// to 1 that picks a point in the jitter range; it is ignored without jitter.
    pub fn delay(&self, retry: u32, unit: f64) -> Duration {
        let factor = 1_u32 << retry.min(16);
        let capped = self.initial_delay.saturating_mul(factor).min(self.max_delay);
        if !self.jitter {
            return capped;
        }
        let unit = if unit.is_finite() { unit.clamp(0.0, 1.0) } else { 0.0 };
        capped.mul_f64(0.5 + 0.5 * unit)
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::standard()
    }
}

/// How a [`RetryingBackend`] waits.
pub trait Sleeper: Send + Sync + fmt::Debug {
    /// Returns after `duration`.
    fn sleep(&self, duration: Duration);
}

/// Waits by blocking the thread.
#[derive(Debug, Default)]
pub struct ThreadSleeper;

impl Sleeper for ThreadSleeper {
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Never waits. For tests.
#[derive(Debug, Default)]
pub struct ImmediateSleeper;

impl Sleeper for ImmediateSleeper {
    fn sleep(&self, _duration: Duration) {}
}

/// A random number in `[0, 1)`; 0.5 if the system cannot provide randomness.
fn jitter_unit() -> f64 {
    crate::util::random_bytes::<4>().map_or(0.5, |bytes| {
        f64::from(u32::from_le_bytes(bytes)) / 4_294_967_296.0
    })
}

/// Wraps a backend and retries calls that fail transiently. See the module documentation for what
/// is retried and how writes whose answer was lost are recognized.
#[derive(Debug)]
pub struct RetryingBackend {
    inner: Arc<dyn Backend>,
    policy: RetryPolicy,
    sleeper: Arc<dyn Sleeper>,
}

impl RetryingBackend {
    /// Retries `inner` according to `policy`, waiting by blocking the thread.
    pub fn new(inner: Arc<dyn Backend>, policy: RetryPolicy) -> Self {
        Self::with_sleeper(inner, policy, Arc::new(ThreadSleeper))
    }

    /// Like [`RetryingBackend::new`] with a different way of waiting.
    pub fn with_sleeper(
        inner: Arc<dyn Backend>,
        policy: RetryPolicy,
        sleeper: Arc<dyn Sleeper>,
    ) -> Self {
        Self {
            inner,
            policy,
            sleeper,
        }
    }

    /// Calls `call` with the number of the attempt (0 for the first) until it succeeds, fails in
    /// a way that is not transient, or the attempts are used up; the last error is returned.
    fn run<T>(
        &self,
        operation: &'static str,
        mut call: impl FnMut(u32) -> Result<T>,
    ) -> Result<T> {
        let mut attempt = 0;
        loop {
            match call(attempt) {
                Err(err) if err.is_transient() && attempt + 1 < self.policy.max_attempts => {
                    let delay = self.policy.delay(attempt, jitter_unit());
                    tracing::debug!(
                        operation,
                        retry = attempt + 1,
                        delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                        error = %err,
                        "retrying after a transient storage error"
                    );
                    self.sleeper.sleep(delay);
                    attempt += 1;
                }
                other => return other,
            }
        }
    }

    /// A conditional write was refused after an earlier attempt failed. If the object holds
    /// exactly `data`, that attempt landed and this is the result of the write; otherwise the
    /// refusal stands.
    fn landed(&self, name: &str, data: &[u8], refusal: Error) -> Result<Version> {
        match self.inner.get(name, data.len() as u64) {
            Ok(object) if object.data == data => Ok(object.info.version),
            Err(err) if err.is_transient() => Err(err),
            _ => Err(refusal),
        }
    }
}

impl Backend for RetryingBackend {
    fn root(&self) -> &Path {
        self.inner.root()
    }

    fn local_path(&self) -> Option<&Path> {
        self.inner.local_path()
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    fn probe(&self) -> Result<LocationState> {
        self.run("probe", |_| self.inner.probe())
    }

    fn create_root(&self) -> Result<()> {
        self.run("create_root", |_| self.inner.create_root())
    }

    fn create_dir(&self, dir: &str) -> Result<()> {
        self.run("create_dir", |_| self.inner.create_dir(dir))
    }

    fn dir_exists(&self, dir: &str) -> Result<bool> {
        self.run("dir_exists", |_| self.inner.dir_exists(dir))
    }

    fn exists(&self, name: &str) -> Result<bool> {
        self.run("exists", |_| self.inner.exists(name))
    }

    fn stat(&self, name: &str) -> Result<ObjectInfo> {
        self.run("stat", |_| self.inner.stat(name))
    }

    fn get(&self, name: &str, max_len: u64) -> Result<Object> {
        self.run("get", |_| self.inner.get(name, max_len))
    }

    fn get_prefix(&self, name: &str, max: usize) -> Result<(Vec<u8>, u64)> {
        self.run("get_prefix", |_| self.inner.get_prefix(name, max))
    }

    fn list_dir(&self, dir: &str) -> Result<Vec<DirEntryInfo>> {
        self.run("list_dir", |_| self.inner.list_dir(dir))
    }

    fn put(&self, name: &str, data: &[u8], mode: PutMode) -> Result<Version> {
        self.run("put", |attempt| {
            match self.inner.put(name, data, mode.clone()) {
                Err(err) if attempt > 0 && err.is_conflict() => self.landed(name, data, err),
                other => other,
            }
        })
    }

    fn delete(&self, name: &str) -> Result<()> {
        self.run("delete", |attempt| match self.inner.delete(name) {
            Err(err) if attempt > 0 && err.is_not_found() => Ok(()),
            other => other,
        })
    }

    fn sync_dir(&self, dir: &str) -> Result<()> {
        self.run("sync_dir", |_| self.inner.sync_dir(dir))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::super::contract::{self, Fixture};
    use super::super::{Fault, FlakyBackend, LocalBackend, MemoryBackend};
    use super::*;

    /// Remembers how long it was asked to wait, without waiting.
    #[derive(Debug, Default)]
    struct Recorder(Mutex<Vec<Duration>>);

    impl Recorder {
        fn waits(&self) -> Vec<Duration> {
            self.0.lock().expect("lock").clone()
        }
    }

    impl Sleeper for Recorder {
        fn sleep(&self, duration: Duration) {
            self.0.lock().expect("lock").push(duration);
        }
    }

    fn policy(attempts: u32) -> RetryPolicy {
        RetryPolicy {
            max_attempts: attempts,
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(1),
            jitter: false,
        }
    }

    struct Rig {
        inner: Arc<MemoryBackend>,
        flaky: Arc<FlakyBackend>,
        retrying: RetryingBackend,
        recorder: Arc<Recorder>,
    }

    fn rig(faults: impl IntoIterator<Item = Fault>, attempts: u32) -> Rig {
        let inner = Arc::new(MemoryBackend::new());
        let flaky = Arc::new(FlakyBackend::scripted(inner.clone(), faults));
        let recorder = Arc::new(Recorder::default());
        let retrying = RetryingBackend::with_sleeper(flaky.clone(), policy(attempts), recorder.clone());
        Rig {
            inner,
            flaky,
            retrying,
            recorder,
        }
    }

    #[test]
    fn delays_double_up_to_the_cap_and_never_overflow() {
        let policy = policy(10);
        let millis = |retry| policy.delay(retry, 0.9).as_millis();
        assert_eq!([millis(0), millis(1), millis(2), millis(3)], [100, 200, 400, 800]);
        assert_eq!([millis(4), millis(5), millis(20), millis(u32::MAX)], [1000, 1000, 1000, 1000]);
    }

    #[test]
    fn jitter_spreads_the_delay_between_half_and_all_of_it() {
        let policy = RetryPolicy {
            jitter: true,
            ..policy(10)
        };
        assert_eq!(policy.delay(1, 0.0), Duration::from_millis(100));
        assert_eq!(policy.delay(1, 1.0), Duration::from_millis(200));
        assert_eq!(policy.delay(1, 0.5), Duration::from_millis(150));
        assert_eq!(policy.delay(1, 7.0), Duration::from_millis(200), "clamped");
        assert_eq!(policy.delay(1, -3.0), Duration::from_millis(100), "clamped");
        assert_eq!(policy.delay(1, f64::NAN), Duration::from_millis(100), "not a number");
    }

    #[test]
    fn the_standard_policy_waits_a_little_and_then_gives_up() {
        let standard = RetryPolicy::standard();
        assert_eq!(standard, RetryPolicy::default());
        assert_eq!(standard.max_attempts, 5);
        assert_eq!(RetryPolicy::none().max_attempts, 1);
        assert!(standard.delay(0, 1.0) <= Duration::from_millis(100));
        assert!(standard.delay(30, 1.0) <= standard.max_delay);
    }

    #[test]
    fn transient_failures_are_retried_with_growing_waits_until_the_call_succeeds() {
        let rig = rig([Fault::Before, Fault::Before], 5);
        rig.inner.put("x", b"1", PutMode::Create).expect("create");
        assert_eq!(rig.retrying.get("x", 10).expect("succeeds on the third try").data, b"1");
        assert_eq!(rig.flaky.calls(), 3);
        assert_eq!(
            rig.recorder.waits(),
            [Duration::from_millis(100), Duration::from_millis(200)]
        );
    }

    #[test]
    fn the_last_transient_error_is_returned_when_the_attempts_run_out() {
        let rig = rig([Fault::Before; 10], 3);
        let err = rig.retrying.stat("x").expect_err("never succeeds");
        assert!(err.is_transient(), "{err}");
        assert_eq!(rig.flaky.calls(), 3, "exactly the allowed number of attempts");
        assert_eq!(rig.recorder.waits().len(), 2, "no wait after the last attempt");
    }

    #[test]
    fn no_retries_means_the_first_failure_is_the_answer() {
        let rig = rig([Fault::Before], 1);
        assert!(rig.retrying.stat("x").expect_err("fails").is_transient());
        assert_eq!(rig.flaky.calls(), 1);
        assert!(rig.recorder.waits().is_empty());
    }

    #[test]
    fn answers_that_are_not_transient_are_not_retried() {
        let rig = rig([], 5);
        let err = rig.retrying.get("missing", 10).expect_err("missing");
        assert!(err.is_not_found());
        let err = rig.retrying.get("../bad", 10).expect_err("invalid");
        assert!(matches!(err, Error::InvalidName(_)));
        rig.retrying.put("x", b"1", PutMode::Create).expect("create");
        let err = rig.retrying.put("x", b"2", PutMode::Create).expect_err("exists");
        assert!(matches!(err, Error::AlreadyExists { .. }), "{err}");
        assert_eq!(rig.flaky.calls(), 4, "one attempt each");
        assert!(rig.recorder.waits().is_empty());
    }

    #[test]
    fn a_create_whose_answer_was_lost_is_reported_as_done() {
        let rig = rig([Fault::After], 5);
        let version = rig.retrying.put("x", b"mine", PutMode::Create).expect("it landed");
        assert_eq!(rig.inner.stat("x").expect("stat").version, version);
        assert_eq!(rig.inner.get("x", 10).expect("get").data, b"mine");
    }

    #[test]
    fn a_create_that_lost_to_someone_else_stays_a_conflict_after_a_retry() {
        let rig = rig([Fault::Before], 5);
        rig.inner.put("x", b"theirs", PutMode::Create).expect("someone else creates it");
        let err = rig.retrying.put("x", b"mine", PutMode::Create).expect_err("conflict");
        assert!(matches!(err, Error::AlreadyExists { .. }), "{err}");
        assert_eq!(rig.inner.get("x", 10).expect("get").data, b"theirs");
        assert_eq!(rig.flaky.calls(), 3, "tried again, looked at what is there, took the refusal");
    }

    #[test]
    fn an_update_whose_answer_was_lost_is_reported_as_done() {
        let rig = rig([], 5);
        let v1 = rig.inner.put("meta", b"one", PutMode::Create).expect("create");
        let flaky = Arc::new(FlakyBackend::scripted_for(rig.inner.clone(), &["put"], [Fault::After]));
        let retrying = RetryingBackend::with_sleeper(flaky, policy(5), Arc::new(ImmediateSleeper));
        let version = retrying.put("meta", b"two", PutMode::Update(v1)).expect("it landed");
        assert_eq!(rig.inner.stat("meta").expect("stat").version, version);
        assert_eq!(rig.inner.get("meta", 10).expect("get").data, b"two");
    }

    #[test]
    fn a_stale_update_stays_refused_after_a_retry() {
        let rig = rig([], 5);
        let v1 = rig.inner.put("meta", b"one", PutMode::Create).expect("create");
        rig.inner.put("meta", b"theirs", PutMode::Overwrite).expect("someone else updates it");
        let flaky = Arc::new(FlakyBackend::scripted_for(rig.inner.clone(), &["put"], [Fault::Before]));
        let retrying = RetryingBackend::with_sleeper(flaky, policy(5), Arc::new(ImmediateSleeper));
        let err = retrying.put("meta", b"mine", PutMode::Update(v1)).expect_err("stale");
        assert!(matches!(err, Error::PreconditionFailed { .. }), "{err}");
        assert_eq!(rig.inner.get("meta", 10).expect("get").data, b"theirs");
    }

    #[test]
    fn a_delete_whose_answer_was_lost_is_reported_as_done() {
        // A file system reports a second delete as not found, which is what a retry sees.
        let dir = tempfile::tempdir().expect("create temporary directory");
        let local = Arc::new(LocalBackend::new(dir.path()));
        local.put("x", b"1", PutMode::Create).expect("create");
        let flaky = Arc::new(FlakyBackend::scripted_for(local.clone(), &["delete"], [Fault::After]));
        let retrying = RetryingBackend::with_sleeper(flaky, policy(5), Arc::new(ImmediateSleeper));
        retrying.delete("x").expect("it was deleted by the first attempt");
        assert!(!local.exists("x").expect("exists"));
        let err = retrying.delete("x").expect_err("a plain second delete is still not found");
        assert!(err.is_not_found(), "{err}");
    }

    #[test]
    fn a_wrapper_describes_the_same_place_as_what_it_wraps() {
        let rig = rig([], 2);
        assert_eq!(rig.retrying.root(), rig.inner.root());
        assert_eq!(rig.retrying.capabilities(), rig.inner.capabilities());
        assert!(rig.retrying.local_path().is_none());
    }

    #[test]
    fn the_contract_holds_through_retries_when_calls_keep_failing() {
        contract::run(&|| {
            let inner = Arc::new(MemoryBackend::new());
            let flaky = Arc::new(FlakyBackend::periodic(inner, 3));
            let retrying = RetryingBackend::with_sleeper(flaky, policy(12), Arc::new(ImmediateSleeper));
            Fixture::new(Arc::new(retrying))
        });
    }

    #[test]
    fn the_contract_does_not_hold_without_retries_when_calls_keep_failing() {
        let failures = contract::check(&|| {
            let flaky = Arc::new(FlakyBackend::periodic(Arc::new(MemoryBackend::new()), 3));
            Fixture::new(Arc::new(RetryingBackend::with_sleeper(
                flaky,
                RetryPolicy::none(),
                Arc::new(ImmediateSleeper),
            )))
        });
        assert!(!failures.is_empty(), "the injected failures must be felt without retries");
    }
}
