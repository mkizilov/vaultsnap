//! Committing, loading, and listing snapshots.

use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

use super::codec;
use super::id::{SnapshotId, SnapshotRef};
use super::model::{Manifest, NodeKind, SnapshotSummary};
use super::object::{self, MAX_OBJECT_BYTES};
use crate::blocking::blocking;
use crate::chunk_store::{ChunkId, ChunkStore};
use crate::crypto::{KeyPurpose, SecretKey};
use crate::error::{Error, Result};
use crate::keys::RepositoryKeys;
use crate::repository::RepositoryLayout;
use crate::store::Store;

/// One snapshot in a listing: its id and either its summary or the reason it could not be read.
/// A damaged snapshot is reported, never silently left out and never presented as good.
#[derive(Debug)]
pub struct SnapshotListing {
    /// The snapshot id (the file name).
    pub id: SnapshotId,
    /// The verified summary, or why verification failed.
    pub summary: Result<SnapshotSummary>,
}

/// Reads and writes the encrypted snapshot manifests of one repository.
///
/// * **Atomic commit.** A snapshot becomes visible only when its file is published, in a single
///   no-overwrite step, after the whole manifest has been built, validated, and encrypted, and
///   after every chunk it refers to has been confirmed present. A crash or error at any earlier
///   point leaves no snapshot behind, only (at worst) an ignored temporary file and unreferenced
///   chunks. Snapshots are immutable and never overwritten.
/// * **Verified loading.** [`SnapshotStore::load`] authenticates the file against its id,
///   decrypts it, decodes and validates the manifest, and checks that the manifest names the same
///   snapshot and repository as the file does. Historical snapshots load through the same path;
///   unknown versions are reported as such.
///
/// The store is cheap to clone and safe to share between threads.
#[derive(Clone)]
pub struct SnapshotStore {
    store: Store,
    key: Arc<SecretKey>,
    chunks: ChunkStore,
    repository_id: String,
}

impl SnapshotStore {
    /// A snapshot store over the repository directory of `store` for the repository
    /// `repository_id`, using keys derived from the unlocked repository keys. Performs no I/O.
    pub fn new(store: Store, keys: &RepositoryKeys, repository_id: &str) -> Result<Self> {
        Ok(Self {
            chunks: ChunkStore::new(store.clone(), keys)?,
            key: Arc::new(keys.derive(KeyPurpose::MetadataEncryption)?),
            store,
            repository_id: repository_id.to_owned(),
        })
    }

    /// The chunks referenced by `manifest` that are not in the chunk store, each listed once.
    pub fn missing_chunks(&self, manifest: &Manifest) -> Result<Vec<ChunkId>> {
        let mut seen = HashSet::new();
        let mut missing = Vec::new();
        let files = manifest
            .roots
            .iter()
            .flat_map(|root| root.entries.iter())
            .filter_map(|entry| match &entry.kind {
                NodeKind::File { chunks } => Some(chunks),
                _ => None,
            });
        for chunks in files {
            for chunk in chunks {
                if seen.insert(chunk.id) && !self.chunks.contains(&chunk.id)? {
                    missing.push(chunk.id);
                }
            }
        }
        Ok(missing)
    }

    /// Commits `manifest` as a new snapshot.
    ///
    /// Fails, leaving nothing behind, if the manifest is not well formed, belongs to another
    /// repository, refers to chunks that are not in the repository
    /// ([`Error::IncompleteSnapshot`]), or if a snapshot with this id already exists
    /// ([`Error::SnapshotExists`]).
    pub fn commit(&self, manifest: &Manifest) -> Result<()> {
        manifest.validate()?;
        if manifest.repository_id != self.repository_id {
            return Err(Error::InvalidManifest(
                "the manifest belongs to a different repository".to_owned(),
            ));
        }
        let missing = self.missing_chunks(manifest)?;
        if let Some(first) = missing.first() {
            return Err(Error::IncompleteSnapshot {
                missing: missing.len(),
                first: first.to_hex(),
            });
        }
        let body = codec::encode(manifest)?;
        let file = object::seal(&self.key, &manifest.id, &body)?;
        match self.store.write_new(&manifest.id.object_name(), &file) {
            Ok(()) => Ok(()),
            Err(Error::AlreadyExists { .. }) => Err(Error::SnapshotExists(manifest.id.to_string())),
            Err(err) => Err(err),
        }
    }

    /// Loads and fully verifies the snapshot `id`.
    pub fn load(&self, id: &SnapshotId) -> Result<Manifest> {
        let corrupt = |reason: &str| Error::CorruptSnapshot {
            id: id.to_string(),
            reason: reason.to_owned(),
        };
        let file = match self.store.read(&id.object_name(), MAX_OBJECT_BYTES) {
            Ok(file) => file,
            Err(err) if err.is_not_found() => return Err(Error::SnapshotNotFound(id.to_string())),
            Err(Error::TooLarge { .. }) => return Err(corrupt("larger than any valid snapshot")),
            Err(err) => return Err(err),
        };
        let body = object::open(&self.key, id, &file)?;
        let manifest = codec::decode(&body)?;
        if manifest.id != *id {
            return Err(corrupt(
                "the manifest inside records a different snapshot id",
            ));
        }
        if manifest.repository_id != self.repository_id {
            return Err(corrupt(
                "the manifest inside belongs to a different repository",
            ));
        }
        Ok(manifest)
    }

    /// The ids of all committed snapshots, in ascending order. Temporary files left by an
    /// interrupted commit and files that are not named like a snapshot id are ignored. This
    /// reads names only; it does not verify the snapshots.
    pub fn ids(&self) -> Result<Vec<SnapshotId>> {
        let names = self.store.list(RepositoryLayout::SNAPSHOTS_DIR)?;
        Ok(names
            .iter()
            .filter(|name| !Store::is_temp_name(name))
            .filter_map(|name| name.parse::<SnapshotId>().ok())
            .collect())
    }

    /// Deletes the snapshot file `id`. Only a prune may call this.
    pub(crate) fn remove(&self, id: &SnapshotId) -> Result<()> {
        self.store.remove(&id.object_name())
    }

    /// Makes the removal of snapshot files durable (see [`Store::sync_directory`]).
    pub(crate) fn sync_removals(&self) -> Result<()> {
        self.store.sync_directory(RepositoryLayout::SNAPSHOTS_DIR)
    }

    /// The snapshot a reference names: a full id as is, or the one snapshot whose id starts with
    /// the given text. Reads file names only, so the result is a candidate; [`SnapshotStore::load`]
    /// is what authenticates it. Fails with [`Error::SnapshotNotFound`] if nothing matches and
    /// [`Error::AmbiguousSnapshot`] if several snapshots do.
    pub fn resolve(&self, reference: &SnapshotRef) -> Result<SnapshotId> {
        if reference.is_full() {
            return reference.as_str().parse();
        }
        let mut matching: Vec<SnapshotId> = self
            .ids()?
            .into_iter()
            .filter(|id| id.as_str().starts_with(reference.as_str()))
            .collect();
        match matching.len() {
            0 => Err(Error::SnapshotNotFound(reference.to_string())),
            1 => Ok(matching.remove(0)),
            count => Err(Error::AmbiguousSnapshot {
                prefix: reference.to_string(),
                count,
            }),
        }
    }

    /// The ids of all committed snapshots, newest file first (ties broken by id). The order
    /// comes from the files' modification times, which is when each snapshot was committed; it
    /// is a hint for choosing a likely parent for an incremental backup, never something to
    /// trust: a snapshot's authentic creation time is inside its manifest.
    pub fn ids_newest_first(&self) -> Result<Vec<SnapshotId>> {
        let mut dated = Vec::new();
        for id in self.ids()? {
            match self.store.modified(&id.object_name()) {
                Ok(time) => dated.push((time, id)),
                Err(err) if err.is_not_found() => {}
                Err(err) => return Err(err),
            }
        }
        dated.sort_by(|(a_time, a_id), (b_time, b_id)| {
            b_time.cmp(a_time).then_with(|| a_id.cmp(b_id))
        });
        Ok(dated.into_iter().map(|(_, id)| id).collect())
    }

    /// Loads and verifies every snapshot, returning a summary or the reason for each. Snapshots
    /// are decrypted in full, so this costs time proportional to their total size.
    pub fn list(&self) -> Result<Vec<SnapshotListing>> {
        Ok(self
            .ids()?
            .into_iter()
            .map(|id| {
                let summary = self.load(&id).map(|manifest| manifest.summary());
                SnapshotListing { id, summary }
            })
            .collect())
    }

    /// [`SnapshotStore::commit`] on Tokio's blocking pool.
    pub async fn commit_async(&self, manifest: Manifest) -> Result<()> {
        let store = self.clone();
        blocking(move || store.commit(&manifest)).await
    }

    /// [`SnapshotStore::load`] on Tokio's blocking pool.
    pub async fn load_async(&self, id: SnapshotId) -> Result<Manifest> {
        let store = self.clone();
        blocking(move || store.load(&id)).await
    }
}

#[cfg(test)]
impl SnapshotStore {
    /// Seals and publishes `manifest` exactly as [`SnapshotStore::commit`] does, but without
    /// validating it or checking that its chunks exist, so that tests can plant authentic
    /// snapshots that a correct writer would never produce.
    pub(crate) fn commit_unchecked(&self, manifest: &Manifest) -> Result<()> {
        let body = codec::encode(manifest)?;
        let file = object::seal(&self.key, &manifest.id, &body)?;
        self.store.write_new(&manifest.id.object_name(), &file)
    }
}

impl fmt::Debug for SnapshotStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SnapshotStore")
            .field("root", &self.store.root())
            .field("repository_id", &self.repository_id)
            .field("keys", &"<redacted>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::super::model::testing::{minimal, rich, REPO_ID};
    use super::super::model::ChunkRef;
    use super::*;
    use crate::scan::Timestamp;

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
            let chunks = ChunkStore::new(local.clone(), &keys).expect("chunk store");
            let snapshots = SnapshotStore::new(local, &keys, REPO_ID).expect("snapshot store");
            Self {
                dir,
                chunks,
                snapshots,
            }
        }

        fn path_of(&self, id: &SnapshotId) -> PathBuf {
            self.dir.path().join(id.object_name())
        }

        /// Hand-built manifests use made-up chunk ids that are not hashes of any content. This
        /// stores a real chunk (filled with the first byte of the made-up id) for each distinct
        /// made-up id and rewrites the manifest to refer to the real chunk ids.
        fn manifest_with_real_chunks(&self, mut manifest: Manifest) -> Manifest {
            let mut contents = std::collections::HashMap::new();
            for root in &mut manifest.roots {
                for entry in &mut root.entries {
                    if let NodeKind::File { chunks } = &mut entry.kind {
                        for chunk in chunks.iter_mut() {
                            let len = usize::try_from(chunk.len).expect("small");
                            let data = contents
                                .entry(chunk.id)
                                .or_insert_with(|| vec![chunk.id.as_bytes()[0]; len])
                                .clone();
                            let put = self.chunks.put(&data).expect("put chunk");
                            *chunk = ChunkRef {
                                id: put.id,
                                len: chunk.len,
                            };
                        }
                    }
                }
            }
            manifest
        }

        fn committed(&self) -> Manifest {
            let manifest = self.manifest_with_real_chunks(rich());
            self.snapshots.commit(&manifest).expect("commit");
            manifest
        }
    }

    fn fresh_id(manifest: &mut Manifest) {
        manifest.id = SnapshotId::generate().expect("randomness");
    }

    #[test]
    fn a_committed_snapshot_loads_back_identically() {
        let fx = Fixture::new();
        let manifest = fx.committed();
        assert_eq!(fx.snapshots.load(&manifest.id).expect("load"), manifest);
        assert_eq!(
            fx.snapshots.ids().expect("ids"),
            std::slice::from_ref(&manifest.id)
        );
    }

    #[test]
    fn the_snapshot_file_is_named_by_its_id_and_hides_the_contents() {
        let fx = Fixture::new();
        let manifest = fx.committed();
        let raw = fs::read(fx.path_of(&manifest.id)).expect("read file");
        for secret in [
            &b"report.txt"[..],
            b"/home/me/Documents",
            b"workstation",
            b"laptop",
            b"unicode-",
            b"docs",
        ] {
            let found = raw.windows(secret.len()).any(|window| window == secret);
            assert!(
                !found,
                "{:?} is visible in the snapshot file",
                String::from_utf8_lossy(secret)
            );
        }
        assert!(fx.path_of(&manifest.id).ends_with(manifest.id.as_str()));
    }

    #[test]
    fn historical_snapshots_stay_loadable_next_to_new_ones() {
        let fx = Fixture::new();
        let mut ids = Vec::new();
        for round in 0..5_i64 {
            let mut manifest = fx.manifest_with_real_chunks(rich());
            fresh_id(&mut manifest);
            manifest.created = Timestamp {
                secs: 1_000 + round,
                nanos: 0,
            };
            fx.snapshots.commit(&manifest).expect("commit");
            ids.push(manifest.id.clone());
        }
        ids.sort();
        assert_eq!(fx.snapshots.ids().expect("ids"), ids);
        for id in &ids {
            assert_eq!(&fx.snapshots.load(id).expect("load").id, id);
        }
        let listing = fx.snapshots.list().expect("list");
        assert_eq!(listing.len(), 5);
        assert!(listing.iter().all(|entry| entry.summary.is_ok()));
    }

    #[test]
    fn a_snapshot_is_never_overwritten() {
        let fx = Fixture::new();
        let manifest = fx.committed();
        let before = fs::read(fx.path_of(&manifest.id)).expect("read");
        let mut changed = manifest.clone();
        changed.host = "someone else".to_owned();
        let err = fx.snapshots.commit(&changed).expect_err("must fail");
        assert!(matches!(err, Error::SnapshotExists(_)), "{err}");
        assert_eq!(fs::read(fx.path_of(&manifest.id)).expect("read"), before);
        assert_eq!(
            fx.snapshots.load(&manifest.id).expect("load").host,
            "workstation"
        );
    }

    #[test]
    fn a_snapshot_with_missing_chunks_is_not_committed() {
        let fx = Fixture::new();
        // The hand-built ids refer to chunks that were never stored.
        let manifest = rich();
        let err = fx.snapshots.commit(&manifest).expect_err("must fail");
        match err {
            Error::IncompleteSnapshot { missing, .. } => assert_eq!(missing, 3),
            other => panic!("expected IncompleteSnapshot, got {other}"),
        }
        assert!(fx.snapshots.ids().expect("ids").is_empty());
        assert!(fs::read_dir(fx.dir.path().join("snapshots"))
            .expect("read_dir")
            .next()
            .is_none());
        assert!(matches!(
            fx.snapshots.load(&manifest.id),
            Err(Error::SnapshotNotFound(_))
        ));
    }

    #[test]
    fn missing_chunks_are_listed_once_each() {
        let fx = Fixture::new();
        let manifest = rich();
        let missing = fx.snapshots.missing_chunks(&manifest).expect("check");
        assert_eq!(missing.len(), 3, "{missing:?}");
        let ready = fx.manifest_with_real_chunks(rich());
        assert!(fx
            .snapshots
            .missing_chunks(&ready)
            .expect("check")
            .is_empty());
    }

    #[test]
    fn invalid_manifests_and_foreign_repositories_are_refused_before_anything_is_written() {
        let fx = Fixture::new();
        let mut invalid = fx.manifest_with_real_chunks(minimal());
        invalid.stats.files = 5;
        assert!(matches!(
            fx.snapshots.commit(&invalid),
            Err(Error::InvalidManifest(_))
        ));

        let mut foreign = fx.manifest_with_real_chunks(minimal());
        foreign.repository_id = "fedcba9876543210fedcba9876543210".to_owned();
        assert!(matches!(
            fx.snapshots.commit(&foreign),
            Err(Error::InvalidManifest(_))
        ));

        let mut future = fx.manifest_with_real_chunks(minimal());
        future.format_version = 2;
        assert!(matches!(
            fx.snapshots.commit(&future),
            Err(Error::UnsupportedManifestVersion { found: 2, .. })
        ));
        assert!(fx.snapshots.ids().expect("ids").is_empty());
    }

    #[test]
    fn an_interrupted_commit_leaves_no_snapshot_behind() {
        let fx = Fixture::new();
        // A crash while the file is being staged leaves a temporary file with part of the data.
        let manifest = fx.manifest_with_real_chunks(rich());
        let body = codec::encode(&manifest).expect("encode");
        let complete = object::seal(&fx.snapshots.key, &manifest.id, &body).expect("seal");
        let stray = fx
            .dir
            .path()
            .join("snapshots")
            .join(".tmp-0123456789abcdef");
        fs::write(&stray, &complete[..complete.len() / 2]).expect("write partial file");

        assert!(fx.snapshots.ids().expect("ids").is_empty());
        assert!(fx.snapshots.list().expect("list").is_empty());
        assert!(matches!(
            fx.snapshots.load(&manifest.id),
            Err(Error::SnapshotNotFound(_))
        ));

        // The same snapshot can then be committed normally, next to the leftover.
        fx.snapshots.commit(&manifest).expect("commit");
        assert_eq!(
            fx.snapshots.ids().expect("ids"),
            std::slice::from_ref(&manifest.id)
        );
        assert!(stray.exists(), "leftovers are not touched");
    }

    #[test]
    fn a_failed_commit_leaves_no_temporary_file() {
        let fx = Fixture::new();
        let manifest = fx.committed();
        let _ = fx.snapshots.commit(&manifest).expect_err("duplicate");
        let names: Vec<_> = fs::read_dir(fx.dir.path().join("snapshots"))
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, [manifest.id.to_string()]);
    }

    #[test]
    fn damaged_snapshot_files_are_detected_and_reported_not_hidden() {
        let fx = Fixture::new();
        let good = fx.committed();
        let mut other = fx.manifest_with_real_chunks(rich());
        fresh_id(&mut other);
        fx.snapshots.commit(&other).expect("commit");

        let path = fx.path_of(&other.id);
        let original = fs::read(&path).expect("read");
        let damaged_variants: Vec<(&str, Vec<u8>)> = vec![
            ("empty", Vec::new()),
            ("garbage", b"this is not a snapshot".to_vec()),
            ("truncated", original[..original.len() / 2].to_vec()),
            ("one byte short", original[..original.len() - 1].to_vec()),
            ("extended", [original.clone(), vec![0]].concat()),
            ("flipped", {
                let mut copy = original.clone();
                let middle = copy.len() / 2;
                copy[middle] ^= 0x40;
                copy
            }),
        ];
        for (label, bytes) in damaged_variants {
            fs::write(&path, &bytes).expect("write damaged file");
            let result = fx.snapshots.load(&other.id);
            assert!(
                matches!(result, Err(Error::CorruptSnapshot { .. })),
                "{label}: {result:?}"
            );
            let listing = fx.snapshots.list().expect("list still works");
            assert_eq!(listing.len(), 2, "{label}: both snapshots are listed");
            let by_id = |id: &SnapshotId| listing.iter().find(|l| &l.id == id).expect("listed");
            assert!(
                by_id(&good.id).summary.is_ok(),
                "{label}: the good one is unaffected"
            );
            assert!(
                by_id(&other.id).summary.is_err(),
                "{label}: the damaged one is flagged"
            );
        }
        fs::write(&path, &original).expect("restore");
        fx.snapshots.load(&other.id).expect("healthy again");
    }

    #[test]
    fn a_snapshot_filed_under_another_id_is_rejected() {
        let fx = Fixture::new();
        let a = fx.committed();
        let mut b = fx.manifest_with_real_chunks(rich());
        fresh_id(&mut b);
        fx.snapshots.commit(&b).expect("commit");
        // Swap B's file in under A's name, as a malicious or confused storage layer might.
        fs::copy(fx.path_of(&b.id), fx.path_of(&a.id)).expect("copy over");
        assert!(matches!(
            fx.snapshots.load(&a.id),
            Err(Error::CorruptSnapshot { .. })
        ));
        assert_eq!(fx.snapshots.load(&b.id).expect("load").id, b.id);
    }

    #[test]
    fn the_wrong_keys_cannot_read_a_snapshot() {
        let fx = Fixture::new();
        let manifest = fx.committed();
        let intruder_keys = RepositoryKeys::generate().expect("generate");
        let intruder = SnapshotStore::new(Store::local(fx.dir.path()), &intruder_keys, REPO_ID)
            .expect("store");
        assert!(matches!(
            intruder.load(&manifest.id),
            Err(Error::CorruptSnapshot { .. })
        ));
    }

    #[test]
    fn a_snapshot_of_another_repository_is_rejected_even_if_authentic() {
        let fx = Fixture::new();
        let mut manifest = fx.manifest_with_real_chunks(minimal());
        manifest.repository_id = "fedcba9876543210fedcba9876543210".to_owned();
        let body = codec::encode(&manifest).expect("encode");
        let file = object::seal(&fx.snapshots.key, &manifest.id, &body).expect("seal");
        fs::write(fx.path_of(&manifest.id), file).expect("plant");
        assert!(matches!(
            fx.snapshots.load(&manifest.id),
            Err(Error::CorruptSnapshot { .. })
        ));
    }

    #[test]
    fn an_authentic_manifest_recording_a_different_id_is_rejected() {
        let fx = Fixture::new();
        let manifest = fx.manifest_with_real_chunks(minimal());
        let claimed = SnapshotId::generate().expect("randomness");
        let body = codec::encode(&manifest).expect("encode");
        let file = object::seal(&fx.snapshots.key, &claimed, &body).expect("seal");
        fs::write(fx.path_of(&claimed), file).expect("plant");
        let err = fx.snapshots.load(&claimed).expect_err("must fail");
        assert!(matches!(err, Error::CorruptSnapshot { .. }), "{err}");
    }

    #[test]
    fn newer_manifest_versions_inside_an_authentic_file_are_reported_as_unsupported() {
        let fx = Fixture::new();
        let manifest = fx.manifest_with_real_chunks(minimal());
        let mut body = codec::encode(&manifest).expect("encode").to_vec();
        body[..4].copy_from_slice(&2_u32.to_le_bytes());
        let file = object::seal(&fx.snapshots.key, &manifest.id, &body).expect("seal");
        fs::write(fx.path_of(&manifest.id), file).expect("plant");
        let err = fx.snapshots.load(&manifest.id).expect_err("must fail");
        assert!(
            matches!(
                err,
                Error::UnsupportedManifestVersion {
                    found: 2,
                    supported: 1
                }
            ),
            "{err}"
        );
        let listing = fx.snapshots.list().expect("list");
        assert!(matches!(
            listing[0].summary,
            Err(Error::UnsupportedManifestVersion { found: 2, .. })
        ));
    }

    #[test]
    fn newer_container_versions_are_reported_as_unsupported() {
        let fx = Fixture::new();
        let manifest = fx.committed();
        let path = fx.path_of(&manifest.id);
        let mut file = fs::read(&path).expect("read");
        file[4] = 2;
        fs::write(&path, file).expect("write");
        let err = fx.snapshots.load(&manifest.id).expect_err("must fail");
        assert!(
            matches!(
                err,
                Error::UnsupportedSnapshotVersion {
                    found: 2,
                    supported: 1
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn an_authentic_but_malformed_manifest_is_an_error_not_a_panic() {
        let fx = Fixture::new();
        let manifest = fx.manifest_with_real_chunks(minimal());
        let mut body = codec::encode(&manifest).expect("encode").to_vec();
        body.truncate(body.len() - 10);
        let file = object::seal(&fx.snapshots.key, &manifest.id, &body).expect("seal");
        fs::write(fx.path_of(&manifest.id), file).expect("plant");
        let err = fx.snapshots.load(&manifest.id).expect_err("must fail");
        assert!(matches!(err, Error::InvalidManifest(_)), "{err}");
    }

    #[test]
    fn stray_files_in_the_snapshots_directory_are_ignored() {
        let fx = Fixture::new();
        let manifest = fx.committed();
        let dir = fx.dir.path().join("snapshots");
        fs::write(dir.join("README"), b"hello").expect("write");
        fs::write(dir.join("not-an-id"), b"hello").expect("write");
        fs::write(dir.join("ABCDEF".repeat(11)), b"hello").expect("write");
        fs::write(dir.join(".tmp-abc"), b"hello").expect("write");
        assert_eq!(
            fx.snapshots.ids().expect("ids"),
            std::slice::from_ref(&manifest.id)
        );
        assert_eq!(fx.snapshots.list().expect("list").len(), 1);
    }

    #[test]
    fn ids_can_be_listed_newest_first() {
        let fx = Fixture::new();
        let mut committed = Vec::new();
        for _ in 0..3 {
            let mut manifest = fx.manifest_with_real_chunks(minimal());
            fresh_id(&mut manifest);
            fx.snapshots.commit(&manifest).expect("commit");
            let file = fs::File::options()
                .write(true)
                .open(fx.path_of(&manifest.id))
                .expect("open");
            let age = std::time::Duration::from_secs(1000 * (committed.len() as u64 + 1));
            file.set_modified(std::time::SystemTime::now() - age)
                .expect("set mtime");
            committed.push(manifest.id);
        }
        // The first commit was given the smallest age, so it is the newest.
        assert_eq!(fx.snapshots.ids_newest_first().expect("ids"), committed);
    }

    #[test]
    fn a_snapshot_can_be_removed_and_only_that_one() {
        let fx = Fixture::new();
        let first = commit_with_id(&fx, &"aa".repeat(32));
        let second = commit_with_id(&fx, &"bb".repeat(32));
        fx.snapshots.remove(&first).expect("remove");
        fx.snapshots.sync_removals().expect("sync");
        assert_eq!(
            fx.snapshots.ids().expect("ids"),
            std::slice::from_ref(&second)
        );
        assert!(matches!(
            fx.snapshots.load(&first),
            Err(Error::SnapshotNotFound(_))
        ));
        assert!(fx.snapshots.load(&second).is_ok());
        assert!(fx
            .snapshots
            .remove(&first)
            .expect_err("already gone")
            .is_not_found());
    }

    fn commit_with_id(fx: &Fixture, id: &str) -> SnapshotId {
        let mut manifest = fx.manifest_with_real_chunks(minimal());
        manifest.id = id.parse().expect("valid id");
        fx.snapshots.commit(&manifest).expect("commit");
        manifest.id
    }

    #[test]
    fn references_resolve_by_unique_prefix() {
        let fx = Fixture::new();
        let first = commit_with_id(&fx, &"aa".repeat(32));
        let second = commit_with_id(&fx, &format!("ab{}", "c".repeat(62)));
        let resolve = |text: &str| fx.snapshots.resolve(&text.parse().expect("syntax"));

        assert_eq!(resolve("aaaa").expect("unique"), first);
        assert_eq!(resolve(&"aa".repeat(32)).expect("full id"), first);
        assert_eq!(resolve("abcc").expect("unique"), second);
        assert_eq!(
            resolve(&format!("ab{}", "c".repeat(30))).expect("unique"),
            second
        );
    }

    #[test]
    fn ambiguous_and_unknown_references_are_reported() {
        let fx = Fixture::new();
        commit_with_id(&fx, &format!("abcd{}", "0".repeat(60)));
        let second = commit_with_id(&fx, &format!("abcd{}", "1".repeat(60)));
        let resolve = |text: &str| fx.snapshots.resolve(&text.parse().expect("syntax"));

        match resolve("abcd") {
            Err(Error::AmbiguousSnapshot { prefix, count }) => {
                assert_eq!(prefix, "abcd");
                assert_eq!(count, 2);
            }
            other => panic!("expected an ambiguity: {other:?}"),
        }
        assert_eq!(resolve("abcd1").expect("unique"), second);
        assert!(matches!(resolve("bbbb"), Err(Error::SnapshotNotFound(_))));
    }

    #[test]
    fn a_full_reference_is_only_checked_against_the_repository_when_loaded() {
        let fx = Fixture::new();
        let unknown = "0f".repeat(32);
        let id = fx
            .snapshots
            .resolve(&unknown.parse().expect("syntax"))
            .expect("full id");
        assert!(matches!(
            fx.snapshots.load(&id),
            Err(Error::SnapshotNotFound(_))
        ));
    }

    #[test]
    fn unknown_ids_are_reported_as_not_found() {
        let fx = Fixture::new();
        let id = SnapshotId::generate().expect("randomness");
        assert!(matches!(
            fx.snapshots.load(&id),
            Err(Error::SnapshotNotFound(_))
        ));
    }

    #[test]
    fn concurrent_commits_of_different_snapshots_all_succeed() {
        let fx = Fixture::new();
        let base = fx.manifest_with_real_chunks(rich());
        let ids: Vec<SnapshotId> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let (store, base) = (&fx.snapshots, &base);
                    scope.spawn(move || {
                        let mut manifest = base.clone();
                        fresh_id(&mut manifest);
                        store.commit(&manifest).expect("commit");
                        manifest.id
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("thread panicked"))
                .collect()
        });
        let mut expected = ids;
        expected.sort();
        assert_eq!(fx.snapshots.ids().expect("ids"), expected);
        assert!(fx
            .snapshots
            .list()
            .expect("list")
            .iter()
            .all(|l| l.summary.is_ok()));
    }

    #[test]
    fn concurrent_commits_of_the_same_snapshot_succeed_exactly_once() {
        let fx = Fixture::new();
        let manifest = fx.manifest_with_real_chunks(rich());
        let results: Vec<Result<()>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let (store, manifest) = (&fx.snapshots, &manifest);
                    scope.spawn(move || store.commit(manifest))
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("thread panicked"))
                .collect()
        });
        assert_eq!(
            results.iter().filter(|r| r.is_ok()).count(),
            1,
            "{results:?}"
        );
        for result in results.iter().filter_map(|r| r.as_ref().err()) {
            assert!(matches!(result, Error::SnapshotExists(_)), "{result}");
        }
        assert_eq!(fx.snapshots.load(&manifest.id).expect("load"), manifest);
        let names = fs::read_dir(fx.dir.path().join("snapshots"))
            .expect("read_dir")
            .count();
        assert_eq!(names, 1, "no temporary files remain");
    }

    #[tokio::test]
    async fn the_async_wrappers_commit_and_load() {
        let fx = Fixture::new();
        let manifest = fx.manifest_with_real_chunks(rich());
        fx.snapshots
            .commit_async(manifest.clone())
            .await
            .expect("commit");
        let loaded = fx
            .snapshots
            .load_async(manifest.id.clone())
            .await
            .expect("load");
        assert_eq!(loaded, manifest);
    }

    #[test]
    fn debug_output_hides_the_keys() {
        let fx = Fixture::new();
        let shown = format!("{:?}", fx.snapshots);
        assert!(shown.contains("redacted"), "{shown}");
    }
}
