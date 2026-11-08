//! Comparing the tree being backed up with an earlier snapshot, to avoid re-reading unchanged
//! files.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::scan::{Metadata, Timestamp};
use crate::snapshot::{relative_path_bytes, Attributes, ChunkRef, Manifest, NodeKind, SnapshotId};

/// One root of an earlier snapshot, indexed by path.
pub(super) struct ParentRoot {
    snapshot: Arc<Manifest>,
    root: usize,
    files: HashMap<Vec<u8>, usize>,
}

impl ParentRoot {
    /// Indexes root `root` of `snapshot` (which must exist).
    pub(super) fn new(snapshot: Arc<Manifest>, root: usize) -> Self {
        let tree = &snapshot.roots[root];
        let mut files = HashMap::new();
        for (index, entry) in tree.entries.iter().enumerate() {
            if matches!(entry.kind, NodeKind::File { .. }) {
                files.insert(tree.relative_path(index), index);
            }
        }
        Self {
            snapshot,
            root,
            files,
        }
    }

    pub(super) fn snapshot_id(&self) -> &SnapshotId {
        &self.snapshot.id
    }

    /// The chunk list of the file at `path` in the parent, if the file is there and `current`
    /// shows it unchanged (see [`unchanged`]).
    pub(super) fn reusable(
        &self,
        path: &Path,
        current: &Metadata,
        margin: Duration,
    ) -> Option<&[ChunkRef]> {
        let index = *self.files.get(&relative_path_bytes(path))?;
        let entry = &self.snapshot.roots[self.root].entries[index];
        let NodeKind::File { chunks } = &entry.kind else {
            return None;
        };
        unchanged(&entry.attributes, current, self.snapshot.created, margin)
            .then_some(chunks.as_slice())
    }
}

/// Whether a file with attributes `previous` in a snapshot whose backup began at
/// `parent_started` is the same file, with the same content, as the one now described by
/// `current`.
///
/// Size, modification time, inode-change time, inode, and device must all be recorded on both
/// sides and equal. The inode-change time moves whenever the content, permissions, ownership, or
/// link count change and cannot be set by ordinary programs, so a program that rewrites a file
/// and restores its modification time is still caught. Platforms that cannot supply these fields
/// never match, which means every file is read: safe, just slower.
///
/// Finally the file must have been last modified and changed clearly *before* the parent backup
/// began (by at least `margin`). Otherwise it might have been modified again after the backup
/// read it, within the same timestamp tick, and look unchanged although its content differs.
pub(super) fn unchanged(
    previous: &Attributes,
    current: &Metadata,
    parent_started: Timestamp,
    margin: Duration,
) -> bool {
    let (Some(prev_modified), Some(prev_changed), Some(prev_inode), Some(prev_device)) = (
        previous.modified,
        previous.changed,
        previous.inode,
        previous.device,
    ) else {
        return false;
    };
    let (Some(modified), Some(changed), Some(inode), Some(device)) = (
        current.modified,
        current.changed,
        current.inode,
        current.device,
    ) else {
        return false;
    };
    let same = previous.size == current.size
        && prev_modified == modified
        && prev_changed == changed
        && prev_inode == inode
        && prev_device == device;
    same && older_than(changed, parent_started, margin)
        && older_than(modified, parent_started, margin)
}

/// Whether `time` is strictly earlier than `reference` minus `margin`.
fn older_than(time: Timestamp, reference: Timestamp, margin: Duration) -> bool {
    let limit = Timestamp::new(
        reference
            .secs
            .saturating_sub(i64::try_from(margin.as_secs()).unwrap_or(i64::MAX)),
        i64::from(reference.nanos) - i64::from(margin.subsec_nanos()),
    );
    time < limit
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk_store::ChunkId;
    use crate::chunking::ChunkerConfig;
    use crate::scan::ScanOptions;
    use crate::snapshot::{
        BackupConfig, ManifestEntry, ManifestStats, Root, MANIFEST_FORMAT_VERSION,
    };

    fn at(secs: i64) -> Timestamp {
        Timestamp { secs, nanos: 0 }
    }

    fn attributes() -> Attributes {
        Attributes {
            size: 100,
            modified: Some(at(1000)),
            accessed: None,
            changed: Some(at(1001)),
            created: None,
            mode: Some(0o644),
            uid: Some(1),
            gid: Some(1),
            readonly: false,
            device: Some(7),
            inode: Some(77),
            links: Some(1),
        }
    }

    fn metadata() -> Metadata {
        Metadata {
            size: 100,
            modified: Some(at(1000)),
            accessed: Some(at(5000)),
            changed: Some(at(1001)),
            created: None,
            mode: Some(0o600),
            uid: Some(2),
            gid: Some(2),
            readonly: false,
            device: Some(7),
            inode: Some(77),
            links: Some(1),
        }
    }

    const MARGIN: Duration = Duration::from_secs(2);

    /// A change to a file's metadata, to try one difference at a time.
    type Change = Box<dyn Fn(&mut Metadata)>;

    #[test]
    fn identical_old_files_are_unchanged() {
        assert!(unchanged(&attributes(), &metadata(), at(2000), MARGIN));
        assert!(unchanged(
            &attributes(),
            &metadata(),
            at(2000),
            Duration::ZERO
        ));
    }

    #[test]
    fn permissions_ownership_and_access_time_do_not_matter_by_themselves() {
        // These differ between attributes() and metadata() and the file still matches: they are
        // recorded from the current scan when a file is reused, and ctime would move for real
        // permission changes anyway.
        assert!(unchanged(&attributes(), &metadata(), at(2000), MARGIN));
    }

    #[test]
    fn any_difference_in_the_identity_of_the_file_means_changed() {
        let parent = at(2000);
        let cases: Vec<(&str, Change)> = vec![
            ("size", Box::new(|m: &mut Metadata| m.size = 101)),
            (
                "modified",
                Box::new(|m: &mut Metadata| m.modified = Some(at(999))),
            ),
            (
                "modified nanoseconds",
                Box::new(|m: &mut Metadata| {
                    m.modified = Some(Timestamp {
                        secs: 1000,
                        nanos: 1,
                    })
                }),
            ),
            (
                "changed",
                Box::new(|m: &mut Metadata| m.changed = Some(at(1002))),
            ),
            ("inode", Box::new(|m: &mut Metadata| m.inode = Some(78))),
            ("device", Box::new(|m: &mut Metadata| m.device = Some(8))),
        ];
        for (what, change) in cases {
            let mut current = metadata();
            change(&mut current);
            assert!(
                !unchanged(&attributes(), &current, parent, MARGIN),
                "{what}"
            );
        }
    }

    #[test]
    fn missing_information_never_matches() {
        let parent = at(2000);
        let clear: Vec<(&str, Change)> = vec![
            ("modified", Box::new(|m: &mut Metadata| m.modified = None)),
            ("changed", Box::new(|m: &mut Metadata| m.changed = None)),
            ("inode", Box::new(|m: &mut Metadata| m.inode = None)),
            ("device", Box::new(|m: &mut Metadata| m.device = None)),
        ];
        for (what, change) in clear {
            let mut current = metadata();
            change(&mut current);
            assert!(
                !unchanged(&attributes(), &current, parent, MARGIN),
                "current {what}"
            );
        }
        let mut previous = attributes();
        previous.inode = None;
        assert!(
            !unchanged(&previous, &metadata(), parent, MARGIN),
            "previous inode"
        );
        let mut previous = attributes();
        previous.changed = None;
        assert!(
            !unchanged(&previous, &metadata(), parent, MARGIN),
            "previous changed"
        );
    }

    #[test]
    fn files_touched_around_the_parent_backup_are_never_trusted() {
        // modified 1000, changed 1001.
        assert!(
            unchanged(&attributes(), &metadata(), at(1004), MARGIN),
            "3 s before: fine"
        );
        assert!(
            !unchanged(&attributes(), &metadata(), at(1003), MARGIN),
            "2 s before: racy"
        );
        assert!(
            !unchanged(&attributes(), &metadata(), at(1002), MARGIN),
            "1 s before: racy"
        );
        assert!(
            !unchanged(&attributes(), &metadata(), at(1001), MARGIN),
            "same second: racy"
        );
        assert!(
            !unchanged(&attributes(), &metadata(), at(1000), MARGIN),
            "before the change"
        );
        // With no margin only strictly earlier counts.
        assert!(unchanged(
            &attributes(),
            &metadata(),
            at(1002),
            Duration::ZERO
        ));
        assert!(
            !unchanged(&attributes(), &metadata(), at(1001), Duration::ZERO),
            "equal is racy"
        );
    }

    #[test]
    fn the_margin_is_applied_with_nanosecond_precision() {
        let margin = Duration::from_millis(1500);
        let mut previous = attributes();
        let mut current = metadata();
        previous.changed = Some(at(1001));
        current.changed = Some(at(1001));
        // 1001 < 1002.5 - 1.5 = 1001.0? No: equal, so not strictly older.
        let started = Timestamp {
            secs: 1002,
            nanos: 500_000_000,
        };
        assert!(!unchanged(&previous, &current, started, margin));
        let started = Timestamp {
            secs: 1002,
            nanos: 500_000_001,
        };
        assert!(unchanged(&previous, &current, started, margin));
    }

    fn manifest_with_file() -> Arc<Manifest> {
        let id = ChunkId::from_bytes([5; 32]);
        let entries = vec![
            ManifestEntry {
                parent: None,
                name: Vec::new(),
                kind: NodeKind::Directory,
                attributes: attributes(),
            },
            ManifestEntry {
                parent: Some(0),
                name: b"dir".to_vec(),
                kind: NodeKind::Directory,
                attributes: attributes(),
            },
            ManifestEntry {
                parent: Some(1),
                name: b"file".to_vec(),
                kind: NodeKind::File {
                    chunks: vec![ChunkRef { id, len: 100 }],
                },
                attributes: attributes(),
            },
        ];
        Arc::new(Manifest {
            format_version: MANIFEST_FORMAT_VERSION,
            id: "ab".repeat(32).parse().expect("valid id"),
            repository_id: "0123456789abcdef0123456789abcdef".to_owned(),
            created: at(2000),
            host: String::new(),
            tags: Vec::new(),
            program: String::new(),
            config: BackupConfig::new(ChunkerConfig::default(), &ScanOptions::default()),
            roots: vec![Root {
                source: b"/src".to_vec(),
                entries,
            }],
            skipped: Vec::new(),
            stats: ManifestStats::default(),
        })
    }

    #[test]
    fn files_are_found_by_relative_path() {
        let parent = ParentRoot::new(manifest_with_file(), 0);
        let found = parent.reusable(Path::new("dir/file"), &metadata(), MARGIN);
        assert_eq!(found.map(<[ChunkRef]>::len), Some(1));
        assert_eq!(parent.snapshot_id().as_str(), "ab".repeat(32));
    }

    #[test]
    fn directories_unknown_paths_and_changed_files_are_not_reusable() {
        let parent = ParentRoot::new(manifest_with_file(), 0);
        assert!(parent
            .reusable(Path::new("dir"), &metadata(), MARGIN)
            .is_none());
        assert!(parent
            .reusable(Path::new(""), &metadata(), MARGIN)
            .is_none());
        assert!(parent
            .reusable(Path::new("dir/other"), &metadata(), MARGIN)
            .is_none());
        let mut changed = metadata();
        changed.size = 5;
        assert!(parent
            .reusable(Path::new("dir/file"), &changed, MARGIN)
            .is_none());
    }
}
