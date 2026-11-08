use std::path::Path;

use crate::blocking::blocking;
use crate::error::Result;
use crate::keys::RepositoryKeys;
use crate::lock::LockKind;
use crate::repository::Repository;
use crate::restore::{self, RestoreOptions, RestoreReport};

impl Repository {
    /// Restores a snapshot, or selected paths of it, into a directory. See the
    /// [`restore`](crate::restore) module for the guarantees, and [`RestoreOptions`] for the
    /// choices.
    ///
    /// The snapshot is loaded with authenticated decryption and validated, every chunk is
    /// authenticated before its bytes are used, and nothing is written outside the target. A
    /// shared lock is held for the duration, so a concurrent `prune` cannot remove chunks under
    /// the restore. Entries that cannot be restored are listed in the report; a conflict in
    /// [`ConflictPolicy::Fail`](crate::restore::ConflictPolicy::Fail) mode, an unsafe path, a bad
    /// target, a missing snapshot, or a cancellation end the restore with an error.
    ///
    /// The work runs on a blocking thread.
    pub async fn restore(
        &self,
        keys: &RepositoryKeys,
        options: RestoreOptions,
    ) -> Result<RestoreReport> {
        let lock = self.lock(LockKind::Shared).await?;
        let snapshots = self.snapshot_store(keys).await?;
        let chunks = self.chunk_store(keys)?;
        // A repository that is not a directory here cannot contain the target.
        let repository_root = self.backend().local_path().map(Path::to_path_buf);
        blocking(move || {
            let _lock = lock;
            let id = snapshots.resolve(&options.snapshot)?;
            let manifest = snapshots.load(&id)?;
            restore::run(&manifest, &chunks, repository_root.as_deref(), &options)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::error::Error;
    use crate::snapshot::testing::{chunk, dir, file, minimal};
    use crate::snapshot::{Manifest, Root, SnapshotRef};
    use crate::testing::{init, passphrase};

    async fn repository() -> (tempfile::TempDir, Repository, RepositoryKeys) {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let repo = Repository::at(dir.path().join("repo"));
        init(&repo).await.expect("init");
        let keys = repo.unlock(&passphrase()).await.expect("unlock");
        (dir, repo, keys)
    }

    fn options(reference: &str, target: std::path::PathBuf) -> RestoreOptions {
        RestoreOptions::new(reference.parse::<SnapshotRef>().expect("valid reference"), target)
    }

    /// Plants an authentic snapshot, sealed with the repository key, that no correct writer would
    /// produce: it is neither validated nor checked for chunks.
    async fn plant(repo: &Repository, keys: &RepositoryKeys, source: &[u8], names: &[&[u8]]) -> Manifest {
        let config = repo.load_config().await.expect("config");
        let mut manifest = minimal();
        manifest.repository_id = config.repository_id;
        let mut entries = vec![dir(None, b"")];
        for name in names {
            entries.push(file(0, name, vec![chunk(1, 4)]));
        }
        manifest.roots = vec![Root {
            source: source.to_vec(),
            entries,
        }];
        manifest.stats = manifest.compute_stats();
        repo.snapshot_store(keys)
            .await
            .expect("snapshot store")
            .commit_unchecked(&manifest)
            .expect("planted");
        manifest
    }

    #[tokio::test]
    async fn a_missing_repository_is_reported() {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let keys = RepositoryKeys::generate().expect("generate");
        let repo = Repository::at(dir.path().join("nope"));
        let target = dir.path().join("out");
        let err = repo.restore(&keys, options("abcd", target.clone())).await.expect_err("must fail");
        assert!(matches!(err, Error::NotARepository { .. }), "{err}");
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn an_unknown_snapshot_fails_before_the_target_is_touched() {
        let (dir, repo, keys) = repository().await;
        let target = dir.path().join("out");
        let err = repo.restore(&keys, options("abcd", target.clone())).await.expect_err("must fail");
        assert!(matches!(err, Error::SnapshotNotFound(_)), "{err}");
        assert!(!target.exists());
        assert!(repo.store().list("locks").expect("list").is_empty(), "the lock was released");
    }

    #[tokio::test]
    async fn a_forged_snapshot_with_unsafe_names_never_gets_as_far_as_the_target() {
        let (dir, repo, keys) = repository().await;
        let hostile: [&[u8]; 4] = [b"..", b"a/b", b"", b"x\0y"];
        for name in hostile {
            let manifest = plant(&repo, &keys, b"/data", &[name]).await;
            let target = dir.path().join("out");
            let reference = manifest.id.as_str()[..16].to_owned();
            let err = repo.restore(&keys, options(&reference, target.clone())).await.expect_err("refused");
            assert!(matches!(err, Error::InvalidManifest(_)), "{name:?}: {err}");
            assert!(!target.exists(), "{name:?}");
            fs::remove_file(repo.layout().snapshot_file(&manifest.id)).expect("remove the planted snapshot");
        }
    }

    #[tokio::test]
    async fn a_forged_snapshot_with_a_source_that_climbs_is_refused_by_the_restore_itself() {
        let (dir, repo, keys) = repository().await;
        // The manifest rules allow any non-empty source path without NUL, so this one loads;
        // the restore is what must refuse it.
        let manifest = plant(&repo, &keys, b"/data/../../escape", &[b"file"]).await;
        let target = dir.path().join("out");
        let full = manifest.id.to_string();
        let err = repo.restore(&keys, options(&full, target.clone())).await.expect_err("refused");
        assert!(matches!(err, Error::UnsafePath { .. }), "{err}");
        assert!(!target.exists());
        assert!(!dir.path().join("escape").exists());
    }
}
