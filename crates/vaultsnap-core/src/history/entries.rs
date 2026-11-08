//! The entries of a snapshot as full paths, in the order the snapshot stores them.
//!
//! A snapshot is a set of roots, each a tree of single-component names. The *logical path* of an
//! entry is its root's source path followed by the names down to it, joined with `/`, which is the
//! absolute path it had on the machine that was backed up. All paths are raw bytes: a file name is
//! not necessarily valid text, and callers decide how to show one.

use std::path::Path;

use crate::snapshot::{os_bytes, Manifest, ManifestEntry, NodeKind, Root, SkippedRecord};

/// What an entry is, without the details that belong to each kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryType {
    /// A directory.
    Directory,
    /// A regular file.
    File,
    /// A symbolic link.
    Symlink,
    /// Another name for a regular file of the same root.
    HardLink,
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

impl EntryType {
    /// The type of a manifest node.
    pub fn of(kind: &NodeKind) -> Self {
        match kind {
            NodeKind::Directory => Self::Directory,
            NodeKind::File { .. } => Self::File,
            NodeKind::Symlink { .. } => Self::Symlink,
            NodeKind::HardLink { .. } => Self::HardLink,
            NodeKind::Fifo => Self::Fifo,
            NodeKind::Socket => Self::Socket,
            NodeKind::CharDevice => Self::CharDevice,
            NodeKind::BlockDevice => Self::BlockDevice,
            NodeKind::Other => Self::Other,
        }
    }

    /// The name used in machine-readable output. These names are part of the output contract and
    /// never change.
    pub fn name(self) -> &'static str {
        match self {
            Self::Directory => "directory",
            Self::File => "file",
            Self::Symlink => "symlink",
            Self::HardLink => "hardlink",
            Self::Fifo => "fifo",
            Self::Socket => "socket",
            Self::CharDevice => "chardev",
            Self::BlockDevice => "blockdev",
            Self::Other => "other",
        }
    }
}

/// The size of the content of an entry, for the entries that have content: regular files and
/// hard links. Directories, links, and special files report none, because the size the file
/// system gives them says nothing about what was backed up.
pub fn content_size(entry: &ManifestEntry) -> Option<u64> {
    match entry.kind {
        NodeKind::File { .. } | NodeKind::HardLink { .. } => Some(entry.attributes.size),
        _ => None,
    }
}

/// `source` followed by `relative`, joined with exactly one `/`. A source that is itself `/`, or
/// that ends in `/`, does not get a second one, and an empty `relative` (the root itself) gives
/// `source` unchanged.
pub(crate) fn join_path(source: &[u8], relative: &[u8]) -> Vec<u8> {
    let mut path = source.to_vec();
    if !relative.is_empty() {
        if path.last() != Some(&b'/') {
            path.push(b'/');
        }
        path.extend_from_slice(relative);
    }
    path
}

/// The logical path of entry `index` of `root`.
pub(crate) fn logical_path(root: &Root, index: usize) -> Vec<u8> {
    join_path(&root.source, &root.relative_path(index))
}

/// The logical path of a path the backup left out, in the same form as the paths of entries.
pub fn skipped_path(manifest: &Manifest, record: &SkippedRecord) -> Vec<u8> {
    let root = usize::try_from(record.root)
        .ok()
        .and_then(|index| manifest.roots.get(index));
    match root {
        Some(root) => join_path(&root.source, &record.path),
        None => record.path.clone(),
    }
}

/// One entry of a snapshot with its logical path.
#[derive(Debug, Clone)]
pub struct ListedEntry<'a> {
    /// Index of the root it belongs to.
    pub root: usize,
    /// Its index inside that root.
    pub index: usize,
    /// The logical path (raw bytes).
    pub path: Vec<u8>,
    /// The manifest entry.
    pub entry: &'a ManifestEntry,
    /// For a hard link, the logical path of the file that carries the data.
    pub link_to: Option<Vec<u8>>,
}

impl ListedEntry<'_> {
    /// The type of the entry.
    pub fn entry_type(&self) -> EntryType {
        EntryType::of(&self.entry.kind)
    }

    /// The size of the entry's content, if it has any (see [`content_size`]).
    pub fn size(&self) -> Option<u64> {
        content_size(self.entry)
    }
}

/// A path filter without trailing slashes, except that `/` stays `/`.
fn normalized_prefix(path: &[u8]) -> Vec<u8> {
    let mut end = path.len();
    while end > 1 && path[end - 1] == b'/' {
        end -= 1;
    }
    path[..end].to_vec()
}

/// Whether `path` is `prefix` itself or lies below it. Whole components only: `/a/b` is not
/// below `/a/bc`.
fn is_under(path: &[u8], prefix: &[u8]) -> bool {
    if prefix == b"/" {
        return path.starts_with(b"/");
    }
    path.starts_with(prefix) && (path.len() == prefix.len() || path[prefix.len()] == b'/')
}

/// All entries of `manifest`, roots in order and each root's entries in stored order, which is
/// the order the backup walked them (directories before their contents, names sorted by bytes
/// within a directory). With `under`, only that path and everything below it; the path is
/// compared with logical paths, so it is the absolute path on the backed-up machine.
pub fn entries<'a>(
    manifest: &'a Manifest,
    under: Option<&Path>,
) -> impl Iterator<Item = ListedEntry<'a>> + 'a {
    let prefix = under.map(|path| normalized_prefix(&os_bytes(path.as_os_str())));
    manifest
        .roots
        .iter()
        .enumerate()
        .flat_map(move |(root_index, root)| {
            let prefix = prefix.clone();
            root.entries
                .iter()
                .enumerate()
                .filter_map(move |(index, entry)| {
                    let path = logical_path(root, index);
                    if prefix
                        .as_ref()
                        .is_some_and(|prefix| !is_under(&path, prefix))
                    {
                        return None;
                    }
                    let link_to = match entry.kind {
                        NodeKind::HardLink { target } => Some(logical_path(
                            root,
                            usize::try_from(target).unwrap_or(usize::MAX),
                        )),
                        _ => None,
                    };
                    Some(ListedEntry {
                        root: root_index,
                        index,
                        path,
                        entry,
                        link_to,
                    })
                })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::testing::{minimal, rich};

    fn paths(manifest: &Manifest, under: Option<&str>) -> Vec<String> {
        entries(manifest, under.map(Path::new))
            .map(|listed| String::from_utf8_lossy(&listed.path).into_owned())
            .collect()
    }

    #[test]
    fn every_entry_gets_its_source_prefixed_path_in_stored_order() {
        let listed = paths(&rich(), None);
        let documents = "/home/me/Documents";
        assert_eq!(
            listed,
            [
                documents.to_owned(),
                format!("{documents}/docs"),
                format!("{documents}/docs/report.txt"),
                format!("{documents}/docs/empty"),
                format!("{documents}/docs/alias"),
                format!("{documents}/link"),
                format!("{documents}/bad-\u{fffd}\u{fffd}-name"),
                format!("{documents}/unicode-日本語"),
                format!("{documents}/sock"),
                format!("{documents}/tty"),
                format!("{documents}/disk"),
                format!("{documents}/other"),
                "/home/me/notes.txt".to_owned(),
            ]
        );
    }

    #[test]
    fn names_that_are_not_text_keep_their_exact_bytes() {
        let manifest = rich();
        let odd = entries(&manifest, None)
            .find(|listed| listed.path.ends_with(b"-name"))
            .expect("the odd entry exists");
        assert_eq!(odd.path, b"/home/me/Documents/bad-\xff\xfe-name");
    }

    #[test]
    fn a_hard_link_names_the_path_it_shares_data_with() {
        let manifest = rich();
        let alias = entries(&manifest, None)
            .find(|listed| listed.path.ends_with(b"/alias"))
            .expect("the hard link exists");
        assert_eq!(alias.entry_type(), EntryType::HardLink);
        assert_eq!(
            alias.link_to.as_deref(),
            Some(&b"/home/me/Documents/docs/report.txt"[..])
        );
        assert_eq!(
            entries(&manifest, None)
                .filter(|l| l.link_to.is_some())
                .count(),
            1
        );
    }

    #[test]
    fn only_entries_with_content_report_a_size() {
        let manifest = rich();
        let sizes: Vec<(EntryType, Option<u64>)> = entries(&manifest, None)
            .map(|listed| (listed.entry_type(), listed.size()))
            .collect();
        assert!(sizes.contains(&(EntryType::File, Some(1500))));
        assert!(sizes.contains(&(EntryType::File, Some(0))));
        assert!(sizes.contains(&(EntryType::HardLink, Some(1500))));
        assert!(sizes.contains(&(EntryType::Directory, None)));
        assert!(sizes.contains(&(EntryType::Symlink, None)));
        assert!(sizes.contains(&(EntryType::Fifo, None)));
    }

    #[test]
    fn a_filter_selects_a_path_and_what_is_below_it_by_whole_components() {
        let manifest = rich();
        assert_eq!(paths(&manifest, Some("/home/me/Documents/docs")).len(), 4);
        assert_eq!(paths(&manifest, Some("/home/me/Documents/docs/")).len(), 4);
        assert_eq!(
            paths(&manifest, Some("/home/me/Documents/docs/report.txt")).len(),
            1
        );
        assert_eq!(paths(&manifest, Some("/home/me/Documents")).len(), 12);
        assert_eq!(
            paths(&manifest, Some("/home/me/notes.txt")),
            ["/home/me/notes.txt"]
        );
        assert_eq!(paths(&manifest, Some("/home/me")).len(), 13);
        assert_eq!(paths(&manifest, Some("/")).len(), 13);
    }

    #[test]
    fn a_filter_never_matches_part_of_a_name() {
        let manifest = rich();
        assert!(paths(&manifest, Some("/home/me/Documents/doc")).is_empty());
        assert!(paths(&manifest, Some("/home/me/Doc")).is_empty());
        assert!(paths(&manifest, Some("/home/me/notes")).is_empty());
        assert!(paths(&manifest, Some("/elsewhere")).is_empty());
    }

    #[test]
    fn a_root_source_ending_in_a_slash_does_not_double_it() {
        assert_eq!(join_path(b"/", b"etc/passwd"), b"/etc/passwd");
        assert_eq!(join_path(b"/data/", b"a"), b"/data/a");
        assert_eq!(join_path(b"/data", b"a/b"), b"/data/a/b");
        assert_eq!(join_path(b"/data", b""), b"/data");

        let mut manifest = minimal();
        manifest.roots[0].source = b"/".to_vec();
        assert_eq!(paths(&manifest, None), ["/"]);
    }

    #[test]
    fn skipped_paths_are_given_like_entry_paths() {
        let manifest = rich();
        let paths: Vec<Vec<u8>> = manifest
            .skipped
            .iter()
            .map(|record| skipped_path(&manifest, record))
            .collect();
        assert_eq!(paths[0], b"/home/me/Documents/locked");
        assert_eq!(paths[1], b"/home/me/Documents/secret");
        assert_eq!(paths[2], b"/home/me/notes.txt", "the root itself");

        let mut stray = manifest.skipped[0].clone();
        stray.root = 99;
        assert_eq!(skipped_path(&manifest, &stray), b"locked");
    }

    #[test]
    fn type_names_are_stable() {
        let names = [
            (EntryType::Directory, "directory"),
            (EntryType::File, "file"),
            (EntryType::Symlink, "symlink"),
            (EntryType::HardLink, "hardlink"),
            (EntryType::Fifo, "fifo"),
            (EntryType::Socket, "socket"),
            (EntryType::CharDevice, "chardev"),
            (EntryType::BlockDevice, "blockdev"),
            (EntryType::Other, "other"),
        ];
        for (kind, name) in names {
            assert_eq!(kind.name(), name);
        }
    }
}
