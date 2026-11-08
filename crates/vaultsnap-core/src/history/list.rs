//! Listing the snapshots of a repository and describing one in detail.
//!
//! Every snapshot is read through [`SnapshotStore::load`], which authenticates and decrypts the
//! file against its id and validates the manifest. A snapshot that fails that is *damaged*: it is
//! reported with the reason, never listed with unverified contents, and never hides the others.

use super::sizes::{SizeLedger, SnapshotSizes};
use crate::chunk_store::ChunkStore;
use crate::error::Result;
use crate::snapshot::{
    Manifest, ManifestStats, SkippedReason, SnapshotId, SnapshotStore, SnapshotSummary,
};

/// How many paths of a snapshot should have been backed up but could not be read (unreadable, or
/// kept changing while being read). Above zero, the snapshot is incomplete. Paths left out by
/// policy or because they disappeared do not count.
pub fn unreadable_paths(manifest: &Manifest) -> u64 {
    manifest
        .skipped
        .iter()
        .filter(|record| matches!(record.reason, SkippedReason::Unreadable { .. }))
        .count() as u64
}

/// One verified snapshot in a listing.
#[derive(Debug, Clone)]
pub struct SnapshotInfo {
    /// What the snapshot says about itself.
    pub summary: SnapshotSummary,
    /// Its sizes. Unique sizes consider the snapshots that could be read; see
    /// [`SnapshotList::unique_sizes_exact`].
    pub sizes: SnapshotSizes,
    /// Paths that could not be backed up; above zero means the snapshot is incomplete.
    pub unreadable: u64,
}

/// A snapshot that failed verification.
#[derive(Debug, Clone)]
pub struct DamagedSnapshot {
    /// The snapshot id (the file name; nothing inside could be trusted).
    pub id: SnapshotId,
    /// Why it failed, in words that never contain file names.
    pub error: String,
}

/// All snapshots of a repository.
#[derive(Debug, Clone)]
pub struct SnapshotList {
    /// The snapshots that verified, oldest first: ordered by the creation time recorded inside
    /// the authenticated manifest, ties broken by id. File times play no part.
    pub snapshots: Vec<SnapshotInfo>,
    /// The snapshots that failed verification, by id.
    pub damaged: Vec<DamagedSnapshot>,
    /// Distinct chunks referenced by the snapshots that verified.
    pub referenced_chunks: u64,
    /// Size of the objects holding them: the repository's data as far as the listing sees it.
    pub referenced_stored_bytes: u64,
}

impl SnapshotList {
    /// Whether the unique sizes are exact. A damaged snapshot might refer to chunks that look
    /// unique to another snapshot, so with any damaged snapshot they can be overstated.
    pub fn unique_sizes_exact(&self) -> bool {
        self.damaged.is_empty()
    }
}

/// Reads every snapshot, orders them, and measures them.
///
/// Snapshots are loaded twice, once to count which chunks are shared and once to size each
/// of them, so that memory use is bounded by the largest manifest plus one entry per distinct
/// chunk, not by the number of snapshots.
pub(crate) fn list(snapshots: &SnapshotStore, chunks: &ChunkStore) -> Result<SnapshotList> {
    let mut ledger = SizeLedger::default();
    let mut readable: Vec<SnapshotSummary> = Vec::new();
    let mut damaged = Vec::new();
    for id in snapshots.ids()? {
        match snapshots.load(&id) {
            Ok(manifest) => {
                ledger.add(&manifest);
                readable.push(manifest.summary());
            }
            Err(err) => damaged.push(DamagedSnapshot {
                id,
                error: err.to_string(),
            }),
        }
    }
    ledger.measure(chunks)?;

    readable.sort_by(|a, b| a.created.cmp(&b.created).then_with(|| a.id.cmp(&b.id)));
    let mut infos = Vec::with_capacity(readable.len());
    for summary in readable {
        let manifest = snapshots.load(&summary.id)?;
        infos.push(SnapshotInfo {
            sizes: ledger.sizes_of(&manifest),
            unreadable: unreadable_paths(&manifest),
            summary,
        });
    }
    let (referenced_chunks, referenced_stored_bytes) = ledger.totals();
    Ok(SnapshotList {
        snapshots: infos,
        damaged,
        referenced_chunks,
        referenced_stored_bytes,
    })
}

/// Everything about one verified snapshot.
#[derive(Debug, Clone)]
pub struct SnapshotDetails {
    /// The verified manifest.
    pub manifest: Manifest,
    /// Statistics of each root, in the manifest's order; `skipped` counts the skipped paths of
    /// that root.
    pub roots: Vec<ManifestStats>,
    /// The sizes of the snapshot.
    pub sizes: SnapshotSizes,
    /// Whether the unique sizes are exact: false if another snapshot of the repository is damaged
    /// and could not be taken into account.
    pub unique_sizes_exact: bool,
}

/// Loads snapshot `id` and measures it against all the other snapshots.
pub(crate) fn details(
    snapshots: &SnapshotStore,
    chunks: &ChunkStore,
    id: &SnapshotId,
) -> Result<SnapshotDetails> {
    let manifest = snapshots.load(id)?;
    let mut ledger = SizeLedger::default();
    ledger.add(&manifest);
    let mut exact = true;
    for other in snapshots.ids()? {
        if other == *id {
            continue;
        }
        match snapshots.load(&other) {
            Ok(other) => ledger.add(&other),
            Err(_) => exact = false,
        }
    }
    ledger.measure(chunks)?;

    let roots = manifest
        .roots
        .iter()
        .enumerate()
        .map(|(index, root)| {
            let mut stats = root.stats();
            stats.skipped = manifest
                .skipped
                .iter()
                .filter(|record| usize::try_from(record.root).is_ok_and(|root| root == index))
                .count() as u64;
            stats
        })
        .collect();
    Ok(SnapshotDetails {
        sizes: ledger.sizes_of(&manifest),
        roots,
        unique_sizes_exact: exact,
        manifest,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;
    use crate::chunk_store::ChunkStore;
    use crate::chunking::test_util::pseudo_random;
    use crate::error::Error;
    use crate::keys::RepositoryKeys;
    use crate::scan::Timestamp;
    use crate::snapshot::testing::{file, minimal, REPO_ID};
    use crate::snapshot::ChunkRef;
    use crate::store::Store;

    struct Fixture {
        dir: TempDir,
        chunks: ChunkStore,
        snapshots: SnapshotStore,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("create temporary directory");
            let local = Store::local(dir.path());
            local.create_dir("data").expect("mkdir");
            local.create_dir("snapshots").expect("mkdir");
            let keys = RepositoryKeys::generate().expect("generate");
            Self {
                chunks: ChunkStore::new(local.clone(), &keys).expect("chunk store"),
                snapshots: SnapshotStore::new(local, &keys, REPO_ID).expect("snapshot store"),
                dir,
            }
        }

        /// Commits a snapshot with the given id (two hex digits repeated), creation time, and
        /// files; each file is one chunk of 1000 pseudo-random bytes chosen by its seed, so equal
        /// seeds share a chunk.
        fn commit(&self, id: &str, secs: i64, files: &[(&str, u64)]) -> Manifest {
            let mut manifest = minimal();
            manifest.id = id.repeat(32).parse().expect("valid id");
            manifest.created = Timestamp { secs, nanos: 0 };
            for (name, seed) in files {
                let put = self.chunks.put(&pseudo_random(1000, *seed)).expect("put");
                let chunks = vec![ChunkRef {
                    id: put.id,
                    len: 1000,
                }];
                manifest.roots[0]
                    .entries
                    .push(file(0, name.as_bytes(), chunks));
            }
            manifest.stats = manifest.compute_stats();
            self.snapshots.commit(&manifest).expect("commit");
            manifest
        }

        fn corrupt(&self, manifest: &Manifest) {
            let path = self.dir.path().join(manifest.id.object_name());
            let mut bytes = fs::read(&path).expect("read snapshot file");
            let middle = bytes.len() / 2;
            bytes[middle] ^= 0x01;
            fs::write(&path, bytes).expect("write snapshot file");
        }
    }

    fn ids(list: &SnapshotList) -> Vec<String> {
        list.snapshots
            .iter()
            .map(|info| info.summary.id.as_str()[..2].to_owned())
            .collect()
    }

    #[test]
    fn snapshots_are_ordered_by_their_authenticated_creation_time_then_id() {
        let fx = Fixture::new();
        // Committed in an order that is neither by time nor by id.
        fx.commit("cc", 300, &[]);
        fx.commit("bb", 100, &[]);
        fx.commit("aa", 200, &[]);
        fx.commit("dd", 100, &[]);

        let list = list(&fx.snapshots, &fx.chunks).expect("list");
        assert_eq!(ids(&list), ["bb", "dd", "aa", "cc"]);
        assert!(list.damaged.is_empty());
        assert!(list.unique_sizes_exact());
    }

    #[test]
    fn file_times_do_not_influence_the_order() {
        let fx = Fixture::new();
        let older = fx.commit("ff", 100, &[]);
        let newer = fx.commit("11", 200, &[]);
        // Make the file of the older snapshot look the newest on disk.
        let file = fs::File::options()
            .write(true)
            .open(fx.dir.path().join(older.id.object_name()))
            .expect("open");
        file.set_modified(std::time::SystemTime::now())
            .expect("set mtime");
        drop(file);

        let list = list(&fx.snapshots, &fx.chunks).expect("list");
        assert_eq!(list.snapshots[0].summary.id, older.id);
        assert_eq!(list.snapshots[1].summary.id, newer.id);
    }

    #[test]
    fn an_empty_repository_has_an_empty_list() {
        let fx = Fixture::new();
        let list = list(&fx.snapshots, &fx.chunks).expect("list");
        assert!(list.snapshots.is_empty() && list.damaged.is_empty());
        assert_eq!(
            (list.referenced_chunks, list.referenced_stored_bytes),
            (0, 0)
        );
    }

    #[test]
    fn sizes_separate_shared_from_unique_data() {
        let fx = Fixture::new();
        fx.commit("aa", 100, &[("one", 1), ("two", 2)]);
        fx.commit("bb", 200, &[("two", 2), ("three", 3), ("three-again", 3)]);

        let list = list(&fx.snapshots, &fx.chunks).expect("list");
        let (first, second) = (&list.snapshots[0].sizes, &list.snapshots[1].sizes);
        assert_eq!(first.logical_bytes, 2000);
        assert_eq!(
            second.logical_bytes, 3000,
            "a repeated chunk still counts in the logical size"
        );
        assert_eq!(
            (first.chunks, second.chunks),
            (2, 2),
            "repeats inside a snapshot count once"
        );
        assert_eq!(
            (first.deduplicated_bytes, second.deduplicated_bytes),
            (2000, 2000)
        );
        assert_eq!(
            (first.unique_chunks, second.unique_chunks),
            (1, 1),
            "`two` is shared"
        );
        assert!(first.stored_bytes >= 2000 && first.stored_bytes > first.unique_stored_bytes);
        let shared = first.stored_bytes - first.unique_stored_bytes;
        assert_eq!(
            first.unique_stored_bytes + second.unique_stored_bytes + shared,
            list.referenced_stored_bytes,
            "the unique parts plus the shared part make up the whole"
        );
        assert_eq!(list.referenced_chunks, 3);
        assert_eq!((first.missing_chunks, second.missing_chunks), (0, 0));
    }

    #[test]
    fn a_damaged_snapshot_is_reported_and_does_not_hide_the_others() {
        let fx = Fixture::new();
        fx.commit("aa", 100, &[("one", 1)]);
        let broken = fx.commit("bb", 200, &[("two", 2)]);
        fx.commit("cc", 300, &[("three", 3)]);
        fx.corrupt(&broken);

        let list = list(&fx.snapshots, &fx.chunks).expect("list");
        assert_eq!(ids(&list), ["aa", "cc"]);
        assert_eq!(list.damaged.len(), 1);
        assert_eq!(list.damaged[0].id, broken.id);
        assert!(!list.damaged[0].error.is_empty());
        assert!(!list.unique_sizes_exact());
    }

    #[test]
    fn incomplete_snapshots_are_counted_by_their_unreadable_paths() {
        let fx = Fixture::new();
        let mut manifest = minimal();
        manifest.id = "ab".repeat(32).parse().expect("valid id");
        // `rich` has two roots and `minimal` one, so every record is moved to root 0.
        manifest.skipped = crate::snapshot::testing::rich()
            .skipped
            .into_iter()
            .map(|record| crate::snapshot::SkippedRecord { root: 0, ..record })
            .collect();
        manifest.stats = manifest.compute_stats();
        fx.snapshots.commit(&manifest).expect("commit");

        let list = list(&fx.snapshots, &fx.chunks).expect("list");
        assert_eq!(
            list.snapshots[0].unreadable, 1,
            "one of the four skipped paths was unreadable"
        );
        assert_eq!(list.snapshots[0].summary.stats.skipped, 4);
        assert_eq!(unreadable_paths(&minimal()), 0);
    }

    #[test]
    fn details_measure_a_snapshot_against_all_the_others() {
        let fx = Fixture::new();
        let first = fx.commit("aa", 100, &[("one", 1), ("two", 2)]);
        fx.commit("bb", 200, &[("two", 2)]);

        let found = details(&fx.snapshots, &fx.chunks, &first.id).expect("details");
        assert_eq!(found.manifest, first);
        assert_eq!(
            found.sizes.unique_chunks, 1,
            "only `one` is particular to the first"
        );
        assert_eq!(found.sizes.chunks, 2);
        assert!(found.unique_sizes_exact);
        assert_eq!(found.roots.len(), 1);
        assert_eq!(found.roots[0].files, 2);
    }

    #[test]
    fn details_say_when_another_snapshot_could_not_be_taken_into_account() {
        let fx = Fixture::new();
        let first = fx.commit("aa", 100, &[("one", 1)]);
        let broken = fx.commit("bb", 200, &[("one", 1)]);
        fx.corrupt(&broken);

        let found = details(&fx.snapshots, &fx.chunks, &first.id).expect("details");
        assert!(!found.unique_sizes_exact);
        assert_eq!(
            found.sizes.unique_chunks, 1,
            "overstated, which is what the flag warns about"
        );
    }

    #[test]
    fn details_per_root_count_their_own_skipped_paths() {
        let fx = Fixture::new();
        let mut manifest = crate::snapshot::testing::rich();
        manifest.id = "ab".repeat(32).parse().expect("valid id");
        // The hand-built chunk ids are not stored, so give the files real chunks.
        let put = fx.chunks.put(&pseudo_random(1000, 5)).expect("put");
        let real = ChunkRef {
            id: put.id,
            len: 1000,
        };
        for root in &mut manifest.roots {
            for entry in &mut root.entries {
                if let crate::snapshot::NodeKind::File { chunks } = &mut entry.kind {
                    for chunk in chunks.iter_mut() {
                        chunk.id = real.id;
                    }
                }
            }
        }
        fx.snapshots.commit(&manifest).expect("commit");

        let found = details(&fx.snapshots, &fx.chunks, &manifest.id).expect("details");
        assert_eq!(found.roots[0].skipped, 3);
        assert_eq!(found.roots[1].skipped, 1);
    }

    #[test]
    fn details_of_an_unknown_or_damaged_snapshot_fail() {
        let fx = Fixture::new();
        let broken = fx.commit("aa", 100, &[]);
        fx.corrupt(&broken);
        let unknown = "bb".repeat(32).parse().expect("valid id");

        let err = details(&fx.snapshots, &fx.chunks, &unknown).expect_err("must fail");
        assert!(matches!(err, Error::SnapshotNotFound(_)), "{err}");
        let err = details(&fx.snapshots, &fx.chunks, &broken.id).expect_err("must fail");
        assert!(!matches!(err, Error::SnapshotNotFound(_)), "{err}");
    }
}
