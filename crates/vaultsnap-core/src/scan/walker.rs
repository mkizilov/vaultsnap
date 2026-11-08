//! The scanner: a streaming, deterministic, policy-driven directory walk.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::entry::{Entry, EntryKind, Metadata, ScanItem, ScanStats, SkipReason, Skipped};
use super::filter::Filter;
use super::options::{
    ErrorPolicy, HardLinkPolicy, ScanOptions, SpecialFilePolicy, SymlinkPolicy,
};
use super::reader::{self, FileReader};
use crate::error::{Error, Result};

/// A configured scan of one root (a directory, or a single file).
///
/// The root is resolved once, with symbolic links followed (it is the path the user asked for),
/// and every entry is reported relative to it. [`Scanner::scan`] walks the tree lazily;
/// [`Scanner::open`] reads a file's content as a stream.
#[derive(Debug)]
pub struct Scanner {
    root: PathBuf,
    root_device: Option<u64>,
    options: ScanOptions,
    filter: Filter,
}

impl Scanner {
    /// Prepares a scan of `root` with `options`. Fails if the root cannot be resolved or a
    /// pattern is invalid. Does not walk anything yet.
    pub fn new(root: impl AsRef<Path>, options: ScanOptions) -> Result<Self> {
        let given = root.as_ref();
        let root =
            fs::canonicalize(given).map_err(|err| Error::io("resolve the scan root", given, err))?;
        let meta = fs::symlink_metadata(&root)
            .map_err(|err| Error::io("inspect the scan root", &root, err))?;
        let filter = Filter::new(&options.includes, &options.excludes)?;
        Ok(Self {
            root_device: Metadata::from_fs(&meta).device,
            root,
            options,
            filter,
        })
    }

    /// The resolved root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The options in force.
    pub fn options(&self) -> &ScanOptions {
        &self.options
    }

    /// Starts a walk. The stream is deterministic: depth first, a directory before its contents,
    /// and the names within a directory in ascending byte order (Unix) or UTF-16 code unit order
    /// (Windows). Memory use is the names of the directories currently being walked, not the
    /// size of the tree, plus one small record per multiply-linked file.
    pub fn scan(&self) -> Scan<'_> {
        Scan {
            scanner: self,
            state: State::Start,
            frames: Vec::new(),
            links: HashMap::new(),
            stats: ScanStats::default(),
        }
    }

    /// Opens a regular-file entry produced by this scanner for streaming reads.
    pub fn open(&self, entry: &Entry) -> Result<FileReader> {
        reader::open(self, entry)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Start,
    Running,
    Finished,
}

/// A directory being walked.
struct Frame {
    rel: PathBuf,
    /// The directory's fully resolved path, used to recognize loops.
    canonical: PathBuf,
    names: std::vec::IntoIter<OsString>,
}

/// What a path is, after any symbolic link has been dealt with.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    Directory,
    File,
    Symlink,
    Special(EntryKind),
}

impl Node {
    fn of(file_type: &fs::FileType) -> Self {
        if file_type.is_dir() {
            Self::Directory
        } else if file_type.is_file() {
            Self::File
        } else if file_type.is_symlink() {
            Self::Symlink
        } else {
            Self::Special(special_kind(file_type))
        }
    }
}

#[cfg(unix)]
fn special_kind(file_type: &fs::FileType) -> EntryKind {
    use std::os::unix::fs::FileTypeExt;

    if file_type.is_fifo() {
        EntryKind::Fifo
    } else if file_type.is_socket() {
        EntryKind::Socket
    } else if file_type.is_char_device() {
        EntryKind::CharDevice
    } else if file_type.is_block_device() {
        EntryKind::BlockDevice
    } else {
        EntryKind::Other
    }
}

#[cfg(not(unix))]
fn special_kind(_file_type: &fs::FileType) -> EntryKind {
    EntryKind::Other
}

/// The lazy stream of a walk; see [`Scanner::scan`].
///
/// Items are `Result<ScanItem>`: an `Err` is only produced under [`ErrorPolicy::Fail`] and ends
/// the stream.
pub struct Scan<'a> {
    scanner: &'a Scanner,
    state: State,
    frames: Vec<Frame>,
    links: HashMap<(u64, u64), PathBuf>,
    stats: ScanStats,
}

impl Scan<'_> {
    /// Totals so far.
    pub fn stats(&self) -> &ScanStats {
        &self.stats
    }

    fn next_item(&mut self) -> Option<Result<ScanItem>> {
        loop {
            match self.state {
                State::Finished => return None,
                State::Start => {
                    self.state = State::Running;
                    let root = self.scanner.root.clone();
                    if let Some(item) = self.visit(PathBuf::new(), root) {
                        return Some(item);
                    }
                }
                State::Running => {
                    let name = match self.frames.last_mut() {
                        Some(frame) => frame.names.next(),
                        None => {
                            self.state = State::Finished;
                            return None;
                        }
                    };
                    let Some(name) = name else {
                        self.frames.pop();
                        continue;
                    };
                    let Some(frame) = self.frames.last() else {
                        continue;
                    };
                    let rel = frame.rel.join(&name);
                    let canonical = frame.canonical.join(&name);
                    if let Some(item) = self.visit(rel, canonical) {
                        return Some(item);
                    }
                }
            }
        }
    }

    /// Examines one path. `None` means it was omitted by a pattern and nothing is reported.
    /// `canonical` is the resolved path the entry would have if it is a real directory.
    fn visit(&mut self, rel: PathBuf, canonical: PathBuf) -> Option<Result<ScanItem>> {
        let scanner = self.scanner;
        let is_root = rel.as_os_str().is_empty();
        let abs = if is_root {
            scanner.root.clone()
        } else {
            scanner.root.join(&rel)
        };

        let link_meta = match fs::symlink_metadata(&abs) {
            Ok(meta) => meta,
            Err(err) => return self.problem("inspect", rel, &abs, err),
        };
        let mut meta = link_meta.clone();
        let mut node = Node::of(&link_meta.file_type());
        let mut canonical = canonical;
        let mut followed = false;

        if node == Node::Symlink && scanner.options.symlinks != SymlinkPolicy::Preserve {
            if let Some((target_meta, target_canonical)) = resolve_symlink(scanner, &abs) {
                let target_node = Node::of(&target_meta.file_type());
                let loops = target_node == Node::Directory
                    && self.frames.iter().any(|frame| frame.canonical == target_canonical);
                if !loops {
                    meta = target_meta;
                    node = target_node;
                    canonical = target_canonical;
                    followed = true;
                }
            }
        }

        let is_directory = node == Node::Directory;
        if !is_root {
            if scanner.filter.is_excluded(&rel, is_directory) {
                self.stats.excluded += 1;
                return None;
            }
            if !is_directory && !scanner.filter.is_included(&rel) {
                self.stats.not_included += 1;
                return None;
            }
        }
        let metadata = Metadata::from_fs(&meta);

        let kind = match node {
            Node::Directory => {
                let crossing = crosses_filesystem(
                    scanner.root_device,
                    metadata.device,
                    scanner.options.cross_filesystems,
                );
                if !is_root && crossing {
                    return Some(Ok(self.skip(rel, SkipReason::OtherFilesystem)));
                }
                let names = match list_directory(&abs) {
                    Ok(names) => names,
                    Err(err) => return self.problem("read directory", rel, &abs, err),
                };
                self.frames.push(Frame {
                    rel: rel.clone(),
                    canonical,
                    names: names.into_iter(),
                });
                EntryKind::Directory
            }
            Node::File => self.file_kind(&rel, &metadata),
            Node::Symlink => match fs::read_link(&abs) {
                Ok(target) => EntryKind::Symlink { target },
                Err(err) => return self.problem("read symbolic link", rel, &abs, err),
            },
            Node::Special(kind) => {
                if scanner.options.special_files == SpecialFilePolicy::Skip {
                    return Some(Ok(self.skip(rel, SkipReason::SpecialFile(kind))));
                }
                kind
            }
        };

        self.count(&kind, &metadata);
        Some(Ok(ScanItem::Entry(Entry {
            path: rel,
            kind,
            metadata,
            followed_symlink: followed,
        })))
    }

    /// `File`, or `HardLink` if this is another path to a file already reported.
    fn file_kind(&mut self, rel: &Path, metadata: &Metadata) -> EntryKind {
        if self.scanner.options.hard_links == HardLinkPolicy::Preserve {
            if let (Some(device), Some(inode), Some(links)) =
                (metadata.device, metadata.inode, metadata.links)
            {
                if links > 1 {
                    if let Some(primary) = self.links.get(&(device, inode)) {
                        return EntryKind::HardLink {
                            target: primary.clone(),
                        };
                    }
                    self.links.insert((device, inode), rel.to_path_buf());
                }
            }
        }
        EntryKind::File
    }

    fn count(&mut self, kind: &EntryKind, metadata: &Metadata) {
        let stats = &mut self.stats;
        stats.entries += 1;
        match kind {
            EntryKind::Directory => stats.directories += 1,
            EntryKind::File => {
                stats.files += 1;
                stats.bytes += metadata.size;
            }
            EntryKind::Symlink { .. } => stats.symlinks += 1,
            EntryKind::HardLink { .. } => stats.hard_links += 1,
            _ => stats.special_files += 1,
        }
    }

    fn skip(&mut self, path: PathBuf, reason: SkipReason) -> ScanItem {
        self.stats.skipped += 1;
        ScanItem::Skipped(Skipped { path, reason })
    }

    /// Applies the error policy to a failed operation. A path that vanished is never an error:
    /// it is a race with another process and is reported as skipped under every policy.
    fn problem(
        &mut self,
        operation: &'static str,
        rel: PathBuf,
        abs: &Path,
        err: io::Error,
    ) -> Option<Result<ScanItem>> {
        if err.kind() == io::ErrorKind::NotFound {
            return Some(Ok(self.skip(rel, SkipReason::Vanished)));
        }
        match self.scanner.options.on_error {
            ErrorPolicy::Skip => {
                let reason = SkipReason::Unreadable {
                    operation,
                    kind: err.kind(),
                    message: err.to_string(),
                };
                Some(Ok(self.skip(rel, reason)))
            }
            ErrorPolicy::Fail => {
                self.state = State::Finished;
                Some(Err(Error::io(operation, abs, err)))
            }
        }
    }
}

impl Iterator for Scan<'_> {
    type Item = Result<ScanItem>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_item()
    }
}

/// Where a symbolic link ultimately points, if the policy allows following it. A link that
/// cannot be resolved (dangling, looping, unreadable) or that points outside the root under
/// `FollowInsideRoot` yields `None` and is recorded as a link.
fn resolve_symlink(scanner: &Scanner, link: &Path) -> Option<(fs::Metadata, PathBuf)> {
    let canonical = fs::canonicalize(link).ok()?;
    if scanner.options.symlinks == SymlinkPolicy::FollowInsideRoot
        && !canonical.starts_with(&scanner.root)
    {
        return None;
    }
    let meta = fs::metadata(&canonical).ok()?;
    Some((meta, canonical))
}

/// Whether descending into a directory would leave the root's filesystem when that is not
/// allowed. Unknown devices (non-Unix platforms) never count as crossing.
fn crosses_filesystem(root_device: Option<u64>, device: Option<u64>, allowed: bool) -> bool {
    !allowed && matches!((root_device, device), (Some(root), Some(this)) if root != this)
}

fn list_directory(path: &Path) -> io::Result<Vec<OsString>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(path)? {
        names.push(entry?.file_name());
    }
    names.sort_unstable();
    Ok(names)
}

impl std::fmt::Debug for Scan<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scan")
            .field("root", &self.scanner.root)
            .field("state", &self.state)
            .field("depth", &self.frames.len())
            .field("stats", &self.stats)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_filesystem_boundary_decision() {
        // Stay on the root's filesystem by default.
        assert!(!crosses_filesystem(Some(1), Some(1), false));
        assert!(crosses_filesystem(Some(1), Some(2), false));
        // Crossing explicitly allowed.
        assert!(!crosses_filesystem(Some(1), Some(2), true));
        // Platforms without device ids cannot detect a boundary.
        assert!(!crosses_filesystem(None, None, false));
        assert!(!crosses_filesystem(Some(1), None, false));
        assert!(!crosses_filesystem(None, Some(2), false));
    }
}
