//! The contract every storage backend must pass.
//!
//! The cases here state, as executable checks, what [`Backend`] promises and what the rest of
//! VaultSnap relies on: exclusive creation, atomic replacement, conditional updates, honest
//! listings, tolerant deletes, the size limit on reads, name validation. A backend that passes
//! all of them can hold a repository; one that fails a case would lose or corrupt data in the
//! way the case names.
//!
//! To test a backend, give [`run`] a function that builds a fresh, empty instance:
//!
//! ```ignore
//! #[test]
//! fn my_backend_passes_the_contract() {
//!     contract::run(&|| Fixture::new(Arc::new(MyBackend::connect_to_test_bucket())));
//! }
//! ```
//!
//! Every case gets its own instance, so cases do not depend on each other. A case that fails
//! panics with a message that names what was expected; [`check`] runs all of them and returns
//! the failures instead of stopping at the first. The cases adapt to the backend's declared
//! [`Capabilities`](super::Capabilities) where the contract allows backends to differ (a backend
//! with directories must refuse a write below a missing one; one without must not).

use std::any::Any;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::{
    is_temp_name, Backend, DirEntryKind, Guarantee, LocationState, PutMode, Version, MAX_NAME_LEN,
};
use crate::error::Error;

/// A fresh backend under test, plus whatever must stay alive as long as it does (a temporary
/// directory, a test server).
pub struct Fixture {
    backend: Arc<dyn Backend>,
    _guard: Option<Box<dyn Any + Send>>,
}

impl Fixture {
    /// A fixture that needs nothing else kept alive.
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        Self {
            backend,
            _guard: None,
        }
    }

    /// A fixture that keeps `guard` alive for as long as the backend is in use.
    pub fn keeping(backend: Arc<dyn Backend>, guard: impl Any + Send) -> Self {
        Self {
            backend,
            _guard: Some(Box::new(guard)),
        }
    }

    /// The backend.
    pub fn backend(&self) -> &dyn Backend {
        self.backend.as_ref()
    }
}

/// One case of the contract.
pub type Case = fn(&dyn Backend);

/// Every case, by name.
pub const CASES: &[(&str, Case)] = &[
    ("names_are_validated", names_are_validated),
    ("objects_read_back_exactly", objects_read_back_exactly),
    ("empty_objects_are_objects", empty_objects_are_objects),
    ("big_objects_round_trip", big_objects_round_trip),
    ("create_never_overwrites", create_never_overwrites),
    ("overwrite_creates_or_replaces", overwrite_creates_or_replaces),
    ("conditional_replace_compares_versions", conditional_replace_compares_versions),
    ("exactly_one_creator_wins", exactly_one_creator_wins),
    ("exactly_one_conditional_replacer_wins", exactly_one_conditional_replacer_wins),
    ("independent_creators_do_not_interfere", independent_creators_do_not_interfere),
    ("replacement_is_atomic_for_readers", replacement_is_atomic_for_readers),
    ("missing_objects_are_not_found", missing_objects_are_not_found),
    ("the_size_limit_is_enforced", the_size_limit_is_enforced),
    ("prefix_reads_report_the_whole_size", prefix_reads_report_the_whole_size),
    ("listings_show_one_level_in_order", listings_show_one_level_in_order),
    ("listing_a_missing_directory", listing_a_missing_directory),
    ("deleting_is_tolerant_and_final", deleting_is_tolerant_and_final),
    ("directories_follow_the_declared_capability", directories_follow_the_declared_capability),
    ("no_temporary_objects_remain", no_temporary_objects_remain),
    ("the_durability_barrier_succeeds", the_durability_barrier_succeeds),
    ("the_location_can_be_prepared", the_location_can_be_prepared),
    ("the_capabilities_suffice_for_a_repository", the_capabilities_suffice_for_a_repository),
];

/// Runs every case, each on a fresh backend from `make`, and returns one line per failure.
pub fn check(make: &dyn Fn() -> Fixture) -> Vec<String> {
    let mut failures = Vec::new();
    for (name, case) in CASES {
        let fixture = make();
        let outcome = catch_unwind(AssertUnwindSafe(|| case(fixture.backend())));
        if let Err(payload) = outcome {
            failures.push(format!("{name}: {}", panic_message(&*payload)));
        }
    }
    failures
}

/// Runs the whole contract and panics, listing every failed case, if any case fails.
pub fn run(make: &dyn Fn() -> Fixture) {
    let failures = check(make);
    assert!(
        failures.is_empty(),
        "{} of {} contract cases failed:\n{}",
        failures.len(),
        CASES.len(),
        failures.join("\n")
    );
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else {
        "panicked without a message".to_owned()
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Larger than any object the cases write.
const BIG: u64 = 64 << 20;

fn put(b: &dyn Backend, name: &str, data: &[u8], mode: PutMode) -> Version {
    b.put(name, data, mode)
        .unwrap_or_else(|err| panic!("put {name:?} failed: {err}"))
}

fn create(b: &dyn Backend, name: &str, data: &[u8]) -> Version {
    put(b, name, data, PutMode::Create)
}

fn read(b: &dyn Backend, name: &str) -> Vec<u8> {
    b.get(name, BIG)
        .unwrap_or_else(|err| panic!("get {name:?} failed: {err}"))
        .data
}

fn current_version(b: &dyn Backend, name: &str) -> Version {
    b.stat(name)
        .unwrap_or_else(|err| panic!("stat {name:?} failed: {err}"))
        .version
}

fn make_dir(b: &dyn Backend, dir: &str) {
    b.create_dir(dir)
        .unwrap_or_else(|err| panic!("create_dir {dir:?} failed: {err}"));
}

fn names_in(b: &dyn Backend, dir: &str) -> Vec<String> {
    b.list_dir(dir)
        .unwrap_or_else(|err| panic!("list_dir {dir:?} failed: {err}"))
        .into_iter()
        .map(|entry| entry.name)
        .collect()
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

// ---------------------------------------------------------------------------------------------
// Names
// ---------------------------------------------------------------------------------------------

fn names_are_validated(b: &dyn Backend) {
    let long = "a".repeat(MAX_NAME_LEN + 1);
    let bad_names = [
        "", "/", "/abs", "a//b", "a/", "../x", "a/../b", ".hidden", "a/.b", ".tmp-0011", "a b",
        "a\\b", long.as_str(),
    ];
    for bad in bad_names {
        assert!(matches!(b.stat(bad), Err(Error::InvalidName(_))), "stat {bad:?}");
        assert!(matches!(b.exists(bad), Err(Error::InvalidName(_))), "exists {bad:?}");
        assert!(matches!(b.get(bad, 10), Err(Error::InvalidName(_))), "get {bad:?}");
        assert!(matches!(b.get_prefix(bad, 10), Err(Error::InvalidName(_))), "get_prefix {bad:?}");
        assert!(
            matches!(b.put(bad, b"x", PutMode::Create), Err(Error::InvalidName(_))),
            "put (create) {bad:?}"
        );
        assert!(
            matches!(b.put(bad, b"x", PutMode::Overwrite), Err(Error::InvalidName(_))),
            "put (overwrite) {bad:?}"
        );
        assert!(matches!(b.delete(bad), Err(Error::InvalidName(_))), "delete {bad:?}");
        assert!(matches!(b.create_dir(bad), Err(Error::InvalidName(_))), "create_dir {bad:?}");
    }
    for bad in ["../x", "a b", "/abs"] {
        assert!(matches!(b.list_dir(bad), Err(Error::InvalidName(_))), "list_dir {bad:?}");
        assert!(matches!(b.sync_dir(bad), Err(Error::InvalidName(_))), "sync_dir {bad:?}");
    }
    assert!(names_in(b, "").is_empty(), "refused names must not create anything");
}

// ---------------------------------------------------------------------------------------------
// Reading and writing
// ---------------------------------------------------------------------------------------------

fn objects_read_back_exactly(b: &dyn Backend) {
    let data = pattern(100_000);
    let version = create(b, "config", &data);
    let object = b.get("config", BIG).expect("get");
    assert_eq!(object.data, data, "the bytes read are the bytes written");
    assert_eq!(object.info.len, data.len() as u64);
    assert_eq!(object.info.version, version, "put returns the version of what it wrote");
    let info = b.stat("config").expect("stat");
    assert_eq!(info.len, data.len() as u64);
    assert_eq!(info.version, version, "stat reports the same version as put and get");
    assert!(b.exists("config").expect("exists"));
    assert!(!b.exists("other").expect("exists"));
}

fn empty_objects_are_objects(b: &dyn Backend) {
    create(b, "empty", b"");
    let object = b.get("empty", 0).expect("an empty object fits any limit");
    assert!(object.data.is_empty());
    assert_eq!(object.info.len, 0);
    assert!(b.exists("empty").expect("exists"));
    assert_eq!(b.get_prefix("empty", 10).expect("prefix"), (Vec::new(), 0));
}

fn big_objects_round_trip(b: &dyn Backend) {
    let data = pattern(3 * 1024 * 1024 + 17);
    create(b, "big", &data);
    assert!(read(b, "big") == data, "a 3 MiB object must read back unchanged");
    assert_eq!(b.stat("big").expect("stat").len, data.len() as u64);
}

fn create_never_overwrites(b: &dyn Backend) {
    let first = create(b, "once", b"first");
    let err = b
        .put("once", b"second", PutMode::Create)
        .expect_err("creating an existing object must fail");
    assert!(matches!(err, Error::AlreadyExists { .. }), "{err}");
    assert_eq!(read(b, "once"), b"first", "the existing object is untouched");
    assert_eq!(current_version(b, "once"), first);
}

fn overwrite_creates_or_replaces(b: &dyn Backend) {
    let first = put(b, "obj", b"one", PutMode::Overwrite);
    assert_eq!(read(b, "obj"), b"one", "overwrite creates a missing object");
    let second = put(b, "obj", b"second value", PutMode::Overwrite);
    assert_ne!(first, second, "a replacement is a new version");
    let object = b.get("obj", BIG).expect("get");
    assert_eq!(object.data, b"second value");
    assert_eq!(object.info.version, second);
    assert_eq!(current_version(b, "obj"), second);
}

fn conditional_replace_compares_versions(b: &dyn Backend) {
    if b.capabilities().replace_if_unchanged == Guarantee::None {
        return;
    }
    let v1 = create(b, "meta", b"one");
    let v2 = put(b, "meta", b"two", PutMode::Update(v1.clone()));
    assert_ne!(v1, v2);
    assert_eq!(read(b, "meta"), b"two");
    assert_eq!(current_version(b, "meta"), v2);

    let err = b
        .put("meta", b"three", PutMode::Update(v1))
        .expect_err("a stale version must be refused");
    assert!(matches!(err, Error::PreconditionFailed { .. }), "{err}");
    assert!(err.is_conflict());
    assert_eq!(read(b, "meta"), b"two", "a refused update changes nothing");

    let err = b
        .put("absent", b"x", PutMode::Update(v2.clone()))
        .expect_err("a missing object has no version to match");
    assert!(matches!(err, Error::PreconditionFailed { .. }), "{err}");
    assert!(!b.exists("absent").expect("exists"), "a refused update creates nothing");

    put(b, "meta", b"three", PutMode::Update(v2.clone()));
    assert_eq!(read(b, "meta"), b"three", "the version returned by the last write is current");
}

// ---------------------------------------------------------------------------------------------
// Concurrency
// ---------------------------------------------------------------------------------------------

fn exactly_one_creator_wins(b: &dyn Backend) {
    let results: Vec<Result<Version, Error>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|writer| {
                scope.spawn(move || {
                    let data = format!("writer {writer}");
                    b.put("race", data.as_bytes(), PutMode::Create)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("a creator panicked"))
            .collect()
    });
    let winners: Vec<usize> = results
        .iter()
        .enumerate()
        .filter(|(_, result)| result.is_ok())
        .map(|(writer, _)| writer)
        .collect();
    assert_eq!(winners.len(), 1, "exactly one creator may succeed: {results:?}");
    for err in results.iter().filter_map(|result| result.as_ref().err()) {
        assert!(matches!(err, Error::AlreadyExists { .. }), "a loser got: {err}");
    }
    assert_eq!(read(b, "race"), format!("writer {}", winners[0]).into_bytes());
}

fn exactly_one_conditional_replacer_wins(b: &dyn Backend) {
    if b.capabilities().replace_if_unchanged == Guarantee::None {
        return;
    }
    let start = create(b, "meta", b"start");
    let start = &start;
    let results: Vec<Result<Version, Error>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|writer| {
                scope.spawn(move || {
                    let data = format!("writer {writer}");
                    b.put("meta", data.as_bytes(), PutMode::Update(start.clone()))
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("a writer panicked"))
            .collect()
    });
    let winners: Vec<usize> = results
        .iter()
        .enumerate()
        .filter(|(_, result)| result.is_ok())
        .map(|(writer, _)| writer)
        .collect();
    assert_eq!(winners.len(), 1, "exactly one update of the same version may succeed: {results:?}");
    for err in results.iter().filter_map(|result| result.as_ref().err()) {
        assert!(matches!(err, Error::PreconditionFailed { .. }), "a loser got: {err}");
    }
    assert_eq!(read(b, "meta"), format!("writer {}", winners[0]).into_bytes());
}

fn independent_creators_do_not_interfere(b: &dyn Backend) {
    make_dir(b, "many");
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..16)
            .map(|writer| {
                scope.spawn(move || {
                    let name = format!("many/object-{writer:02}");
                    let data = format!("contents {writer}");
                    b.put(&name, data.as_bytes(), PutMode::Create)
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("a creator panicked").expect("independent creates succeed");
        }
    });
    let names = names_in(b, "many");
    assert_eq!(names.len(), 16, "{names:?}");
    for writer in 0..16 {
        let contents = read(b, &format!("many/object-{writer:02}"));
        assert_eq!(contents, format!("contents {writer}").into_bytes());
    }
}

fn replacement_is_atomic_for_readers(b: &dyn Backend) {
    const LEN: usize = 64 * 1024;
    let all_a = vec![b'A'; LEN];
    let all_b = vec![b'B'; LEN];
    create(b, "flip", &all_a);
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let readers: Vec<_> = (0..3)
            .map(|_| {
                scope.spawn(|| {
                    while !stop.load(Ordering::SeqCst) {
                        let data = b.get("flip", BIG).expect("read").data;
                        assert_eq!(data.len(), LEN, "a reader saw a partial object");
                        assert!(data.iter().all(|byte| *byte == data[0]), "a reader saw a mixture");
                    }
                })
            })
            .collect();
        for round in 0..40 {
            let next = if round % 2 == 0 { &all_b } else { &all_a };
            put(b, "flip", next, PutMode::Overwrite);
        }
        stop.store(true, Ordering::SeqCst);
        for reader in readers {
            reader.join().expect("a reader saw a torn object");
        }
    });
}

// ---------------------------------------------------------------------------------------------
// Missing objects, limits, prefixes
// ---------------------------------------------------------------------------------------------

fn missing_objects_are_not_found(b: &dyn Backend) {
    let results = [
        b.stat("nope").map(|_| ()),
        b.get("nope", 10).map(|_| ()),
        b.get_prefix("nope", 10).map(|_| ()),
    ];
    for result in results {
        let err = result.expect_err("a missing object cannot be read");
        assert!(err.is_not_found(), "{err}");
        assert!(!err.is_transient(), "{err}");
    }
    assert!(!b.exists("nope").expect("a missing object is not an error for exists"));
}

fn the_size_limit_is_enforced(b: &dyn Backend) {
    create(b, "ten", b"0123456789");
    assert_eq!(b.get("ten", 10).expect("exactly at the limit").data.len(), 10);
    let err = b.get("ten", 9).expect_err("one byte over the limit");
    assert!(matches!(err, Error::TooLarge { max_len: 9, .. }), "{err}");
    let err = b.get("ten", 0).expect_err("anything over a zero limit");
    assert!(matches!(err, Error::TooLarge { max_len: 0, .. }), "{err}");
}

fn prefix_reads_report_the_whole_size(b: &dyn Backend) {
    create(b, "digits", b"0123456789");
    assert_eq!(b.get_prefix("digits", 4).expect("prefix"), (b"0123".to_vec(), 10));
    assert_eq!(b.get_prefix("digits", 100).expect("prefix"), (b"0123456789".to_vec(), 10));
    assert_eq!(b.get_prefix("digits", 0).expect("prefix"), (Vec::new(), 10));
}

// ---------------------------------------------------------------------------------------------
// Listing, deleting, directories
// ---------------------------------------------------------------------------------------------

fn listings_show_one_level_in_order(b: &dyn Backend) {
    make_dir(b, "a");
    make_dir(b, "a/sub");
    create(b, "top", b"123");
    create(b, "a/two", b"1");
    create(b, "a/one", b"12");
    create(b, "a/sub/deep", b"1234");
    let describe = |dir: &str| -> Vec<(String, DirEntryKind, u64)> {
        b.list_dir(dir)
            .expect("list")
            .into_iter()
            .map(|entry| (entry.name, entry.kind, entry.len))
            .collect()
    };
    let item = |name: &str, kind: DirEntryKind, len: u64| (name.to_owned(), kind, len);
    assert_eq!(
        describe(""),
        [item("a", DirEntryKind::Directory, 0), item("top", DirEntryKind::File, 3)]
    );
    assert_eq!(
        describe("a"),
        [
            item("one", DirEntryKind::File, 2),
            item("sub", DirEntryKind::Directory, 0),
            item("two", DirEntryKind::File, 1)
        ]
    );
    assert_eq!(describe("a/sub"), [item("deep", DirEntryKind::File, 4)]);
}

fn listing_a_missing_directory(b: &dyn Backend) {
    match b.list_dir("missing") {
        Ok(entries) => assert!(entries.is_empty(), "{entries:?}"),
        Err(err) => assert!(err.is_not_found(), "{err}"),
    }
}

fn deleting_is_tolerant_and_final(b: &dyn Backend) {
    create(b, "keep", b"k");
    create(b, "drop", b"d");
    b.delete("drop").expect("delete");
    assert!(!b.exists("drop").expect("exists"));
    assert!(b.get("drop", 10).expect_err("deleted").is_not_found());
    assert!(b.exists("keep").expect("exists"), "other objects are untouched");
    match b.delete("drop") {
        Ok(()) => {}
        Err(err) => assert!(err.is_not_found(), "deleting twice may only fail as not found: {err}"),
    }
    create(b, "drop", b"again");
    assert_eq!(read(b, "drop"), b"again", "a deleted name can be created again");
}

fn directories_follow_the_declared_capability(b: &dyn Backend) {
    make_dir(b, "d");
    make_dir(b, "d");
    assert!(b.dir_exists("d").expect("answer"));
    if b.capabilities().directories {
        assert!(!b.dir_exists("other").expect("answer"));
        assert!(
            b.put("other/x", b"x", PutMode::Create).is_err(),
            "a write below a missing directory must fail"
        );
        assert!(!b.dir_exists("other").expect("answer"), "and must not create the directory");
        create(b, "plain", b"x");
        assert!(b.create_dir("plain").is_err(), "an object is not a directory");
        assert!(b.create_dir("missing/child").is_err(), "the parent must exist");
        make_dir(b, "d/e");
        create(b, "d/e/x", b"nested");
        assert_eq!(read(b, "d/e/x"), b"nested");
    } else {
        assert!(b.dir_exists("anything").expect("every directory exists"));
        create(b, "free/standing/x", b"nested");
        assert_eq!(read(b, "free/standing/x"), b"nested");
    }
}

fn no_temporary_objects_remain(b: &dyn Backend) {
    make_dir(b, "d");
    create(b, "d/a", b"1");
    put(b, "d/a", b"22", PutMode::Overwrite);
    assert!(b.put("d/a", b"333", PutMode::Create).is_err());
    create(b, "top", b"x");
    assert_eq!(names_in(b, "d"), ["a"]);
    assert!(names_in(b, "").iter().all(|name| !is_temp_name(name)));
}

// ---------------------------------------------------------------------------------------------
// The place itself
// ---------------------------------------------------------------------------------------------

fn the_durability_barrier_succeeds(b: &dyn Backend) {
    make_dir(b, "d");
    create(b, "d/x", b"1");
    b.delete("d/x").expect("delete");
    b.sync_dir("d").expect("a barrier on an existing directory succeeds");
}

fn the_location_can_be_prepared(b: &dyn Backend) {
    b.create_root().expect("create the root");
    b.create_root().expect("creating it again succeeds");
    assert_eq!(b.probe().expect("probe"), LocationState::Available);
}

fn the_capabilities_suffice_for_a_repository(b: &dyn Backend) {
    b.capabilities()
        .require_repository_support()
        .expect("a backend that holds repositories must provide what they depend on");
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::super::{
        Capabilities, DirEntryInfo, LocationState, MemoryBackend, Object, ObjectInfo,
    };
    use super::*;
    use crate::error::Result;

    /// A backend with one deliberate defect: `Create` replaces instead of refusing.
    #[derive(Debug)]
    struct LaxCreate(MemoryBackend);

    impl Backend for LaxCreate {
        fn root(&self) -> &Path {
            self.0.root()
        }
        fn capabilities(&self) -> Capabilities {
            self.0.capabilities()
        }
        fn probe(&self) -> Result<LocationState> {
            self.0.probe()
        }
        fn create_root(&self) -> Result<()> {
            self.0.create_root()
        }
        fn create_dir(&self, dir: &str) -> Result<()> {
            self.0.create_dir(dir)
        }
        fn dir_exists(&self, dir: &str) -> Result<bool> {
            self.0.dir_exists(dir)
        }
        fn stat(&self, name: &str) -> Result<ObjectInfo> {
            self.0.stat(name)
        }
        fn get(&self, name: &str, max_len: u64) -> Result<Object> {
            self.0.get(name, max_len)
        }
        fn get_prefix(&self, name: &str, max: usize) -> Result<(Vec<u8>, u64)> {
            self.0.get_prefix(name, max)
        }
        fn list_dir(&self, dir: &str) -> Result<Vec<DirEntryInfo>> {
            self.0.list_dir(dir)
        }
        fn put(&self, name: &str, data: &[u8], mode: PutMode) -> Result<Version> {
            let mode = if mode == PutMode::Create { PutMode::Overwrite } else { mode };
            self.0.put(name, data, mode)
        }
        fn delete(&self, name: &str) -> Result<()> {
            self.0.delete(name)
        }
        fn sync_dir(&self, dir: &str) -> Result<()> {
            self.0.sync_dir(dir)
        }
    }

    #[test]
    fn the_contract_catches_a_backend_that_lets_creation_overwrite() {
        let failures = check(&|| Fixture::new(Arc::new(LaxCreate(MemoryBackend::new()))));
        let failed = |case: &str| failures.iter().any(|line| line.starts_with(case));
        assert!(failed("create_never_overwrites"), "{failures:?}");
        assert!(failed("exactly_one_creator_wins"), "{failures:?}");
        assert!(
            !failed("objects_read_back_exactly"),
            "unrelated cases must still pass: {failures:?}"
        );
    }

    #[test]
    fn the_contract_catches_a_backend_that_is_not_consistent() {
        let weak = Capabilities {
            read_after_write: false,
            ..MemoryBackend::new().capabilities()
        };
        let failures = check(&|| Fixture::new(Arc::new(MemoryBackend::with_capabilities(weak.clone()))));
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].starts_with("the_capabilities_suffice_for_a_repository"), "{failures:?}");
    }

    #[test]
    fn every_case_has_a_distinct_name() {
        let mut names: Vec<&str> = CASES.iter().map(|(name, _)| *name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before);
    }

    #[test]
    fn a_failing_case_is_reported_with_its_message_and_does_not_stop_the_others() {
        let failures = check(&|| Fixture::new(Arc::new(LaxCreate(MemoryBackend::new()))));
        assert!(failures.len() >= 2 && failures.len() < CASES.len(), "{failures:?}");
        let line = failures.iter().find(|line| line.starts_with("create_never_overwrites"));
        assert!(line.is_some_and(|line| line.contains("must fail")), "{failures:?}");
    }
}
