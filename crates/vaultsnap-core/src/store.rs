//! Local filesystem store: validated names, no symlink traversal, atomic writes, private
//! permissions (`0700` directories, `0600` files on Unix).
//!
//! Every write is staged in a temporary file next to its destination, flushed to disk, and then
//! published in a single filesystem step, so a reader or a crash sees either the old state or the
//! complete new file, never a partial one. Objects are addressed by repository-relative names such
//! as `config` or `locks/ab12.lock`; see `validate_name` for the accepted syntax.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::util::random_hex;

const MAX_NAME_LEN: usize = 255;
const TEMP_PREFIX: &str = ".tmp-";

/// What a directory entry is, looked at without following symbolic links.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirEntryKind {
    /// A regular file.
    File,
    /// A directory.
    Directory,
    /// A symbolic link (never followed).
    Symlink,
    /// Anything else: a FIFO, a socket, a device.
    Other,
}

/// One entry of a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntryInfo {
    /// The entry's name. A name that is not valid UTF-8 is converted lossily.
    pub name: String,
    /// What it is.
    pub kind: DirEntryKind,
    /// Size in bytes for a regular file, 0 for anything else.
    pub len: u64,
}

/// A directory on the local filesystem used as repository storage.
#[derive(Debug, Clone)]
pub struct LocalStore {
    root: PathBuf,
}

impl LocalStore {
    /// Refers to the store rooted at `root`. Performs no I/O.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The store root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// True for names of temporary files created while staging a write.
    pub fn is_temp_name(name: &str) -> bool {
        name.starts_with(TEMP_PREFIX)
    }

    /// Creates the root directory (and missing parents) if it does not exist yet. The root itself
    /// is created private; parents get default permissions.
    pub fn create_root(&self) -> Result<()> {
        let parent = self.root.parent().filter(|p| !p.as_os_str().is_empty());
        if let Some(parent) = parent {
            fs::create_dir_all(parent).map_err(|err| Error::io("create directory", parent, err))?;
        }
        create_private_dir(&self.root)
    }

    /// Maps a store name to a filesystem path, rejecting unsafe names and any symbolic link
    /// along the way. The path need not exist.
    pub fn resolve(&self, name: &str) -> Result<PathBuf> {
        let relative = validate_name(name)?;
        let mut current = self.root.clone();
        for component in relative.components() {
            current.push(component);
            match fs::symlink_metadata(&current) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(Error::Symlink { path: current });
                }
                Ok(_) => {}
                Err(err) if err.kind() == ErrorKind::NotFound => {}
                Err(err) => return Err(Error::io("inspect", &current, err)),
            }
        }
        Ok(current)
    }

    /// Whether `name` exists.
    pub fn exists(&self, name: &str) -> Result<bool> {
        let path = self.resolve(name)?;
        match fs::symlink_metadata(&path) {
            Ok(_) => Ok(true),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(false),
            Err(err) => Err(Error::io("inspect", &path, err)),
        }
    }

    /// Reads the whole file `name`, failing with [`Error::TooLarge`] if it exceeds `max_len`.
    pub fn read(&self, name: &str, max_len: u64) -> Result<Vec<u8>> {
        let path = self.resolve(name)?;
        let file = File::open(&path).map_err(|err| Error::io("open", &path, err))?;
        let mut data = Vec::new();
        file.take(max_len.saturating_add(1))
            .read_to_end(&mut data)
            .map_err(|err| Error::io("read", &path, err))?;
        if data.len() as u64 > max_len {
            return Err(Error::TooLarge { path, max_len });
        }
        Ok(data)
    }

    /// Lists the entries of the directory `dir` (the empty string is the root) with their kind
    /// and size, in ascending order of name. Entries that disappear while the directory is read
    /// are left out. Symbolic links are reported as such, not followed.
    pub fn list_entries(&self, dir: &str) -> Result<Vec<DirEntryInfo>> {
        let path = if dir.is_empty() {
            self.root.clone()
        } else {
            self.resolve(dir)?
        };
        let mut entries = Vec::new();
        for entry in fs::read_dir(&path).map_err(|err| Error::io("list", &path, err))? {
            let entry = entry.map_err(|err| Error::io("list", &path, err))?;
            let meta = match entry.metadata() {
                Ok(meta) => meta,
                Err(err) if err.kind() == ErrorKind::NotFound => continue,
                Err(err) => return Err(Error::io("inspect", &entry.path(), err)),
            };
            let file_type = meta.file_type();
            let kind = if file_type.is_symlink() {
                DirEntryKind::Symlink
            } else if file_type.is_dir() {
                DirEntryKind::Directory
            } else if file_type.is_file() {
                DirEntryKind::File
            } else {
                DirEntryKind::Other
            };
            entries.push(DirEntryInfo {
                name: entry.file_name().to_string_lossy().into_owned(),
                kind,
                len: if kind == DirEntryKind::File { meta.len() } else { 0 },
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    /// Reads at most the first `max` bytes of the file `name` and returns them with the size of
    /// the whole file.
    pub fn read_prefix(&self, name: &str, max: usize) -> Result<(Vec<u8>, u64)> {
        let path = self.resolve(name)?;
        let file = File::open(&path).map_err(|err| Error::io("open", &path, err))?;
        let len = file
            .metadata()
            .map_err(|err| Error::io("inspect", &path, err))?
            .len();
        let mut prefix = Vec::with_capacity(max);
        file.take(max as u64)
            .read_to_end(&mut prefix)
            .map_err(|err| Error::io("read", &path, err))?;
        Ok((prefix, len))
    }

    /// The size in bytes of the file `name`, from its metadata (the file is not read).
    pub fn file_len(&self, name: &str) -> Result<u64> {
        let path = self.resolve(name)?;
        fs::symlink_metadata(&path)
            .map(|meta| meta.len())
            .map_err(|err| Error::io("inspect", &path, err))
    }

    /// The modification time of `name`.
    pub fn modified(&self, name: &str) -> Result<std::time::SystemTime> {
        let path = self.resolve(name)?;
        fs::symlink_metadata(&path)
            .and_then(|meta| meta.modified())
            .map_err(|err| Error::io("inspect", &path, err))
    }

    /// Lists entry names of the directory `dir` in sorted order. The empty string lists the root.
    pub fn list(&self, dir: &str) -> Result<Vec<String>> {
        let path = if dir.is_empty() {
            self.root.clone()
        } else {
            self.resolve(dir)?
        };
        let mut names = Vec::new();
        for entry in fs::read_dir(&path).map_err(|err| Error::io("list", &path, err))? {
            let entry = entry.map_err(|err| Error::io("list", &path, err))?;
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        names.sort();
        Ok(names)
    }

    /// Creates the directory `name` with private permissions. Succeeds if it already exists.
    pub fn create_dir(&self, name: &str) -> Result<()> {
        create_private_dir(&self.resolve(name)?)
    }

    /// Atomically creates the file `name` with `data`. Never overwrites: if `name` exists the
    /// call fails with [`Error::AlreadyExists`] and the existing file is untouched.
    pub fn write_new(&self, name: &str, data: &[u8]) -> Result<()> {
        let dest = self.resolve(name)?;
        let dir = self.parent_of(&dest);
        let temp = write_temp(dir, data)?;
        let published = fs::hard_link(&temp, &dest);
        discard_temp(&temp);
        match published {
            Ok(()) => sync_dir(dir),
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                Err(Error::AlreadyExists { path: dest })
            }
            Err(err) => Err(Error::io("publish", &dest, err)),
        }
    }

    /// Atomically creates or replaces the file `name` with `data`.
    pub fn write_replace(&self, name: &str, data: &[u8]) -> Result<()> {
        let dest = self.resolve(name)?;
        let dir = self.parent_of(&dest);
        let temp = write_temp(dir, data)?;
        if let Err(err) = fs::rename(&temp, &dest) {
            discard_temp(&temp);
            return Err(Error::io("publish", &dest, err));
        }
        sync_dir(dir)
    }

    /// Removes the file `name`.
    pub fn remove(&self, name: &str) -> Result<()> {
        let path = self.resolve(name)?;
        fs::remove_file(&path).map_err(|err| Error::io("remove", &path, err))
    }

    fn parent_of<'a>(&'a self, path: &'a Path) -> &'a Path {
        path.parent().unwrap_or(&self.root)
    }
}

/// Accepts `/`-separated segments of `[A-Za-z0-9._-]`, none empty, none starting with `.`.
/// This excludes absolute paths, `.`, `..`, and the store's own temporary files.
fn validate_name(name: &str) -> Result<PathBuf> {
    let invalid = || Error::InvalidName(name.to_owned());
    if name.is_empty() {
        return Err(invalid());
    }
    let mut relative = PathBuf::new();
    for segment in name.split('/') {
        let valid_char = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-');
        let valid = !segment.is_empty()
            && segment.len() <= MAX_NAME_LEN
            && !segment.starts_with('.')
            && segment.bytes().all(valid_char);
        if !valid {
            return Err(invalid());
        }
        relative.push(segment);
    }
    Ok(relative)
}

fn create_private_dir(path: &Path) -> Result<()> {
    match private_dir_builder().create(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        Err(err) => Err(Error::io("create directory", path, err)),
    }
}

fn write_temp(dir: &Path, data: &[u8]) -> Result<PathBuf> {
    let temp = dir.join(format!("{TEMP_PREFIX}{}", random_hex(8)?));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    private_file_options(&mut options);
    let mut file = options
        .open(&temp)
        .map_err(|err| Error::io("create", &temp, err))?;
    if let Err(err) = file.write_all(data).and_then(|()| file.sync_all()) {
        discard_temp(&temp);
        return Err(Error::io("write", &temp, err));
    }
    Ok(temp)
}

fn discard_temp(temp: &Path) {
    if let Err(err) = fs::remove_file(temp) {
        tracing::warn!(path = %temp.display(), error = %err, "failed to remove temporary file");
    }
}

#[cfg(unix)]
fn private_dir_builder() -> DirBuilder {
    use std::os::unix::fs::DirBuilderExt;

    let mut builder = DirBuilder::new();
    builder.mode(0o700);
    builder
}

#[cfg(not(unix))]
fn private_dir_builder() -> DirBuilder {
    DirBuilder::new()
}

#[cfg(unix)]
fn private_file_options(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;

    options.mode(0o600);
}

#[cfg(not(unix))]
fn private_file_options(_options: &mut OpenOptions) {}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> Result<()> {
    File::open(dir)
        .and_then(|handle| handle.sync_all())
        .map_err(|err| Error::io("sync directory", dir, err))
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn store() -> (TempDir, LocalStore) {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let store = LocalStore::new(dir.path());
        (dir, store)
    }

    #[test]
    fn entries_are_listed_with_their_kind_and_size_in_name_order() {
        let (dir, store) = store();
        store.write_new("config", b"12345").expect("write");
        store.create_dir("data").expect("mkdir");
        fs::write(dir.path().join(".hidden"), b"").expect("write");
        #[cfg(unix)]
        std::os::unix::fs::symlink("config", dir.path().join("link")).expect("symlink");

        let entries = store.list_entries("").expect("list");
        let described: Vec<(&str, DirEntryKind, u64)> = entries
            .iter()
            .map(|entry| (entry.name.as_str(), entry.kind, entry.len))
            .collect();
        let mut expected = vec![
            (".hidden", DirEntryKind::File, 0),
            ("config", DirEntryKind::File, 5),
            ("data", DirEntryKind::Directory, 0),
        ];
        #[cfg(unix)]
        expected.push(("link", DirEntryKind::Symlink, 0));
        assert_eq!(described, expected);
        assert!(store.list_entries("data").expect("list").is_empty());
        assert!(store.list_entries("missing").is_err());
    }

    #[test]
    fn a_prefix_is_read_together_with_the_size_of_the_whole_file() {
        let (_dir, store) = store();
        store.write_new("config", b"0123456789").expect("write");
        assert_eq!(store.read_prefix("config", 4).expect("read"), (b"0123".to_vec(), 10));
        assert_eq!(store.read_prefix("config", 100).expect("read"), (b"0123456789".to_vec(), 10));
        assert_eq!(store.read_prefix("config", 0).expect("read"), (Vec::new(), 10));
        let err = store.read_prefix("absent", 4).expect_err("must fail");
        assert!(err.is_not_found(), "{err}");
    }

    #[test]
    fn write_new_then_read_round_trips() {
        let (_dir, store) = store();
        store.write_new("config", b"hello").expect("write");
        assert_eq!(store.read("config", 100).expect("read"), b"hello");
        assert!(store.exists("config").expect("exists"));
    }

    #[test]
    fn write_new_never_overwrites() {
        let (_dir, store) = store();
        store.write_new("config", b"first").expect("write");
        let err = store.write_new("config", b"second").expect_err("must fail");
        assert!(matches!(err, Error::AlreadyExists { .. }));
        assert_eq!(store.read("config", 100).expect("read"), b"first");
    }

    #[test]
    fn writes_leave_no_temporary_files() {
        let (_dir, store) = store();
        store.write_new("a", b"1").expect("write");
        let _ = store.write_new("a", b"2");
        store.write_replace("b", b"1").expect("write");
        store.write_replace("b", b"2").expect("write");
        assert_eq!(store.list("").expect("list"), ["a", "b"]);
    }

    #[test]
    fn write_replace_overwrites_atomically() {
        let (_dir, store) = store();
        store.write_replace("index", b"old").expect("write");
        store.write_replace("index", b"new").expect("write");
        assert_eq!(store.read("index", 100).expect("read"), b"new");
    }

    #[test]
    fn missing_file_reads_as_not_found() {
        let (_dir, store) = store();
        let err = store.read("nothing", 10).expect_err("must fail");
        assert!(err.is_not_found());
        assert!(!store.exists("nothing").expect("exists"));
    }

    #[test]
    fn oversized_file_is_rejected() {
        let (_dir, store) = store();
        store.write_new("big", &[7_u8; 11]).expect("write");
        assert!(matches!(store.read("big", 10), Err(Error::TooLarge { max_len: 10, .. })));
        assert_eq!(store.read("big", 11).expect("read").len(), 11);
    }

    #[test]
    fn unsafe_names_are_rejected() {
        let (_dir, store) = store();
        let long = "a".repeat(MAX_NAME_LEN + 1);
        let names = [
            "",
            "/",
            "/etc/passwd",
            "..",
            ".",
            "../x",
            "a/../b",
            "a//b",
            "a/",
            "/a",
            ".hidden",
            ".tmp-1",
            "a b",
            "a\\b",
            "a\0b",
            "caf\u{e9}",
            long.as_str(),
        ];
        for name in names {
            assert!(matches!(store.resolve(name), Err(Error::InvalidName(_))), "{name:?}");
            assert!(store.write_new(name, b"x").is_err(), "{name:?}");
        }
        assert!(store.list("").expect("list").is_empty());
    }

    #[test]
    fn nested_names_work_inside_existing_directories() {
        let (dir, store) = store();
        store.create_dir("locks").expect("mkdir");
        store.write_new("locks/a.lock", b"x").expect("write");
        assert!(dir.path().join("locks").join("a.lock").is_file());
        assert_eq!(store.list("locks").expect("list"), ["a.lock"]);
    }

    #[test]
    fn writing_into_a_missing_directory_fails_without_creating_it() {
        let (dir, store) = store();
        let err = store.write_new("missing/file", b"x").expect_err("must fail");
        assert!(err.is_not_found(), "{err}");
        assert!(!dir.path().join("missing").exists());
    }

    #[test]
    fn create_dir_is_idempotent_but_refuses_files() {
        let (_dir, store) = store();
        store.create_dir("data").expect("mkdir");
        store.create_dir("data").expect("mkdir again");
        store.write_new("file", b"x").expect("write");
        assert!(store.create_dir("file").is_err());
    }

    #[test]
    fn create_root_makes_missing_parents() {
        let (dir, _) = store();
        let root = dir.path().join("a").join("b").join("repo");
        let store = LocalStore::new(&root);
        store.create_root().expect("create root");
        store.create_root().expect("create root again");
        assert!(root.is_dir());
    }

    #[test]
    fn create_root_fails_when_a_parent_is_a_file() {
        let (dir, store) = store();
        store.write_new("file", b"x").expect("write");
        let nested = LocalStore::new(dir.path().join("file").join("repo"));
        assert!(nested.create_root().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_followed() {
        let (_dir, store) = store();
        let (outside, _) = self::store();
        std::os::unix::fs::symlink(outside.path(), store.root().join("link")).expect("symlink");
        let err = store.write_new("link/file", b"x").expect_err("must fail");
        assert!(matches!(err, Error::Symlink { .. }));
        assert!(matches!(store.read("link", 10), Err(Error::Symlink { .. })));
        assert!(fs::read_dir(outside.path()).expect("read_dir").next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn files_and_directories_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, store) = store();
        store.create_dir("data").expect("mkdir");
        store.write_new("config", b"x").expect("write");
        store.write_replace("data/obj", b"x").expect("write");
        let mode = |name: &str| {
            let path = store.resolve(name).expect("resolve");
            fs::metadata(path).expect("metadata").permissions().mode() & 0o777
        };
        assert_eq!(mode("data"), 0o700);
        assert_eq!(mode("config"), 0o600);
        assert_eq!(mode("data/obj"), 0o600);
    }
}
