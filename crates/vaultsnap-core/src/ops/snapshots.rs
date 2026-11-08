use crate::blocking::blocking;
use crate::error::Result;
use crate::history::{self, Diff, SnapshotDetails, SnapshotList};
use crate::keys::RepositoryKeys;
use crate::lock::LockKind;
use crate::repository::Repository;
use crate::snapshot::{Manifest, SnapshotRef};

impl Repository {
    /// Lists the snapshots of the repository, oldest first, with their sizes.
    ///
    /// Every snapshot is authenticated and decrypted with `keys`; one that fails is reported in
    /// [`SnapshotList::damaged`] instead of failing the listing. Costs time proportional to the
    /// total size of the manifests, plus one `stat` per distinct chunk. Holds a shared lock.
    pub async fn snapshots(&self, keys: &RepositoryKeys) -> Result<SnapshotList> {
        let lock = self.lock(LockKind::Shared).await?;
        let snapshots = self.snapshot_store(keys).await?;
        let chunks = self.chunk_store(keys)?;
        blocking(move || {
            let _lock = lock;
            history::list(&snapshots, &chunks)
        })
        .await
    }

    /// Loads the snapshot `reference` names: authenticated, decrypted, and validated. Fails with
    /// [`Error::SnapshotNotFound`](crate::Error::SnapshotNotFound), with
    /// [`Error::AmbiguousSnapshot`](crate::Error::AmbiguousSnapshot), or, if the snapshot file is
    /// damaged or was tampered with, with [`Error::CorruptSnapshot`](crate::Error::CorruptSnapshot)
    /// or [`Error::Decryption`](crate::Error::Decryption).
    pub async fn snapshot(
        &self,
        keys: &RepositoryKeys,
        reference: &SnapshotRef,
    ) -> Result<Manifest> {
        let lock = self.lock(LockKind::Shared).await?;
        let snapshots = self.snapshot_store(keys).await?;
        let reference = reference.clone();
        blocking(move || {
            let _lock = lock;
            let id = snapshots.resolve(&reference)?;
            snapshots.load(&id)
        })
        .await
    }

    /// Loads one snapshot and measures it against all the others (see
    /// [`SnapshotDetails::unique_sizes_exact`] for when that is exact).
    pub async fn snapshot_details(
        &self,
        keys: &RepositoryKeys,
        reference: &SnapshotRef,
    ) -> Result<SnapshotDetails> {
        let lock = self.lock(LockKind::Shared).await?;
        let snapshots = self.snapshot_store(keys).await?;
        let chunks = self.chunk_store(keys)?;
        let reference = reference.clone();
        blocking(move || {
            let _lock = lock;
            let id = snapshots.resolve(&reference)?;
            history::details(&snapshots, &chunks, &id)
        })
        .await
    }

    /// Compares two snapshots, `from` (normally the older) and `to`: the paths added, deleted, and
    /// modified, and the stored data particular to each. See [`Diff`] for what counts as a
    /// difference. Both snapshots are authenticated and validated; both references are resolved
    /// before any snapshot is read.
    pub async fn compare_snapshots(
        &self,
        keys: &RepositoryKeys,
        from: &SnapshotRef,
        to: &SnapshotRef,
    ) -> Result<Diff> {
        let lock = self.lock(LockKind::Shared).await?;
        let snapshots = self.snapshot_store(keys).await?;
        let chunks = self.chunk_store(keys)?;
        let (from, to) = (from.clone(), to.clone());
        blocking(move || {
            let _lock = lock;
            let from_id = snapshots.resolve(&from)?;
            let to_id = snapshots.resolve(&to)?;
            let older = snapshots.load(&from_id)?;
            let newer = snapshots.load(&to_id)?;
            history::compare(&older, &newer, &chunks)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
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

    fn reference(text: &str) -> SnapshotRef {
        text.parse().expect("valid reference")
    }

    #[tokio::test]
    async fn a_new_repository_has_no_snapshots() {
        let (_dir, repo, keys) = repository().await;
        let list = repo.snapshots(&keys).await.expect("list");
        assert!(list.snapshots.is_empty() && list.damaged.is_empty());
        assert!(list.unique_sizes_exact());
    }

    #[tokio::test]
    async fn a_missing_repository_is_reported() {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let keys = RepositoryKeys::generate().expect("generate");
        let repo = Repository::at(dir.path().join("nope"));
        let err = repo.snapshots(&keys).await.expect_err("must fail");
        assert!(matches!(err, Error::NotARepository { .. }), "{err}");
        let err = repo.snapshot(&keys, &reference("abcd")).await.expect_err("must fail");
        assert!(matches!(err, Error::NotARepository { .. }), "{err}");
    }

    #[tokio::test]
    async fn unknown_references_are_reported_and_the_lock_is_released() {
        let (_dir, repo, keys) = repository().await;
        let unknown = reference("abcd");
        let err = repo.snapshot(&keys, &unknown).await.expect_err("must fail");
        assert!(matches!(err, Error::SnapshotNotFound(_)), "{err}");
        let err = repo.snapshot_details(&keys, &unknown).await.expect_err("must fail");
        assert!(matches!(err, Error::SnapshotNotFound(_)), "{err}");
        let err = repo.compare_snapshots(&keys, &unknown, &unknown).await.expect_err("must fail");
        assert!(matches!(err, Error::SnapshotNotFound(_)), "{err}");
        assert!(repo.store().list("locks").expect("list").is_empty(), "the lock was released");
    }
}
