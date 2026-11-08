//! The local file system backend, the reference implementation of [`Backend`]: validated names,
//! no symlink traversal, atomic writes, private permissions (`0700` directories, `0600` files on
//! Unix).
//!
//! Every write is staged in a temporary file next to its destination, flushed to disk, and then
//! published in a single file system step: a hard link for [`PutMode::Create`], which fails if
//! the name exists and so makes creation exclusive across processes, and a rename for
//! replacements. A reader or a crash sees either the old state or the complete new object,
//! never a partial one. Objects are addressed by repository-relative names such as `config` or
//! `locks/ab12.lock`; see [`validate_name`](super::validate_name) for the accepted syntax.
//!
//! A file system has no compare-and-swap, so [`PutMode::Update`] is made atomic with a lock
//! inside this process: it excludes other updates made through any `LocalBackend` of the same
//! process, and nothing else. Writers in other processes must be kept out with the repository
//! lock, which is why [`Capabilities::replace_if_unchanged`] is only [`Guarantee::Process`].

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use super::{
    validate_dir, validate_name, Backend, Capabilities, DirEntryInfo, DirEntryKind, Guarantee,
    LocationState, Object, ObjectInfo, PutMode, Version, TEMP_PREFIX,
};
use crate::error::{Error, Result};
use crate::util::random_hex;

/// Serializes the check and the replacement of [`PutMode::Update`] within the process.
static REPLACE_LOCK: Mutex<()> = Mutex::new(());

/// A directory on the local file system used as repository storage.
#[derive(Debug, Clone)]
pub struct LocalBackend {
    root: PathBuf,
}

impl LocalBackend {
    /// Refers to the store rooted at `root`. Performs no I/O.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Maps an object name to a file system path, rejecting unsafe names and any symbolic link
    /// along the way. The path need not exist. This is specific to the file system and not part
    /// of [`Backend`]; tests use it to damage objects on purpose.
    pub fn resolve(&self, name: &str) -> Result<PathBuf> {
        validate_name(name)?;
        let mut current = self.root.clone();
        for component in name.split('/') {
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

    fn parent_of<'a>(&'a self, path: &'a Path) -> &'a Path {
        path.parent().unwrap_or(&self.root)
    }

    /// Creates `name` with `data` and fails if it exists.
    fn publish_new(&self, name: &str, data: &[u8]) -> Result<Version> {
        let dest = self.resolve(name)?;
        let dir = self.parent_of(&dest);
        let (temp, version) = write_temp(dir, data)?;
        let published = fs::hard_link(&temp, &dest);
        discard_temp(&temp);
        match published {
            Ok(()) => sync_dir(dir).map(|()| version),
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                Err(Error::AlreadyExists { path: dest })
            }
            Err(err) => Err(Error::io("publish", &dest, err)),
        }
    }

    /// Creates or replaces `name` with `data`.
    fn publish_replace(&self, name: &str, data: &[u8]) -> Result<Version> {
        let dest = self.resolve(name)?;
        let dir = self.parent_of(&dest);
        let (temp, version) = write_temp(dir, data)?;
        if let Err(err) = fs::rename(&temp, &dest) {
            discard_temp(&temp);
            return Err(Error::io("publish", &dest, err));
        }
        sync_dir(dir).map(|()| version)
    }
}

impl Backend for LocalBackend {
    fn root(&self) -> &Path {
        &self.root
    }

    fn local_path(&self) -> Option<&Path> {
        Some(&self.root)
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            read_after_write: true,
            create_if_absent: Guarantee::Global,
            replace_if_unchanged: Guarantee::Process,
            atomic_put: true,
            directories: true,
        }
    }

    fn probe(&self) -> Result<LocationState> {
        match fs::metadata(&self.root) {
            Ok(meta) if meta.is_dir() => Ok(LocationState::Available),
            Ok(_) => Ok(LocationState::Unusable(format!(
                "{} exists and is not a directory",
                self.root.display()
            ))),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(LocationState::Absent),
            Err(err) => Err(Error::io("inspect", &self.root, err)),
        }
    }

    /// Creates the root directory (and missing parents) if it does not exist yet. The root itself
    /// is created private; parents get default permissions.
    fn create_root(&self) -> Result<()> {
        let parent = self.root.parent().filter(|p| !p.as_os_str().is_empty());
        if let Some(parent) = parent {
            fs::create_dir_all(parent).map_err(|err| Error::io("create directory", parent, err))?;
        }
        create_private_dir(&self.root)
    }

    fn create_dir(&self, dir: &str) -> Result<()> {
        create_private_dir(&self.resolve(dir)?)
    }

    fn dir_exists(&self, dir: &str) -> Result<bool> {
        Ok(self.resolve(dir)?.is_dir())
    }

    fn exists(&self, name: &str) -> Result<bool> {
        let path = self.resolve(name)?;
        match fs::symlink_metadata(&path) {
            Ok(_) => Ok(true),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(false),
            Err(err) => Err(Error::io("inspect", &path, err)),
        }
    }

    fn stat(&self, name: &str) -> Result<ObjectInfo> {
        let path = self.resolve(name)?;
        let meta = fs::symlink_metadata(&path).map_err(|err| Error::io("inspect", &path, err))?;
        let modified = meta.modified().map_err(|err| Error::io("inspect", &path, err))?;
        Ok(ObjectInfo {
            len: meta.len(),
            modified,
            version: version_of(&meta),
        })
    }

    fn get(&self, name: &str, max_len: u64) -> Result<Object> {
        let path = self.resolve(name)?;
        let file = File::open(&path).map_err(|err| Error::io("open", &path, err))?;
        // The version comes from the open file, so it describes the bytes read below even if the
        // name is replaced in the meantime.
        let meta = file.metadata().map_err(|err| Error::io("inspect", &path, err))?;
        let modified = meta.modified().map_err(|err| Error::io("inspect", &path, err))?;
        let mut data = Vec::new();
        file.take(max_len.saturating_add(1))
            .read_to_end(&mut data)
            .map_err(|err| Error::io("read", &path, err))?;
        if data.len() as u64 > max_len {
            return Err(Error::TooLarge { path, max_len });
        }
        Ok(Object {
            info: ObjectInfo {
                len: data.len() as u64,
                modified,
                version: version_of(&meta),
            },
            data,
        })
    }

    fn get_prefix(&self, name: &str, max: usize) -> Result<(Vec<u8>, u64)> {
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

    fn list_dir(&self, dir: &str) -> Result<Vec<DirEntryInfo>> {
        validate_dir(dir)?;
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

    fn put(&self, name: &str, data: &[u8], mode: PutMode) -> Result<Version> {
        match mode {
            PutMode::Create => self.publish_new(name, data),
            PutMode::Overwrite => self.publish_replace(name, data),
            PutMode::Update(expected) => {
                let _guard = REPLACE_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
                let unchanged = match self.stat(name) {
                    Ok(current) => current.version == expected,
                    Err(err) if err.is_not_found() => false,
                    Err(err) => return Err(err),
                };
                if unchanged {
                    self.publish_replace(name, data)
                } else {
                    Err(Error::PreconditionFailed {
                        path: self.resolve(name)?,
                    })
                }
            }
        }
    }

    fn delete(&self, name: &str) -> Result<()> {
        let path = self.resolve(name)?;
        fs::remove_file(&path).map_err(|err| Error::io("remove", &path, err))
    }

    fn sync_dir(&self, dir: &str) -> Result<()> {
        sync_dir(&self.resolve(dir)?)
    }
}

/// A version made of what identifies one file and what changes when it is replaced: device and
/// inode (a replacement is a new file), modification time, and size.
#[cfg(unix)]
fn version_of(meta: &fs::Metadata) -> Version {
    use std::os::unix::fs::MetadataExt;

    Version::new(format!(
        "{}-{}-{}.{}-{}",
        meta.dev(),
        meta.ino(),
        meta.mtime(),
        meta.mtime_nsec(),
        meta.len()
    ))
}

/// Without inodes, the modification time and the size are all there is.
#[cfg(not(unix))]
fn version_of(meta: &fs::Metadata) -> Version {
    use std::time::UNIX_EPOCH;

    let modified = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |elapsed| elapsed.as_nanos());
    Version::new(format!("{}-{modified}", meta.len()))
}

fn create_private_dir(path: &Path) -> Result<()> {
    match private_dir_builder().create(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        Err(err) => Err(Error::io("create directory", path, err)),
    }
}

/// Writes `data` to a new private temporary file in `dir` and flushes it. Returns the file's
/// path and the version the object will have once the file is published under its name.
fn write_temp(dir: &Path, data: &[u8]) -> Result<(PathBuf, Version)> {
    let temp = dir.join(format!("{TEMP_PREFIX}{}", random_hex(8)?));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    private_file_options(&mut options);
    let mut file = options
        .open(&temp)
        .map_err(|err| Error::io("create", &temp, err))?;
    let written = file
        .write_all(data)
        .and_then(|()| file.sync_all())
        .and_then(|()| file.metadata());
    match written {
        Ok(meta) => Ok((temp, version_of(&meta))),
        Err(err) => {
            discard_temp(&temp);
            Err(Error::io("write", &temp, err))
        }
    }
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
    use std::sync::Arc;

    use tempfile::TempDir;

    use super::super::contract::{self, Fixture};
    use super::*;

    fn backend() -> (TempDir, LocalBackend) {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let backend = LocalBackend::new(dir.path());
        (dir, backend)
    }

    #[test]
    fn the_local_backend_passes_the_contract() {
        contract::run(&|| {
            let dir = tempfile::tempdir().expect("create temporary directory");
            let backend = Arc::new(LocalBackend::new(dir.path()));
            Fixture::keeping(backend, dir)
        });
    }

    #[test]
    fn the_probe_tells_a_missing_directory_from_a_usable_one_and_from_a_file() {
        let (dir, backend) = backend();
        assert_eq!(backend.probe().expect("probe"), LocationState::Available);

        let missing = LocalBackend::new(dir.path().join("missing"));
        assert_eq!(missing.probe().expect("probe"), LocationState::Absent);
        missing.create_root().expect("create");
        assert_eq!(missing.probe().expect("probe"), LocationState::Available);

        let file = dir.path().join("file");
        fs::write(&file, b"x").expect("write");
        let state = LocalBackend::new(&file).probe().expect("probe");
        match state {
            LocationState::Unusable(reason) => assert!(reason.contains("not a directory"), "{reason}"),
            other => panic!("expected Unusable, got {other:?}"),
        }
    }

    #[test]
    fn the_version_of_an_object_changes_when_it_is_replaced_even_by_the_same_bytes() {
        let (_dir, backend) = backend();
        let first = backend.put("a", b"same", PutMode::Create).expect("create");
        assert_eq!(backend.stat("a").expect("stat").version, first);
        let second = backend.put("a", b"same", PutMode::Overwrite).expect("overwrite");
        assert_ne!(first, second);
        let read = backend.get("a", 100).expect("get");
        assert_eq!(read.info.version, second);
        assert_eq!(backend.stat("a").expect("stat").version, second);
    }

    #[test]
    fn an_update_made_after_a_replacement_is_refused_and_changes_nothing() {
        let (_dir, backend) = backend();
        let seen = backend.put("meta", b"one", PutMode::Create).expect("create");
        backend.put("meta", b"two", PutMode::Overwrite).expect("someone else replaces it");

        let err = backend
            .put("meta", b"three", PutMode::Update(seen))
            .expect_err("stale version");
        assert!(matches!(err, Error::PreconditionFailed { .. }), "{err}");
        assert_eq!(backend.get("meta", 100).expect("get").data, b"two");

        let current = backend.stat("meta").expect("stat").version;
        backend.put("meta", b"three", PutMode::Update(current)).expect("current version");
        assert_eq!(backend.get("meta", 100).expect("get").data, b"three");
    }

    #[test]
    fn a_published_object_leaves_no_temporary_file_and_is_private() {
        let (dir, backend) = backend();
        backend.create_dir("d").expect("mkdir");
        backend.put("d/x", b"data", PutMode::Create).expect("create");
        backend.put("d/x", b"data2", PutMode::Overwrite).expect("overwrite");
        let err = backend.put("d/x", b"again", PutMode::Create).expect_err("exists");
        assert!(matches!(err, Error::AlreadyExists { .. }), "{err}");

        let names: Vec<String> = backend
            .list_dir("d")
            .expect("list")
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        assert_eq!(names, ["x"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = |path: PathBuf| fs::metadata(path).expect("metadata").permissions().mode() & 0o777;
            assert_eq!(mode(dir.path().join("d")), 0o700);
            assert_eq!(mode(dir.path().join("d/x")), 0o600);
        }
        #[cfg(not(unix))]
        let _ = dir;
    }

    #[test]
    fn the_names_that_resolve_are_the_valid_ones_and_links_are_refused() {
        let (dir, backend) = backend();
        assert_eq!(backend.resolve("a/b").expect("resolve"), dir.path().join("a").join("b"));
        assert!(matches!(backend.resolve("../x"), Err(Error::InvalidName(_))));
        assert!(matches!(backend.resolve(""), Err(Error::InvalidName(_))));
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().expect("create temporary directory");
            std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).expect("symlink");
            assert!(matches!(backend.resolve("link/x"), Err(Error::Symlink { .. })));
            assert!(matches!(backend.get("link/x", 10), Err(Error::Symlink { .. })));
        }
    }

    #[test]
    fn a_local_backend_knows_its_directory() {
        let (dir, backend) = backend();
        assert_eq!(backend.root(), dir.path());
        assert_eq!(backend.local_path(), Some(dir.path()));
        let caps = backend.capabilities();
        assert!(caps.directories && caps.read_after_write && caps.atomic_put);
        assert_eq!(caps.create_if_absent, Guarantee::Global);
        assert_eq!(caps.replace_if_unchanged, Guarantee::Process);
        assert!(caps.require_repository_support().is_ok());
    }
}
