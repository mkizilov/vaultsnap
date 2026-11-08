//! Turns a scan and the chunk lists of the files read into a [`Manifest`].

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use super::id::SnapshotId;
use super::model::{
    Attributes, BackupConfig, ChunkRef, Manifest, ManifestEntry, ManifestStats, NodeKind, Root,
    SkippedReason, SkippedRecord, MANIFEST_FORMAT_VERSION,
};
use crate::error::{Error, Result};
use crate::scan::{Entry, EntryKind, SkipReason, Skipped, Timestamp};

/// A path component or path as the raw bytes the manifest stores. On Unix these are the exact
/// bytes of the name. Elsewhere names are converted lossily, which is a documented limitation.
#[cfg(unix)]
pub(crate) fn os_bytes(text: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;

    text.as_bytes().to_vec()
}

#[cfg(not(unix))]
pub(crate) fn os_bytes(text: &OsStr) -> Vec<u8> {
    text.to_string_lossy().into_owned().into_bytes()
}

/// A scan-root-relative path as the bytes a manifest stores for it: the components' raw bytes
/// joined with `/`, whatever the platform's separator. This is the form [`Root::relative_path`]
/// produces, so the two can be compared.
pub(crate) fn relative_path_bytes(path: &Path) -> Vec<u8> {
    let parts: Vec<Vec<u8>> = path
        .components()
        .map(|component| os_bytes(component.as_os_str()))
        .collect();
    parts.join(&b'/')
}

struct RootState {
    root: Root,
    /// Relative path of every directory seen so far to its entry index, so a child finds its
    /// parent. Entries arrive in scan order, in which a directory precedes its contents.
    directories: HashMap<PathBuf, u32>,
    /// Relative path of every file that has more than one link to its entry index, so a later
    /// hard link finds the entry that carries the data.
    link_targets: HashMap<PathBuf, u32>,
}

/// Collects scan entries into a manifest.
///
/// Feed it, for each root, the scan's entries in the order the scanner produced them together
/// with the chunk references of every regular file (the chunks that were stored for it, in file
/// order), then call [`ManifestBuilder::finish`]. The builder checks that the pieces fit
/// together and the finished manifest is validated.
pub struct ManifestBuilder {
    repository_id: String,
    host: String,
    tags: Vec<String>,
    config: BackupConfig,
    roots: Vec<RootState>,
    skipped: Vec<SkippedRecord>,
}

impl ManifestBuilder {
    /// Starts a manifest for the repository `repository_id`.
    pub fn new(repository_id: impl Into<String>, config: BackupConfig) -> Self {
        Self {
            repository_id: repository_id.into(),
            host: String::new(),
            tags: Vec::new(),
            config,
            roots: Vec::new(),
            skipped: Vec::new(),
        }
    }

    /// Records the host name the backup runs on (empty if unknown).
    #[must_use]
    pub fn host(mut self, host: impl Into<String>) -> Self {
        self.host = host.into();
        self
    }

    /// Records user-supplied labels.
    #[must_use]
    pub fn tags(mut self, tags: Vec<String>) -> Self {
        self.tags = tags;
        self
    }

    /// Adds a root, the resolved path that was scanned (`Scanner::root`), and returns its index.
    pub fn add_root(&mut self, source: &Path) -> usize {
        self.roots.push(RootState {
            root: Root {
                source: os_bytes(source.as_os_str()),
                entries: Vec::new(),
            },
            directories: HashMap::new(),
            link_targets: HashMap::new(),
        });
        self.roots.len() - 1
    }

    /// Adds the next scan entry of root `root`.
    ///
    /// `chunks` must be the ordered chunk references of a regular file (`EntryKind::File`) and
    /// empty for everything else. The stored size of a file is the sum of its chunk lengths,
    /// that is, the content that was actually read. The first entry of a root must be the root
    /// itself, and every entry must come after its parent directory, as the scanner delivers
    /// them.
    pub fn add_entry(&mut self, root: usize, entry: &Entry, chunks: Vec<ChunkRef>) -> Result<()> {
        let state = self
            .roots
            .get_mut(root)
            .ok_or(Error::InvalidArgument("unknown root index"))?;
        let index = u32::try_from(state.root.entries.len())
            .map_err(|_| Error::InvalidArgument("too many entries in one root"))?;

        let is_root_entry = entry.path.as_os_str().is_empty();
        if is_root_entry != (index == 0) {
            return Err(Error::InvalidArgument(
                "the first entry of a root, and only the first, must be the root itself",
            ));
        }
        let (parent, name) = if is_root_entry {
            (None, Vec::new())
        } else {
            let parent_path = entry.path.parent().unwrap_or_else(|| Path::new(""));
            let parent = *state.directories.get(parent_path).ok_or(Error::InvalidArgument(
                "an entry arrived before its parent directory",
            ))?;
            let name = entry
                .path
                .file_name()
                .map(os_bytes)
                .ok_or(Error::InvalidArgument("an entry has no file name"))?;
            (Some(parent), name)
        };

        let is_file = entry.kind == EntryKind::File;
        if !is_file && !chunks.is_empty() {
            return Err(Error::InvalidArgument("only regular files have chunks"));
        }
        let mut attributes = Attributes::from_scan(&entry.metadata);
        let kind = match &entry.kind {
            EntryKind::Directory => NodeKind::Directory,
            EntryKind::File => {
                let size: u64 = chunks.iter().map(|chunk| u64::from(chunk.len)).sum();
                if chunks.is_empty() && entry.metadata.size != 0 {
                    return Err(Error::InvalidArgument("a non-empty file was given no chunks"));
                }
                attributes.size = size;
                NodeKind::File { chunks }
            }
            EntryKind::Symlink { target } => NodeKind::Symlink {
                target: os_bytes(target.as_os_str()),
            },
            EntryKind::HardLink { target } => {
                let target = *state.link_targets.get(target).ok_or(Error::InvalidArgument(
                    "a hard link arrived before the file it links to",
                ))?;
                NodeKind::HardLink { target }
            }
            EntryKind::Fifo => NodeKind::Fifo,
            EntryKind::Socket => NodeKind::Socket,
            EntryKind::CharDevice => NodeKind::CharDevice,
            EntryKind::BlockDevice => NodeKind::BlockDevice,
            EntryKind::Other => NodeKind::Other,
        };

        match &kind {
            NodeKind::Directory => {
                state.directories.insert(entry.path.clone(), index);
            }
            NodeKind::File { .. } if entry.metadata.links.is_some_and(|links| links > 1) => {
                state.link_targets.insert(entry.path.clone(), index);
            }
            _ => {}
        }
        state.root.entries.push(ManifestEntry {
            parent,
            name,
            kind,
            attributes,
        });
        Ok(())
    }

    /// Records a path the scan reported as skipped.
    pub fn add_skipped(&mut self, root: usize, skipped: &Skipped) -> Result<()> {
        if root >= self.roots.len() {
            return Err(Error::InvalidArgument("unknown root index"));
        }
        let reason = match &skipped.reason {
            SkipReason::Vanished => SkippedReason::Vanished,
            SkipReason::Unreadable {
                operation, message, ..
            } => SkippedReason::Unreadable {
                detail: format!("{operation}: {message}"),
            },
            SkipReason::OtherFilesystem => SkippedReason::OtherFilesystem,
            SkipReason::SpecialFile(kind) => SkippedReason::SpecialFile {
                kind: format!("{kind:?}"),
            },
        };
        self.skipped.push(SkippedRecord {
            root: u32::try_from(root).map_err(|_| Error::InvalidArgument("too many roots"))?,
            path: os_bytes(skipped.path.as_os_str()),
            reason,
        });
        Ok(())
    }

    /// Completes the manifest: assigns a fresh random snapshot id, computes the totals, and
    /// validates the result.
    pub fn finish(self, created: Timestamp) -> Result<Manifest> {
        let mut manifest = Manifest {
            format_version: MANIFEST_FORMAT_VERSION,
            id: SnapshotId::generate()?,
            repository_id: self.repository_id,
            created,
            host: self.host,
            tags: self.tags,
            program: format!("vaultsnap {}", env!("CARGO_PKG_VERSION")),
            config: self.config,
            roots: self.roots.into_iter().map(|state| state.root).collect(),
            skipped: self.skipped,
            stats: ManifestStats::default(),
        };
        manifest.stats = manifest.compute_stats();
        manifest.validate()?;
        Ok(manifest)
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::testing::{chunk, config, REPO_ID};
    use super::*;
    use crate::scan::Metadata;

    fn metadata(size: u64, links: u64) -> Metadata {
        Metadata {
            size,
            modified: Some(Timestamp { secs: 100, nanos: 7 }),
            accessed: None,
            changed: None,
            created: None,
            mode: Some(0o640),
            uid: Some(1),
            gid: Some(2),
            readonly: false,
            device: Some(9),
            inode: Some(10),
            links: Some(links),
        }
    }

    fn entry(path: &str, kind: EntryKind, size: u64) -> Entry {
        Entry {
            path: PathBuf::from(path),
            kind,
            metadata: metadata(size, 1),
            followed_symlink: false,
        }
    }

    fn builder() -> (ManifestBuilder, usize) {
        let mut builder = ManifestBuilder::new(REPO_ID, config())
            .host("host")
            .tags(vec!["t".to_owned()]);
        let root = builder.add_root(Path::new("/data"));
        (builder, root)
    }

    fn built() -> Manifest {
        let (mut b, root) = builder();
        let mut shared = entry("shared", EntryKind::File, 30);
        shared.metadata.links = Some(2);
        let mut alias = entry("sub/alias", EntryKind::HardLink { target: PathBuf::from("shared") }, 30);
        alias.metadata.links = Some(2);
        let steps: Vec<(Entry, Vec<ChunkRef>)> = vec![
            (entry("", EntryKind::Directory, 0), vec![]),
            (entry("a.txt", EntryKind::File, 30), vec![chunk(1, 10), chunk(2, 20)]),
            (entry("empty", EntryKind::File, 0), vec![]),
            (shared, vec![chunk(3, 30)]),
            (entry("sub", EntryKind::Directory, 0), vec![]),
            (alias, vec![]),
            (entry("sub/link", EntryKind::Symlink { target: PathBuf::from("../a.txt") }, 8), vec![]),
            (entry("sub/pipe", EntryKind::Fifo, 0), vec![]),
        ];
        for (e, chunks) in steps {
            b.add_entry(root, &e, chunks).expect("entry accepted");
        }
        b.add_skipped(
            root,
            &Skipped { path: PathBuf::from("locked"), reason: SkipReason::Vanished },
        )
        .expect("skipped accepted");
        b.finish(Timestamp { secs: 5, nanos: 6 }).expect("finish")
    }

    #[test]
    fn builds_a_tree_with_parents_names_and_chunks() {
        let manifest = built();
        let root = &manifest.roots[0];
        assert_eq!(root.source, b"/data");
        let names: Vec<&[u8]> = root.entries.iter().map(|e| e.name.as_slice()).collect();
        assert_eq!(
            names,
            [&b""[..], b"a.txt", b"empty", b"shared", b"sub", b"alias", b"link", b"pipe"]
        );
        let parents: Vec<Option<u32>> = root.entries.iter().map(|e| e.parent).collect();
        assert_eq!(
            parents,
            [None, Some(0), Some(0), Some(0), Some(0), Some(4), Some(4), Some(4)]
        );
        assert_eq!(root.relative_path(5), b"sub/alias");
        assert_eq!(manifest.host, "host");
        assert_eq!(manifest.tags, ["t"]);
        assert_eq!(manifest.created, Timestamp { secs: 5, nanos: 6 });
        assert!(manifest.program.starts_with("vaultsnap "));
        assert_eq!(manifest.format_version, MANIFEST_FORMAT_VERSION);
    }

    #[test]
    fn file_sizes_come_from_the_chunks_and_links_point_at_their_target() {
        let manifest = built();
        let entries = &manifest.roots[0].entries;
        assert_eq!(entries[1].attributes.size, 30);
        assert_eq!(entries[1].kind, NodeKind::File { chunks: vec![chunk(1, 10), chunk(2, 20)] });
        assert_eq!(entries[2].kind, NodeKind::File { chunks: Vec::new() });
        assert_eq!(entries[5].kind, NodeKind::HardLink { target: 3 });
        assert_eq!(entries[6].kind, NodeKind::Symlink { target: b"../a.txt".to_vec() });
        assert_eq!(entries[7].kind, NodeKind::Fifo);
        assert_eq!(entries[1].attributes.mode, Some(0o640));
        assert_eq!(entries[1].attributes.modified, Some(Timestamp { secs: 100, nanos: 7 }));
    }

    #[test]
    fn statistics_and_skipped_records_are_filled_in() {
        let manifest = built();
        assert_eq!(manifest.stats.directories, 2);
        assert_eq!(manifest.stats.files, 3);
        assert_eq!(manifest.stats.hard_links, 1);
        assert_eq!(manifest.stats.symlinks, 1);
        assert_eq!(manifest.stats.special_files, 1);
        assert_eq!(manifest.stats.bytes, 60);
        assert_eq!(manifest.stats.skipped, 1);
        assert_eq!(manifest.skipped[0].path, b"locked");
        assert_eq!(manifest.skipped[0].reason, SkippedReason::Vanished);
        manifest.validate().expect("valid");
    }

    #[test]
    fn relative_paths_use_slashes_and_match_the_manifest_form() {
        assert_eq!(relative_path_bytes(Path::new("")), b"");
        assert_eq!(relative_path_bytes(Path::new("a")), b"a");
        assert_eq!(relative_path_bytes(Path::new("a/b/c.txt")), b"a/b/c.txt");
        let manifest = built();
        assert_eq!(
            relative_path_bytes(Path::new("sub/alias")),
            manifest.roots[0].relative_path(5)
        );
    }

    #[test]
    fn every_manifest_gets_its_own_id() {
        assert_ne!(built().id, built().id);
    }

    #[test]
    fn a_single_file_can_be_a_root() {
        let mut b = ManifestBuilder::new(REPO_ID, config());
        let root = b.add_root(Path::new("/notes.txt"));
        b.add_entry(root, &entry("", EntryKind::File, 3), vec![chunk(1, 3)]).expect("accepted");
        let manifest = b.finish(Timestamp { secs: 0, nanos: 0 }).expect("finish");
        assert_eq!(manifest.roots[0].entries.len(), 1);
    }

    #[test]
    fn several_roots_are_kept_apart() {
        let mut b = ManifestBuilder::new(REPO_ID, config());
        let first = b.add_root(Path::new("/one"));
        let second = b.add_root(Path::new("/two"));
        for root in [first, second] {
            b.add_entry(root, &entry("", EntryKind::Directory, 0), vec![]).expect("accepted");
            b.add_entry(root, &entry("x", EntryKind::File, 1), vec![chunk(4, 1)]).expect("accepted");
        }
        let manifest = b.finish(Timestamp { secs: 0, nanos: 0 }).expect("finish");
        assert_eq!(manifest.roots.len(), 2);
        assert_eq!(manifest.roots[1].source, b"/two");
        assert_eq!(manifest.stats.files, 2);
    }

    #[test]
    fn out_of_order_or_inconsistent_input_is_refused() {
        let accepts = |steps: Vec<(Entry, Vec<ChunkRef>)>| -> Result<()> {
            let (mut b, root) = builder();
            for (e, chunks) in steps {
                b.add_entry(root, &e, chunks)?;
            }
            b.finish(Timestamp { secs: 0, nanos: 0 }).map(|_| ())
        };
        let dir = |p: &str| (entry(p, EntryKind::Directory, 0), vec![]);
        let file = |p: &str| (entry(p, EntryKind::File, 1), vec![chunk(1, 1)]);

        assert!(accepts(vec![dir(""), file("f")]).is_ok());
        assert!(accepts(vec![file("f")]).is_err(), "the root entry must come first");
        assert!(accepts(vec![dir(""), dir("")]).is_err(), "a second root entry");
        assert!(accepts(vec![dir(""), file("d/f")]).is_err(), "child before its parent");
        assert!(accepts(vec![dir(""), file("f"), file("f/g")]).is_err(), "a file as a parent");
        assert!(
            accepts(vec![dir(""), (entry("f", EntryKind::File, 5), vec![])]).is_err(),
            "a non-empty file without chunks"
        );
        assert!(
            accepts(vec![dir(""), (entry("d", EntryKind::Directory, 0), vec![chunk(1, 1)])])
                .is_err(),
            "chunks on a directory"
        );
        let link = (entry("l", EntryKind::HardLink { target: PathBuf::from("f") }, 1), vec![]);
        assert!(accepts(vec![dir(""), link]).is_err(), "a hard link without its target");
        assert!(
            accepts(vec![dir(""), file("a"), file("a")]).is_err(),
            "the same path twice is a duplicate name"
        );
    }

    #[test]
    fn unknown_root_indexes_are_refused() {
        let (mut b, _) = builder();
        let e = entry("", EntryKind::Directory, 0);
        assert!(b.add_entry(5, &e, vec![]).is_err());
        let skipped = Skipped { path: PathBuf::from("x"), reason: SkipReason::Vanished };
        assert!(b.add_skipped(5, &skipped).is_err());
    }

    #[test]
    fn skip_reasons_are_translated() {
        let (mut b, root) = builder();
        b.add_entry(root, &entry("", EntryKind::Directory, 0), vec![]).expect("accepted");
        let reasons = [
            SkipReason::OtherFilesystem,
            SkipReason::SpecialFile(EntryKind::Socket),
            SkipReason::Unreadable {
                operation: "read directory",
                kind: std::io::ErrorKind::PermissionDenied,
                message: "denied".to_owned(),
            },
        ];
        for reason in reasons {
            b.add_skipped(root, &Skipped { path: PathBuf::from("p"), reason }).expect("accepted");
        }
        let manifest = b.finish(Timestamp { secs: 0, nanos: 0 }).expect("finish");
        assert_eq!(manifest.skipped[0].reason, SkippedReason::OtherFilesystem);
        assert_eq!(
            manifest.skipped[1].reason,
            SkippedReason::SpecialFile { kind: "Socket".to_owned() }
        );
        assert_eq!(
            manifest.skipped[2].reason,
            SkippedReason::Unreadable { detail: "read directory: denied".to_owned() }
        );
    }

    #[test]
    fn a_manifest_without_roots_cannot_be_finished() {
        let b = ManifestBuilder::new(REPO_ID, config());
        assert!(b.finish(Timestamp { secs: 0, nanos: 0 }).is_err());
    }
}
