//! Repository handle and on-disk layout (specified in `docs/FORMAT.md`).

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::blocking::blocking;
use crate::chunk_store::ChunkStore;
use crate::config::RepositoryConfig;
use crate::error::{Error, Result};
use crate::keys::RepositoryKeys;
use crate::lock::{LockKind, RepositoryLock};
use crate::snapshot::{SnapshotId, SnapshotStore};
use crate::store::LocalStore;

/// Repository format version written to the `config` file.
pub const FORMAT_VERSION: u32 = 1;

/// Directories `init` creates and every later operation requires.
pub(crate) const REQUIRED_DIRS: [&str; 4] = [
    RepositoryLayout::DATA_DIR,
    RepositoryLayout::SNAPSHOTS_DIR,
    RepositoryLayout::LOCKS_DIR,
    RepositoryLayout::KEYS_DIR,
];

/// Paths of the well-known entries inside a repository root. Pure path arithmetic, no I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryLayout {
    root: PathBuf,
}

impl RepositoryLayout {
    /// Repository configuration file name.
    pub const CONFIG_FILE: &'static str = "config";
    /// Directory holding key files: the passphrase-wrapped master key.
    pub const KEYS_DIR: &'static str = "keys";
    /// Directory holding snapshot metadata objects.
    pub const SNAPSHOTS_DIR: &'static str = "snapshots";
    /// Directory holding index objects. Created by the stage that introduces the index.
    pub const INDEX_DIR: &'static str = "index";
    /// Directory holding object (pack) storage.
    pub const DATA_DIR: &'static str = "data";
    /// Directory holding advisory lock files.
    pub const LOCKS_DIR: &'static str = "locks";

    /// Creates a layout rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The repository root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path of the `config` file.
    pub fn config_file(&self) -> PathBuf {
        self.root.join(Self::CONFIG_FILE)
    }

    /// Path of the `keys` directory.
    pub fn keys_dir(&self) -> PathBuf {
        self.root.join(Self::KEYS_DIR)
    }

    /// Path of the `snapshots` directory.
    pub fn snapshots_dir(&self) -> PathBuf {
        self.root.join(Self::SNAPSHOTS_DIR)
    }

    /// Path of the `index` directory.
    pub fn index_dir(&self) -> PathBuf {
        self.root.join(Self::INDEX_DIR)
    }

    /// Path of the `data` directory.
    pub fn data_dir(&self) -> PathBuf {
        self.root.join(Self::DATA_DIR)
    }

    /// Path of the `locks` directory.
    pub fn locks_dir(&self) -> PathBuf {
        self.root.join(Self::LOCKS_DIR)
    }

    /// Path of the object file for `id`.
    pub fn snapshot_file(&self, id: &SnapshotId) -> PathBuf {
        self.snapshots_dir().join(id.as_str())
    }
}

/// Handle to a repository location. Creating a handle performs no I/O.
#[derive(Debug, Clone)]
pub struct Repository {
    layout: RepositoryLayout,
}

impl Repository {
    /// Refers to the repository rooted at `root`.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self {
            layout: RepositoryLayout::new(root),
        }
    }

    /// The repository layout.
    pub fn layout(&self) -> &RepositoryLayout {
        &self.layout
    }

    /// The local store backing this repository.
    pub fn store(&self) -> LocalStore {
        LocalStore::new(self.layout.root())
    }

    /// The encrypted chunk store of this repository, using keys derived from `keys` (obtained
    /// with [`Repository::unlock`]). Performs no I/O; the repository should have been opened.
    pub fn chunk_store(&self, keys: &RepositoryKeys) -> Result<ChunkStore> {
        ChunkStore::new(self.store(), keys)
    }

    /// The snapshot store of this repository, using keys derived from `keys`. Reads the
    /// repository configuration to learn the repository id, which every snapshot is bound to,
    /// and fails if the repository cannot be opened.
    pub async fn snapshot_store(&self, keys: &RepositoryKeys) -> Result<SnapshotStore> {
        let config = self.load_config().await?;
        SnapshotStore::new(self.store(), keys, &config.repository_id)
    }

    /// Opens the repository: checks that it exists, that its format version is supported, and
    /// that its required directories are present. Returns the parsed configuration.
    pub async fn load_config(&self) -> Result<RepositoryConfig> {
        let store = self.store();
        blocking(move || open_repository(&store)).await
    }

    /// Opens the repository (see [`Repository::load_config`]) and takes a lock on it. The lock
    /// is released when the returned guard is dropped.
    pub async fn lock(&self, kind: LockKind) -> Result<RepositoryLock> {
        let store = self.store();
        blocking(move || {
            open_repository(&store)?;
            RepositoryLock::acquire(&store, kind)
        })
        .await
    }
}

/// Checks that the location is a directory with a valid `config` and returns it. Nothing else
/// about the repository is required, which is what `check` needs to describe a repository whose
/// other parts are damaged.
pub(crate) fn read_config(store: &LocalStore) -> Result<RepositoryConfig> {
    let root = store.root();
    let not_a_repository = || Error::NotARepository {
        path: root.to_path_buf(),
    };
    match fs::metadata(root) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Err(not_a_repository()),
        Err(err) if err.kind() == ErrorKind::NotFound => return Err(not_a_repository()),
        Err(err) => return Err(Error::io("inspect", root, err)),
    }
    RepositoryConfig::load(store).map_err(|err| {
        if err.is_not_found() {
            not_a_repository()
        } else {
            err
        }
    })
}

pub(crate) fn open_repository(store: &LocalStore) -> Result<RepositoryConfig> {
    let config = read_config(store)?;
    for dir in REQUIRED_DIRS {
        if !store.resolve(dir)?.is_dir() {
            return Err(Error::Corrupt(format!("missing directory `{dir}`")));
        }
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::init;

    #[test]
    fn layout_matches_the_documented_format() {
        let layout = RepositoryLayout::new("/repo");
        assert_eq!(layout.root(), Path::new("/repo"));
        assert_eq!(layout.config_file(), Path::new("/repo/config"));
        assert_eq!(layout.keys_dir(), Path::new("/repo/keys"));
        assert_eq!(layout.snapshots_dir(), Path::new("/repo/snapshots"));
        assert_eq!(layout.index_dir(), Path::new("/repo/index"));
        assert_eq!(layout.data_dir(), Path::new("/repo/data"));
        assert_eq!(layout.locks_dir(), Path::new("/repo/locks"));
    }

    #[test]
    fn snapshot_file_stays_inside_the_snapshots_dir() {
        let layout = RepositoryLayout::new("/repo");
        let id: SnapshotId = "ab".repeat(32).parse().expect("valid id");
        let path = layout.snapshot_file(&id);
        assert_eq!(path.parent(), Some(layout.snapshots_dir().as_path()));
    }

    #[test]
    fn creating_a_handle_does_not_touch_the_filesystem() {
        let repo = Repository::at("/definitely/not/a/real/path");
        assert!(!repo.layout().root().exists());
    }

    #[test]
    fn format_version_is_one() {
        assert_eq!(FORMAT_VERSION, 1);
    }

    #[test]
    fn required_dirs_are_data_snapshots_locks_and_keys() {
        assert_eq!(REQUIRED_DIRS, ["data", "snapshots", "locks", "keys"]);
    }

    #[tokio::test]
    async fn load_config_on_a_missing_path_is_not_a_repository() {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let repo = Repository::at(dir.path().join("nope"));
        let err = repo.load_config().await.expect_err("must fail");
        assert!(matches!(err, Error::NotARepository { .. }), "{err}");
    }

    #[tokio::test]
    async fn load_config_on_an_unrelated_directory_is_not_a_repository() {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let repo = Repository::at(dir.path());
        let err = repo.load_config().await.expect_err("must fail");
        assert!(matches!(err, Error::NotARepository { .. }), "{err}");
    }

    #[tokio::test]
    async fn load_config_on_a_file_is_not_a_repository() {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let file = dir.path().join("file");
        fs::write(&file, b"x").expect("write file");
        let repo = Repository::at(&file);
        let err = repo.load_config().await.expect_err("must fail");
        assert!(matches!(err, Error::NotARepository { .. }), "{err}");
    }

    #[tokio::test]
    async fn open_detects_missing_directories_and_newer_versions() {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let repo = Repository::at(dir.path());
        init(&repo).await.expect("init");
        repo.load_config().await.expect("healthy repository opens");

        fs::remove_dir(repo.layout().locks_dir()).expect("remove locks");
        let err = repo.load_config().await.expect_err("must fail");
        assert!(matches!(err, Error::Corrupt(_)), "{err}");
        fs::create_dir(repo.layout().locks_dir()).expect("restore locks");

        let config = fs::read_to_string(repo.layout().config_file()).expect("read config");
        fs::write(
            repo.layout().config_file(),
            config.replace("format_version=1", "format_version=7"),
        )
        .expect("rewrite config");
        let err = repo.load_config().await.expect_err("must fail");
        assert!(matches!(err, Error::UnsupportedVersion { found: 7, .. }), "{err}");
        let err = repo.lock(LockKind::Shared).await.expect_err("must fail");
        assert!(matches!(err, Error::UnsupportedVersion { found: 7, .. }), "{err}");
    }

    #[tokio::test]
    async fn lock_requires_an_initialized_repository_and_conflicts_are_reported() {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let repo = Repository::at(dir.path());
        let err = repo.lock(LockKind::Shared).await.expect_err("must fail");
        assert!(matches!(err, Error::NotARepository { .. }), "{err}");

        init(&repo).await.expect("init");
        let exclusive = repo.lock(LockKind::Exclusive).await.expect("exclusive");
        let err = repo.lock(LockKind::Shared).await.expect_err("must fail");
        assert!(matches!(err, Error::Locked { .. }), "{err}");
        drop(exclusive);
        repo.lock(LockKind::Shared).await.expect("free again");
    }
}
