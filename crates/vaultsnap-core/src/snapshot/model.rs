//! The in-memory snapshot manifest and the rules that make one well formed.
//!
//! A manifest is everything a restore needs to know about one backup: when and what was backed
//! up, how, the directory trees with every entry's attributes, and for every file the ordered
//! list of chunks that make up its content. It is stored encrypted (see `docs/FORMAT.md`); this
//! module is the plaintext view, and [`Manifest::validate`] is the single definition of "well
//! formed" that is applied to every manifest before it is written and after it is read.
//!
//! Error messages deliberately name entries by index, never by file name, because names are
//! sensitive and error messages get logged.

use std::collections::HashSet;

use super::id::SnapshotId;
use crate::chunk_store::{ChunkId, MAX_CHUNK_BYTES};
use crate::chunking::ChunkerConfig;
use crate::error::{Error, Result};
use crate::scan::{
    ChangePolicy, ErrorPolicy, HardLinkPolicy, Metadata, ScanOptions, SpecialFilePolicy,
    SymlinkPolicy, Timestamp,
};
use crate::util::decode_hex;

/// Version of the manifest schema written by this build.
pub const MANIFEST_FORMAT_VERSION: u32 = 1;
/// Longest single path component accepted, in bytes.
pub const MAX_NAME_LEN: usize = 4096;
/// Longest root source path accepted, in bytes.
pub const MAX_SOURCE_LEN: usize = 1 << 16;

fn invalid<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::InvalidManifest(message.into()))
}

/// How a backup was configured: enough to understand, and later to repeat, the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupConfig {
    /// Chunk size limits used for the files' chunks.
    pub chunker: ChunkerConfig,
    /// Symbolic link policy of the scan.
    pub symlinks: SymlinkPolicy,
    /// Hard link policy of the scan.
    pub hard_links: HardLinkPolicy,
    /// Unreadable-path policy of the scan.
    pub on_error: ErrorPolicy,
    /// Special file policy of the scan.
    pub special_files: SpecialFilePolicy,
    /// Changed-file policy of the scan.
    pub changed_files: ChangePolicy,
    /// Whether other filesystems were entered.
    pub cross_filesystems: bool,
    /// Include patterns as given.
    pub includes: Vec<String>,
    /// Exclude patterns as given.
    pub excludes: Vec<String>,
}

impl BackupConfig {
    /// Captures the chunker settings and scan options of a backup.
    pub fn new(chunker: ChunkerConfig, options: &ScanOptions) -> Self {
        Self {
            chunker,
            symlinks: options.symlinks,
            hard_links: options.hard_links,
            on_error: options.on_error,
            special_files: options.special_files,
            changed_files: options.changed_files,
            cross_filesystems: options.cross_filesystems,
            includes: options.includes.clone(),
            excludes: options.excludes.clone(),
        }
    }
}

/// One chunk of a file, in file order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkRef {
    /// The chunk's id in the chunk store.
    pub id: ChunkId,
    /// The chunk's plaintext length in bytes; the lengths of a file's chunks add up to its size.
    pub len: u32,
}

/// File attributes kept in a manifest. A subset of [`Metadata`]: what a restore applies and what
/// the next backup needs to recognize an unchanged file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attributes {
    /// Size in bytes. For a regular file, the length of its content.
    pub size: u64,
    /// Last content modification.
    pub modified: Option<Timestamp>,
    /// Last access.
    pub accessed: Option<Timestamp>,
    /// Last inode change.
    pub changed: Option<Timestamp>,
    /// Creation time.
    pub created: Option<Timestamp>,
    /// Unix permission bits (`0o7777`).
    pub mode: Option<u32>,
    /// Owning user id.
    pub uid: Option<u32>,
    /// Owning group id.
    pub gid: Option<u32>,
    /// Read-only flag.
    pub readonly: bool,
    /// Device id at backup time (for change detection).
    pub device: Option<u64>,
    /// Inode number at backup time (for change detection).
    pub inode: Option<u64>,
    /// Hard link count at backup time.
    pub links: Option<u64>,
}

impl Attributes {
    /// The manifest's view of scanned metadata.
    pub fn from_scan(metadata: &Metadata) -> Self {
        Self {
            size: metadata.size,
            modified: metadata.modified,
            accessed: metadata.accessed,
            changed: metadata.changed,
            created: metadata.created,
            mode: metadata.mode,
            uid: metadata.uid,
            gid: metadata.gid,
            readonly: metadata.readonly,
            device: metadata.device,
            inode: metadata.inode,
            links: metadata.links,
        }
    }
}

/// What an entry is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeKind {
    /// A directory; its children name it as their parent.
    Directory,
    /// A regular file with its content as an ordered list of chunks.
    File {
        /// The chunks, in file order. Empty for an empty file.
        chunks: Vec<ChunkRef>,
    },
    /// A symbolic link; the target text is stored as is and never resolved.
    Symlink {
        /// The link target as raw bytes.
        target: Vec<u8>,
    },
    /// Another name for a file earlier in the same root.
    HardLink {
        /// Index (in the same root) of the entry carrying the data; always a regular file that
        /// comes before this entry.
        target: u32,
    },
    /// A named pipe.
    Fifo,
    /// A Unix domain socket.
    Socket,
    /// A character device node.
    CharDevice,
    /// A block device node.
    BlockDevice,
    /// Anything else.
    Other,
}

/// One node of a root's tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestEntry {
    /// Index of the parent directory in the same root; `None` only for the root entry (index 0).
    /// A parent always has a smaller index than its children.
    pub parent: Option<u32>,
    /// The entry's name: a single path component as raw bytes (empty for the root entry).
    pub name: Vec<u8>,
    /// What it is.
    pub kind: NodeKind,
    /// Its attributes.
    pub attributes: Attributes,
}

/// One backed-up root: a directory tree or a single file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    /// The path that was backed up, as resolved at backup time, in raw bytes.
    pub source: Vec<u8>,
    /// The tree in pre-order. Entry 0 is the root itself.
    pub entries: Vec<ManifestEntry>,
}

impl Root {
    /// Counts of this root's entries by kind and the total size of its regular files. The
    /// `skipped` count is always 0 here: skipped paths are recorded on the manifest.
    pub fn stats(&self) -> ManifestStats {
        let mut stats = ManifestStats::default();
        for entry in &self.entries {
            match &entry.kind {
                NodeKind::Directory => stats.directories += 1,
                NodeKind::File { .. } => {
                    stats.files += 1;
                    stats.bytes = stats.bytes.saturating_add(entry.attributes.size);
                }
                NodeKind::Symlink { .. } => stats.symlinks += 1,
                NodeKind::HardLink { .. } => stats.hard_links += 1,
                _ => stats.special_files += 1,
            }
        }
        stats
    }

    /// The path of entry `index` relative to the root: names joined with `/`, empty for the
    /// root itself. Names never contain `/`, so the result is unambiguous.
    pub fn relative_path(&self, index: usize) -> Vec<u8> {
        let mut parts: Vec<&[u8]> = Vec::new();
        let mut current = Some(index);
        // Bounded so that even an unvalidated manifest cannot loop.
        for _ in 0..=self.entries.len() {
            let Some(entry) = current.and_then(|i| self.entries.get(i)) else {
                break;
            };
            if entry.parent.is_some() {
                parts.push(&entry.name);
            }
            current = entry.parent.and_then(|parent| usize::try_from(parent).ok());
        }
        parts.reverse();
        parts.join(&b'/')
    }

    fn validate(&self, root: usize) -> Result<()> {
        if self.source.is_empty() || self.source.len() > MAX_SOURCE_LEN || self.source.contains(&0)
        {
            return invalid(format!("root {root} has an invalid source path"));
        }
        let Some(first) = self.entries.first() else {
            return invalid(format!("root {root} has no entries"));
        };
        if u32::try_from(self.entries.len()).is_err() {
            return invalid(format!("root {root} has too many entries"));
        }
        if first.parent.is_some() || !first.name.is_empty() {
            return invalid(format!(
                "root {root}: entry 0 must be the unnamed root entry"
            ));
        }
        if !matches!(first.kind, NodeKind::Directory) && self.entries.len() != 1 {
            return invalid(format!(
                "root {root} is not a directory but has more entries"
            ));
        }

        let mut names: HashSet<(u32, &[u8])> = HashSet::new();
        for (index, entry) in self.entries.iter().enumerate() {
            let context = |what: &str| format!("root {root}, entry {index}: {what}");
            check_attributes(&entry.attributes)
                .map_err(|what| Error::InvalidManifest(context(what)))?;

            if index > 0 {
                let Some(parent) = entry.parent else {
                    return invalid(context("only entry 0 may lack a parent"));
                };
                let parent_index = usize::try_from(parent).unwrap_or(usize::MAX);
                if parent_index >= index {
                    return invalid(context("a parent must come before its children"));
                }
                if !matches!(self.entries[parent_index].kind, NodeKind::Directory) {
                    return invalid(context("the parent is not a directory"));
                }
                if !is_valid_name(&entry.name) {
                    return invalid(context("the name is not a valid path component"));
                }
                if !names.insert((parent, entry.name.as_slice())) {
                    return invalid(context("the name is used twice in one directory"));
                }
            }

            match &entry.kind {
                NodeKind::File { chunks } => {
                    let mut total = 0_u64;
                    for chunk in chunks {
                        let len = usize::try_from(chunk.len).unwrap_or(usize::MAX);
                        if len == 0 || len > MAX_CHUNK_BYTES {
                            return invalid(context("a chunk has an impossible length"));
                        }
                        total = total.saturating_add(u64::from(chunk.len));
                    }
                    if total != entry.attributes.size {
                        return invalid(context("the chunk lengths do not add up to the size"));
                    }
                }
                NodeKind::Symlink { target } => {
                    if target.is_empty() || target.contains(&0) {
                        return invalid(context("the symbolic link target is invalid"));
                    }
                }
                NodeKind::HardLink { target } => {
                    let target_index = usize::try_from(*target).unwrap_or(usize::MAX);
                    let valid = target_index < index
                        && matches!(self.entries[target_index].kind, NodeKind::File { .. });
                    if !valid {
                        return invalid(context(
                            "a hard link must name an earlier regular file in its root",
                        ));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// A path component is safe to create on restore if it is one non-empty component of bounded
/// length that cannot climb or split a path.
pub fn is_valid_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name != b"."
        && name != b".."
        && !name.iter().any(|&byte| byte == b'/' || byte == 0)
}

fn check_attributes(attributes: &Attributes) -> std::result::Result<(), &'static str> {
    let times = [
        attributes.modified,
        attributes.accessed,
        attributes.changed,
        attributes.created,
    ];
    if times
        .iter()
        .flatten()
        .any(|time| time.nanos >= 1_000_000_000)
    {
        return Err("a timestamp has out-of-range nanoseconds");
    }
    Ok(())
}

/// Why something was left out of the backup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkippedReason {
    /// It disappeared during the scan.
    Vanished,
    /// It could not be read; the detail is the operation and the system's message.
    Unreadable {
        /// What failed and why.
        detail: String,
    },
    /// A mount point that was not entered.
    OtherFilesystem,
    /// A special file excluded by policy.
    SpecialFile {
        /// Which kind of special file.
        kind: String,
    },
}

/// A path that is not in the snapshot, with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedRecord {
    /// Index of the root it belongs to.
    pub root: u32,
    /// Its path relative to that root, as raw bytes (`/`-joined components as scanned).
    pub path: Vec<u8>,
    /// Why it is missing.
    pub reason: SkippedReason,
}

/// Totals over a manifest; stored so a listing need not walk the entries, and checked against
/// the entries on every load.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ManifestStats {
    /// Directory entries.
    pub directories: u64,
    /// Regular file entries.
    pub files: u64,
    /// Symbolic link entries.
    pub symlinks: u64,
    /// Hard link entries.
    pub hard_links: u64,
    /// FIFOs, sockets, devices, and other special entries.
    pub special_files: u64,
    /// Total size of the regular files, in bytes.
    pub bytes: u64,
    /// Number of skipped paths. A snapshot with skipped paths is a partial backup.
    pub skipped: u64,
}

/// A complete snapshot manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// Schema version of this manifest; see [`MANIFEST_FORMAT_VERSION`].
    pub format_version: u32,
    /// The snapshot's unique id (also its file name and authenticated with it).
    pub id: SnapshotId,
    /// The repository it belongs to.
    pub repository_id: String,
    /// When the snapshot was created.
    pub created: Timestamp,
    /// The host it was taken on, as supplied by the caller (may be empty).
    pub host: String,
    /// User-supplied labels.
    pub tags: Vec<String>,
    /// The program and version that wrote it.
    pub program: String,
    /// How the backup was configured.
    pub config: BackupConfig,
    /// The backed-up roots with their trees.
    pub roots: Vec<Root>,
    /// Everything that was found but not backed up.
    pub skipped: Vec<SkippedRecord>,
    /// Totals; must match the entries.
    pub stats: ManifestStats,
}

impl Manifest {
    /// Computes the totals from the entries and skipped records.
    pub fn compute_stats(&self) -> ManifestStats {
        let mut total = ManifestStats::default();
        for stats in self.roots.iter().map(Root::stats) {
            total.directories += stats.directories;
            total.files += stats.files;
            total.symlinks += stats.symlinks;
            total.hard_links += stats.hard_links;
            total.special_files += stats.special_files;
            total.bytes = total.bytes.saturating_add(stats.bytes);
        }
        total.skipped = self.skipped.len() as u64;
        total
    }

    /// Checks every rule of a well-formed manifest:
    ///
    /// * the schema version is supported and the repository id is well formed;
    /// * there is at least one root, each with a source path and an unnamed root entry first;
    /// * every other entry has a parent that is an earlier directory and a name that is a single
    ///   safe path component (no `/`, no NUL, not `.` or `..`, not empty), unique among its
    ///   siblings; this makes path traversal and cycles impossible by construction;
    /// * file chunk lengths are positive, within the chunk limit, and sum to the file size;
    /// * hard links name an earlier regular file; symbolic link targets are non-empty;
    /// * timestamps are in range, skipped records name an existing root, and the stored totals
    ///   equal the totals of the entries.
    pub fn validate(&self) -> Result<()> {
        if self.format_version != MANIFEST_FORMAT_VERSION {
            return Err(Error::UnsupportedManifestVersion {
                found: self.format_version,
                supported: MANIFEST_FORMAT_VERSION,
            });
        }
        let id_ok = decode_hex(&self.repository_id).is_some_and(|bytes| bytes.len() == 16);
        if !id_ok {
            return invalid("the repository id must be 32 lowercase hexadecimal characters");
        }
        if self.created.nanos >= 1_000_000_000 {
            return invalid("the creation time has out-of-range nanoseconds");
        }
        if self.roots.is_empty() {
            return invalid("a snapshot needs at least one root");
        }
        for (index, root) in self.roots.iter().enumerate() {
            root.validate(index)?;
        }
        let root_exists = |root: u32| usize::try_from(root).is_ok_and(|i| i < self.roots.len());
        if self.skipped.iter().any(|record| !root_exists(record.root)) {
            return invalid("a skipped record names a root that does not exist");
        }
        if self.stats != self.compute_stats() {
            return invalid("the stored totals do not match the entries");
        }
        Ok(())
    }

    /// A small description for listings.
    pub fn summary(&self) -> SnapshotSummary {
        SnapshotSummary {
            id: self.id.clone(),
            format_version: self.format_version,
            created: self.created,
            host: self.host.clone(),
            tags: self.tags.clone(),
            program: self.program.clone(),
            roots: self.roots.iter().map(|root| root.source.clone()).collect(),
            stats: self.stats.clone(),
        }
    }
}

/// What a snapshot listing shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotSummary {
    /// The snapshot id.
    pub id: SnapshotId,
    /// Manifest schema version.
    pub format_version: u32,
    /// When it was taken.
    pub created: Timestamp,
    /// Host name recorded by the caller.
    pub host: String,
    /// Labels.
    pub tags: Vec<String>,
    /// Program and version that wrote it.
    pub program: String,
    /// The backed-up root paths, as raw bytes.
    pub roots: Vec<Vec<u8>>,
    /// Totals. `skipped > 0` marks a partial backup.
    pub stats: ManifestStats,
}

#[cfg(test)]
pub(crate) mod testing {
    //! Hand-built manifests for tests.

    use super::*;

    pub(crate) const REPO_ID: &str = "0123456789abcdef0123456789abcdef";

    pub(crate) fn attributes(size: u64) -> Attributes {
        Attributes {
            size,
            modified: Some(Timestamp {
                secs: 1_700_000_000,
                nanos: 123_456_789,
            }),
            accessed: None,
            changed: Some(Timestamp {
                secs: 1_700_000_001,
                nanos: 0,
            }),
            created: None,
            mode: Some(0o644),
            uid: Some(1000),
            gid: Some(1000),
            readonly: false,
            device: Some(42),
            inode: Some(4242),
            links: Some(1),
        }
    }

    pub(crate) fn bare_attributes() -> Attributes {
        Attributes {
            size: 0,
            modified: None,
            accessed: None,
            changed: None,
            created: None,
            mode: None,
            uid: None,
            gid: None,
            readonly: false,
            device: None,
            inode: None,
            links: None,
        }
    }

    pub(crate) fn chunk(seed: u8, len: u32) -> ChunkRef {
        ChunkRef {
            id: ChunkId::from_bytes([seed; 32]),
            len,
        }
    }

    pub(crate) fn dir(parent: Option<u32>, name: &[u8]) -> ManifestEntry {
        ManifestEntry {
            parent,
            name: name.to_vec(),
            kind: NodeKind::Directory,
            attributes: attributes(0),
        }
    }

    pub(crate) fn file(parent: u32, name: &[u8], chunks: Vec<ChunkRef>) -> ManifestEntry {
        let size = chunks.iter().map(|c| u64::from(c.len)).sum();
        ManifestEntry {
            parent: Some(parent),
            name: name.to_vec(),
            kind: NodeKind::File { chunks },
            attributes: attributes(size),
        }
    }

    pub(crate) fn config() -> BackupConfig {
        BackupConfig::new(ChunkerConfig::default(), &ScanOptions::default())
    }

    /// A manifest that exercises every kind of entry.
    pub(crate) fn rich() -> Manifest {
        let entries = vec![
            dir(None, b""),
            dir(Some(0), b"docs"),
            file(1, b"report.txt", vec![chunk(1, 1000), chunk(2, 500)]),
            file(1, b"empty", Vec::new()),
            ManifestEntry {
                parent: Some(1),
                name: b"alias".to_vec(),
                kind: NodeKind::HardLink { target: 2 },
                attributes: attributes(1500),
            },
            ManifestEntry {
                parent: Some(0),
                name: b"link".to_vec(),
                kind: NodeKind::Symlink {
                    target: b"docs/report.txt".to_vec(),
                },
                attributes: attributes(15),
            },
            ManifestEntry {
                parent: Some(0),
                name: b"bad-\xff\xfe-name".to_vec(),
                kind: NodeKind::File {
                    chunks: vec![chunk(1, 1000)],
                },
                attributes: attributes(1000),
            },
            ManifestEntry {
                parent: Some(0),
                name: "unicode-日本語".as_bytes().to_vec(),
                kind: NodeKind::Fifo,
                attributes: bare_attributes(),
            },
            ManifestEntry {
                parent: Some(0),
                name: b"sock".to_vec(),
                kind: NodeKind::Socket,
                attributes: bare_attributes(),
            },
            ManifestEntry {
                parent: Some(0),
                name: b"tty".to_vec(),
                kind: NodeKind::CharDevice,
                attributes: bare_attributes(),
            },
            ManifestEntry {
                parent: Some(0),
                name: b"disk".to_vec(),
                kind: NodeKind::BlockDevice,
                attributes: bare_attributes(),
            },
            ManifestEntry {
                parent: Some(0),
                name: b"other".to_vec(),
                kind: NodeKind::Other,
                attributes: bare_attributes(),
            },
        ];
        let roots = vec![
            Root {
                source: b"/home/me/Documents".to_vec(),
                entries,
            },
            Root {
                source: b"/home/me/notes.txt".to_vec(),
                entries: vec![ManifestEntry {
                    parent: None,
                    name: Vec::new(),
                    kind: NodeKind::File {
                        chunks: vec![chunk(9, 77)],
                    },
                    attributes: attributes(77),
                }],
            },
        ];
        let skipped = vec![
            SkippedRecord {
                root: 0,
                path: b"locked".to_vec(),
                reason: SkippedReason::Vanished,
            },
            SkippedRecord {
                root: 0,
                path: b"secret".to_vec(),
                reason: SkippedReason::Unreadable {
                    detail: "read directory: denied".to_owned(),
                },
            },
            SkippedRecord {
                root: 1,
                path: Vec::new(),
                reason: SkippedReason::OtherFilesystem,
            },
            SkippedRecord {
                root: 0,
                path: b"pipe2".to_vec(),
                reason: SkippedReason::SpecialFile {
                    kind: "Fifo".to_owned(),
                },
            },
        ];
        let mut manifest = Manifest {
            format_version: MANIFEST_FORMAT_VERSION,
            id: "ab".repeat(32).parse().expect("valid id"),
            repository_id: REPO_ID.to_owned(),
            created: Timestamp {
                secs: 1_750_000_000,
                nanos: 5,
            },
            host: "workstation".to_owned(),
            tags: vec!["laptop".to_owned(), "weekly".to_owned()],
            program: "vaultsnap 0.1.0".to_owned(),
            config: BackupConfig {
                includes: vec!["*.txt".to_owned()],
                excludes: vec!["target/".to_owned(), "*.tmp".to_owned()],
                ..config()
            },
            roots,
            skipped,
            stats: ManifestStats::default(),
        };
        manifest.stats = manifest.compute_stats();
        manifest
    }

    /// The smallest valid manifest: one empty directory.
    pub(crate) fn minimal() -> Manifest {
        let mut manifest = Manifest {
            format_version: MANIFEST_FORMAT_VERSION,
            id: "cd".repeat(32).parse().expect("valid id"),
            repository_id: REPO_ID.to_owned(),
            created: Timestamp { secs: 0, nanos: 0 },
            host: String::new(),
            tags: Vec::new(),
            program: String::new(),
            config: config(),
            roots: vec![Root {
                source: b"/r".to_vec(),
                entries: vec![ManifestEntry {
                    parent: None,
                    name: Vec::new(),
                    kind: NodeKind::Directory,
                    attributes: bare_attributes(),
                }],
            }],
            skipped: Vec::new(),
            stats: ManifestStats::default(),
        };
        manifest.stats = manifest.compute_stats();
        manifest
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    fn rejects(mut manifest: Manifest, change: impl FnOnce(&mut Manifest), why: &str) {
        change(&mut manifest);
        manifest.stats = manifest.compute_stats();
        let result = manifest.validate();
        assert!(
            matches!(result, Err(Error::InvalidManifest(_))),
            "{why}: {result:?}"
        );
    }

    #[test]
    fn the_hand_built_manifests_are_valid() {
        rich().validate().expect("rich manifest is valid");
        minimal().validate().expect("minimal manifest is valid");
    }

    #[test]
    fn statistics_are_computed_from_the_entries() {
        let stats = rich().stats;
        assert_eq!(stats.directories, 2);
        assert_eq!(stats.files, 4);
        assert_eq!(stats.symlinks, 1);
        assert_eq!(stats.hard_links, 1);
        assert_eq!(stats.special_files, 5);
        assert_eq!(stats.bytes, 1500 + 1000 + 77);
        assert_eq!(stats.skipped, 4);
    }

    #[test]
    fn each_root_has_its_own_statistics_that_add_up_to_the_totals() {
        let manifest = rich();
        let documents = manifest.roots[0].stats();
        assert_eq!(
            (documents.directories, documents.files, documents.bytes),
            (2, 3, 2500)
        );
        assert_eq!(
            (
                documents.symlinks,
                documents.hard_links,
                documents.special_files
            ),
            (1, 1, 5)
        );
        assert_eq!(documents.skipped, 0, "skipped paths belong to the manifest");
        let notes = manifest.roots[1].stats();
        assert_eq!((notes.directories, notes.files, notes.bytes), (0, 1, 77));
        assert_eq!(documents.bytes + notes.bytes, manifest.stats.bytes);
    }

    #[test]
    fn stored_totals_must_match() {
        let mut manifest = rich();
        manifest.stats.files += 1;
        assert!(matches!(
            manifest.validate(),
            Err(Error::InvalidManifest(_))
        ));
    }

    #[test]
    fn relative_paths_join_names_from_the_root() {
        let manifest = rich();
        let root = &manifest.roots[0];
        assert_eq!(root.relative_path(0), b"");
        assert_eq!(root.relative_path(1), b"docs");
        assert_eq!(root.relative_path(2), b"docs/report.txt");
        assert_eq!(root.relative_path(6), b"bad-\xff\xfe-name");
        assert_eq!(root.relative_path(999), b"", "out of range yields nothing");
        assert_eq!(manifest.roots[1].relative_path(0), b"");
    }

    #[test]
    fn unsupported_schema_versions_are_reported_as_such() {
        let mut manifest = minimal();
        manifest.format_version = 2;
        assert!(matches!(
            manifest.validate(),
            Err(Error::UnsupportedManifestVersion {
                found: 2,
                supported: 1
            })
        ));
    }

    #[test]
    fn structural_rules_are_enforced() {
        let base = rich();
        rejects(base.clone(), |m| m.roots.clear(), "no roots");
        rejects(base.clone(), |m| m.roots[0].entries.clear(), "no entries");
        rejects(base.clone(), |m| m.roots[0].source.clear(), "empty source");
        rejects(
            base.clone(),
            |m| m.roots[0].source = b"a\0b".to_vec(),
            "NUL in source",
        );
        rejects(
            base.clone(),
            |m| m.repository_id = "XYZ".to_owned(),
            "bad repository id",
        );
        rejects(
            base.clone(),
            |m| m.created.nanos = 1_000_000_000,
            "bad creation nanos",
        );
        rejects(
            base.clone(),
            |m| {
                m.roots[0].entries[2].attributes.modified = Some(Timestamp {
                    secs: 0,
                    nanos: 2_000_000_000,
                })
            },
            "bad entry nanos",
        );
        rejects(
            base.clone(),
            |m| m.roots[0].entries[0].name = b"x".to_vec(),
            "named root entry",
        );
        rejects(
            base.clone(),
            |m| m.roots[0].entries[0].parent = Some(0),
            "root with parent",
        );
        rejects(
            base.clone(),
            |m| m.roots[0].entries[1].parent = None,
            "second root entry",
        );
        rejects(
            base.clone(),
            |m| m.roots[0].entries[2].parent = Some(2),
            "own parent",
        );
        rejects(
            base.clone(),
            |m| m.roots[0].entries[2].parent = Some(9),
            "parent after child",
        );
        rejects(
            base.clone(),
            |m| m.roots[0].entries[4].parent = Some(2),
            "file as parent",
        );
        rejects(
            base.clone(),
            |m| m.roots[0].entries[2].parent = Some(500),
            "parent out of range",
        );
        rejects(
            base.clone(),
            |m| m.skipped[0].root = 7,
            "skipped record for a missing root",
        );
        rejects(
            base,
            |m| m.roots[1].entries.push(dir(Some(0), b"below-a-file")),
            "entries below a file root",
        );
    }

    #[test]
    fn names_that_could_escape_the_restore_directory_are_rejected() {
        let hostile: [&[u8]; 9] = [
            b"",
            b".",
            b"..",
            b"a/b",
            b"/etc",
            b"x/",
            b"nul\0byte",
            b"../../etc/passwd",
            b"a\0",
        ];
        for name in hostile {
            let mut manifest = rich();
            manifest.roots[0].entries[2].name = name.to_vec();
            let result = manifest.validate();
            assert!(
                matches!(result, Err(Error::InvalidManifest(_))),
                "{name:?}: {result:?}"
            );
        }
        let too_long = [b'x'; MAX_NAME_LEN + 1];
        assert!(!is_valid_name(&too_long));
        assert!(is_valid_name(&too_long[..MAX_NAME_LEN]));
        assert!(is_valid_name(b"..."));
        assert!(is_valid_name(b".hidden"));
        assert!(is_valid_name("日本語".as_bytes()));
    }

    #[test]
    fn duplicate_sibling_names_are_rejected_but_equal_names_elsewhere_are_fine() {
        let mut manifest = rich();
        manifest.roots[0].entries[3].name = b"report.txt".to_vec();
        assert!(matches!(
            manifest.validate(),
            Err(Error::InvalidManifest(_))
        ));

        let mut ok = rich();
        let nested = dir(Some(1), b"link");
        ok.roots[0].entries.push(nested);
        ok.stats = ok.compute_stats();
        ok.validate()
            .expect("the same name in a different directory is fine");
    }

    #[test]
    fn file_chunk_rules_are_enforced() {
        let base = rich();
        let set = |chunks: Vec<ChunkRef>, size: u64| {
            let mut manifest = base.clone();
            manifest.roots[0].entries[2].kind = NodeKind::File { chunks };
            manifest.roots[0].entries[2].attributes.size = size;
            manifest.stats = manifest.compute_stats();
            manifest.validate()
        };
        assert!(set(vec![chunk(1, 10), chunk(2, 20)], 30).is_ok());
        assert!(
            set(vec![chunk(1, 10)], 11).is_err(),
            "sum differs from the size"
        );
        assert!(
            set(vec![chunk(1, 10), chunk(2, 0)], 10).is_err(),
            "zero-length chunk"
        );
        assert!(
            set(vec![chunk(1, u32::MAX)], u64::from(u32::MAX)).is_err(),
            "oversized chunk"
        );
        assert!(set(Vec::new(), 5).is_err(), "no chunks but a size");
        assert!(set(Vec::new(), 0).is_ok(), "empty file");
    }

    #[test]
    fn hard_link_and_symlink_rules_are_enforced() {
        let with_link_target = |target: u32| {
            let mut manifest = rich();
            manifest.roots[0].entries[4].kind = NodeKind::HardLink { target };
            manifest.validate()
        };
        assert!(with_link_target(2).is_ok());
        assert!(with_link_target(4).is_err(), "link to itself");
        assert!(with_link_target(9).is_err(), "link to a later entry");
        assert!(with_link_target(1).is_err(), "link to a directory");
        assert!(
            with_link_target(5).is_err(),
            "link to a symlink (and a later entry)"
        );
        assert!(with_link_target(77).is_err(), "link out of range");

        for target in [Vec::new(), b"a\0b".to_vec()] {
            let mut manifest = rich();
            manifest.roots[0].entries[5].kind = NodeKind::Symlink { target };
            assert!(manifest.validate().is_err());
        }
        let mut dangling_is_fine = rich();
        dangling_is_fine.roots[0].entries[5].kind = NodeKind::Symlink {
            target: b"../../outside/../x".to_vec(),
        };
        dangling_is_fine
            .validate()
            .expect("link targets are data and are never followed");
    }

    #[test]
    fn error_messages_never_contain_file_names() {
        let mut manifest = rich();
        manifest.roots[0].entries[2].name = b"SECRET-NAME/with-slash".to_vec();
        let message = manifest.validate().expect_err("invalid").to_string();
        assert!(!message.contains("SECRET"), "{message}");
    }

    #[test]
    fn summary_reflects_the_manifest() {
        let manifest = rich();
        let summary = manifest.summary();
        assert_eq!(summary.id, manifest.id);
        assert_eq!(summary.host, "workstation");
        assert_eq!(
            summary.roots,
            vec![
                b"/home/me/Documents".to_vec(),
                b"/home/me/notes.txt".to_vec()
            ]
        );
        assert_eq!(summary.stats, manifest.stats);
    }
}
