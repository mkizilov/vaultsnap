//! Comparing two snapshots.
//!
//! Entries are matched by **logical path** (the backed-up source path plus the path below it), so
//! a file counts as the same file only if it has the same absolute path in both snapshots. There
//! is no rename detection: a moved file is a deletion plus an addition.
//!
//! Two files have the same *content* if their sizes and ordered chunk lists are equal. Chunk ids
//! are keyed hashes of the data, so equal lists mean equal bytes, and nothing has to be read.
//! One caveat: if the two backups used different chunk size limits, identical data can produce
//! different chunk lists, so a file of unchanged size may be reported as modified;
//! [`Diff::chunker_differs`] says when that can happen.
//!
//! Hard links are compared as regular files holding the content of the file they link to, so
//! which of several names carries the data is not a difference.

use std::collections::HashMap;

use super::entries::{content_size, entries, EntryType};
use super::sizes::SizeLedger;
use crate::chunk_store::ChunkStore;
use crate::error::Result;
use crate::snapshot::{ChunkRef, Manifest, ManifestEntry, NodeKind, Root, SnapshotSummary};

/// What happened to a path between the two snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Only in the newer snapshot.
    Added,
    /// Only in the older snapshot.
    Deleted,
    /// In both, and different.
    Modified,
}

impl Status {
    /// The name used in machine-readable output; part of the output contract.
    pub fn name(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Deleted => "deleted",
            Self::Modified => "modified",
        }
    }
}

/// What differs about a modified path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aspect {
    /// The content of a regular file (size or chunks). Hard links count with the file they
    /// link to.
    Content,
    /// The kind of entry, for example a file replaced by a symbolic link. When the type differs,
    /// nothing else is compared.
    Type,
    /// The target of a symbolic link.
    LinkTarget,
    /// The permission bits.
    Mode,
    /// The owning user or group.
    Owner,
    /// The modification time. Not compared for directories, whose time changes whenever an
    /// entry is added or removed inside them.
    ModifiedTime,
}

impl Aspect {
    /// The name used in machine-readable output; part of the output contract.
    pub fn name(self) -> &'static str {
        match self {
            Self::Content => "content",
            Self::Type => "type",
            Self::LinkTarget => "link_target",
            Self::Mode => "mode",
            Self::Owner => "owner",
            Self::ModifiedTime => "mtime",
        }
    }
}

/// One path that differs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// What happened to it.
    pub status: Status,
    /// The logical path (raw bytes).
    pub path: Vec<u8>,
    /// Its type in the newer snapshot, or in the older one for a deletion.
    pub kind: EntryType,
    /// For a modification, its type in the older snapshot.
    pub previous_kind: Option<EntryType>,
    /// The size of its content (see [`content_size`]) in the newer snapshot, or in the older one
    /// for a deletion.
    pub size: Option<u64>,
    /// For a modification, the size of its content in the older snapshot.
    pub previous_size: Option<u64>,
    /// For a modification, what differs; empty otherwise.
    pub aspects: Vec<Aspect>,
}

impl Change {
    /// The bytes of file content this change adds or removes: the size of an added or deleted
    /// regular file, and nothing for anything else (a hard link carries no data of its own).
    fn file_bytes(&self) -> u64 {
        match (self.status, self.kind) {
            (Status::Added | Status::Deleted, EntryType::File) => self.size.unwrap_or(0),
            _ => 0,
        }
    }
}

/// The difference between an older and a newer snapshot.
#[derive(Debug, Clone)]
pub struct Diff {
    /// The older snapshot.
    pub from: SnapshotSummary,
    /// The newer snapshot.
    pub to: SnapshotSummary,
    /// Every path that differs, in ascending byte order of the path.
    pub changes: Vec<Change>,
    /// How many paths are the same in both.
    pub unchanged: u64,
    /// Total size of the regular files that were added.
    pub added_bytes: u64,
    /// Total size of the regular files that were deleted.
    pub deleted_bytes: u64,
    /// Distinct chunks only the older snapshot refers to.
    pub chunks_only_in_from: u64,
    /// Distinct chunks only the newer snapshot refers to: the data that is new in it.
    pub chunks_only_in_to: u64,
    /// Size of the objects holding the chunks only the older snapshot refers to.
    pub stored_only_in_from: u64,
    /// Size of the objects holding the chunks only the newer snapshot refers to.
    pub stored_only_in_to: u64,
    /// Distinct chunks of either snapshot whose object is missing; above zero means damage.
    pub missing_chunks: u64,
    /// Whether the two backups used different chunk size limits (see the module documentation).
    pub chunker_differs: bool,
}

impl Diff {
    /// How many paths have the given status.
    pub fn count(&self, status: Status) -> usize {
        self.changes
            .iter()
            .filter(|change| change.status == status)
            .count()
    }

    /// True if the two snapshots hold the same paths with the same content and attributes.
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}

/// One entry together with the root it lives in, which hard links need to find their data.
struct Node<'a> {
    root: &'a Root,
    entry: &'a ManifestEntry,
}

fn index(manifest: &Manifest) -> HashMap<Vec<u8>, Node<'_>> {
    entries(manifest, None)
        .map(|listed| {
            let node = Node {
                root: &manifest.roots[listed.root],
                entry: listed.entry,
            };
            (listed.path, node)
        })
        .collect()
}

/// The chunks that make up the content of a regular file or of the file a hard link names.
fn chunks_of<'a>(node: &Node<'a>) -> Option<&'a [ChunkRef]> {
    let entry = match &node.entry.kind {
        NodeKind::HardLink { target } => node.root.entries.get(usize::try_from(*target).ok()?)?,
        _ => node.entry,
    };
    match &entry.kind {
        NodeKind::File { chunks } => Some(chunks),
        _ => None,
    }
}

/// The type used to decide whether two entries are comparable: hard links count as the files
/// they link to.
fn comparable_type(node: &Node<'_>) -> EntryType {
    match EntryType::of(&node.entry.kind) {
        EntryType::HardLink => EntryType::File,
        other => other,
    }
}

fn aspects(old: &Node<'_>, new: &Node<'_>) -> Vec<Aspect> {
    let kind = comparable_type(old);
    if kind != comparable_type(new) {
        return vec![Aspect::Type];
    }
    let (before, after) = (&old.entry.attributes, &new.entry.attributes);
    let mut aspects = Vec::new();
    match kind {
        EntryType::File => {
            if before.size != after.size || chunks_of(old) != chunks_of(new) {
                aspects.push(Aspect::Content);
            }
        }
        EntryType::Symlink => {
            if let (NodeKind::Symlink { target: a }, NodeKind::Symlink { target: b }) =
                (&old.entry.kind, &new.entry.kind)
            {
                if a != b {
                    aspects.push(Aspect::LinkTarget);
                }
            }
        }
        _ => {}
    }
    if before.mode != after.mode {
        aspects.push(Aspect::Mode);
    }
    if before.uid != after.uid || before.gid != after.gid {
        aspects.push(Aspect::Owner);
    }
    if kind != EntryType::Directory && before.modified != after.modified {
        aspects.push(Aspect::ModifiedTime);
    }
    aspects
}

/// The result of comparing the trees of two manifests.
pub(crate) struct TreeComparison {
    pub changes: Vec<Change>,
    pub unchanged: u64,
}

/// Compares the entries of `from` (older) and `to` (newer).
pub(crate) fn compare_trees(from: &Manifest, to: &Manifest) -> TreeComparison {
    let old = index(from);
    let new = index(to);
    let mut changes = Vec::new();
    let mut unchanged = 0;

    for (path, old_node) in &old {
        match new.get(path) {
            None => changes.push(Change {
                status: Status::Deleted,
                path: path.clone(),
                kind: EntryType::of(&old_node.entry.kind),
                previous_kind: None,
                size: content_size(old_node.entry),
                previous_size: None,
                aspects: Vec::new(),
            }),
            Some(new_node) => {
                let aspects = aspects(old_node, new_node);
                if aspects.is_empty() {
                    unchanged += 1;
                } else {
                    changes.push(Change {
                        status: Status::Modified,
                        path: path.clone(),
                        kind: EntryType::of(&new_node.entry.kind),
                        previous_kind: Some(EntryType::of(&old_node.entry.kind)),
                        size: content_size(new_node.entry),
                        previous_size: content_size(old_node.entry),
                        aspects,
                    });
                }
            }
        }
    }
    for (path, new_node) in &new {
        if !old.contains_key(path) {
            changes.push(Change {
                status: Status::Added,
                path: path.clone(),
                kind: EntryType::of(&new_node.entry.kind),
                previous_kind: None,
                size: content_size(new_node.entry),
                previous_size: None,
                aspects: Vec::new(),
            });
        }
    }
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    TreeComparison { changes, unchanged }
}

/// Compares two verified snapshots and measures how much stored data is particular to each.
pub(crate) fn compare(from: &Manifest, to: &Manifest, chunks: &ChunkStore) -> Result<Diff> {
    let TreeComparison { changes, unchanged } = compare_trees(from, to);

    let mut ledger = SizeLedger::default();
    ledger.add(from);
    ledger.add(to);
    ledger.measure(chunks)?;
    let (older, newer) = (ledger.sizes_of(from), ledger.sizes_of(to));

    let bytes = |status: Status| {
        changes
            .iter()
            .filter(|change| change.status == status)
            .fold(0_u64, |total, change| {
                total.saturating_add(change.file_bytes())
            })
    };
    Ok(Diff {
        from: from.summary(),
        to: to.summary(),
        unchanged,
        added_bytes: bytes(Status::Added),
        deleted_bytes: bytes(Status::Deleted),
        chunks_only_in_from: older.unique_chunks,
        chunks_only_in_to: newer.unique_chunks,
        stored_only_in_from: older.unique_stored_bytes,
        stored_only_in_to: newer.unique_stored_bytes,
        missing_chunks: ledger.missing(),
        chunker_differs: from.config.chunker != to.config.chunker,
        changes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk_store::ChunkId;
    use crate::chunking::test_util::pseudo_random;
    use crate::chunking::ChunkerConfig;
    use crate::keys::RepositoryKeys;
    use crate::snapshot::testing::{chunk, file, minimal, rich};
    use crate::store::Store;

    const DOCS: &str = "/home/me/Documents";

    fn change<'a>(result: &'a TreeComparison, path: &str) -> &'a Change {
        result
            .changes
            .iter()
            .find(|change| change.path == path.as_bytes())
            .unwrap_or_else(|| panic!("no change for {path}: {:?}", result.changes))
    }

    fn entry_mut<'a>(manifest: &'a mut Manifest, name: &[u8]) -> &'a mut ManifestEntry {
        manifest.roots[0]
            .entries
            .iter_mut()
            .find(|entry| entry.name == name)
            .expect("entry exists")
    }

    #[test]
    fn identical_snapshots_have_no_changes() {
        let manifest = rich();
        let result = compare_trees(&manifest, &manifest);
        assert!(result.changes.is_empty());
        assert_eq!(result.unchanged, 13);
    }

    #[test]
    fn added_and_deleted_paths_are_reported_with_their_sizes() {
        let old = rich();
        let mut new = rich();
        for _ in 0..3 {
            new.roots[0].entries.pop(); // `other`, `disk`, and `tty`, the last three entries
        }
        new.roots[0]
            .entries
            .push(file(0, b"new.txt", vec![chunk(5, 10), chunk(6, 20)]));

        let result = compare_trees(&old, &new);
        let added = change(&result, &format!("{DOCS}/new.txt"));
        assert_eq!(added.status, Status::Added);
        assert_eq!((added.kind, added.size), (EntryType::File, Some(30)));
        assert!(added.aspects.is_empty() && added.previous_size.is_none());
        for name in ["other", "disk", "tty"] {
            let removed = change(&result, &format!("{DOCS}/{name}"));
            assert_eq!(removed.status, Status::Deleted);
        }
        assert_eq!(result.changes.len(), 4);
        assert_eq!(result.unchanged, 13 - 3);
    }

    #[test]
    fn a_changed_chunk_list_is_a_content_change_that_also_reaches_hard_links() {
        let old = rich();
        let mut new = rich();
        if let NodeKind::File { chunks } = &mut entry_mut(&mut new, b"report.txt").kind {
            chunks[1] = chunk(77, 500);
        }

        let result = compare_trees(&old, &new);
        let report = change(&result, &format!("{DOCS}/docs/report.txt"));
        assert_eq!(report.status, Status::Modified);
        assert_eq!(report.aspects, [Aspect::Content]);
        assert_eq!(
            (report.previous_size, report.size),
            (Some(1500), Some(1500))
        );
        let alias = change(&result, &format!("{DOCS}/docs/alias"));
        assert_eq!(
            alias.aspects,
            [Aspect::Content],
            "the link shares the changed data"
        );
        assert_eq!(alias.kind, EntryType::HardLink);
        assert_eq!(result.changes.len(), 2);
    }

    #[test]
    fn a_different_size_is_a_content_change() {
        let old = rich();
        let mut new = rich();
        entry_mut(&mut new, b"report.txt").attributes.size = 1;
        let result = compare_trees(&old, &new);
        assert_eq!(
            change(&result, &format!("{DOCS}/docs/report.txt")).aspects,
            [Aspect::Content]
        );
    }

    #[test]
    fn attribute_changes_are_named() {
        let old = rich();
        let mut new = rich();
        entry_mut(&mut new, b"empty").attributes.mode = Some(0o600);
        let report = entry_mut(&mut new, b"report.txt");
        report.attributes.uid = Some(0);
        report.attributes.modified = Some(crate::scan::Timestamp { secs: 5, nanos: 6 });

        let result = compare_trees(&old, &new);
        assert_eq!(
            change(&result, &format!("{DOCS}/docs/empty")).aspects,
            [Aspect::Mode]
        );
        assert_eq!(
            change(&result, &format!("{DOCS}/docs/report.txt")).aspects,
            [Aspect::Owner, Aspect::ModifiedTime]
        );
        assert_eq!(result.changes.len(), 2);
    }

    #[test]
    fn the_modification_time_of_a_directory_is_not_a_difference() {
        let old = rich();
        let mut new = rich();
        entry_mut(&mut new, b"docs").attributes.modified =
            Some(crate::scan::Timestamp { secs: 1, nanos: 1 });
        assert!(compare_trees(&old, &new).changes.is_empty());

        entry_mut(&mut new, b"docs").attributes.mode = Some(0o700);
        let result = compare_trees(&old, &new);
        assert_eq!(
            change(&result, &format!("{DOCS}/docs")).aspects,
            [Aspect::Mode]
        );
    }

    #[test]
    fn a_changed_type_hides_every_other_difference() {
        let old = rich();
        let mut new = rich();
        let report = entry_mut(&mut new, b"report.txt");
        report.kind = NodeKind::Symlink {
            target: b"elsewhere".to_vec(),
        };
        report.attributes.mode = Some(0o777);

        let result = compare_trees(&old, &new);
        let changed = change(&result, &format!("{DOCS}/docs/report.txt"));
        assert_eq!(changed.aspects, [Aspect::Type]);
        assert_eq!(changed.previous_kind, Some(EntryType::File));
        assert_eq!(changed.kind, EntryType::Symlink);
    }

    #[test]
    fn a_retargeted_symbolic_link_is_named() {
        let old = rich();
        let mut new = rich();
        entry_mut(&mut new, b"link").kind = NodeKind::Symlink {
            target: b"other/place".to_vec(),
        };
        let result = compare_trees(&old, &new);
        assert_eq!(
            change(&result, &format!("{DOCS}/link")).aspects,
            [Aspect::LinkTarget]
        );
    }

    #[test]
    fn which_name_carries_the_data_of_a_hard_link_is_not_a_difference() {
        let old = rich();
        let mut new = rich();
        let alias = entry_mut(&mut new, b"alias");
        alias.kind = NodeKind::File {
            chunks: vec![chunk(1, 1000), chunk(2, 500)],
        };
        assert!(compare_trees(&old, &new).changes.is_empty());
    }

    #[test]
    fn changes_are_ordered_by_the_bytes_of_the_path() {
        let build = |names: &[&[u8]]| {
            let mut manifest = minimal();
            for name in names {
                manifest.roots[0]
                    .entries
                    .push(file(0, name, vec![chunk(3, 5)]));
            }
            manifest
        };
        let old = build(&[]);
        let new = build(&[b"zebra", b"a b", b"a", b"\xff", b"Apple"]);
        let result = compare_trees(&old, &new);
        let order: Vec<&[u8]> = result.changes.iter().map(|c| c.path.as_slice()).collect();
        let expected: [&[u8]; 5] = [b"/r/Apple", b"/r/a", b"/r/a b", b"/r/zebra", b"/r/\xff"];
        assert_eq!(
            order, expected,
            "plain byte order: uppercase first, 0xff last"
        );
    }

    #[test]
    fn paths_that_are_not_text_keep_their_exact_bytes_in_a_change() {
        let old = rich();
        let mut new = rich();
        entry_mut(&mut new, b"bad-\xff\xfe-name").attributes.mode = Some(0o600);
        let result = compare_trees(&old, &new);
        assert_eq!(result.changes.len(), 1);
        assert_eq!(
            result.changes[0].path,
            b"/home/me/Documents/bad-\xff\xfe-name"
        );
    }

    #[test]
    fn a_different_source_path_makes_everything_added_and_deleted() {
        let old = minimal();
        let mut new = minimal();
        new.roots[0].source = b"/elsewhere".to_vec();
        let result = compare_trees(&old, &new);
        let statuses: Vec<Status> = result.changes.iter().map(|c| c.status).collect();
        assert_eq!(
            statuses,
            [Status::Added, Status::Deleted],
            "/elsewhere sorts before /r"
        );
        assert_eq!(result.unchanged, 0);
    }

    #[test]
    fn status_and_aspect_names_are_stable() {
        assert_eq!(Status::Added.name(), "added");
        assert_eq!(Status::Deleted.name(), "deleted");
        assert_eq!(Status::Modified.name(), "modified");
        let aspects = [
            (Aspect::Content, "content"),
            (Aspect::Type, "type"),
            (Aspect::LinkTarget, "link_target"),
            (Aspect::Mode, "mode"),
            (Aspect::Owner, "owner"),
            (Aspect::ModifiedTime, "mtime"),
        ];
        for (aspect, name) in aspects {
            assert_eq!(aspect.name(), name);
        }
    }

    #[test]
    fn a_full_comparison_measures_the_data_particular_to_each_snapshot() {
        let dir = tempfile::tempdir().expect("create temporary directory");
        Store::local(dir.path()).create_dir("data").expect("mkdir");
        let keys = RepositoryKeys::generate().expect("generate");
        let store = ChunkStore::new(Store::local(dir.path()), &keys).expect("chunk store");
        let mut stored_len = HashMap::new();
        let mut put = |seed: u64| {
            let data = pseudo_random(2000, seed);
            let result = store.put(&data).expect("put");
            stored_len.insert(result.id, store.stored_len(&result.id).expect("length"));
            ChunkRef {
                id: result.id,
                len: 2000,
            }
        };
        let (shared, only_old, only_new) = (put(1), put(2), put(3));

        let manifest_with = |chunks: Vec<ChunkRef>| {
            let mut manifest = minimal();
            manifest.roots[0].entries.push(file(0, b"data.bin", chunks));
            manifest.stats = manifest.compute_stats();
            manifest
        };
        let old = manifest_with(vec![shared, only_old]);
        let mut new = manifest_with(vec![shared, only_new]);
        new.config.chunker = ChunkerConfig::new(1024, 4096, 16_384).expect("valid");

        let diff = compare(&old, &new, &store).expect("compare");
        assert_eq!(diff.count(Status::Modified), 1);
        assert_eq!(diff.changes[0].aspects, [Aspect::Content]);
        assert_eq!((diff.chunks_only_in_from, diff.chunks_only_in_to), (1, 1));
        assert_eq!(diff.stored_only_in_from, stored_len[&only_old.id]);
        assert_eq!(diff.stored_only_in_to, stored_len[&only_new.id]);
        assert_eq!(
            (diff.added_bytes, diff.deleted_bytes, diff.missing_chunks),
            (0, 0, 0)
        );
        assert!(diff.chunker_differs);
        assert!(!diff.is_empty());
        assert_eq!(diff.from.id, old.id);

        let same = compare(&old, &old, &store).expect("compare");
        assert!(same.is_empty() && !same.chunker_differs);
        assert_eq!((same.chunks_only_in_from, same.stored_only_in_to), (0, 0));
    }

    #[test]
    fn missing_objects_are_counted_not_fatal() {
        let dir = tempfile::tempdir().expect("create temporary directory");
        Store::local(dir.path()).create_dir("data").expect("mkdir");
        let keys = RepositoryKeys::generate().expect("generate");
        let store = ChunkStore::new(Store::local(dir.path()), &keys).expect("chunk store");
        let mut manifest = minimal();
        let missing: ChunkId = store.chunk_id(b"never stored");
        manifest.roots[0].entries.push(file(
            0,
            b"f",
            vec![ChunkRef {
                id: missing,
                len: 10,
            }],
        ));
        manifest.stats = manifest.compute_stats();
        let diff = compare(&manifest, &manifest, &store).expect("compare");
        assert_eq!(diff.missing_chunks, 1);
    }
}
