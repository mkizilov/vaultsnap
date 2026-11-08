//! Repository handle and on-disk layout (specified in `docs/FORMAT.md`).

use std::path::{Path, PathBuf};

use crate::snapshot::SnapshotId;

/// Repository format version written to the `config` file.
pub const FORMAT_VERSION: u32 = 1;

/// Paths of the well-known entries inside a repository root. Pure path arithmetic, no I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryLayout {
    root: PathBuf,
}

impl RepositoryLayout {
    /// Repository configuration file name.
    pub const CONFIG_FILE: &'static str = "config";
    /// Directory holding key slots.
    pub const KEYS_DIR: &'static str = "keys";
    /// Directory holding snapshot objects.
    pub const SNAPSHOTS_DIR: &'static str = "snapshots";
    /// Directory holding index objects.
    pub const INDEX_DIR: &'static str = "index";
    /// Directory holding pack files.
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
