use std::fs;
use std::io::ErrorKind;

use crate::blocking::blocking;
use crate::config::RepositoryConfig;
use crate::error::{Error, Result};
use crate::repository::{Repository, RepositoryLayout, REQUIRED_DIRS};
use crate::store::LocalStore;
use crate::util::{random_hex, unix_now};

impl Repository {
    /// Creates a new, empty repository at the repository root and returns its configuration.
    ///
    /// Safe to repeat: an existing repository is never modified. If one is found the call fails
    /// with [`Error::AlreadyInitialized`], or with [`Error::UnsupportedVersion`] when it was
    /// written by an incompatible format version. A directory holding anything other than an
    /// interrupted `init` is rejected with [`Error::InvalidPath`]. The `config` file is written
    /// last, so an interrupted `init` leaves no half-valid repository and can simply be re-run.
    pub async fn init(&self) -> Result<RepositoryConfig> {
        let store = self.store();
        blocking(move || init_repository(&store)).await
    }
}

fn init_repository(store: &LocalStore) -> Result<RepositoryConfig> {
    let root = store.root();
    if root.as_os_str().is_empty() {
        return Err(Error::InvalidPath("the path is empty".to_owned()));
    }
    match fs::metadata(root) {
        Ok(meta) if meta.is_dir() => check_existing(store)?,
        Ok(_) => {
            let message = format!("{} exists and is not a directory", root.display());
            return Err(Error::InvalidPath(message));
        }
        Err(err) if err.kind() == ErrorKind::NotFound => store.create_root()?,
        Err(err) => return Err(Error::io("inspect", root, err)),
    }

    for dir in REQUIRED_DIRS {
        store.create_dir(dir)?;
    }
    let config = RepositoryConfig::new(random_hex(16)?, unix_now());
    match store.write_new(RepositoryLayout::CONFIG_FILE, &config.to_bytes()) {
        Ok(()) => Ok(config),
        Err(Error::AlreadyExists { .. }) => Err(already_initialized(store)),
        Err(err) => Err(err),
    }
}

/// Ok if `store.root()` may be initialized: it is empty or holds only the leftovers of an
/// interrupted `init`.
fn check_existing(store: &LocalStore) -> Result<()> {
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
    REQUIRED_DIRS.contains(&name) || LocalStore::is_temp_name(name)
}

fn already_initialized(store: &LocalStore) -> Error {
    Error::AlreadyInitialized {
        path: store.root().to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tempfile::TempDir;

    use super::*;

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
        let config = repo.init().await.expect("init");

        let layout = repo.layout();
        assert!(layout.data_dir().is_dir());
        assert!(layout.snapshots_dir().is_dir());
        assert!(layout.locks_dir().is_dir());
        assert!(layout.config_file().is_file());
        assert_eq!(config.format_version, 1);
        assert_eq!(config.repository_id.len(), 32);
        assert_eq!(repo.load_config().await.expect("loads"), config);
        assert!(repo.store().list("").expect("list").iter().all(|n| !n.starts_with('.')));
    }

    #[tokio::test]
    async fn creates_missing_parent_directories() {
        let dir = temp();
        let repo = Repository::at(dir.path().join("a").join("b").join("repo"));
        repo.init().await.expect("init");
        repo.load_config().await.expect("loads");
    }

    #[tokio::test]
    async fn initializes_an_existing_empty_directory() {
        let dir = temp();
        Repository::at(dir.path()).init().await.expect("init");
    }

    #[tokio::test]
    async fn each_repository_gets_a_unique_id() {
        let dir = temp();
        let repo_a = Repository::at(dir.path().join("a"));
        let repo_b = Repository::at(dir.path().join("b"));
        let a = repo_a.init().await.expect("init a");
        let b = repo_b.init().await.expect("init b");
        assert_ne!(a.repository_id, b.repository_id);
    }

    #[tokio::test]
    async fn repeated_init_fails_and_changes_nothing() {
        let dir = temp();
        let repo = Repository::at(dir.path().join("repo"));
        let first = repo.init().await.expect("init");
        let before = tree_state(dir.path());

        for _ in 0..3 {
            let err = repo.init().await.expect_err("must fail");
            assert!(matches!(err, Error::AlreadyInitialized { .. }), "{err}");
        }
        assert_eq!(tree_state(dir.path()), before);
        assert_eq!(repo.load_config().await.expect("loads"), first);
    }

    #[tokio::test]
    async fn init_never_overwrites_data_in_an_existing_repository() {
        let dir = temp();
        let repo = Repository::at(dir.path());
        repo.init().await.expect("init");
        fs::write(repo.layout().data_dir().join("object"), b"precious").expect("write object");
        let _ = repo.init().await;
        let kept = fs::read(repo.layout().data_dir().join("object")).expect("read object");
        assert_eq!(kept, b"precious");
    }

    #[tokio::test]
    async fn incompatible_versions_are_reported_and_left_untouched() {
        for version in ["0", "2", "99"] {
            let dir = temp();
            let repo = Repository::at(dir.path());
            repo.init().await.expect("init");
            let path = repo.layout().config_file();
            let text = fs::read_to_string(&path).expect("read config");
            let modified = text.replace("format_version=1", &format!("format_version={version}"));
            fs::write(&path, &modified).expect("write config");
            let before = tree_state(dir.path());

            let err = repo.init().await.expect_err("must fail");
            assert!(matches!(err, Error::UnsupportedVersion { .. }), "{version}: {err}");
            assert_eq!(tree_state(dir.path()), before);
            assert_eq!(fs::read_to_string(&path).expect("read config"), modified);
        }
    }

    #[tokio::test]
    async fn a_corrupt_config_is_never_replaced() {
        let dir = temp();
        fs::write(dir.path().join("config"), b"garbage").expect("write config");
        let repo = Repository::at(dir.path());
        let err = repo.init().await.expect_err("must fail");
        assert!(matches!(err, Error::InvalidConfig(_)), "{err}");
        assert_eq!(fs::read(dir.path().join("config")).expect("read"), b"garbage");
        assert!(!dir.path().join("data").exists());
    }

    #[tokio::test]
    async fn an_empty_path_is_rejected() {
        let err = Repository::at("").init().await.expect_err("must fail");
        assert!(matches!(err, Error::InvalidPath(_)), "{err}");
    }

    #[tokio::test]
    async fn a_file_in_place_of_the_repository_is_rejected() {
        let dir = temp();
        let file = dir.path().join("repo");
        fs::write(&file, b"not a directory").expect("write file");
        let repo = Repository::at(&file);
        let err = repo.init().await.expect_err("must fail");
        assert!(matches!(err, Error::InvalidPath(_)), "{err}");
        assert_eq!(fs::read(&file).expect("read"), b"not a directory");
    }

    #[tokio::test]
    async fn a_path_below_a_file_is_rejected() {
        let dir = temp();
        let file = dir.path().join("file");
        fs::write(&file, b"x").expect("write file");
        let repo = Repository::at(file.join("repo"));
        assert!(repo.init().await.is_err());
        assert_eq!(fs::read(&file).expect("read"), b"x");
    }

    #[tokio::test]
    async fn a_non_empty_unrelated_directory_is_rejected_untouched() {
        let dir = temp();
        fs::write(dir.path().join("photos.jpg"), b"jpeg").expect("write file");
        let before = tree_state(dir.path());
        let repo = Repository::at(dir.path());
        let err = repo.init().await.expect_err("must fail");
        assert!(matches!(err, Error::InvalidPath(_)), "{err}");
        assert!(err.to_string().contains("photos.jpg"), "{err}");
        assert_eq!(tree_state(dir.path()), before);
    }

    #[tokio::test]
    async fn an_interrupted_init_can_be_resumed() {
        let dir = temp();
        fs::create_dir(dir.path().join("data")).expect("mkdir");
        fs::write(dir.path().join(".tmp-0123456789abcdef"), b"half").expect("write temp");
        let repo = Repository::at(dir.path());
        repo.init().await.expect("init resumes");
        repo.load_config().await.expect("loads");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn repository_files_and_directories_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp();
        let repo = Repository::at(dir.path().join("repo"));
        repo.init().await.expect("init");
        let layout = repo.layout();
        let mode = |path: &Path| {
            let metadata = fs::metadata(path).expect("metadata");
            metadata.permissions().mode() & 0o777
        };
        assert_eq!(mode(layout.root()), 0o700);
        assert_eq!(mode(&layout.data_dir()), 0o700);
        assert_eq!(mode(&layout.snapshots_dir()), 0o700);
        assert_eq!(mode(&layout.locks_dir()), 0o700);
        assert_eq!(mode(&layout.config_file()), 0o600);
    }

    #[test]
    fn concurrent_init_has_exactly_one_winner() {
        let dir = temp();
        let store = LocalStore::new(dir.path().join("repo"));
        let results: Vec<Result<RepositoryConfig>> = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for _ in 0..8 {
                handles.push(scope.spawn(|| init_repository(&store)));
            }
            handles
                .into_iter()
                .map(|handle| handle.join().expect("thread panicked"))
                .collect()
        });

        let winners = results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(winners, 1, "{results:?}");
        for result in results.iter().filter_map(|r| r.as_ref().err()) {
            assert!(matches!(result, Error::AlreadyInitialized { .. }), "{result}");
        }
        let winner = results.iter().find_map(|r| r.as_ref().ok()).expect("a winner");
        assert_eq!(&RepositoryConfig::load(&store).expect("loads"), winner);
    }
}
