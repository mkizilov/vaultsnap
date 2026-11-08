//! Storage backends: the one interface through which a repository reads and writes its files.
//!
//! A repository is a set of named objects (`config`, `keys/<id>`, `snapshots/<id>`,
//! `data/ab/<id>`, `locks/<id>.lock`; see `docs/FORMAT.md`). Everything above this module (keys,
//! locks, the chunk store, the snapshot store, `check`, `prune`) reaches them only through a
//! [`Backend`], by way of the [`Store`](crate::Store) handle, so the same code can run on a
//! directory, in memory, or, one day, on an S3-compatible object store.
//!
//! The interface is the *least common denominator of a file system and an object store*:
//!
//! * a flat namespace of objects named by validated, `/`-separated names (see [`validate_name`]);
//!   directories are prefixes, and a backend with [`Capabilities::directories`] = `false` has no
//!   empty ones;
//! * whole-object reads and prefix reads, whole-object writes, no appends, no renames, no hard
//!   links, no in-place modification;
//! * three kinds of write ([`PutMode`]): create only if absent, replace unconditionally, and
//!   replace only if the object is still in the [`Version`] that was read;
//! * deletes, listings of one directory level, and a durability barrier;
//! * an honest description of what the backend guarantees ([`Capabilities`]), which the
//!   repository checks before it trusts the backend with data.
//!
//! The reference implementation is [`LocalBackend`]. [`MemoryBackend`] has object-store
//! semantics and is the proof that nothing above this module needs a file system.
//! [`RetryingBackend`] adds retries with backoff to any backend, and [`FlakyBackend`] injects the
//! failures that test them. [`S3Backend`] keeps a repository in an S3-compatible bucket (see
//! [`s3`]). [`contract`] is the test suite every backend must pass. How each rule here maps onto
//! S3 is in `docs/STORAGE.md`, and how to use a bucket in `docs/S3.md`.
//!
//! Backends are synchronous. The repository calls them on Tokio's blocking pool (see
//! `blocking`), so a network backend can block on its own client without touching the async
//! runtime of the caller.

pub mod contract;
mod fault;
mod local;
mod memory;
mod retry;
pub mod s3;

use std::fmt;
use std::path::Path;
use std::time::SystemTime;

use crate::error::{Error, Result};

pub use fault::{Fault, FlakyBackend};
pub use local::LocalBackend;
pub use memory::MemoryBackend;
pub use retry::{ImmediateSleeper, RetryPolicy, RetryingBackend, Sleeper, ThreadSleeper};
pub use s3::{S3Backend, S3Config};

/// The longest accepted name segment, in bytes.
pub const MAX_NAME_LEN: usize = 255;

/// The prefix of the names of temporary files a backend may use to stage a write. Valid object
/// names never start with it, so a temporary file can never be mistaken for an object.
pub const TEMP_PREFIX: &str = ".tmp-";

/// True for names of temporary files created while staging a write.
pub fn is_temp_name(name: &str) -> bool {
    name.starts_with(TEMP_PREFIX)
}

/// Accepts `/`-separated segments of `[A-Za-z0-9._-]`, none empty, none starting with `.`, none
/// longer than [`MAX_NAME_LEN`]. This excludes absolute paths, `.`, `..`, hidden files, and a
/// backend's own temporary files, and it means a name is safe to turn into a file system path
/// or an object key. Every backend must apply it to every name it is given.
pub fn validate_name(name: &str) -> Result<()> {
    let invalid = || Error::InvalidName(name.to_owned());
    if name.is_empty() {
        return Err(invalid());
    }
    for segment in name.split('/') {
        let valid_char = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-');
        let valid = !segment.is_empty()
            && segment.len() <= MAX_NAME_LEN
            && !segment.starts_with('.')
            && segment.bytes().all(valid_char);
        if !valid {
            return Err(invalid());
        }
    }
    Ok(())
}

/// Like [`validate_name`], but the empty string is accepted and means the root.
pub fn validate_dir(dir: &str) -> Result<()> {
    if dir.is_empty() {
        Ok(())
    } else {
        validate_name(dir)
    }
}

/// What a directory entry is, looked at without following symbolic links.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirEntryKind {
    /// A regular file: an object.
    File,
    /// A directory: a prefix that has objects below it.
    Directory,
    /// A symbolic link (never followed). Only a file system has these.
    Symlink,
    /// Anything else: a FIFO, a socket, a device. Only a file system has these.
    Other,
}

/// One entry of a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntryInfo {
    /// The entry's name, without its directory. A name that is not valid UTF-8 is converted
    /// lossily.
    pub name: String,
    /// What it is.
    pub kind: DirEntryKind,
    /// Size in bytes for a regular file, 0 for anything else.
    pub len: u64,
}

/// An opaque token that identifies one version of an object: it changes whenever the object is
/// written, and it is what [`PutMode::Update`] compares. On S3 it is the ETag; on a file system
/// it is derived from the file's identity and times. Callers must treat it as opaque and compare
/// it only for equality, and only with versions of the same object from the same backend.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Version(String);

impl Version {
    /// A version with the given token.
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    /// The token.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a backend knows about one object without reading it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectInfo {
    /// Size in bytes.
    pub len: u64,
    /// When the object was last written, by the clock of whoever stores it.
    pub modified: SystemTime,
    /// The version.
    pub version: Version,
}

/// A whole object and what is known about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Object {
    /// The bytes.
    pub data: Vec<u8>,
    /// The size, time, and version of exactly these bytes.
    pub info: ObjectInfo,
}

/// The condition of a write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PutMode {
    /// Create the object; fail with [`Error::AlreadyExists`] if it exists. Exactly one of any
    /// number of concurrent creators of a name succeeds. On S3 this is a `PUT` with
    /// `If-None-Match: *`.
    Create,
    /// Create the object or replace it, whatever is there. Readers see the old object or the new
    /// one, never a mixture. On S3 this is a plain `PUT`.
    Overwrite,
    /// Replace the object only if it still has this version; otherwise fail with
    /// [`Error::PreconditionFailed`]. A missing object has no version, so this fails for it too.
    /// On S3 this is a `PUT` with `If-Match: <etag>`.
    Update(Version),
}

/// The state of the place a repository lives in, before anything is read from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocationState {
    /// There is nothing there yet, but it can be created.
    Absent,
    /// It exists and can hold objects (it may be empty).
    Available,
    /// It exists and cannot hold a repository; the text says why, for a person to read.
    Unusable(String),
}

/// The scope in which a conditional write is atomic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Guarantee {
    /// Not guaranteed: two writers can both succeed.
    None,
    /// Atomic among the threads of one process, not against other processes.
    Process,
    /// Atomic among all clients of the same storage.
    Global,
}

/// What a backend guarantees. A backend must not claim more than it provides: the repository's
/// safety (locks, the all-or-nothing commit of a snapshot, no-overwrite chunk publication)
/// rests on these statements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// After a write or delete returns, every later read, existence check, and listing by any
    /// client sees it. Lock acquisition depends on this: it lists the lock directory right
    /// after creating its own lock file.
    pub read_after_write: bool,
    /// How well [`PutMode::Create`] excludes concurrent creators.
    pub create_if_absent: Guarantee,
    /// How well [`PutMode::Update`] excludes concurrent writers.
    pub replace_if_unchanged: Guarantee,
    /// An object becomes visible whole or not at all, including when it replaces an older one,
    /// and a failed write leaves nothing visible.
    pub atomic_put: bool,
    /// Empty directories exist as entities: a directory can be created, can be missing, and a
    /// write below a missing directory fails. Object stores have none of this.
    pub directories: bool,
}

impl Capabilities {
    /// Checks that these guarantees are enough to hold a repository, and says which one is
    /// missing if not. Only [`PutMode::Create`] has to be globally exclusive: locks, snapshot
    /// commits, and chunk publication use nothing stronger.
    pub fn require_repository_support(&self) -> Result<()> {
        let unsupported = |reason: &str| Err(Error::UnsupportedBackend(reason.to_owned()));
        if !self.read_after_write {
            return unsupported(
                "a completed write or delete must be visible to every later read and listing \
                 (read-after-write consistency); lock acquisition depends on it",
            );
        }
        if self.create_if_absent != Guarantee::Global {
            return unsupported(
                "creating an object only if it does not exist must be atomic for all clients \
                 (a conditional create); locks, snapshot commits, and chunk publication depend \
                 on it",
            );
        }
        if !self.atomic_put {
            return unsupported("an object must become visible whole, never partly written");
        }
        Ok(())
    }
}

/// A place objects are stored. See the module documentation for the model, `docs/STORAGE.md` for
/// the reasoning, and [`contract`] for the tests every implementation must pass.
///
/// All names are validated with [`validate_name`] (directories with [`validate_dir`]) and an
/// invalid one fails with [`Error::InvalidName`] before anything else happens. Errors that mean
/// "does not exist" must satisfy [`Error::is_not_found`], errors that may go away on a retry
/// [`Error::is_transient`]. Implementations are shared between threads and must be safe to call
/// concurrently.
pub trait Backend: Send + Sync + fmt::Debug {
    /// Where the repository lives, for messages and for comparing against the user's paths: the
    /// directory for a file system, a URL such as `s3://bucket/prefix` for anything else. Never
    /// used to reach data.
    fn root(&self) -> &Path;

    /// The directory the repository occupies on this machine, if it is a directory here. This is
    /// what lets `backup` leave the repository out of its own sources and `restore` refuse to
    /// write into it. `None` for anything that is not a local directory.
    fn local_path(&self) -> Option<&Path> {
        None
    }

    /// What this backend guarantees.
    fn capabilities(&self) -> Capabilities;

    /// What the place is like, without creating anything. An error means it could not be found
    /// out (permission, network), which is different from [`LocationState::Absent`].
    fn probe(&self) -> Result<LocationState>;

    /// Creates the place (a directory and its missing parents, a bucket prefix is nothing to
    /// create). Succeeds if it exists already.
    fn create_root(&self) -> Result<()>;

    /// Creates the directory `dir` below the root with private permissions where the backend has
    /// such a thing; its parent must exist. Succeeds if it exists already. A no-op for backends
    /// without directories.
    fn create_dir(&self, dir: &str) -> Result<()>;

    /// Whether the directory `dir` exists. A backend without directories answers `true` for
    /// every valid name; check [`Capabilities::directories`] before reading anything into it.
    fn dir_exists(&self, dir: &str) -> Result<bool>;

    /// Whether the object `name` exists.
    fn exists(&self, name: &str) -> Result<bool> {
        match self.stat(name) {
            Ok(_) => Ok(true),
            Err(err) if err.is_not_found() => Ok(false),
            Err(err) => Err(err),
        }
    }

    /// Size, modification time, and version of `name`, without reading it. Fails with a
    /// not-found error if there is no such object. On S3 this is `HEAD`.
    fn stat(&self, name: &str) -> Result<ObjectInfo>;

    /// Reads the whole object. Fails with [`Error::TooLarge`] if it is longer than `max_len`
    /// bytes, without reading it all, and with a not-found error if it does not exist. The
    /// returned version is the version of exactly the bytes returned. On S3 this is `GET`.
    fn get(&self, name: &str, max_len: u64) -> Result<Object>;

    /// Reads at most the first `max` bytes of the object and returns them with the size of the
    /// whole object. On S3 this is a ranged `GET`.
    fn get_prefix(&self, name: &str, max: usize) -> Result<(Vec<u8>, u64)>;

    /// The entries of the directory `dir` (the empty string is the root), one level only, in
    /// ascending order of name. An entry that disappears while the listing is made may be left
    /// out. A backend with directories fails for a directory that does not exist; one without
    /// returns an empty list. On S3 this is a `LIST` with a `/` delimiter; paging through the
    /// results is the backend's business.
    fn list_dir(&self, dir: &str) -> Result<Vec<DirEntryInfo>>;

    /// Writes the object `name` under the condition `mode` and returns the version it now has.
    /// A write is atomic (see [`Capabilities::atomic_put`]) and fails if the directory it would
    /// go into does not exist, on a backend with directories.
    fn put(&self, name: &str, data: &[u8], mode: PutMode) -> Result<Version>;

    /// Deletes the object. Deleting an object that does not exist either succeeds or fails with
    /// a not-found error; callers accept both, because object stores cannot tell the
    /// difference.
    fn delete(&self, name: &str) -> Result<()>;

    /// Makes the creations and deletions made so far below `dir` durable: after this returns, a
    /// crash cannot bring back a deleted object or lose a created one. A backend whose every
    /// completed operation is already durable does nothing. `prune` calls this between removing
    /// snapshots and removing chunks.
    fn sync_dir(&self, dir: &str) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_documented_syntax() {
        for good in ["config", "keys/ab12", "data/ab/abcdef", "a.b-c_d", "x/y/z"] {
            assert!(validate_name(good).is_ok(), "{good:?}");
        }
        let long_ok = "a".repeat(MAX_NAME_LEN);
        let long_bad = "a".repeat(MAX_NAME_LEN + 1);
        assert!(validate_name(&long_ok).is_ok());
        for bad in [
            "",
            "/",
            "/abs",
            "a//b",
            "a/",
            "../x",
            "a/../b",
            ".",
            "..",
            ".hidden",
            "a/.b",
            ".tmp-0011",
            "a b",
            "a\\b",
            "a\0b",
            "naïve",
            long_bad.as_str(),
        ] {
            assert!(
                matches!(validate_name(bad), Err(Error::InvalidName(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn the_empty_directory_name_is_the_root() {
        assert!(validate_dir("").is_ok());
        assert!(validate_dir("data").is_ok());
        assert!(validate_dir("../data").is_err());
    }

    #[test]
    fn temporary_names_can_never_be_object_names() {
        assert!(is_temp_name(".tmp-0123"));
        assert!(!is_temp_name("tmp-0123"));
        assert!(validate_name(".tmp-0123").is_err());
    }

    fn caps() -> Capabilities {
        Capabilities {
            read_after_write: true,
            create_if_absent: Guarantee::Global,
            replace_if_unchanged: Guarantee::None,
            atomic_put: true,
            directories: false,
        }
    }

    #[test]
    fn a_backend_needs_consistency_conditional_create_and_atomic_writes() {
        assert!(caps().require_repository_support().is_ok());
        let cases = [
            Capabilities {
                read_after_write: false,
                ..caps()
            },
            Capabilities {
                create_if_absent: Guarantee::Process,
                ..caps()
            },
            Capabilities {
                create_if_absent: Guarantee::None,
                ..caps()
            },
            Capabilities {
                atomic_put: false,
                ..caps()
            },
        ];
        for case in cases {
            let err = case
                .require_repository_support()
                .expect_err("must be refused");
            assert!(
                matches!(err, Error::UnsupportedBackend(_)),
                "{case:?}: {err}"
            );
        }
    }

    #[test]
    fn replacing_conditionally_is_not_needed_to_hold_a_repository() {
        let caps = Capabilities {
            replace_if_unchanged: Guarantee::None,
            ..caps()
        };
        assert!(caps.require_repository_support().is_ok());
    }

    #[test]
    fn guarantees_are_ordered_from_none_to_global() {
        assert!(Guarantee::None < Guarantee::Process && Guarantee::Process < Guarantee::Global);
    }

    #[test]
    fn versions_are_compared_by_their_token() {
        assert_eq!(Version::new("a"), Version::new(String::from("a")));
        assert_ne!(Version::new("a"), Version::new("b"));
        assert_eq!(Version::new("a").to_string(), "a");
        assert_eq!(Version::new("a").as_str(), "a");
    }
}
