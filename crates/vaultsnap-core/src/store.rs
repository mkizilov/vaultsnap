//! The repository's view of its storage: a cheap, cloneable handle on a
//! [`Backend`](crate::storage::Backend).
//!
//! Everything that reads or writes repository files (keys, locks, the chunk store, the snapshot
//! store, `check`, `prune`) holds a [`Store`] and nothing else, so the same code runs on any
//! backend: the local directory that is the reference implementation, memory, or one day an
//! object store. The handle adds no behavior of its own beyond convenience methods that are
//! defined in terms of the backend's operations, and [`Store::update`], the transactional
//! read-modify-write that conditional writes make possible.
//!
//! Objects are addressed by repository-relative names such as `config` or `locks/ab12.lock`; see
//! [`validate_name`](crate::storage::validate_name) for the accepted syntax.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use crate::error::{Error, Result};
use crate::storage::{
    self, Backend, Capabilities, Guarantee, LocalBackend, LocationState, Object, ObjectInfo,
    PutMode, Version,
};

pub use crate::storage::{DirEntryInfo, DirEntryKind};

/// How often [`Store::update`] starts over when another writer got in between.
const UPDATE_ATTEMPTS: usize = 16;

/// A handle on the storage of one repository. Cloning shares the backend.
#[derive(Debug, Clone)]
pub struct Store {
    backend: Arc<dyn Backend>,
}

impl Store {
    /// A handle on `backend`.
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        Self { backend }
    }

    /// A handle on the directory `root` of the local file system. Performs no I/O.
    pub fn local(root: impl Into<PathBuf>) -> Self {
        Self::new(Arc::new(LocalBackend::new(root)))
    }

    /// The backend behind this handle.
    pub fn backend(&self) -> &Arc<dyn Backend> {
        &self.backend
    }

    /// Where the repository lives, for messages (see [`Backend::root`]).
    pub fn root(&self) -> &Path {
        self.backend.root()
    }

    /// What the backend guarantees.
    pub fn capabilities(&self) -> Capabilities {
        self.backend.capabilities()
    }

    /// True for names of temporary files created while staging a write.
    pub fn is_temp_name(name: &str) -> bool {
        storage::is_temp_name(name)
    }

    /// What the place is like, without creating anything.
    pub fn probe(&self) -> Result<LocationState> {
        self.backend.probe()
    }

    /// Creates the place the repository lives in, with missing parents, if it is not there yet.
    pub fn create_root(&self) -> Result<()> {
        self.backend.create_root()
    }

    /// Creates the directory `name` with private permissions. Succeeds if it already exists.
    pub fn create_dir(&self, name: &str) -> Result<()> {
        self.backend.create_dir(name)
    }

    /// Whether the directory `dir` exists. Backends without directories say yes.
    pub fn dir_exists(&self, dir: &str) -> Result<bool> {
        self.backend.dir_exists(dir)
    }

    /// Whether `name` exists.
    pub fn exists(&self, name: &str) -> Result<bool> {
        self.backend.exists(name)
    }

    /// Size, modification time, and version of `name`, without reading it.
    pub fn stat(&self, name: &str) -> Result<ObjectInfo> {
        self.backend.stat(name)
    }

    /// Reads the whole object `name`, failing with [`Error::TooLarge`] if it exceeds `max_len`.
    pub fn read(&self, name: &str, max_len: u64) -> Result<Vec<u8>> {
        self.backend.get(name, max_len).map(|object| object.data)
    }

    /// Like [`Store::read`], and also returns the version of the bytes that were read.
    pub fn get(&self, name: &str, max_len: u64) -> Result<Object> {
        self.backend.get(name, max_len)
    }

    /// Lists the entries of the directory `dir` (the empty string is the root) with their kind
    /// and size, in ascending order of name. Entries that disappear while the directory is read
    /// are left out. Symbolic links are reported as such, not followed.
    pub fn list_entries(&self, dir: &str) -> Result<Vec<DirEntryInfo>> {
        self.backend.list_dir(dir)
    }

    /// Reads at most the first `max` bytes of the object `name` and returns them with the size
    /// of the whole object.
    pub fn read_prefix(&self, name: &str, max: usize) -> Result<(Vec<u8>, u64)> {
        self.backend.get_prefix(name, max)
    }

    /// The size in bytes of the object `name`, from its metadata (it is not read).
    pub fn file_len(&self, name: &str) -> Result<u64> {
        self.backend.stat(name).map(|info| info.len)
    }

    /// The modification time of `name`.
    pub fn modified(&self, name: &str) -> Result<SystemTime> {
        self.backend.stat(name).map(|info| info.modified)
    }

    /// Lists entry names of the directory `dir` in sorted order. The empty string lists the root.
    pub fn list(&self, dir: &str) -> Result<Vec<String>> {
        Ok(self
            .backend
            .list_dir(dir)?
            .into_iter()
            .map(|entry| entry.name)
            .collect())
    }

    /// Atomically creates the object `name` with `data`. Never overwrites: if `name` exists the
    /// call fails with [`Error::AlreadyExists`] and the existing object is untouched.
    pub fn write_new(&self, name: &str, data: &[u8]) -> Result<()> {
        self.backend.put(name, data, PutMode::Create).map(|_| ())
    }

    /// Atomically creates or replaces the object `name` with `data`.
    pub fn write_replace(&self, name: &str, data: &[u8]) -> Result<()> {
        self.backend.put(name, data, PutMode::Overwrite).map(|_| ())
    }

    /// Writes `name` under the condition `mode` and returns its new version.
    pub fn put(&self, name: &str, data: &[u8], mode: PutMode) -> Result<Version> {
        self.backend.put(name, data, mode)
    }

    /// Makes the creations and removals made so far below `dir` durable: after this returns, a
    /// crash cannot bring a removed object back.
    pub fn sync_directory(&self, dir: &str) -> Result<()> {
        self.backend.sync_dir(dir)
    }

    /// Removes the object `name`.
    pub fn remove(&self, name: &str) -> Result<()> {
        self.backend.delete(name)
    }

    /// Changes the object `name` as one transaction: reads it (`None` if it does not exist yet),
    /// calls `change` with its bytes to get the new bytes, and writes them only if nobody else
    /// wrote the object in between; if somebody did, it starts over with what they wrote.
    /// Returns the version of what was written.
    ///
    /// `change` may therefore run several times and must not have effects of its own. If it
    /// fails, nothing is written and its error is returned. Needs a backend that can replace an
    /// object conditionally ([`Capabilities::replace_if_unchanged`]). The guarantee is only as
    /// wide as the backend's: on the local file system it covers the threads of this process,
    /// and other processes must be kept out with the repository lock.
    pub fn update<F>(&self, name: &str, max_len: u64, mut change: F) -> Result<Version>
    where
        F: FnMut(Option<&[u8]>) -> Result<Vec<u8>>,
    {
        if self.capabilities().replace_if_unchanged == Guarantee::None {
            return Err(Error::UnsupportedBackend(
                "it cannot replace an object only if it is unchanged, which a transactional \
                 update needs"
                    .to_owned(),
            ));
        }
        for _ in 0..UPDATE_ATTEMPTS {
            let (mode, next) = match self.backend.get(name, max_len) {
                Ok(object) => {
                    let next = change(Some(&object.data))?;
                    (PutMode::Update(object.info.version), next)
                }
                Err(err) if err.is_not_found() => (PutMode::Create, change(None)?),
                Err(err) => return Err(err),
            };
            match self.backend.put(name, &next, mode) {
                Err(err) if err.is_conflict() => {}
                other => return other,
            }
        }
        Err(Error::PreconditionFailed {
            path: self.root().join(name),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;
    use crate::storage::{MemoryBackend, MAX_NAME_LEN};

    fn store() -> (TempDir, Store) {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let store = Store::local(dir.path());
        (dir, store)
    }

    #[test]
    fn entries_are_listed_with_their_kind_and_size_in_name_order() {
        let (dir, store) = store();
        store.write_new("config", b"12345").expect("write");
        store.create_dir("data").expect("mkdir");
        fs::write(dir.path().join(".hidden"), b"").expect("write");
        #[cfg(unix)]
        std::os::unix::fs::symlink("config", dir.path().join("link")).expect("symlink");

        let entries = store.list_entries("").expect("list");
        let described: Vec<(&str, DirEntryKind, u64)> = entries
            .iter()
            .map(|entry| (entry.name.as_str(), entry.kind, entry.len))
            .collect();
        let mut expected = vec![
            (".hidden", DirEntryKind::File, 0),
            ("config", DirEntryKind::File, 5),
            ("data", DirEntryKind::Directory, 0),
        ];
        #[cfg(unix)]
        expected.push(("link", DirEntryKind::Symlink, 0));
        assert_eq!(described, expected);
        assert!(store.list_entries("data").expect("list").is_empty());
        assert!(store.list_entries("missing").is_err());
    }

    #[test]
    fn removals_can_be_made_durable_and_a_missing_directory_is_an_error() {
        let (_dir, store) = store();
        store.create_dir("snapshots").expect("mkdir");
        store.write_new("snapshots/a", b"x").expect("write");
        store.remove("snapshots/a").expect("remove");
        store.sync_directory("snapshots").expect("sync");
        assert!(!store.exists("snapshots/a").expect("exists"));
        assert!(store.sync_directory("missing").is_err());
    }

    #[test]
    fn a_prefix_is_read_together_with_the_size_of_the_whole_file() {
        let (_dir, store) = store();
        store.write_new("config", b"0123456789").expect("write");
        assert_eq!(store.read_prefix("config", 4).expect("read"), (b"0123".to_vec(), 10));
        assert_eq!(store.read_prefix("config", 100).expect("read"), (b"0123456789".to_vec(), 10));
        assert_eq!(store.read_prefix("config", 0).expect("read"), (Vec::new(), 10));
        let err = store.read_prefix("absent", 4).expect_err("must fail");
        assert!(err.is_not_found(), "{err}");
    }

    #[test]
    fn write_new_then_read_round_trips() {
        let (_dir, store) = store();
        store.write_new("config", b"hello").expect("write");
        assert_eq!(store.read("config", 100).expect("read"), b"hello");
        assert!(store.exists("config").expect("exists"));
    }

    #[test]
    fn write_new_never_overwrites() {
        let (_dir, store) = store();
        store.write_new("config", b"first").expect("write");
        let err = store.write_new("config", b"second").expect_err("must fail");
        assert!(matches!(err, Error::AlreadyExists { .. }));
        assert_eq!(store.read("config", 100).expect("read"), b"first");
    }

    #[test]
    fn writes_leave_no_temporary_files() {
        let (_dir, store) = store();
        store.write_new("a", b"1").expect("write");
        let _ = store.write_new("a", b"2");
        store.write_replace("b", b"1").expect("write");
        store.write_replace("b", b"2").expect("write");
        assert_eq!(store.list("").expect("list"), ["a", "b"]);
    }

    #[test]
    fn write_replace_overwrites_atomically() {
        let (_dir, store) = store();
        store.write_replace("index", b"old").expect("write");
        store.write_replace("index", b"new").expect("write");
        assert_eq!(store.read("index", 100).expect("read"), b"new");
    }

    #[test]
    fn missing_file_reads_as_not_found() {
        let (_dir, store) = store();
        let err = store.read("nothing", 10).expect_err("must fail");
        assert!(err.is_not_found());
        assert!(!store.exists("nothing").expect("exists"));
    }

    #[test]
    fn oversized_file_is_rejected() {
        let (_dir, store) = store();
        store.write_new("big", &[7_u8; 11]).expect("write");
        assert!(matches!(store.read("big", 10), Err(Error::TooLarge { max_len: 10, .. })));
        assert_eq!(store.read("big", 11).expect("read").len(), 11);
    }

    #[test]
    fn unsafe_names_are_rejected() {
        let (_dir, store) = store();
        let long = "a".repeat(MAX_NAME_LEN + 1);
        let names = [
            "",
            "/",
            "/etc/passwd",
            "..",
            ".",
            "../x",
            "a/../b",
            "a//b",
            "a/",
            "/a",
            ".hidden",
            ".tmp-1",
            "a b",
            "a\\b",
            "a\0b",
            "caf\u{e9}",
            long.as_str(),
        ];
        for name in names {
            assert!(matches!(store.exists(name), Err(Error::InvalidName(_))), "{name:?}");
            assert!(store.write_new(name, b"x").is_err(), "{name:?}");
        }
        assert!(store.list("").expect("list").is_empty());
    }

    #[test]
    fn nested_names_work_inside_existing_directories() {
        let (dir, store) = store();
        store.create_dir("locks").expect("mkdir");
        store.write_new("locks/a.lock", b"x").expect("write");
        assert!(dir.path().join("locks").join("a.lock").is_file());
        assert_eq!(store.list("locks").expect("list"), ["a.lock"]);
    }

    #[test]
    fn writing_into_a_missing_directory_fails_without_creating_it() {
        let (dir, store) = store();
        let err = store.write_new("missing/file", b"x").expect_err("must fail");
        assert!(err.is_not_found(), "{err}");
        assert!(!dir.path().join("missing").exists());
    }

    #[test]
    fn create_dir_is_idempotent_but_refuses_files() {
        let (_dir, store) = store();
        store.create_dir("data").expect("mkdir");
        store.create_dir("data").expect("mkdir again");
        store.write_new("file", b"x").expect("write");
        assert!(store.create_dir("file").is_err());
    }

    #[test]
    fn create_root_makes_missing_parents() {
        let (dir, _) = store();
        let root = dir.path().join("a").join("b").join("repo");
        let store = Store::local(&root);
        store.create_root().expect("create root");
        store.create_root().expect("create root again");
        assert!(root.is_dir());
    }

    #[test]
    fn create_root_fails_when_a_parent_is_a_file() {
        let (dir, store) = store();
        store.write_new("file", b"x").expect("write");
        let nested = Store::local(dir.path().join("file").join("repo"));
        assert!(nested.create_root().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_followed() {
        let (_dir, store) = store();
        let (outside, _) = self::store();
        std::os::unix::fs::symlink(outside.path(), store.root().join("link")).expect("symlink");
        let err = store.write_new("link/file", b"x").expect_err("must fail");
        assert!(matches!(err, Error::Symlink { .. }));
        assert!(matches!(store.read("link", 10), Err(Error::Symlink { .. })));
        assert!(fs::read_dir(outside.path()).expect("read_dir").next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn files_and_directories_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, store) = store();
        store.create_dir("data").expect("mkdir");
        store.write_new("config", b"x").expect("write");
        store.write_replace("data/obj", b"x").expect("write");
        let mode = |name: &str| {
            let path = store.root().join(name);
            fs::metadata(path).expect("metadata").permissions().mode() & 0o777
        };
        assert_eq!(mode("data"), 0o700);
        assert_eq!(mode("config"), 0o600);
        assert_eq!(mode("data/obj"), 0o600);
    }

    #[test]
    fn size_and_time_come_from_the_metadata() {
        let (_dir, store) = store();
        store.write_new("config", b"12345").expect("write");
        assert_eq!(store.file_len("config").expect("len"), 5);
        assert!(store.modified("config").expect("modified") <= SystemTime::now());
        assert!(store.file_len("absent").expect_err("missing").is_not_found());
        assert!(store.modified("absent").expect_err("missing").is_not_found());
    }

    #[test]
    fn a_handle_is_cheap_to_clone_and_shares_its_backend() {
        let store = Store::new(Arc::new(MemoryBackend::new()));
        let other = store.clone();
        store.write_new("shared", b"1").expect("write");
        assert_eq!(other.read("shared", 10).expect("read"), b"1");
        assert!(Arc::ptr_eq(store.backend(), other.backend()));
        assert!(Store::is_temp_name(".tmp-1") && !Store::is_temp_name("tmp-1"));
    }

    fn counting_threads(store: &Store) {
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        for _ in 0..5 {
                            store
                                .update("counter", 100, |current| {
                                    let text = current.map_or(String::new(), |bytes| {
                                        String::from_utf8_lossy(bytes).into_owned()
                                    });
                                    let count: u32 = text.parse().unwrap_or(0);
                                    Ok((count + 1).to_string().into_bytes())
                                })
                                .expect("update");
                        }
                    })
                })
                .collect();
            for handle in handles {
                handle.join().expect("a counting thread panicked");
            }
        });
        assert_eq!(store.read("counter", 100).expect("read"), b"20", "no increment was lost");
    }

    #[test]
    fn concurrent_updates_lose_nothing_on_the_local_file_system() {
        let (_dir, store) = store();
        counting_threads(&store);
    }

    #[test]
    fn concurrent_updates_lose_nothing_on_an_object_store() {
        counting_threads(&Store::new(Arc::new(MemoryBackend::new())));
    }

    #[test]
    fn an_update_sees_the_current_bytes_or_none_and_returns_the_new_version() {
        let (_dir, store) = store();
        let first = store
            .update("meta", 100, |current| {
                assert!(current.is_none(), "the object does not exist yet");
                Ok(b"one".to_vec())
            })
            .expect("create");
        assert_eq!(store.stat("meta").expect("stat").version, first);
        let second = store
            .update("meta", 100, |current| {
                assert_eq!(current, Some(b"one".as_slice()));
                Ok(b"two".to_vec())
            })
            .expect("replace");
        assert_ne!(first, second);
        assert_eq!(store.read("meta", 100).expect("read"), b"two");
    }

    #[test]
    fn an_update_that_fails_writes_nothing_and_a_missing_change_creates_nothing() {
        let (_dir, store) = store();
        let err = store
            .update("meta", 100, |_| Err(Error::InvalidArgument("no")))
            .expect_err("the change failed");
        assert!(matches!(err, Error::InvalidArgument("no")), "{err}");
        assert!(!store.exists("meta").expect("exists"));

        store.write_new("meta", b"kept").expect("write");
        let err = store
            .update("meta", 100, |_| Err(Error::InvalidArgument("no")))
            .expect_err("the change failed");
        assert!(matches!(err, Error::InvalidArgument("no")), "{err}");
        assert_eq!(store.read("meta", 100).expect("read"), b"kept");
    }

    #[test]
    fn an_update_starts_over_when_somebody_else_wrote_in_between() {
        let (_dir, store) = store();
        store.write_new("meta", b"0").expect("write");
        let mut runs = 0;
        store
            .update("meta", 100, |current| {
                runs += 1;
                if runs == 1 {
                    // Somebody else gets in after we have read.
                    store.write_replace("meta", b"interloper").expect("interfere");
                }
                let mut next = current.map(<[u8]>::to_vec).unwrap_or_default();
                next.extend_from_slice(b"+mine");
                Ok(next)
            })
            .expect("update");
        assert_eq!(runs, 2, "the first try was refused and the change ran again");
        assert_eq!(store.read("meta", 100).expect("read"), b"interloper+mine");
    }

    #[test]
    fn an_update_needs_a_backend_that_can_compare_versions() {
        let incapable = MemoryBackend::with_capabilities(Capabilities {
            replace_if_unchanged: Guarantee::None,
            ..MemoryBackend::new().capabilities()
        });
        let store = Store::new(Arc::new(incapable));
        let err = store.update("meta", 100, |_| Ok(Vec::new())).expect_err("unsupported");
        assert!(matches!(err, Error::UnsupportedBackend(_)), "{err}");
        assert!(!store.exists("meta").expect("exists"));
    }
}
