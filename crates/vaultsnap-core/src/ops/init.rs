use crate::blocking::blocking;
use crate::config::RepositoryConfig;
use crate::crypto::KdfParams;
use crate::error::{Error, Result};
use crate::keys::{write_key_slot, RepositoryKeys};
use crate::repository::{Repository, RepositoryLayout, REQUIRED_DIRS};
use crate::secret::Passphrase;
use crate::storage::LocationState;
use crate::store::Store;
use crate::util::{random_hex, unix_now};

impl Repository {
    /// Creates a new repository at the repository root and returns its configuration.
    ///
    /// A random master key is generated and stored in `keys/`, encrypted under a key derived
    /// from `passphrase` with Argon2id (default cost parameters; see [`KdfParams`]). The master
    /// key itself is never written unencrypted and is zeroed before this call returns.
    ///
    /// Safe to repeat: an existing repository is never modified. If one is found the call fails
    /// with [`Error::AlreadyInitialized`], or with [`Error::UnsupportedVersion`] when it was
    /// written by an incompatible format version. A directory holding anything other than an
    /// interrupted `init` is rejected with [`Error::InvalidPath`]. The `config` file is written
    /// last, so an interrupted `init` leaves no half-valid repository and can simply be re-run.
    pub async fn init(&self, passphrase: &Passphrase) -> Result<RepositoryConfig> {
        self.init_with_kdf(passphrase, KdfParams::default()).await
    }

    /// Like [`Repository::init`] with explicit key derivation cost parameters.
    pub async fn init_with_kdf(
        &self,
        passphrase: &Passphrase,
        kdf: KdfParams,
    ) -> Result<RepositoryConfig> {
        kdf.validate()?;
        let store = self.store();
        let passphrase = passphrase.clone();
        blocking(move || init_repository(&store, &passphrase, kdf)).await
    }
}

fn init_repository(
    store: &Store,
    passphrase: &Passphrase,
    kdf: KdfParams,
) -> Result<RepositoryConfig> {
    if store.root().as_os_str().is_empty() {
        return Err(Error::InvalidPath("the path is empty".to_owned()));
    }
    store.capabilities().require_repository_support()?;
    match store.probe()? {
        LocationState::Available => check_existing(store)?,
        LocationState::Unusable(reason) => return Err(Error::InvalidPath(reason)),
        LocationState::Absent => store.create_root()?,
    }

    for dir in REQUIRED_DIRS {
        store.create_dir(dir)?;
    }
    let config = RepositoryConfig::new(random_hex(16)?, unix_now());
    let keys = RepositoryKeys::generate()?;
    let key_file = write_key_slot(store, &config.repository_id, &keys, passphrase, kdf)?;
    match store.write_new(RepositoryLayout::CONFIG_FILE, &config.to_bytes()) {
        Ok(()) => Ok(config),
        Err(err) => {
            // After a failure that may have been a lost answer (a timeout) the config may exist
            // after all, and then this key file is the one that opens it. An unused key file is
            // harmless: unlocking skips it, and so does the next `init`.
            if !err.is_transient() {
                if let Err(cleanup) = store.remove(&key_file) {
                    tracing::warn!(
                        file = %key_file,
                        error = %cleanup,
                        "failed to remove unused key file"
                    );
                }
            }
            match err {
                Error::AlreadyExists { .. } => Err(already_initialized(store)),
                other => Err(other),
            }
        }
    }
}

/// Ok if `store.root()` may be initialized: it is empty or holds only the leftovers of an
/// interrupted `init`.
fn check_existing(store: &Store) -> Result<()> {
    match RepositoryConfig::load(store) {
        Ok(_) => return Err(already_initialized(store)),
        Err(err) if err.is_not_found() => {}
        Err(err) => return Err(err),
    }
    if let Some(foreign) = store.list("")?.iter().find(|name| !is_leftover(name)) {
        let message = format!(
            "{} is not empty and is not a VaultSnap repository (found `{foreign}`)",
            store.root().display()
        );
        return Err(Error::InvalidPath(message));
    }
    Ok(())
}

fn is_leftover(name: &str) -> bool {
    REQUIRED_DIRS.contains(&name) || Store::is_temp_name(name)
}

fn already_initialized(store: &Store) -> Error {
    Error::AlreadyInitialized {
        path: store.root().to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::Arc;

    use tempfile::TempDir;

    use super::*;
    use crate::storage::{Fault, FlakyBackend, MemoryBackend};
    use crate::testing::{fast_kdf, init, passphrase};

    fn temp() -> TempDir {
        tempfile::tempdir().expect("create temporary directory")
    }

    fn tree_state(root: &Path) -> Vec<String> {
        let mut entries = Vec::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            for entry in fs::read_dir(&dir).expect("read_dir") {
                let path = entry.expect("entry").path();
                let relative = path.strip_prefix(root).expect("inside root");
                if path.is_dir() {
                    pending.push(path.clone());
                    entries.push(format!("{}/", relative.display()));
                } else {
                    let content = fs::read(&path).expect("read");
                    entries.push(format!("{}={content:?}", relative.display()));
                }
            }
        }
        entries.sort();
        entries
    }

    #[tokio::test]
    async fn creates_a_complete_repository() {
        let dir = temp();
        let repo = Repository::at(dir.path().join("repo"));
        let config = init(&repo).await.expect("init");

        let layout = repo.layout();
        assert!(layout.data_dir().is_dir());
        assert!(layout.snapshots_dir().is_dir());
        assert!(layout.locks_dir().is_dir());
        assert!(layout.keys_dir().is_dir());
        assert!(layout.config_file().is_file());
        assert_eq!(config.format_version, 1);
        assert_eq!(config.repository_id.len(), 32);
        assert_eq!(repo.load_config().await.expect("loads"), config);
        let names = repo.store().list("").expect("list");
        assert!(names.iter().all(|name| !name.starts_with('.')), "{names:?}");
    }

    #[tokio::test]
    async fn creates_exactly_one_key_file_that_unlocks() {
        let dir = temp();
        let repo = Repository::at(dir.path());
        init(&repo).await.expect("init");
        assert_eq!(repo.store().list("keys").expect("list").len(), 1);
        repo.unlock(&passphrase()).await.expect("unlocks");
    }

    #[tokio::test]
    async fn creates_missing_parent_directories() {
        let dir = temp();
        let repo = Repository::at(dir.path().join("a").join("b").join("repo"));
        init(&repo).await.expect("init");
        repo.load_config().await.expect("loads");
    }

    #[tokio::test]
    async fn initializes_an_existing_empty_directory() {
        let dir = temp();
        init(&Repository::at(dir.path())).await.expect("init");
    }

    #[tokio::test]
    async fn each_repository_gets_a_unique_id_and_master_key() {
        let dir = temp();
        let repo_a = Repository::at(dir.path().join("a"));
        let repo_b = Repository::at(dir.path().join("b"));
        let a = init(&repo_a).await.expect("init a");
        let b = init(&repo_b).await.expect("init b");
        assert_ne!(a.repository_id, b.repository_id);

        let keys_a = repo_a.unlock(&passphrase()).await.expect("unlock a");
        let keys_b = repo_b.unlock(&passphrase()).await.expect("unlock b");
        let purpose = crate::crypto::KeyPurpose::DataEncryption;
        let derived_a = keys_a.derive(purpose).expect("derive");
        let derived_b = keys_b.derive(purpose).expect("derive");
        assert_ne!(*derived_a, *derived_b);
    }

    #[tokio::test]
    async fn invalid_kdf_parameters_are_rejected_before_touching_the_disk() {
        let dir = temp();
        let repo = Repository::at(dir.path().join("repo"));
        let kdf = KdfParams {
            t_cost: 0,
            ..fast_kdf()
        };
        let err = repo
            .init_with_kdf(&passphrase(), kdf)
            .await
            .expect_err("must fail");
        assert!(matches!(err, Error::InvalidKdfParams(_)), "{err}");
        assert!(!dir.path().join("repo").exists());
    }

    #[tokio::test]
    async fn repeated_init_fails_and_changes_nothing() {
        let dir = temp();
        let repo = Repository::at(dir.path().join("repo"));
        let first = init(&repo).await.expect("init");
        let before = tree_state(dir.path());

        for _ in 0..3 {
            let err = init(&repo).await.expect_err("must fail");
            assert!(matches!(err, Error::AlreadyInitialized { .. }), "{err}");
        }
        assert_eq!(tree_state(dir.path()), before);
        assert_eq!(repo.load_config().await.expect("loads"), first);
    }

    #[tokio::test]
    async fn a_second_init_with_another_passphrase_does_not_replace_the_keys() {
        let dir = temp();
        let repo = Repository::at(dir.path());
        init(&repo).await.expect("init");
        let other = Passphrase::new(b"a different passphrase".to_vec()).expect("valid");
        let err = repo
            .init_with_kdf(&other, fast_kdf())
            .await
            .expect_err("must fail");
        assert!(matches!(err, Error::AlreadyInitialized { .. }), "{err}");
        repo.unlock(&passphrase())
            .await
            .expect("original passphrase still works");
        assert!(repo.unlock(&other).await.is_err());
    }

    #[tokio::test]
    async fn init_never_overwrites_data_in_an_existing_repository() {
        let dir = temp();
        let repo = Repository::at(dir.path());
        init(&repo).await.expect("init");
        fs::write(repo.layout().data_dir().join("object"), b"precious").expect("write object");
        let _ = init(&repo).await;
        let kept = fs::read(repo.layout().data_dir().join("object")).expect("read object");
        assert_eq!(kept, b"precious");
    }

    #[tokio::test]
    async fn incompatible_versions_are_reported_and_left_untouched() {
        for version in ["0", "2", "99"] {
            let dir = temp();
            let repo = Repository::at(dir.path());
            init(&repo).await.expect("init");
            let path = repo.layout().config_file();
            let text = fs::read_to_string(&path).expect("read config");
            let modified = text.replace("format_version=1", &format!("format_version={version}"));
            fs::write(&path, &modified).expect("write config");
            let before = tree_state(dir.path());

            let err = init(&repo).await.expect_err("must fail");
            assert!(
                matches!(err, Error::UnsupportedVersion { .. }),
                "{version}: {err}"
            );
            assert_eq!(tree_state(dir.path()), before);
            assert_eq!(fs::read_to_string(&path).expect("read config"), modified);
        }
    }

    #[tokio::test]
    async fn a_corrupt_config_is_never_replaced() {
        let dir = temp();
        fs::write(dir.path().join("config"), b"garbage").expect("write config");
        let repo = Repository::at(dir.path());
        let err = init(&repo).await.expect_err("must fail");
        assert!(matches!(err, Error::InvalidConfig(_)), "{err}");
        assert_eq!(
            fs::read(dir.path().join("config")).expect("read"),
            b"garbage"
        );
        assert!(!dir.path().join("data").exists());
        assert!(!dir.path().join("keys").exists());
    }

    #[tokio::test]
    async fn an_empty_path_is_rejected() {
        let err = init(&Repository::at("")).await.expect_err("must fail");
        assert!(matches!(err, Error::InvalidPath(_)), "{err}");
    }

    #[tokio::test]
    async fn a_file_in_place_of_the_repository_is_rejected() {
        let dir = temp();
        let file = dir.path().join("repo");
        fs::write(&file, b"not a directory").expect("write file");
        let err = init(&Repository::at(&file)).await.expect_err("must fail");
        assert!(matches!(err, Error::InvalidPath(_)), "{err}");
        assert_eq!(fs::read(&file).expect("read"), b"not a directory");
    }

    #[tokio::test]
    async fn a_path_below_a_file_is_rejected() {
        let dir = temp();
        let file = dir.path().join("file");
        fs::write(&file, b"x").expect("write file");
        let repo = Repository::at(file.join("repo"));
        assert!(init(&repo).await.is_err());
        assert_eq!(fs::read(&file).expect("read"), b"x");
    }

    #[tokio::test]
    async fn a_non_empty_unrelated_directory_is_rejected_untouched() {
        let dir = temp();
        fs::write(dir.path().join("photos.jpg"), b"jpeg").expect("write file");
        let before = tree_state(dir.path());
        let err = init(&Repository::at(dir.path()))
            .await
            .expect_err("must fail");
        assert!(matches!(err, Error::InvalidPath(_)), "{err}");
        assert!(err.to_string().contains("photos.jpg"), "{err}");
        assert_eq!(tree_state(dir.path()), before);
    }

    #[tokio::test]
    async fn an_interrupted_init_can_be_resumed_and_ignores_the_stray_key_file() {
        let dir = temp();
        let repo = Repository::at(dir.path());
        fs::create_dir(dir.path().join("data")).expect("mkdir");
        fs::write(dir.path().join(".tmp-0123456789abcdef"), b"half").expect("write temp");

        // An earlier attempt got as far as writing a key file for a repository id that never
        // received a config.
        let store = repo.store();
        store.create_dir("keys").expect("mkdir");
        let stray = RepositoryKeys::generate().expect("generate");
        let stray_id = "fedcba9876543210fedcba9876543210";
        write_key_slot(&store, stray_id, &stray, &passphrase(), fast_kdf()).expect("stray");

        init(&repo).await.expect("init resumes");
        repo.load_config().await.expect("loads");
        let keys = repo.unlock(&passphrase()).await.expect("unlocks");
        let purpose = crate::crypto::KeyPurpose::ContentId;
        let stray_key = stray.derive(purpose).expect("derive");
        assert_ne!(*keys.derive(purpose).expect("derive"), *stray_key);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn repository_files_and_directories_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp();
        let repo = Repository::at(dir.path().join("repo"));
        init(&repo).await.expect("init");
        let layout = repo.layout();
        let mode = |path: &Path| {
            let metadata = fs::metadata(path).expect("metadata");
            metadata.permissions().mode() & 0o777
        };
        assert_eq!(mode(layout.root()), 0o700);
        assert_eq!(mode(&layout.data_dir()), 0o700);
        assert_eq!(mode(&layout.snapshots_dir()), 0o700);
        assert_eq!(mode(&layout.locks_dir()), 0o700);
        assert_eq!(mode(&layout.keys_dir()), 0o700);
        assert_eq!(mode(&layout.config_file()), 0o600);
        let store = repo.store();
        for name in store.list("keys").expect("list") {
            assert_eq!(mode(&layout.keys_dir().join(name)), 0o600);
        }
    }

    #[tokio::test]
    async fn a_repository_can_be_created_in_any_backend() {
        let repo = Repository::with_backend(Arc::new(MemoryBackend::new()));
        let config = init(&repo).await.expect("init");
        assert_eq!(repo.load_config().await.expect("loads"), config);
        let names = repo.store().list("").expect("list");
        assert_eq!(
            names,
            ["config", "keys"],
            "object stores keep objects, not empty directories"
        );
        repo.unlock(&passphrase()).await.expect("unlocks");
        let err = init(&repo).await.expect_err("must fail");
        assert!(matches!(err, Error::AlreadyInitialized { .. }), "{err}");
    }

    #[tokio::test]
    async fn a_lost_answer_for_the_config_does_not_cost_the_repository_its_key() {
        let memory = Arc::new(MemoryBackend::new());
        // The first put is the key file, the second the config, whose answer is lost.
        let flaky =
            FlakyBackend::scripted_for(memory.clone(), &["put"], [Fault::After]).skipping(1);
        let repo = Repository::with_backend(Arc::new(flaky));

        let err = init(&repo).await.expect_err("the answer was lost");
        assert!(err.is_transient(), "{err}");
        assert!(
            memory.object_names().contains(&"config".to_owned()),
            "but the config was written"
        );
        repo.load_config().await.expect("the repository opens");
        repo.unlock(&passphrase())
            .await
            .expect("and its key file is still there");
    }

    #[tokio::test]
    async fn a_config_that_was_never_written_leaves_a_key_file_that_the_next_init_ignores() {
        let memory = Arc::new(MemoryBackend::new());
        let flaky =
            FlakyBackend::scripted_for(memory.clone(), &["put"], [Fault::Before]).skipping(1);
        let repo = Repository::with_backend(Arc::new(flaky));

        let err = init(&repo).await.expect_err("the request was lost");
        assert!(err.is_transient(), "{err}");
        assert!(!memory.object_names().contains(&"config".to_owned()));

        init(&repo).await.expect("init can simply be run again");
        repo.unlock(&passphrase()).await.expect("unlocks");
    }

    #[test]
    fn concurrent_init_has_exactly_one_winner_and_a_usable_repository() {
        let dir = temp();
        let store = Store::local(dir.path().join("repo"));
        let pass = passphrase();
        let results: Vec<Result<RepositoryConfig>> = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for _ in 0..8 {
                handles.push(scope.spawn(|| init_repository(&store, &pass, fast_kdf())));
            }
            handles
                .into_iter()
                .map(|handle| handle.join().expect("thread panicked"))
                .collect()
        });

        let winners = results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(winners, 1, "{results:?}");
        for result in results.iter().filter_map(|r| r.as_ref().err()) {
            assert!(
                matches!(result, Error::AlreadyInitialized { .. }),
                "{result}"
            );
        }
        let winner = results
            .iter()
            .find_map(|r| r.as_ref().ok())
            .expect("a winner");
        assert_eq!(&RepositoryConfig::load(&store).expect("loads"), winner);
        crate::keys::unlock(&store, &winner.repository_id, &pass).expect("winner's keys unlock");
        assert_eq!(
            store.list("keys").expect("list").len(),
            1,
            "losers remove their key files"
        );
    }
}
