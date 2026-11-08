use crate::blocking::blocking;
use crate::error::Result;
use crate::keys::{self, RepositoryKeys};
use crate::repository::{open_repository, Repository};
use crate::secret::Passphrase;

impl Repository {
    /// Opens the repository and decrypts its master key with `passphrase`.
    ///
    /// Fails with [`Error::WrongPassphrase`](crate::Error::WrongPassphrase) if the passphrase is
    /// wrong or the key file was modified; the two cannot be told apart. Key derivation is
    /// deliberately slow and runs on a blocking thread.
    pub async fn unlock(&self, passphrase: &Passphrase) -> Result<RepositoryKeys> {
        let store = self.store();
        let passphrase = passphrase.clone();
        blocking(move || {
            let config = open_repository(&store)?;
            keys::unlock(&store, &config.repository_id, &passphrase)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::crypto::KeyPurpose;
    use crate::error::Error;
    use crate::testing::{init, passphrase};

    #[tokio::test]
    async fn unlock_accepts_the_right_passphrase_and_rejects_others() {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let repo = Repository::at(dir.path());
        init(&repo).await.expect("init");

        repo.unlock(&passphrase()).await.expect("unlocks");
        let wrong = Passphrase::new(b"not the passphrase".to_vec()).expect("valid");
        let err = repo.unlock(&wrong).await.expect_err("must fail");
        assert!(matches!(err, Error::WrongPassphrase), "{err}");
    }

    #[tokio::test]
    async fn keys_are_stable_across_unlocks_and_repository_handles() {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let repo = Repository::at(dir.path());
        init(&repo).await.expect("init");

        let purpose = KeyPurpose::MetadataEncryption;
        let first = repo.unlock(&passphrase()).await.expect("unlock");
        let sealed = first.seal(purpose, b"snap", b"metadata").expect("seal");
        drop(first);

        let fresh = Repository::at(dir.path());
        let second = fresh.unlock(&passphrase()).await.expect("unlock");
        let opened = second.open(purpose, b"snap", &sealed).expect("open");
        assert_eq!(opened.as_slice(), b"metadata");
    }

    #[tokio::test]
    async fn unlock_requires_a_repository() {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let repo = Repository::at(dir.path().join("missing"));
        let err = repo.unlock(&passphrase()).await.expect_err("must fail");
        assert!(matches!(err, Error::NotARepository { .. }), "{err}");
    }

    #[tokio::test]
    async fn tampering_with_the_key_file_is_detected() {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let repo = Repository::at(dir.path());
        init(&repo).await.expect("init");
        let store = repo.store();
        let name = format!("keys/{}", store.list("keys").expect("list")[0]);
        let mut bytes = store.read(&name, 4096).expect("read");
        let position = bytes.len() - 5;
        bytes[position] = if bytes[position] == b'a' { b'b' } else { b'a' };
        store.write_replace(&name, &bytes).expect("write");

        let err = repo.unlock(&passphrase()).await.expect_err("must fail");
        assert!(matches!(err, Error::WrongPassphrase), "{err}");
    }

    #[tokio::test]
    async fn a_newer_key_file_version_is_reported() {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let repo = Repository::at(dir.path());
        init(&repo).await.expect("init");
        let name = repo.store().list("keys").expect("list").remove(0);
        let path = repo.layout().keys_dir().join(name);
        let text = fs::read_to_string(&path).expect("read");
        fs::write(&path, text.replace("key_version=1", "key_version=2")).expect("write");

        let err = repo.unlock(&passphrase()).await.expect_err("must fail");
        assert!(matches!(err, Error::UnsupportedKeyVersion { found: 2, .. }), "{err}");
    }

    #[tokio::test]
    async fn the_passphrase_never_reaches_the_repository_files() {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let repo = Repository::at(dir.path());
        init(&repo).await.expect("init");
        let store = repo.store();
        for dir_name in ["", "keys"] {
            for name in store.list(dir_name).expect("list") {
                let path = if dir_name.is_empty() {
                    name
                } else {
                    format!("{dir_name}/{name}")
                };
                if let Ok(bytes) = store.read(&path, 4096) {
                    let text = String::from_utf8_lossy(&bytes);
                    assert!(!text.contains("correct horse"), "{path}");
                }
            }
        }
    }
}
