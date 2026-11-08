//! How much a snapshot holds and how much of that is stored only for it.
//!
//! Two different questions are answered from the same data:
//!
//! * the **logical size** of a snapshot is what its files add up to, which is what a restore
//!   would write;
//! * the **stored size** is what its *distinct chunks* occupy in the repository (compressed and
//!   encrypted), and its **unique stored size** is the part of that no other snapshot refers to,
//!   which is what removing the snapshot alone would free.
//!
//! Chunk sizes come from the length of each object file, so they cost one `stat` per distinct
//! chunk and nothing is decrypted. The chunk lists come from authenticated manifests.

use std::collections::HashMap;

use crate::chunk_store::{ChunkId, ChunkStore};
use crate::error::{Error, Result};
use crate::snapshot::{Manifest, NodeKind};

/// The sizes of one snapshot. See the module documentation for what each number means.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnapshotSizes {
    /// Total size of the regular files, in bytes; hard links are counted once.
    pub logical_bytes: u64,
    /// Number of distinct chunks the snapshot refers to.
    pub chunks: u64,
    /// Plaintext size of those distinct chunks: the logical size after removing data that repeats
    /// inside the snapshot.
    pub deduplicated_bytes: u64,
    /// Size of the objects holding those distinct chunks in the repository.
    pub stored_bytes: u64,
    /// How many of the distinct chunks no other known snapshot refers to.
    pub unique_chunks: u64,
    /// Size of the objects holding those chunks.
    pub unique_stored_bytes: u64,
    /// Chunks that the snapshot refers to but whose object is missing from the repository; they
    /// count as zero stored bytes. Any value above zero means the repository is damaged.
    pub missing_chunks: u64,
}

struct Usage {
    /// Plaintext length of the chunk.
    plain: u32,
    /// How many of the added snapshots refer to the chunk.
    holders: u32,
    /// Size of its object, once measured; `None` if there is none.
    stored: Option<u64>,
}

/// Counts which snapshots use which chunks, then answers size questions about each of them.
///
/// Add every snapshot that matters for "unique to this snapshot", [`measure`](Self::measure) once,
/// and ask [`sizes_of`](Self::sizes_of). Only the added snapshots are considered, so unique sizes
/// are exact only if all snapshots of the repository were added.
#[derive(Default)]
pub(crate) struct SizeLedger {
    chunks: HashMap<ChunkId, Usage>,
}

/// The distinct chunks of a manifest with their plaintext lengths.
fn distinct_chunks(manifest: &Manifest) -> HashMap<ChunkId, u32> {
    let mut distinct = HashMap::new();
    for entry in manifest.roots.iter().flat_map(|root| root.entries.iter()) {
        if let NodeKind::File { chunks } = &entry.kind {
            for chunk in chunks {
                distinct.entry(chunk.id).or_insert(chunk.len);
            }
        }
    }
    distinct
}

impl SizeLedger {
    /// Records that `manifest` refers to its chunks (each counted once for this snapshot, however
    /// often a file repeats it).
    pub(crate) fn add(&mut self, manifest: &Manifest) {
        for (id, len) in distinct_chunks(manifest) {
            let usage = self.chunks.entry(id).or_insert(Usage {
                plain: len,
                holders: 0,
                stored: None,
            });
            usage.holders = usage.holders.saturating_add(1);
        }
    }

    /// Looks up the size of every object once. A missing object is recorded as such; any other
    /// failure is an error.
    pub(crate) fn measure(&mut self, store: &ChunkStore) -> Result<()> {
        for (id, usage) in &mut self.chunks {
            usage.stored = match store.stored_len(id) {
                Ok(len) => Some(len),
                Err(Error::ChunkNotFound(_)) => None,
                Err(err) => return Err(err),
            };
        }
        Ok(())
    }

    /// The sizes of a snapshot that was added to the ledger.
    pub(crate) fn sizes_of(&self, manifest: &Manifest) -> SnapshotSizes {
        let mut sizes = SnapshotSizes {
            logical_bytes: manifest.stats.bytes,
            ..SnapshotSizes::default()
        };
        for id in distinct_chunks(manifest).keys() {
            let Some(usage) = self.chunks.get(id) else {
                continue;
            };
            let stored = usage.stored.unwrap_or(0);
            sizes.chunks += 1;
            sizes.deduplicated_bytes =
                sizes.deduplicated_bytes.saturating_add(u64::from(usage.plain));
            sizes.stored_bytes = sizes.stored_bytes.saturating_add(stored);
            if usage.stored.is_none() {
                sizes.missing_chunks += 1;
            }
            if usage.holders <= 1 {
                sizes.unique_chunks += 1;
                sizes.unique_stored_bytes = sizes.unique_stored_bytes.saturating_add(stored);
            }
        }
        sizes
    }

    /// How many distinct chunks have no object. Meaningful after [`measure`](Self::measure).
    pub(crate) fn missing(&self) -> u64 {
        self.chunks.values().filter(|usage| usage.stored.is_none()).count() as u64
    }

    /// The number of distinct chunks added so far and the size of their objects.
    pub(crate) fn totals(&self) -> (u64, u64) {
        let stored = self
            .chunks
            .values()
            .map(|usage| usage.stored.unwrap_or(0))
            .fold(0_u64, u64::saturating_add);
        (self.chunks.len() as u64, stored)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk_store::PutOutcome;
    use crate::chunking::test_util::pseudo_random;
    use crate::keys::RepositoryKeys;
    use crate::snapshot::testing::{chunk, dir, file, minimal};
    use crate::snapshot::{Manifest, ManifestEntry, Root};
    use crate::store::LocalStore;

    /// A manifest with one directory holding the given files, each a list of chunk seeds.
    fn manifest_of(files: &[&[u8]]) -> Manifest {
        let mut manifest = minimal();
        let mut entries: Vec<ManifestEntry> = vec![dir(None, b"")];
        for (index, seeds) in files.iter().enumerate() {
            let chunks = seeds.iter().map(|&seed| chunk(seed, 100)).collect();
            entries.push(file(0, format!("f{index}").as_bytes(), chunks));
        }
        manifest.roots = vec![Root {
            source: b"/r".to_vec(),
            entries,
        }];
        manifest.stats = manifest.compute_stats();
        manifest
    }

    fn measured(ledger: &mut SizeLedger, sizes: &[(u8, u64)]) {
        for (seed, stored) in sizes {
            let usage = ledger.chunks.get_mut(&ChunkId::from_bytes([*seed; 32]));
            usage.expect("chunk was added").stored = Some(*stored);
        }
    }

    #[test]
    fn a_chunk_repeated_inside_a_snapshot_is_counted_once() {
        let manifest = manifest_of(&[&[1, 1, 2], &[2]]);
        let mut ledger = SizeLedger::default();
        ledger.add(&manifest);
        measured(&mut ledger, &[(1, 40), (2, 70)]);

        let sizes = ledger.sizes_of(&manifest);
        assert_eq!(sizes.logical_bytes, 400, "four chunk references of 100 bytes each");
        assert_eq!(sizes.chunks, 2);
        assert_eq!(sizes.deduplicated_bytes, 200);
        assert_eq!(sizes.stored_bytes, 110);
        assert_eq!(sizes.unique_chunks, 2, "nothing else refers to them");
        assert_eq!(sizes.unique_stored_bytes, 110);
        assert_eq!(sizes.missing_chunks, 0);
    }

    #[test]
    fn unique_sizes_exclude_chunks_shared_with_other_snapshots() {
        let older = manifest_of(&[&[1, 2]]);
        let newer = manifest_of(&[&[2, 3]]);
        let mut ledger = SizeLedger::default();
        ledger.add(&older);
        ledger.add(&newer);
        measured(&mut ledger, &[(1, 10), (2, 20), (3, 30)]);

        let older_sizes = ledger.sizes_of(&older);
        assert_eq!((older_sizes.stored_bytes, older_sizes.unique_stored_bytes), (30, 10));
        assert_eq!(older_sizes.unique_chunks, 1);
        let newer_sizes = ledger.sizes_of(&newer);
        assert_eq!((newer_sizes.stored_bytes, newer_sizes.unique_stored_bytes), (50, 30));
        assert_eq!(ledger.totals(), (3, 60), "the shared chunk is stored once");
    }

    #[test]
    fn the_same_snapshot_added_twice_shares_everything_with_itself() {
        let manifest = manifest_of(&[&[1]]);
        let mut ledger = SizeLedger::default();
        ledger.add(&manifest);
        ledger.add(&manifest);
        measured(&mut ledger, &[(1, 10)]);
        let sizes = ledger.sizes_of(&manifest);
        assert_eq!((sizes.stored_bytes, sizes.unique_stored_bytes), (10, 0));
    }

    #[test]
    fn hard_links_and_empty_files_add_no_chunks() {
        let mut manifest = manifest_of(&[&[1]]);
        manifest.roots[0].entries.push(file(0, b"empty", Vec::new()));
        manifest.roots[0].entries.push(ManifestEntry {
            parent: Some(0),
            name: b"alias".to_vec(),
            kind: crate::snapshot::NodeKind::HardLink { target: 1 },
            attributes: crate::snapshot::testing::attributes(100),
        });
        manifest.stats = manifest.compute_stats();
        manifest.validate().expect("valid");

        let mut ledger = SizeLedger::default();
        ledger.add(&manifest);
        measured(&mut ledger, &[(1, 10)]);
        let sizes = ledger.sizes_of(&manifest);
        assert_eq!(sizes.logical_bytes, 100, "the hard link is not a second copy");
        assert_eq!((sizes.chunks, sizes.stored_bytes), (1, 10));
    }

    #[test]
    fn a_snapshot_without_files_has_no_sizes() {
        let manifest = minimal();
        let mut ledger = SizeLedger::default();
        ledger.add(&manifest);
        assert_eq!(ledger.sizes_of(&manifest), SnapshotSizes::default());
        assert_eq!(ledger.totals(), (0, 0));
    }

    #[test]
    fn measuring_reads_object_lengths_and_notices_missing_objects() {
        let dir = tempfile::tempdir().expect("create temporary directory");
        LocalStore::new(dir.path()).create_dir("data").expect("mkdir");
        let keys = RepositoryKeys::generate().expect("generate");
        let store = ChunkStore::new(LocalStore::new(dir.path()), &keys).expect("chunk store");

        let stored = pseudo_random(3000, 1);
        let put = store.put(&stored).expect("put");
        let PutOutcome::Stored { object_len, .. } = put.outcome else {
            panic!("the chunk is new");
        };
        let missing = store.chunk_id(b"never stored");

        let mut manifest = manifest_of(&[&[1]]);
        let NodeKind::File { chunks } = &mut manifest.roots[0].entries[1].kind else {
            panic!("a file");
        };
        chunks.clear();
        chunks.push(crate::snapshot::ChunkRef { id: put.id, len: 3000 });
        chunks.push(crate::snapshot::ChunkRef { id: missing, len: 100 });
        manifest.roots[0].entries[1].attributes.size = 3100;
        manifest.stats = manifest.compute_stats();

        let mut ledger = SizeLedger::default();
        ledger.add(&manifest);
        ledger.measure(&store).expect("measure");
        let sizes = ledger.sizes_of(&manifest);
        assert_eq!(sizes.stored_bytes, object_len);
        assert_eq!(sizes.missing_chunks, 1);
        assert_eq!(sizes.chunks, 2);
    }
}
