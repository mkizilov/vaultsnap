use crate::backup::{self, BackupOptions, BackupReport, Services};
use crate::blocking::blocking;
use crate::error::Result;
use crate::keys::RepositoryKeys;
use crate::lock::LockKind;
use crate::repository::Repository;
use crate::snapshot::SnapshotStore;

impl Repository {
    /// Backs up `options.sources` into a new snapshot and returns what was done.
    ///
    /// Takes a shared repository lock for the duration, so backups can run side by side but not
    /// during a `prune`. A snapshot is created only if the whole backup succeeds; on failure or
    /// cancellation ([`CancelToken`](crate::CancelToken)) nothing is visible afterwards, the lock
    /// is released, and the chunks already stored are reused by the next run. A backup in which
    /// some files could not be read still succeeds, and the report and the snapshot say which
    /// ([`BackupReport::is_partial`]), unless the scan's error policy is to fail.
    ///
    /// The work runs on blocking threads; see [`BackupOptions::jobs`] for the concurrency bound.
    pub async fn backup(
        &self,
        keys: &RepositoryKeys,
        options: BackupOptions,
    ) -> Result<BackupReport> {
        let lock = self.lock(LockKind::Shared).await?;
        let config = self.load_config().await?;
        let services = Services {
            chunks: self.chunk_store(keys)?,
            snapshots: SnapshotStore::new(self.store(), keys, &config.repository_id)?,
            repository_id: config.repository_id,
            repository_root: self.layout().root().to_path_buf(),
        };
        blocking(move || {
            let _lock = lock;
            backup::run(&services, &options)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::error::Error;
    use crate::testing::{init, passphrase};

    async fn repository() -> (tempfile::TempDir, Repository, RepositoryKeys) {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let repo = Repository::at(dir.path().join("repo"));
        init(&repo).await.expect("init");
        let keys = repo.unlock(&passphrase()).await.expect("unlock");
        (dir, repo, keys)
    }

    #[tokio::test]
    async fn at_least_one_source_is_required() {
        let (_dir, repo, keys) = repository().await;
        let err = repo
            .backup(&keys, BackupOptions::default())
            .await
            .expect_err("must fail");
        assert!(matches!(err, Error::InvalidArgument(_)), "{err}");
    }

    #[tokio::test]
    async fn a_missing_repository_is_reported() {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let keys = RepositoryKeys::generate().expect("generate");
        let repo = Repository::at(dir.path().join("nope"));
        let options = BackupOptions {
            sources: vec![dir.path().to_path_buf()],
            ..BackupOptions::default()
        };
        let err = repo.backup(&keys, options).await.expect_err("must fail");
        assert!(matches!(err, Error::NotARepository { .. }), "{err}");
    }

    #[tokio::test]
    async fn a_cancelled_token_stops_before_anything_is_written() {
        let (dir, repo, keys) = repository().await;
        let source = dir.path().join("src");
        fs::create_dir(&source).expect("mkdir");
        fs::write(source.join("file"), b"data").expect("write");
        let options = BackupOptions {
            sources: vec![source],
            ..BackupOptions::default()
        };
        options.cancel.cancel();
        let err = repo.backup(&keys, options).await.expect_err("must fail");
        assert!(matches!(err, Error::Cancelled), "{err}");
        let store = repo.store();
        assert!(store.list("snapshots").expect("list").is_empty());
        assert!(store.list("locks").expect("list").is_empty(), "the lock was released");
    }

    #[tokio::test]
    async fn sources_inside_the_repository_and_duplicates_are_refused() {
        let (dir, repo, keys) = repository().await;
        let source = dir.path().join("src");
        fs::create_dir(&source).expect("mkdir");
        let inside = BackupOptions {
            sources: vec![repo.layout().root().join("data")],
            ..BackupOptions::default()
        };
        let err = repo.backup(&keys, inside).await.expect_err("must fail");
        assert!(matches!(err, Error::InvalidArgument(_)), "{err}");

        let twice = BackupOptions {
            sources: vec![source.clone(), source.join("..").join("src")],
            ..BackupOptions::default()
        };
        let err = repo.backup(&keys, twice).await.expect_err("must fail");
        assert!(matches!(err, Error::InvalidArgument(_)), "{err}");
        assert!(repo.store().list("snapshots").expect("list").is_empty());
    }
}
