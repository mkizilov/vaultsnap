//! An in-memory backend with the semantics of an object store: a flat namespace of immutable
//! objects, directories that are only prefixes, no renames, no links, writes that are atomic
//! because they replace a whole value under one lock, and conditional writes that compare
//! versions.
//!
//! It exists to show that nothing above the [`Backend`] interface needs a file system (a
//! repository can be initialized, backed up to, checked, restored from, and pruned on it, see
//! `tests/storage.rs`), to run the [contract](super::contract) against a second, very different
//! implementation, and to stand in for an object store in tests. It is not a way to keep data:
//! everything is gone when the value is dropped.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

use super::{
    validate_dir, validate_name, Backend, Capabilities, DirEntryInfo, DirEntryKind, Guarantee,
    LocationState, Object, ObjectInfo, PutMode, Version,
};
use crate::error::{BackendErrorKind, Error, Result};

#[derive(Debug, Clone)]
struct Stored {
    data: Vec<u8>,
    version: u64,
    modified: SystemTime,
}

#[derive(Debug, Default)]
struct State {
    objects: BTreeMap<String, Stored>,
    counter: u64,
}

/// Objects kept in memory. See the module documentation.
pub struct MemoryBackend {
    root: PathBuf,
    capabilities: Capabilities,
    state: Mutex<State>,
}

impl fmt::Debug for MemoryBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryBackend")
            .field("root", &self.root)
            .field("objects", &self.state().objects.len())
            .finish_non_exhaustive()
    }
}

impl Default for MemoryBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryBackend {
    /// An empty backend that guarantees everything an object store with strong consistency and
    /// conditional writes does.
    pub fn new() -> Self {
        Self::with_capabilities(Capabilities {
            read_after_write: true,
            create_if_absent: Guarantee::Global,
            replace_if_unchanged: Guarantee::Global,
            atomic_put: true,
            directories: false,
        })
    }

    /// An empty backend that claims `capabilities`, for testing how the repository treats a
    /// backend that cannot hold it. The behavior stays that of [`MemoryBackend::new`] whatever
    /// is claimed.
    pub fn with_capabilities(capabilities: Capabilities) -> Self {
        Self {
            root: PathBuf::from("memory://repository"),
            capabilities,
            state: Mutex::new(State::default()),
        }
    }

    /// The names of all objects, in order.
    pub fn object_names(&self) -> Vec<String> {
        self.state().objects.keys().cloned().collect()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn not_found(operation: &'static str, name: &str) -> Error {
        Error::backend(
            operation,
            name,
            BackendErrorKind::NotFound,
            "no such object",
        )
    }

    fn info(stored: &Stored) -> ObjectInfo {
        ObjectInfo {
            len: stored.data.len() as u64,
            modified: stored.modified,
            version: Version::new(format!("v{}", stored.version)),
        }
    }
}

impl Backend for MemoryBackend {
    fn root(&self) -> &Path {
        &self.root
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }

    fn probe(&self) -> Result<LocationState> {
        Ok(LocationState::Available)
    }

    fn create_root(&self) -> Result<()> {
        Ok(())
    }

    fn create_dir(&self, dir: &str) -> Result<()> {
        validate_name(dir)
    }

    fn dir_exists(&self, dir: &str) -> Result<bool> {
        validate_name(dir)?;
        Ok(true)
    }

    fn stat(&self, name: &str) -> Result<ObjectInfo> {
        validate_name(name)?;
        match self.state().objects.get(name) {
            Some(stored) => Ok(Self::info(stored)),
            None => Err(Self::not_found("stat", name)),
        }
    }

    fn get(&self, name: &str, max_len: u64) -> Result<Object> {
        validate_name(name)?;
        let state = self.state();
        let Some(stored) = state.objects.get(name) else {
            return Err(Self::not_found("get", name));
        };
        if stored.data.len() as u64 > max_len {
            return Err(Error::TooLarge {
                path: self.root.join(name),
                max_len,
            });
        }
        Ok(Object {
            data: stored.data.clone(),
            info: Self::info(stored),
        })
    }

    fn get_prefix(&self, name: &str, max: usize) -> Result<(Vec<u8>, u64)> {
        validate_name(name)?;
        let state = self.state();
        let Some(stored) = state.objects.get(name) else {
            return Err(Self::not_found("get_prefix", name));
        };
        let end = max.min(stored.data.len());
        Ok((stored.data[..end].to_vec(), stored.data.len() as u64))
    }

    fn list_dir(&self, dir: &str) -> Result<Vec<DirEntryInfo>> {
        validate_dir(dir)?;
        let prefix = if dir.is_empty() {
            String::new()
        } else {
            format!("{dir}/")
        };
        let state = self.state();
        let mut entries: Vec<DirEntryInfo> = Vec::new();
        for (key, stored) in state.objects.range(prefix.clone()..) {
            let Some(rest) = key.strip_prefix(prefix.as_str()) else {
                break;
            };
            match rest.split_once('/') {
                Some((first, _)) => {
                    let known = entries
                        .iter()
                        .any(|entry| entry.kind == DirEntryKind::Directory && entry.name == first);
                    if !known {
                        entries.push(DirEntryInfo {
                            name: first.to_owned(),
                            kind: DirEntryKind::Directory,
                            len: 0,
                        });
                    }
                }
                None => entries.push(DirEntryInfo {
                    name: rest.to_owned(),
                    kind: DirEntryKind::File,
                    len: stored.data.len() as u64,
                }),
            }
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    fn put(&self, name: &str, data: &[u8], mode: PutMode) -> Result<Version> {
        validate_name(name)?;
        let mut state = self.state();
        match (&mode, state.objects.get(name)) {
            (PutMode::Create, Some(_)) => {
                return Err(Error::AlreadyExists {
                    path: self.root.join(name),
                });
            }
            (PutMode::Update(expected), current) => {
                let unchanged =
                    current.is_some_and(|stored| Self::info(stored).version == *expected);
                if !unchanged {
                    return Err(Error::PreconditionFailed {
                        path: self.root.join(name),
                    });
                }
            }
            _ => {}
        }
        state.counter += 1;
        let stored = Stored {
            data: data.to_vec(),
            version: state.counter,
            modified: SystemTime::now(),
        };
        let version = Self::info(&stored).version;
        state.objects.insert(name.to_owned(), stored);
        Ok(version)
    }

    fn delete(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        self.state().objects.remove(name);
        Ok(())
    }

    fn sync_dir(&self, dir: &str) -> Result<()> {
        validate_dir(dir)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::contract::{self, Fixture};
    use super::*;

    #[test]
    fn the_memory_backend_passes_the_contract() {
        contract::run(&|| Fixture::new(Arc::new(MemoryBackend::new())));
    }

    #[test]
    fn directories_are_only_prefixes_of_object_names() {
        let backend = MemoryBackend::new();
        backend.put("a/b/c", b"1", PutMode::Create).expect("create");
        backend.put("a/d", b"22", PutMode::Create).expect("create");
        backend.put("top", b"333", PutMode::Create).expect("create");

        let kinds = |dir: &str| -> Vec<(String, DirEntryKind, u64)> {
            backend
                .list_dir(dir)
                .expect("list")
                .into_iter()
                .map(|entry| (entry.name, entry.kind, entry.len))
                .collect()
        };
        assert_eq!(
            kinds(""),
            [
                ("a".to_owned(), DirEntryKind::Directory, 0),
                ("top".to_owned(), DirEntryKind::File, 3)
            ]
        );
        assert_eq!(
            kinds("a"),
            [
                ("b".to_owned(), DirEntryKind::Directory, 0),
                ("d".to_owned(), DirEntryKind::File, 2)
            ]
        );
        assert_eq!(kinds("a/b"), [("c".to_owned(), DirEntryKind::File, 1)]);
        assert!(
            kinds("nothing").is_empty(),
            "a prefix without objects is just empty"
        );
        assert_eq!(backend.object_names(), ["a/b/c", "a/d", "top"]);
        assert!(backend.dir_exists("nothing").expect("answer"));
    }

    #[test]
    fn deleting_a_missing_object_succeeds_as_it_does_on_an_object_store() {
        let backend = MemoryBackend::new();
        backend.delete("never-existed").expect("idempotent");
        backend.put("x", b"1", PutMode::Create).expect("create");
        backend.delete("x").expect("delete");
        backend.delete("x").expect("again");
        assert!(backend.object_names().is_empty());
    }

    #[test]
    fn missing_objects_are_reported_with_the_backend_error_kind() {
        let backend = MemoryBackend::new();
        let err = backend.get("absent", 10).expect_err("missing");
        assert!(err.is_not_found() && !err.is_transient(), "{err}");
        assert!(matches!(
            err,
            Error::Backend {
                kind: BackendErrorKind::NotFound,
                ..
            }
        ));
        assert_eq!(
            err.to_string(),
            "storage backend could not get absent: no such object"
        );
    }

    #[test]
    fn it_is_not_a_local_directory() {
        let backend = MemoryBackend::new();
        assert_eq!(backend.local_path(), None);
        assert_eq!(backend.root(), Path::new("memory://repository"));
    }

    #[test]
    fn the_claimed_capabilities_are_reported_as_given() {
        let claimed = Capabilities {
            read_after_write: false,
            create_if_absent: Guarantee::None,
            replace_if_unchanged: Guarantee::None,
            atomic_put: true,
            directories: false,
        };
        let backend = MemoryBackend::with_capabilities(claimed.clone());
        assert_eq!(backend.capabilities(), claimed);
        assert!(backend.capabilities().require_repository_support().is_err());
    }
}
