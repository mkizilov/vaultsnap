//! The chunk store: content-addressed, deduplicating, encrypted, atomic.

use std::fmt;
use std::sync::Arc;

use super::id::ChunkId;
use super::object::{
    self, ChunkKeys, Compression, ObjectInfo, HEADER_LEN, MAX_CHUNK_BYTES, MAX_OBJECT_BYTES,
};
use crate::blocking::blocking;
use crate::error::{Error, Result};
use crate::keys::RepositoryKeys;
use crate::repository::RepositoryLayout;
use crate::store::{DirEntryKind, LocalStore};

/// What `put` did with a chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutOutcome {
    /// The chunk was new and a new object was written.
    Stored {
        /// Size of the object file in bytes.
        object_len: u64,
        /// How the payload was stored.
        compression: Compression,
    },
    /// An object for this chunk already existed (possibly written a moment ago by a concurrent
    /// call), so nothing was written.
    Duplicate,
}

/// The id of a chunk and what happened when it was stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PutResult {
    /// The chunk's id, whether or not it was new.
    pub id: ChunkId,
    /// Whether an object was written.
    pub outcome: PutOutcome,
}

impl PutResult {
    /// True if this call wrote a new object.
    pub fn is_new(&self) -> bool {
        matches!(self.outcome, PutOutcome::Stored { .. })
    }
}

/// Encrypted, content-addressed storage of chunks inside a repository's `data/` directory.
///
/// * **Addressing**: a chunk is named by the keyed BLAKE3 hash of its plaintext ([`ChunkId`]), so
///   identical chunks have the same id and are stored once; without the repository key ids
///   cannot be computed, so stored data cannot be matched against known files.
/// * **Objects**: each chunk is optionally zstd-compressed, then sealed with XChaCha20-Poly1305
///   under a repository key with a fresh random nonce; the object header and the chunk id are
///   authenticated. See `docs/FORMAT.md`.
/// * **Atomic**: an object is staged in a temporary file, flushed, and published with a single
///   no-overwrite step, so readers only ever see complete objects and concurrent writers of the
///   same chunk cannot damage each other: exactly one wins and the others report
///   [`PutOutcome::Duplicate`].
/// * **Verified on read**: [`ChunkStore::get`] authenticates, decrypts, decompresses within the
///   authenticated length, and checks that the plaintext hashes to the requested id.
///
/// `put` decides whether a chunk is a duplicate by the presence of its object; it does not
/// re-verify the existing object. Damage to an object is found when it is read (or by a
/// repository check), not hidden by deduplication.
///
/// The store is cheap to clone and safe to share between threads. The methods block on file I/O;
/// from async code use [`ChunkStore::put_async`] and [`ChunkStore::get_async`].
#[derive(Clone)]
pub struct ChunkStore {
    store: LocalStore,
    keys: Arc<ChunkKeys>,
}

impl ChunkStore {
    /// A chunk store over the repository directory of `store`, using keys derived from the
    /// unlocked repository keys. Performs no I/O.
    pub fn new(store: LocalStore, keys: &RepositoryKeys) -> Result<Self> {
        Ok(Self {
            store,
            keys: Arc::new(ChunkKeys::derive(keys)?),
        })
    }

    /// The id `plaintext` is stored under.
    pub fn chunk_id(&self, plaintext: &[u8]) -> ChunkId {
        self.keys.chunk_id(plaintext)
    }

    /// Whether an object for `id` exists (without verifying it).
    pub fn contains(&self, id: &ChunkId) -> Result<bool> {
        self.store.exists(&id.object_name())
    }

    /// The size in bytes of the object stored for `id` (compressed and encrypted, with its
    /// header), taken from the file's metadata without reading or verifying it. Fails with
    /// [`Error::ChunkNotFound`] if there is no object.
    pub fn stored_len(&self, id: &ChunkId) -> Result<u64> {
        match self.store.file_len(&id.object_name()) {
            Err(err) if err.is_not_found() => Err(Error::ChunkNotFound(id.to_hex())),
            other => other,
        }
    }

    /// Every chunk object in the store with the size of its file, in ascending order of id.
    ///
    /// Only entries that are chunk objects by name count: a regular file named by 64 lowercase
    /// hexadecimal digits in the `data/xx/` directory its first two digits name. Anything else
    /// (temporary files, strays, links, misplaced files) is not an object and is not listed. The
    /// listing is complete or it fails; a directory that cannot be read is an error.
    pub fn objects(&self) -> Result<Vec<(ChunkId, u64)>> {
        let is_hex = |name: &str, len: usize| {
            name.len() == len && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        };
        let mut objects = Vec::new();
        for shard in self.store.list_entries(RepositoryLayout::DATA_DIR)? {
            if shard.kind != DirEntryKind::Directory || !is_hex(&shard.name, 2) {
                continue;
            }
            let dir = format!("{}/{}", RepositoryLayout::DATA_DIR, shard.name);
            for entry in self.store.list_entries(&dir)? {
                if entry.kind != DirEntryKind::File
                    || !is_hex(&entry.name, 64)
                    || !entry.name.starts_with(&shard.name)
                {
                    continue;
                }
                if let Ok(id) = entry.name.parse::<ChunkId>() {
                    objects.push((id, entry.len));
                }
            }
        }
        objects.sort_by_key(|(id, _)| *id);
        Ok(objects)
    }

    /// Deletes the object for `id`. Only a prune may call this: nothing else ever removes data.
    pub(crate) fn remove(&self, id: &ChunkId) -> Result<()> {
        self.store.remove(&id.object_name())
    }

    /// Examines the object stored for `id` without opening it: reads at most its first 12 bytes
    /// and checks everything that can be checked without a key (magic, version, header fields,
    /// and that the size of the file is possible for what the header declares). Cheap, and it
    /// reads no payload.
    ///
    /// It cannot see damage inside the encrypted payload, or a truncated compressed object that
    /// is still larger than the minimum; only [`ChunkStore::get`] can. Fails with
    /// [`Error::ChunkNotFound`] if there is no object, [`Error::CorruptObject`] if its structure is
    /// wrong, and [`Error::UnsupportedObjectVersion`] for a version this build does not read.
    pub fn peek(&self, id: &ChunkId) -> Result<ObjectInfo> {
        match self.store.read_prefix(&id.object_name(), HEADER_LEN) {
            Ok((prefix, len)) => object::inspect(id, &prefix, len),
            Err(err) if err.is_not_found() => Err(Error::ChunkNotFound(id.to_hex())),
            Err(err) => Err(err),
        }
    }

    /// Stores `plaintext` unless an object for it already exists.
    pub fn put(&self, plaintext: &[u8]) -> Result<PutResult> {
        if plaintext.len() > MAX_CHUNK_BYTES {
            return Err(Error::ChunkTooLarge {
                len: plaintext.len(),
                max: MAX_CHUNK_BYTES,
            });
        }
        let id = self.keys.chunk_id(plaintext);
        let name = id.object_name();
        let duplicate = PutResult {
            id,
            outcome: PutOutcome::Duplicate,
        };
        if self.store.exists(&name)? {
            return Ok(duplicate);
        }

        let (object, compression) = object::encode(&self.keys, &id, plaintext)?;
        self.store.create_dir(&id.shard_dir())?;
        match self.store.write_new(&name, &object) {
            Ok(()) => Ok(PutResult {
                id,
                outcome: PutOutcome::Stored {
                    object_len: object.len() as u64,
                    compression,
                },
            }),
            Err(Error::AlreadyExists { .. }) => Ok(duplicate),
            Err(err) => Err(err),
        }
    }

    /// Reads, authenticates, decrypts, and verifies the chunk stored under `id`.
    ///
    /// Fails with [`Error::ChunkNotFound`] if there is no object, and with
    /// [`Error::CorruptObject`] (or [`Error::UnsupportedObjectVersion`]) if the object is damaged,
    /// truncated, modified, misplaced, or was written under a different key.
    pub fn get(&self, id: &ChunkId) -> Result<Vec<u8>> {
        let corrupt = |reason: &str| Error::CorruptObject {
            id: id.to_hex(),
            reason: reason.to_owned(),
        };
        let object = match self.store.read(&id.object_name(), MAX_OBJECT_BYTES) {
            Ok(object) => object,
            Err(err) if err.is_not_found() => return Err(Error::ChunkNotFound(id.to_hex())),
            Err(Error::TooLarge { .. }) => {
                return Err(corrupt("larger than any valid object"));
            }
            Err(err) => return Err(err),
        };
        object::decode(&self.keys, id, &object)
    }

    /// [`ChunkStore::put`] on Tokio's blocking pool.
    pub async fn put_async(&self, plaintext: Vec<u8>) -> Result<PutResult> {
        let store = self.clone();
        blocking(move || store.put(&plaintext)).await
    }

    /// [`ChunkStore::get`] on Tokio's blocking pool.
    pub async fn get_async(&self, id: ChunkId) -> Result<Vec<u8>> {
        let store = self.clone();
        blocking(move || store.get(&id)).await
    }
}

impl fmt::Debug for ChunkStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChunkStore")
            .field("root", &self.store.root())
            .field("keys", &"<redacted>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::*;
    use crate::chunking::test_util::pseudo_random;

    struct Fixture {
        dir: TempDir,
        keys: RepositoryKeys,
        chunks: ChunkStore,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("create temporary directory");
            LocalStore::new(dir.path()).create_dir("data").expect("mkdir");
            let keys = RepositoryKeys::generate().expect("generate");
            let chunks = ChunkStore::new(LocalStore::new(dir.path()), &keys).expect("chunk store");
            Self { dir, keys, chunks }
        }

        /// Another view of the same directory, with the same keys.
        fn reopen(&self) -> ChunkStore {
            ChunkStore::new(LocalStore::new(self.dir.path()), &self.keys).expect("chunk store")
        }

        fn path_of(&self, id: &ChunkId) -> PathBuf {
            self.dir.path().join(id.object_name())
        }

        /// Every file below `data/`, as paths relative to the repository.
        fn files(&self) -> Vec<String> {
            let data = self.dir.path().join("data");
            let mut files = Vec::new();
            for shard in fs::read_dir(&data).expect("read data") {
                let shard = shard.expect("entry").path();
                for entry in fs::read_dir(&shard).expect("read shard") {
                    let entry = entry.expect("entry").path();
                    let relative = entry.strip_prefix(self.dir.path()).expect("inside");
                    files.push(relative.display().to_string());
                }
            }
            files.sort();
            files
        }
    }

    fn compressible(len: usize) -> Vec<u8> {
        b"the same sentence again and again; "
            .iter()
            .copied()
            .cycle()
            .take(len)
            .collect()
    }

    #[test]
    fn chunks_of_every_size_round_trip() {
        let fx = Fixture::new();
        for (seed, len) in [0, 1, 2, 63, 64, 1000, 65_536, 1 << 20].into_iter().enumerate() {
            let data = pseudo_random(len, seed as u64 + 1);
            let put = fx.chunks.put(&data).expect("put");
            assert!(put.is_new(), "len {len}");
            assert_eq!(put.id, fx.chunks.chunk_id(&data));
            assert!(fx.chunks.contains(&put.id).expect("contains"));
            assert_eq!(fx.chunks.get(&put.id).expect("get"), data, "len {len}");
        }
    }

    #[test]
    fn the_stored_length_is_the_size_of_the_object_file() {
        let fx = Fixture::new();
        let data = pseudo_random(5000, 7);
        let put = fx.chunks.put(&data).expect("put");
        let PutOutcome::Stored { object_len, .. } = put.outcome else {
            panic!("the chunk is new");
        };
        assert_eq!(fx.chunks.stored_len(&put.id).expect("length"), object_len);
        let on_disk = fs::metadata(fx.path_of(&put.id)).expect("metadata").len();
        assert_eq!(on_disk, object_len);
    }

    #[test]
    fn objects_are_listed_by_name_in_id_order_and_everything_else_is_ignored() {
        let fx = Fx::new();
        let mut ids: Vec<ChunkId> = (0..6_u64)
            .map(|seed| fx.chunks.put(&pseudo_random(300 + seed as usize, seed)).expect("put").id)
            .collect();
        ids.sort();
        let data = fx.dir.path().join("data");
        fs::write(data.join("loose"), b"x").expect("write");
        fs::create_dir(data.join("zz")).expect("mkdir");
        fs::write(data.join("zz").join("a".repeat(64)), b"x").expect("write");
        let shard = data.join(&ids[0].to_hex()[..2]);
        fs::write(shard.join(".tmp-0011223344556677"), b"x").expect("write");
        fs::write(shard.join("short"), b"x").expect("write");
        let misplaced = if ids[0].to_hex().starts_with("ff") { "00" } else { "ff" };
        fs::create_dir_all(data.join(misplaced)).expect("mkdir");
        fs::write(data.join(misplaced).join(ids[0].to_hex()), b"x").expect("write");

        let listed = fx.chunks.objects().expect("list");
        let listed_ids: Vec<ChunkId> = listed.iter().map(|(id, _)| *id).collect();
        assert_eq!(listed_ids, ids);
        for (id, len) in listed {
            assert_eq!(len, fx.chunks.stored_len(&id).expect("length"));
        }
    }

    #[test]
    fn an_empty_store_has_no_objects_and_a_missing_data_directory_is_an_error() {
        let fx = Fx::new();
        assert!(fx.chunks.objects().expect("list").is_empty());
        fs::remove_dir(fx.dir.path().join("data")).expect("remove");
        assert!(fx.chunks.objects().is_err(), "a listing that cannot be complete is not given");
    }

    #[test]
    fn removing_an_object_removes_exactly_that_object() {
        let fx = Fx::new();
        let first = fx.chunks.put(&pseudo_random(500, 1)).expect("put").id;
        let second = fx.chunks.put(&pseudo_random(500, 2)).expect("put").id;
        fx.chunks.remove(&first).expect("remove");
        assert!(!fx.chunks.contains(&first).expect("contains"));
        assert!(fx.chunks.contains(&second).expect("contains"));
        assert!(fx.chunks.remove(&first).expect_err("already gone").is_not_found());
    }

    #[test]
    fn peeking_reads_the_header_and_agrees_with_the_stored_length() {
        let fx = Fixture::new();
        let data = pseudo_random(5000, 9);
        let put = fx.chunks.put(&data).expect("put");
        let info = fx.chunks.peek(&put.id).expect("peek");
        assert_eq!(info.plaintext_len, 5000);
        assert_eq!(info.stored_len, fx.chunks.stored_len(&put.id).expect("length"));
        assert_eq!(info.compression, Compression::Uncompressed);
    }

    #[test]
    fn peeking_sees_a_missing_truncated_or_unsupported_object_but_not_a_flipped_bit() {
        let fx = Fixture::new();
        let id = fx.chunks.chunk_id(b"never stored");
        assert!(matches!(fx.chunks.peek(&id), Err(Error::ChunkNotFound(_))));

        let put = fx.chunks.put(&pseudo_random(5000, 10)).expect("put");
        let path = fx.path_of(&put.id);
        let original = fs::read(&path).expect("read");

        let mut flipped = original.clone();
        flipped[original.len() / 2] ^= 1;
        fs::write(&path, flipped).expect("write");
        assert!(fx.chunks.peek(&put.id).is_ok(), "a flipped payload bit needs the full read");
        assert!(matches!(fx.chunks.get(&put.id), Err(Error::CorruptObject { .. })));

        fs::write(&path, &original[..original.len() - 1]).expect("write");
        assert!(matches!(fx.chunks.peek(&put.id), Err(Error::CorruptObject { .. })));

        let mut newer = original;
        newer[4] = 9;
        fs::write(&path, newer).expect("write");
        assert!(matches!(
            fx.chunks.peek(&put.id),
            Err(Error::UnsupportedObjectVersion { found: 9, .. })
        ));
    }

    #[test]
    fn the_stored_length_of_a_missing_chunk_is_chunk_not_found() {
        let fx = Fixture::new();
        let id = fx.chunks.chunk_id(b"never stored");
        let err = fx.chunks.stored_len(&id).expect_err("must fail");
        assert!(matches!(err, Error::ChunkNotFound(_)), "{err}");
    }

    #[test]
    fn identical_chunks_are_stored_once() {
        let fx = Fixture::new();
        let data = pseudo_random(5000, 1);
        let first = fx.chunks.put(&data).expect("put");
        assert!(matches!(first.outcome, PutOutcome::Stored { .. }));
        for _ in 0..3 {
            let again = fx.chunks.put(&data).expect("put");
            assert_eq!(again.id, first.id);
            assert_eq!(again.outcome, PutOutcome::Duplicate);
        }
        assert_eq!(fx.files(), [first.id.object_name()]);
    }

    #[test]
    fn deduplication_works_across_store_handles_and_restarts() {
        let fx = Fixture::new();
        let data = pseudo_random(3000, 2);
        let first = fx.chunks.put(&data).expect("put");
        let second = fx.reopen().put(&data).expect("put");
        assert_eq!(second.id, first.id);
        assert_eq!(second.outcome, PutOutcome::Duplicate);
    }

    #[test]
    fn different_chunks_get_different_ids_and_objects() {
        let fx = Fixture::new();
        let a = fx.chunks.put(&pseudo_random(2000, 3)).expect("put");
        let b = fx.chunks.put(&pseudo_random(2000, 4)).expect("put");
        let c = fx.chunks.put(&pseudo_random(2001, 3)).expect("put");
        assert!(a.id != b.id && a.id != c.id && b.id != c.id);
        assert_eq!(fx.files().len(), 3);
    }

    #[test]
    fn objects_are_named_data_shard_id() {
        let fx = Fixture::new();
        let put = fx.chunks.put(b"hello").expect("put");
        let hex = put.id.to_hex();
        let path = fx.dir.path().join("data").join(&hex[..2]).join(&hex);
        assert!(path.is_file(), "{path:?}");
    }

    #[test]
    fn ids_are_keyed_by_the_repository_secret() {
        let fx = Fixture::new();
        let other = Fixture::new();
        let data = pseudo_random(4000, 5);
        let id = fx.chunks.chunk_id(&data);
        assert_eq!(fx.reopen().chunk_id(&data), id, "stable for one repository");
        assert_ne!(other.chunks.chunk_id(&data), id, "differs between repositories");
        assert_ne!(*id.as_bytes(), *blake3::hash(&data).as_bytes(), "not a plain hash");
    }

    #[test]
    fn compression_is_used_when_beneficial() {
        let fx = Fixture::new();
        let text = fx.chunks.put(&compressible(200_000)).expect("put");
        match text.outcome {
            PutOutcome::Stored { object_len, compression } => {
                assert_eq!(compression, Compression::Zstd);
                assert!(object_len < 10_000, "{object_len}");
            }
            PutOutcome::Duplicate => panic!("expected a new object"),
        }
        let noise = pseudo_random(50_000, 6);
        let put = fx.chunks.put(&noise).expect("put");
        match put.outcome {
            PutOutcome::Stored { object_len, compression } => {
                assert_eq!(compression, Compression::Uncompressed);
                assert_eq!(object_len, 12 + 24 + 50_000 + 16);
            }
            PutOutcome::Duplicate => panic!("expected a new object"),
        }
        assert_eq!(fx.chunks.get(&text.id).expect("get"), compressible(200_000));
        assert_eq!(fx.chunks.get(&put.id).expect("get"), noise);
    }

    #[test]
    fn stored_files_contain_no_plaintext() {
        let fx = Fixture::new();
        let marker = b"CONFIDENTIAL-CUSTOMER-RECORD-0042";
        let mut plain = pseudo_random(3000, 7);
        plain.extend_from_slice(marker);
        let mut repeated = Vec::new();
        for _ in 0..300 {
            repeated.extend_from_slice(marker);
        }
        for data in [plain, repeated] {
            let put = fx.chunks.put(&data).expect("put");
            let raw = fs::read(fx.path_of(&put.id)).expect("read object");
            let leaked = raw.windows(16).any(|w| marker.windows(16).any(|m| m == w));
            assert!(!leaked, "plaintext visible in {}", put.id);
        }
        for name in fx.files() {
            assert!(!name.contains("CONFIDENTIAL"), "{name}");
        }
    }

    #[test]
    fn the_same_chunk_encrypts_differently_each_time_it_is_written() {
        let one = Fixture::new();
        let two = Fixture::new();
        // Same keys, different directories: ids are equal, ciphertexts must not be.
        let shared = ChunkStore::new(LocalStore::new(two.dir.path()), &one.keys).expect("store");
        let data = pseudo_random(2000, 8);
        let a = one.chunks.put(&data).expect("put");
        let b = shared.put(&data).expect("put");
        assert_eq!(a.id, b.id);
        let first = fs::read(one.path_of(&a.id)).expect("read");
        let second = fs::read(two.path_of(&b.id)).expect("read");
        assert_ne!(first, second);
        assert_eq!(first.len(), second.len());
        assert_eq!(one.chunks.get(&a.id).expect("get"), data);
        assert_eq!(shared.get(&b.id).expect("get"), data);
    }

    #[test]
    fn corruption_anywhere_in_an_object_is_detected_on_read() {
        let fx = Fixture::new();
        let put = fx.chunks.put(&pseudo_random(30, 9)).expect("put");
        let path = fx.path_of(&put.id);
        let original = fs::read(&path).expect("read");
        for index in 0..original.len() {
            let mut damaged = original.clone();
            damaged[index] ^= 0x01;
            fs::write(&path, &damaged).expect("write");
            let result = fx.chunks.get(&put.id);
            assert!(
                matches!(
                    result,
                    Err(Error::CorruptObject { .. } | Error::UnsupportedObjectVersion { .. })
                ),
                "byte {index}: {result:?}"
            );
        }
        fs::write(&path, &original).expect("restore");
        assert_eq!(fx.chunks.get(&put.id).expect("get").len(), 30);
    }

    #[test]
    fn truncated_empty_and_garbage_objects_are_detected() {
        let fx = Fixture::new();
        let data = pseudo_random(4000, 10);
        let put = fx.chunks.put(&data).expect("put");
        let path = fx.path_of(&put.id);
        let original = fs::read(&path).expect("read");

        let mut cases: Vec<Vec<u8>> = vec![Vec::new(), b"VSCO".to_vec(), b"not an object".to_vec()];
        for len in [1, 12, 35, 36, 100, original.len() / 2, original.len() - 1] {
            cases.push(original[..len].to_vec());
        }
        let mut extended = original.clone();
        extended.push(0);
        cases.push(extended);
        cases.push(vec![0; original.len()]);
        for case in cases {
            fs::write(&path, &case).expect("write");
            let result = fx.chunks.get(&put.id);
            assert!(matches!(result, Err(Error::CorruptObject { .. })), "{} bytes: {result:?}", case.len());
        }
    }

    #[test]
    fn a_missing_object_is_reported_as_not_found() {
        let fx = Fixture::new();
        let missing = fx.chunks.chunk_id(b"never stored");
        assert!(!fx.chunks.contains(&missing).expect("contains"));
        assert!(matches!(fx.chunks.get(&missing), Err(Error::ChunkNotFound(_))));

        let put = fx.chunks.put(b"stored then removed").expect("put");
        fs::remove_file(fx.path_of(&put.id)).expect("remove");
        assert!(matches!(fx.chunks.get(&put.id), Err(Error::ChunkNotFound(_))));
    }

    #[test]
    fn an_object_moved_to_another_chunks_name_is_rejected() {
        let fx = Fixture::new();
        let a = fx.chunks.put(&pseudo_random(1000, 11)).expect("put");
        let b = fx.chunks.put(&pseudo_random(1000, 12)).expect("put");
        fs::copy(fx.path_of(&a.id), fx.path_of(&b.id)).expect("copy");
        assert!(matches!(fx.chunks.get(&b.id), Err(Error::CorruptObject { .. })));
        assert!(fx.chunks.get(&a.id).is_ok());
    }

    #[test]
    fn objects_cannot_be_read_with_another_repositorys_keys() {
        let fx = Fixture::new();
        let put = fx.chunks.put(&pseudo_random(1000, 13)).expect("put");
        let intruder_keys = RepositoryKeys::generate().expect("generate");
        let intruder = ChunkStore::new(LocalStore::new(fx.dir.path()), &intruder_keys).expect("store");
        assert!(matches!(intruder.get(&put.id), Err(Error::CorruptObject { .. })));
    }

    #[test]
    fn put_trusts_an_existing_object_but_get_still_reports_the_damage() {
        let fx = Fixture::new();
        let data = pseudo_random(1500, 14);
        let put = fx.chunks.put(&data).expect("put");
        let path = fx.path_of(&put.id);
        let mut damaged = fs::read(&path).expect("read");
        damaged[40] ^= 0xff;
        fs::write(&path, &damaged).expect("write");

        assert_eq!(fx.chunks.put(&data).expect("put").outcome, PutOutcome::Duplicate);
        assert!(matches!(fx.chunks.get(&put.id), Err(Error::CorruptObject { .. })));
    }

    #[test]
    fn oversized_chunks_are_refused_before_any_work() {
        let fx = Fixture::new();
        let result = fx.chunks.put(&vec![0_u8; MAX_CHUNK_BYTES + 1]);
        assert!(matches!(result, Err(Error::ChunkTooLarge { .. })));
        assert!(fx.files().is_empty());
    }

    #[test]
    fn a_repository_without_a_data_directory_fails_cleanly() {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let keys = RepositoryKeys::generate().expect("generate");
        let chunks = ChunkStore::new(LocalStore::new(dir.path()), &keys).expect("store");
        assert!(chunks.put(b"data").is_err());
        assert!(fs::read_dir(dir.path()).expect("read_dir").next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn objects_and_shard_directories_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let fx = Fixture::new();
        let put = fx.chunks.put(&pseudo_random(100, 15)).expect("put");
        let mode = |path: &std::path::Path| fs::metadata(path).expect("metadata").permissions().mode() & 0o777;
        let file = fx.path_of(&put.id);
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(file.parent().expect("shard")), 0o700);
    }

    #[test]
    fn concurrent_puts_of_one_chunk_write_exactly_one_object() {
        let fx = Fixture::new();
        let data = pseudo_random(100_000, 16);
        let results: Vec<PutResult> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..16)
                .map(|_| scope.spawn(|| fx.reopen().put(&data).expect("put")))
                .collect();
            handles.into_iter().map(|h| h.join().expect("thread panicked")).collect()
        });
        let stored = results.iter().filter(|r| r.is_new()).count();
        assert_eq!(stored, 1, "{results:?}");
        assert!(results.iter().all(|r| r.id == results[0].id));
        assert_eq!(fx.files(), [results[0].id.object_name()], "no stray temporary files");
        assert_eq!(fx.chunks.get(&results[0].id).expect("get"), data);
    }

    #[test]
    fn concurrent_puts_of_overlapping_sets_store_each_chunk_once() {
        let fx = Fixture::new();
        let chunks: Vec<Vec<u8>> = (0..24).map(|i| pseudo_random(3000 + i, 100 + i as u64)).collect();
        let stored_total: usize = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|worker| {
                    let (fx, chunks) = (&fx, &chunks);
                    scope.spawn(move || {
                        // Every worker writes every chunk, starting at a different place.
                        (0..chunks.len())
                            .map(|step| &chunks[(step + worker * 3) % chunks.len()])
                            .filter(|chunk| fx.chunks.put(chunk).expect("put").is_new())
                            .count()
                    })
                })
                .collect();
            let counts = handles.into_iter().map(|h| h.join().expect("thread panicked"));
            counts.sum::<usize>()
        });
        assert_eq!(stored_total, chunks.len());
        assert_eq!(fx.files().len(), chunks.len());
        for chunk in &chunks {
            assert_eq!(&fx.chunks.get(&fx.chunks.chunk_id(chunk)).expect("get"), chunk);
        }
    }

    #[tokio::test]
    async fn concurrent_async_puts_of_one_chunk_write_exactly_one_object() {
        let fx = Fixture::new();
        let data = pseudo_random(50_000, 17);
        let tasks: Vec<_> = (0..16)
            .map(|_| {
                let chunks = fx.chunks.clone();
                let data = data.clone();
                tokio::spawn(async move { chunks.put_async(data).await.expect("put") })
            })
            .collect();
        let mut results = Vec::new();
        for task in tasks {
            results.push(task.await.expect("task panicked"));
        }
        assert_eq!(results.iter().filter(|r| r.is_new()).count(), 1);
        assert_eq!(fx.files().len(), 1);
        let id = results[0].id;
        assert_eq!(fx.chunks.get_async(id).await.expect("get"), data);
    }

    #[test]
    fn debug_output_hides_the_keys() {
        let fx = Fixture::new();
        let shown = format!("{:?}", fx.chunks);
        assert!(shown.contains("redacted"), "{shown}");
    }
}
