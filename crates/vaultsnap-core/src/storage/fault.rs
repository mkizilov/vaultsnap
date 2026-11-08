//! A backend wrapper that makes calls fail on purpose, to test what the code above a backend does
//! when storage misbehaves: that retries happen, that they are not endless, and above all that a
//! write whose answer was lost is not mistaken for a conflict.
//!
//! Object stores fail in two ways that look the same to the caller (a timeout) and differ in what
//! they did: the request never arrived ([`Fault::Before`]), or it was carried out and the answer
//! was lost ([`Fault::After`]). Both are injected here as a transient error.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use super::{
    Backend, Capabilities, DirEntryInfo, LocationState, Object, ObjectInfo, PutMode, Version,
};
use crate::error::{BackendErrorKind, Error, Result};

/// How a call fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// The call fails and the inner backend is not called: nothing happened.
    Before,
    /// The inner backend carries the call out, and the caller is told it failed anyway: the
    /// effect is there, the answer is lost.
    After,
}

#[derive(Debug)]
enum Plan {
    Never,
    Scripted {
        faults: VecDeque<Fault>,
        only: Vec<&'static str>,
        /// Matching calls that still pass before the first fault is used.
        skip: u64,
    },
    Periodic {
        period: u64,
    },
}

#[derive(Debug)]
struct State {
    plan: Plan,
    calls: u64,
    injected: u64,
}

/// Wraps a backend and makes some of its calls fail with a transient error.
#[derive(Debug)]
pub struct FlakyBackend {
    inner: Arc<dyn Backend>,
    state: Mutex<State>,
}

impl FlakyBackend {
    fn with_plan(inner: Arc<dyn Backend>, plan: Plan) -> Self {
        Self {
            inner,
            state: Mutex::new(State {
                plan,
                calls: 0,
                injected: 0,
            }),
        }
    }

    /// Never fails; only counts calls.
    pub fn reliable(inner: Arc<dyn Backend>) -> Self {
        Self::with_plan(inner, Plan::Never)
    }

    /// Fails the next calls in the order given, one fault per call, then behaves.
    pub fn scripted(inner: Arc<dyn Backend>, faults: impl IntoIterator<Item = Fault>) -> Self {
        Self::scripted_for(inner, &[], faults)
    }

    /// Like [`FlakyBackend::scripted`], but only the named operations (`"get"`, `"put"`,
    /// `"delete"`, and so on, the method names of [`Backend`]) consume faults; the others
    /// pass through. An empty list means all operations.
    pub fn scripted_for(
        inner: Arc<dyn Backend>,
        only: &[&'static str],
        faults: impl IntoIterator<Item = Fault>,
    ) -> Self {
        Self::with_plan(
            inner,
            Plan::Scripted {
                faults: faults.into_iter().collect(),
                only: only.to_vec(),
                skip: 0,
            },
        )
    }

    /// Lets the first `calls` calls that a script applies to pass before its faults start, so a
    /// fault can be aimed at, say, the second `put`. Has no effect on other kinds of plan.
    #[must_use]
    pub fn skipping(mut self, calls: u64) -> Self {
        let state = self.state.get_mut().unwrap_or_else(PoisonError::into_inner);
        if let Plan::Scripted { skip, .. } = &mut state.plan {
            *skip = calls;
        }
        self
    }

    /// Fails every `period`-th call of any operation, alternating between [`Fault::After`] and
    /// [`Fault::Before`]. With a period of 3, calls 3, 9, 15, ... lose their answer and calls 6,
    /// 12, ... never arrive. A period of 0 never fails.
    pub fn periodic(inner: Arc<dyn Backend>, period: u64) -> Self {
        Self::with_plan(inner, Plan::Periodic { period })
    }

    /// How many calls have been made through this wrapper, failed or not.
    pub fn calls(&self) -> u64 {
        self.state().calls
    }

    /// How many calls were made to fail.
    pub fn injected(&self) -> u64 {
        self.state().injected
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn next_fault(&self, operation: &'static str) -> Option<Fault> {
        let mut state = self.state();
        state.calls += 1;
        let calls = state.calls;
        let fault = match &mut state.plan {
            Plan::Never => None,
            Plan::Scripted { faults, only, skip } => {
                let applies = only.is_empty() || only.contains(&operation);
                if !applies {
                    None
                } else if *skip > 0 {
                    *skip -= 1;
                    None
                } else {
                    faults.pop_front()
                }
            }
            Plan::Periodic { period } => {
                if *period > 0 && calls.is_multiple_of(*period) {
                    if (calls / *period) % 2 == 1 {
                        Some(Fault::After)
                    } else {
                        Some(Fault::Before)
                    }
                } else {
                    None
                }
            }
        };
        if fault.is_some() {
            state.injected += 1;
        }
        fault
    }

    fn intercept<T>(
        &self,
        operation: &'static str,
        name: &str,
        call: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let lost = || {
            Error::backend(
                operation,
                name,
                BackendErrorKind::Transient,
                "injected transient failure",
            )
        };
        match self.next_fault(operation) {
            None => call(),
            Some(Fault::Before) => Err(lost()),
            Some(Fault::After) => call().and_then(|_| Err(lost())),
        }
    }
}

impl Backend for FlakyBackend {
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
        self.intercept("probe", "", || self.inner.probe())
    }

    fn create_root(&self) -> Result<()> {
        self.intercept("create_root", "", || self.inner.create_root())
    }

    fn create_dir(&self, dir: &str) -> Result<()> {
        self.intercept("create_dir", dir, || self.inner.create_dir(dir))
    }

    fn dir_exists(&self, dir: &str) -> Result<bool> {
        self.intercept("dir_exists", dir, || self.inner.dir_exists(dir))
    }

    fn exists(&self, name: &str) -> Result<bool> {
        self.intercept("exists", name, || self.inner.exists(name))
    }

    fn stat(&self, name: &str) -> Result<ObjectInfo> {
        self.intercept("stat", name, || self.inner.stat(name))
    }

    fn get(&self, name: &str, max_len: u64) -> Result<Object> {
        self.intercept("get", name, || self.inner.get(name, max_len))
    }

    fn get_prefix(&self, name: &str, max: usize) -> Result<(Vec<u8>, u64)> {
        self.intercept("get_prefix", name, || self.inner.get_prefix(name, max))
    }

    fn list_dir(&self, dir: &str) -> Result<Vec<DirEntryInfo>> {
        self.intercept("list_dir", dir, || self.inner.list_dir(dir))
    }

    fn put(&self, name: &str, data: &[u8], mode: PutMode) -> Result<Version> {
        self.intercept("put", name, || self.inner.put(name, data, mode))
    }

    fn delete(&self, name: &str) -> Result<()> {
        self.intercept("delete", name, || self.inner.delete(name))
    }

    fn sync_dir(&self, dir: &str) -> Result<()> {
        self.intercept("sync_dir", dir, || self.inner.sync_dir(dir))
    }
}

#[cfg(test)]
mod tests {
    use super::super::MemoryBackend;
    use super::*;

    fn memory() -> Arc<MemoryBackend> {
        Arc::new(MemoryBackend::new())
    }

    #[test]
    fn a_fault_before_the_call_leaves_the_inner_backend_untouched() {
        let inner = memory();
        let flaky = FlakyBackend::scripted(inner.clone(), [Fault::Before]);
        let err = flaky.put("x", b"1", PutMode::Create).expect_err("injected");
        assert!(err.is_transient(), "{err}");
        assert!(inner.object_names().is_empty());
        flaky
            .put("x", b"1", PutMode::Create)
            .expect("the script is used up");
        assert_eq!(inner.object_names(), ["x"]);
    }

    #[test]
    fn a_fault_after_the_call_has_its_effect_but_loses_the_answer() {
        let inner = memory();
        let flaky = FlakyBackend::scripted(inner.clone(), [Fault::After]);
        let err = flaky
            .put("x", b"1", PutMode::Create)
            .expect_err("the answer is lost");
        assert!(err.is_transient(), "{err}");
        assert_eq!(inner.object_names(), ["x"], "the write happened");
    }

    #[test]
    fn a_fault_after_a_call_that_failed_reports_the_real_failure() {
        let inner = memory();
        let flaky = FlakyBackend::scripted(inner.clone(), [Fault::After]);
        let err = flaky.get("missing", 10).expect_err("really missing");
        assert!(err.is_not_found() && !err.is_transient(), "{err}");
    }

    #[test]
    fn faults_are_used_in_order_and_only_by_the_named_operations() {
        let inner = memory();
        inner.put("x", b"1", PutMode::Create).expect("create");
        let flaky = FlakyBackend::scripted_for(inner, &["get"], [Fault::Before, Fault::Before]);
        assert!(flaky.exists("x").expect("not named, passes through"));
        assert!(flaky.get("x", 10).is_err());
        assert_eq!(flaky.injected(), 1);
        assert!(flaky.stat("x").is_ok());
        assert!(flaky.get("x", 10).is_err());
        assert!(flaky.get("x", 10).is_ok(), "the script is used up");
        assert_eq!((flaky.calls(), flaky.injected()), (5, 2));
    }

    #[test]
    fn a_script_can_let_the_first_calls_pass() {
        let inner = memory();
        let flaky =
            FlakyBackend::scripted_for(inner.clone(), &["put"], [Fault::Before]).skipping(2);
        flaky
            .put("a", b"1", PutMode::Create)
            .expect("first put passes");
        assert!(flaky
            .exists("a")
            .expect("not a put: does not use up the skip"));
        flaky
            .put("b", b"1", PutMode::Create)
            .expect("second put passes");
        assert!(
            flaky.put("c", b"1", PutMode::Create).is_err(),
            "third put is the fault"
        );
        flaky
            .put("c", b"1", PutMode::Create)
            .expect("and then it is over");
        assert_eq!(inner.object_names(), ["a", "b", "c"]);
    }

    #[test]
    fn periodic_faults_alternate_between_lost_answers_and_lost_requests() {
        let inner = memory();
        let flaky = FlakyBackend::periodic(inner.clone(), 3);
        flaky.put("a", b"1", PutMode::Create).expect("call 1");
        flaky.put("b", b"1", PutMode::Create).expect("call 2");
        let third = flaky.put("c", b"1", PutMode::Create);
        assert!(third.is_err(), "call 3 loses its answer");
        assert!(
            inner.object_names().contains(&"c".to_owned()),
            "but it was carried out"
        );
        flaky.put("d", b"1", PutMode::Create).expect("call 4");
        flaky.put("e", b"1", PutMode::Create).expect("call 5");
        let sixth = flaky.put("f", b"1", PutMode::Create);
        assert!(sixth.is_err(), "call 6 never arrives");
        assert!(!inner.object_names().contains(&"f".to_owned()));
        assert_eq!((flaky.calls(), flaky.injected()), (6, 2));
    }

    #[test]
    fn a_reliable_wrapper_and_a_zero_period_never_fail() {
        let reliable = FlakyBackend::reliable(memory());
        let never = FlakyBackend::periodic(memory(), 0);
        for flaky in [&reliable, &never] {
            for round in 0..20 {
                flaky
                    .put(&format!("o{round}"), b"x", PutMode::Create)
                    .expect("no faults");
            }
            assert_eq!((flaky.calls(), flaky.injected()), (20, 0));
        }
    }

    #[test]
    fn configuration_calls_are_not_counted_as_operations() {
        let flaky = FlakyBackend::scripted(memory(), [Fault::Before]);
        let _ = flaky.root();
        let _ = flaky.capabilities();
        assert_eq!(flaky.calls(), 0);
        assert!(flaky.local_path().is_none());
    }
}
