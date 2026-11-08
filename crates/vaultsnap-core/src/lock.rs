//! Advisory repository locks (see `docs/FORMAT.md`).
//!
//! A lock is a file `locks/<random>.lock` created atomically. Any number of shared locks may
//! coexist; an exclusive lock excludes every other lock. After creating its own file a process
//! scans the others and gives up (removing its file) if it finds a conflict, so two racing
//! exclusive requests may both fail but can never both succeed. A lock left behind by a crashed
//! process is never removed automatically; the error names the file so an operator can.

use crate::error::{Error, Result};
use crate::repository::RepositoryLayout;
use crate::store::Store;
use crate::util::{random_hex, unix_now};

const LOCK_SUFFIX: &str = ".lock";
pub(crate) const MAX_LOCK_BYTES: u64 = 1024;

/// How a lock may be shared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockKind {
    /// Many holders may read concurrently.
    Shared,
    /// A single holder, for operations that remove data.
    Exclusive,
}

impl LockKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::Exclusive => "exclusive",
        }
    }
}

/// What a lock file says about the process that made it. Fields that are missing or cannot be
/// read are `None`: a stale or damaged lock file is described as far as it can be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LockInfo {
    pub kind: Option<LockKind>,
    pub pid: Option<u32>,
    pub created_at_unix: Option<u64>,
}

/// Reads the `key=value` lines of a lock file leniently, since this is used on files that may be
/// leftovers or damaged.
pub(crate) fn describe_lock(data: &[u8]) -> LockInfo {
    let text = String::from_utf8_lossy(data);
    let mut info = LockInfo {
        kind: None,
        pid: None,
        created_at_unix: None,
    };
    for line in text.lines() {
        match line.split_once('=') {
            Some(("kind", "shared")) => info.kind = Some(LockKind::Shared),
            Some(("kind", "exclusive")) => info.kind = Some(LockKind::Exclusive),
            Some(("pid", value)) => info.pid = value.parse().ok(),
            Some(("created_at_unix", value)) => info.created_at_unix = value.parse().ok(),
            _ => {}
        }
    }
    info
}

/// A held repository lock; the lock file is removed when this value is dropped.
#[derive(Debug)]
pub struct RepositoryLock {
    store: Store,
    name: String,
    kind: LockKind,
}

impl RepositoryLock {
    pub(crate) fn acquire(store: &Store, kind: LockKind) -> Result<Self> {
        let file_name = format!("{}{LOCK_SUFFIX}", random_hex(16)?);
        let name = format!("{}/{file_name}", RepositoryLayout::LOCKS_DIR);
        let content = format!(
            "kind={}\npid={}\ncreated_at_unix={}\n",
            kind.as_str(),
            std::process::id(),
            unix_now()
        );
        store.write_new(&name, content.as_bytes())?;
        let lock = Self {
            store: store.clone(),
            name,
            kind,
        };

        for other in store.list(RepositoryLayout::LOCKS_DIR)? {
            if other == file_name || !other.ends_with(LOCK_SUFFIX) {
                continue;
            }
            let other_name = format!("{}/{other}", RepositoryLayout::LOCKS_DIR);
            let data = match store.read(&other_name, MAX_LOCK_BYTES) {
                Ok(data) => data,
                Err(err) if err.is_not_found() => continue,
                Err(err) => return Err(err),
            };
            if kind == LockKind::Exclusive || !is_shared(&data) {
                return Err(Error::Locked { holder: other_name });
            }
        }
        Ok(lock)
    }

    /// The kind of lock held.
    pub fn kind(&self) -> LockKind {
        self.kind
    }

    /// The repository-relative name of the lock file, `locks/<random>.lock`.
    pub(crate) fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for RepositoryLock {
    fn drop(&mut self) {
        if let Err(err) = self.store.remove(&self.name) {
            tracing::warn!(lock = %self.name, error = %err, "failed to remove lock file");
        }
    }
}

/// Anything that is not recognizably a shared lock is treated as exclusive.
fn is_shared(data: &[u8]) -> bool {
    data.split(|byte| *byte == b'\n')
        .any(|line| line == b"kind=shared")
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn store() -> (TempDir, Store) {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let store = Store::local(dir.path());
        store.create_dir(RepositoryLayout::LOCKS_DIR).expect("mkdir");
        (dir, store)
    }

    fn lock_files(store: &Store) -> usize {
        store.list(RepositoryLayout::LOCKS_DIR).expect("list").len()
    }

    #[test]
    fn a_lock_file_is_described_as_far_as_it_can_be_read() {
        let written = "kind=exclusive\npid=4242\ncreated_at_unix=1700000000\n";
        let info = describe_lock(written.as_bytes());
        assert_eq!(info.kind, Some(LockKind::Exclusive));
        assert_eq!((info.pid, info.created_at_unix), (Some(4242), Some(1_700_000_000)));

        let shared = describe_lock(b"kind=shared\n");
        assert_eq!(shared.kind, Some(LockKind::Shared));
        assert_eq!((shared.pid, shared.created_at_unix), (None, None));

        let nonsense = describe_lock(b"\xff\xfe kind=shared pid=x\nkind=other\npid=-3\n=\n");
        assert_eq!((nonsense.kind, nonsense.pid, nonsense.created_at_unix), (None, None, None));
        assert_eq!(describe_lock(b""), nonsense);
    }

    #[test]
    fn the_describe_function_reads_what_acquire_writes() {
        let (_dir, store) = store();
        let lock = RepositoryLock::acquire(&store, LockKind::Shared).expect("lock");
        let data = store.read(lock.name(), MAX_LOCK_BYTES).expect("read");
        let info = describe_lock(&data);
        assert_eq!(info.kind, Some(LockKind::Shared));
        assert_eq!(info.pid, Some(std::process::id()));
        assert!(info.created_at_unix.is_some());
        assert!(lock.name().starts_with("locks/") && lock.name().ends_with(".lock"));
    }

    #[test]
    fn shared_locks_coexist() {
        let (_dir, store) = store();
        let a = RepositoryLock::acquire(&store, LockKind::Shared).expect("first");
        let b = RepositoryLock::acquire(&store, LockKind::Shared).expect("second");
        assert_eq!(a.kind(), LockKind::Shared);
        assert_eq!(b.kind(), LockKind::Shared);
        assert_eq!(lock_files(&store), 2);
    }

    #[test]
    fn exclusive_conflicts_with_shared_in_both_orders() {
        let (_dir, store) = store();
        let shared = RepositoryLock::acquire(&store, LockKind::Shared).expect("shared");
        let err = RepositoryLock::acquire(&store, LockKind::Exclusive).expect_err("must fail");
        assert!(matches!(err, Error::Locked { .. }));
        drop(shared);

        let exclusive = RepositoryLock::acquire(&store, LockKind::Exclusive).expect("exclusive");
        let err = RepositoryLock::acquire(&store, LockKind::Shared).expect_err("must fail");
        assert!(matches!(err, Error::Locked { .. }));
        let err = RepositoryLock::acquire(&store, LockKind::Exclusive).expect_err("must fail");
        assert!(matches!(err, Error::Locked { .. }));
        drop(exclusive);
    }

    #[test]
    fn failed_attempts_and_drops_leave_no_lock_files() {
        let (_dir, store) = store();
        let held = RepositoryLock::acquire(&store, LockKind::Exclusive).expect("exclusive");
        let _ = RepositoryLock::acquire(&store, LockKind::Shared);
        assert_eq!(lock_files(&store), 1);
        drop(held);
        assert_eq!(lock_files(&store), 0);
        RepositoryLock::acquire(&store, LockKind::Exclusive).expect("free again");
    }

    #[test]
    fn unrecognizable_lock_files_block_everyone() {
        let (_dir, store) = store();
        store.write_new("locks/garbage.lock", b"???").expect("write");
        let err = RepositoryLock::acquire(&store, LockKind::Shared).expect_err("must fail");
        assert!(matches!(err, Error::Locked { holder } if holder == "locks/garbage.lock"));
        assert_eq!(lock_files(&store), 1);
    }

    #[test]
    fn unrelated_files_in_the_locks_directory_are_ignored() {
        let (_dir, store) = store();
        store.write_new("locks/notes", b"x").expect("write");
        RepositoryLock::acquire(&store, LockKind::Exclusive).expect("exclusive");
    }
}
