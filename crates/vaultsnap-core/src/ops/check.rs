use crate::blocking::blocking;
use crate::check::{self, CheckOptions, CheckReport};
use crate::chunk_store::ChunkStore;
use crate::error::{Error, Result};
use crate::keys::RepositoryKeys;
use crate::lock::{LockKind, RepositoryLock};
use crate::repository::{read_config, Repository};
use crate::snapshot::SnapshotStore;
use crate::store::Store;

/// Takes the shared lock a check holds. A repository whose `locks` directory is missing or
/// unusable gets no lock, because the check reports that itself; a conflicting lock is an error.
fn lock_if_possible(store: &Store) -> Result<Option<RepositoryLock>> {
    match RepositoryLock::acquire(store, LockKind::Shared) {
        Ok(lock) => Ok(Some(lock)),
        Err(err @ Error::Locked { .. }) => Err(err),
        Err(Error::Io { .. }) => Ok(None),
        Err(err) => Err(err),
    }
}

impl Repository {
    /// Verifies the repository and reports what is wrong with it, without changing anything.
    ///
    /// `keys` come from [`Repository::unlock_for_check`] (or [`Repository::unlock`]). A quick
    /// check ([`CheckOptions::read_data`] false) authenticates every snapshot and examines the
    /// structure of every chunk object; a full check also reads, authenticates, and decrypts
    /// every object. See the [`check`](crate::check) module for exactly what each finds.
    ///
    /// Damage is **reported** in [`CheckReport::findings`], not returned as an error. An error
    /// means the check could not be carried out: this is not a repository, its `config` is
    /// unusable, another process holds an exclusive lock, the repository root cannot be read, or
    /// the check was cancelled. Nothing is ever repaired, moved, or deleted; the only file the
    /// check writes is its own lock, which it removes at the end.
    pub async fn check(&self, keys: &RepositoryKeys, options: CheckOptions) -> Result<CheckReport> {
        let store = self.store();
        let config = {
            let store = store.clone();
            blocking(move || read_config(&store)).await?
        };
        let snapshots = SnapshotStore::new(store.clone(), keys, &config.repository_id)?;
        let chunks = ChunkStore::new(store.clone(), keys)?;
        blocking(move || {
            let lock = lock_if_possible(&store)?;
            let own_lock = lock.as_ref().map(RepositoryLock::name);
            let subject = check::Subject {
                store: &store,
                snapshots: &snapshots,
                chunks: &chunks,
                config: &config,
            };
            let report = check::run(&subject, &options, own_lock);
            drop(lock);
            report
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{init, passphrase};

    async fn repository() -> (tempfile::TempDir, Repository, RepositoryKeys) {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let repo = Repository::at(dir.path().join("repo"));
        init(&repo).await.expect("init");
        let keys = repo.unlock(&passphrase()).await.expect("unlock");
        (dir, repo, keys)
    }

    #[tokio::test]
    async fn a_new_repository_is_clean_in_both_modes() {
        let (_dir, repo, keys) = repository().await;
        for read_data in [false, true] {
            let options = CheckOptions {
                read_data,
                ..CheckOptions::default()
            };
            let report = repo.check(&keys, options).await.expect("check");
            assert!(report.is_clean(), "{:?}", report.findings);
            assert_eq!(report.stats.key_files, 1);
            assert_eq!((report.stats.snapshots, report.stats.objects), (0, 0));
            assert_eq!(report.repository_id.len(), 32);
            assert_eq!(report.format_version, 1);
        }
        assert!(repo.store().list("locks").expect("list").is_empty(), "the lock was released");
    }

    #[tokio::test]
    async fn a_missing_repository_is_an_error_of_the_check_not_a_finding() {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let keys = RepositoryKeys::generate().expect("generate");
        let repo = Repository::at(dir.path().join("nope"));
        let err = repo.check(&keys, CheckOptions::default()).await.expect_err("must fail");
        assert!(matches!(err, Error::NotARepository { .. }), "{err}");
    }

    #[tokio::test]
    async fn an_exclusive_lock_stops_the_check_and_is_left_alone() {
        let (_dir, repo, keys) = repository().await;
        let held = repo.lock(LockKind::Exclusive).await.expect("lock");
        let err = repo.check(&keys, CheckOptions::default()).await.expect_err("must fail");
        assert!(matches!(err, Error::Locked { .. }), "{err}");
        drop(held);
        repo.check(&keys, CheckOptions::default()).await.expect("check");
    }

    #[tokio::test]
    async fn a_shared_lock_does_not_stop_the_check_but_is_reported() {
        let (_dir, repo, keys) = repository().await;
        let held = repo.lock(LockKind::Shared).await.expect("lock");
        let report = repo.check(&keys, CheckOptions::default()).await.expect("check");
        assert!(report.is_ok());
        assert_eq!(report.count(crate::check::FindingKind::LockPresent), 1);
        assert_eq!(report.warnings, 1);
        drop(held);
    }

    #[tokio::test]
    async fn a_missing_locks_directory_is_reported_and_the_check_still_runs() {
        let (_dir, repo, keys) = repository().await;
        std::fs::remove_dir_all(repo.layout().locks_dir()).expect("remove");
        let report = repo.check(&keys, CheckOptions::default()).await.expect("check");
        assert_eq!(report.count(crate::check::FindingKind::MissingDirectory), 1);
        assert_eq!(report.findings[0].subject, "locks");
        assert!(!report.is_ok());
    }

    #[tokio::test]
    async fn a_cancelled_check_fails_and_releases_its_lock() {
        let (_dir, repo, keys) = repository().await;
        let options = CheckOptions::default();
        options.cancel.cancel();
        let err = repo.check(&keys, options).await.expect_err("must fail");
        assert!(matches!(err, Error::CheckCancelled), "{err}");
        assert!(repo.store().list("locks").expect("list").is_empty());
    }
}
