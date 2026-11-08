use crate::blocking::blocking;
use crate::error::Result;
use crate::keys::RepositoryKeys;
use crate::lock::LockKind;
use crate::prune::{self, PruneMode, PruneOptions, PruneReport};
use crate::repository::Repository;

impl Repository {
    /// Applies a retention policy: removes the snapshots it does not keep and then the chunk
    /// objects that no kept snapshot needs. See the [`prune`](crate::prune) module for the
    /// algorithm and the guarantees.
    ///
    /// **Nothing is deleted unless [`PruneOptions::mode`] says so**: the default,
    /// [`PruneMode::DryRun`], only reports the plan. Executing needs either the digest of a plan
    /// someone has reviewed ([`PruneMode::Confirmed`]) or [`PruneMode::Unattended`].
    ///
    /// A dry run holds the shared lock and can run next to a backup (the numbers it reports for
    /// unreferenced objects then include what the backup has stored but not yet committed). A
    /// prune that deletes holds the **exclusive** lock for its whole duration, so it fails with
    /// [`Error::Locked`](crate::Error::Locked) while any backup, restore, check, or other prune
    /// holds the repository, and makes them fail while it runs.
    ///
    /// It refuses, changing nothing, if a snapshot cannot be read or if a snapshot that would be
    /// kept is missing chunks. If it is cancelled or stops on an error, every snapshot that still
    /// exists is complete, and running it again finishes the job.
    pub async fn prune(&self, keys: &RepositoryKeys, options: PruneOptions) -> Result<PruneReport> {
        options.policy.validate()?;
        let kind = if options.mode == PruneMode::DryRun {
            LockKind::Shared
        } else {
            LockKind::Exclusive
        };
        let lock = self.lock(kind).await?;
        let snapshots = self.snapshot_store(keys).await?;
        let chunks = self.chunk_store(keys)?;
        blocking(move || {
            let _lock = lock;
            let subject = prune::Subject {
                snapshots: &snapshots,
                chunks: &chunks,
            };
            prune::run(&subject, &options)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use crate::retention::RetentionPolicy;
    use crate::testing::{init, passphrase};

    async fn repository() -> (tempfile::TempDir, Repository, RepositoryKeys) {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let repo = Repository::at(dir.path().join("repo"));
        init(&repo).await.expect("init");
        let keys = repo.unlock(&passphrase()).await.expect("unlock");
        (dir, repo, keys)
    }

    fn keep_last(count: u32) -> PruneOptions {
        PruneOptions::new(RetentionPolicy {
            keep_last: Some(count),
            ..RetentionPolicy::default()
        })
    }

    #[tokio::test]
    async fn a_policy_that_keeps_nothing_is_rejected_before_the_repository_is_touched() {
        let keys = RepositoryKeys::generate().expect("generate");
        let repo = Repository::at("/does/not/exist");
        let err = repo
            .prune(&keys, PruneOptions::new(RetentionPolicy::default()))
            .await
            .expect_err("must fail");
        assert!(matches!(err, Error::InvalidRetention(_)), "{err}");
    }

    #[tokio::test]
    async fn a_missing_repository_is_reported() {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let keys = RepositoryKeys::generate().expect("generate");
        let repo = Repository::at(dir.path().join("nope"));
        let err = repo.prune(&keys, keep_last(1)).await.expect_err("must fail");
        assert!(matches!(err, Error::NotARepository { .. }), "{err}");
    }

    #[tokio::test]
    async fn an_empty_repository_has_nothing_to_prune_in_any_mode() {
        let (_dir, repo, keys) = repository().await;
        for mode in [PruneMode::DryRun, PruneMode::Unattended] {
            let options = PruneOptions {
                mode,
                ..keep_last(1)
            };
            let report = repo.prune(&keys, options).await.expect("prune");
            assert!(report.is_noop());
            assert_eq!(report.executed, mode == PruneMode::Unattended);
        }
        assert!(repo.store().list("locks").expect("list").is_empty(), "the locks were released");
    }

    #[tokio::test]
    async fn deleting_takes_the_exclusive_lock_and_a_dry_run_only_the_shared_one() {
        let (_dir, repo, keys) = repository().await;

        let shared = repo.lock(LockKind::Shared).await.expect("lock");
        repo.prune(&keys, keep_last(1)).await.expect("a dry run runs next to a shared lock");
        let deleting = PruneOptions {
            mode: PruneMode::Unattended,
            ..keep_last(1)
        };
        let err = repo.prune(&keys, deleting.clone()).await.expect_err("must refuse");
        assert!(matches!(err, Error::Locked { .. }), "{err}");
        drop(shared);

        let exclusive = repo.lock(LockKind::Exclusive).await.expect("lock");
        let err = repo.prune(&keys, keep_last(1)).await.expect_err("even a dry run waits for it");
        assert!(matches!(err, Error::Locked { .. }), "{err}");
        drop(exclusive);

        repo.prune(&keys, deleting).await.expect("and then it runs");
        assert!(repo.store().list("locks").expect("list").is_empty());
    }

    #[tokio::test]
    async fn other_operations_cannot_start_while_a_prune_holds_the_repository() {
        // The lock a deleting prune takes is the one that keeps backups, restores, and checks out.
        let (_dir, repo, _keys) = repository().await;
        let exclusive = repo.lock(LockKind::Exclusive).await.expect("lock");
        for kind in [LockKind::Shared, LockKind::Exclusive] {
            let err = repo.lock(kind).await.expect_err("must fail");
            assert!(matches!(err, Error::Locked { .. }), "{err}");
        }
        drop(exclusive);
        repo.lock(LockKind::Shared).await.expect("free again");
    }
}
