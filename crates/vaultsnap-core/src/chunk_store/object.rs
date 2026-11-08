//! The on-disk format of one chunk object (see `docs/FORMAT.md`, "Chunk objects").
//!
//! ```text
//! header (12 bytes, plaintext but authenticated)
//!   0..4  magic "VSCO"
//!   4     format version (1)
//!   5     object kind (1 = chunk)
//!   6     compression (0 = none, 1 = zstd)
//!   7     flags (reserved, must be 0)
//!   8..12 length of the original chunk, u32 little endian
//! body
//!   nonce (24 random bytes) || XChaCha20-Poly1305 ciphertext || tag (16)
//! ```
//!
//! The AEAD key is the repository's `DataEncryption` key. The associated data is the 12 header
//! bytes followed by the 32-byte chunk id, so neither the header nor the id an object is stored
//! under can be altered or swapped without failing authentication. The encrypted payload is the
//! chunk itself, or its zstd compression when that is smaller.

use zeroize::Zeroizing;

use super::id::ChunkId;
use crate::chunking::ChunkerConfig;
use crate::crypto::{self, KeyPurpose, SecretKey, NONCE_LEN, TAG_LEN};
use crate::error::{Error, Result};
use crate::keys::RepositoryKeys;

/// Version of the chunk object format written by this build.
pub const OBJECT_FORMAT_VERSION: u8 = 1;
/// Largest chunk the format accepts, which is the largest chunk the chunker can produce.
pub const MAX_CHUNK_BYTES: usize = ChunkerConfig::MAX_SIZE_LIMIT;

const MAGIC: [u8; 4] = *b"VSCO";
const KIND_CHUNK: u8 = 1;
pub(super) const HEADER_LEN: usize = 12;
const AAD_LEN: usize = HEADER_LEN + ChunkId::LEN;
const ZSTD_LEVEL: i32 = 3;
/// Bigger than this cannot be a valid object, so it is never read into memory.
pub(super) const MAX_OBJECT_BYTES: u64 =
    (HEADER_LEN + NONCE_LEN + MAX_CHUNK_BYTES + TAG_LEN) as u64;

/// How the payload of an object is stored before encryption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    /// The chunk bytes as they are.
    Uncompressed,
    /// A zstd frame.
    Zstd,
}

impl Compression {
    fn code(self) -> u8 {
        match self {
            Self::Uncompressed => 0,
            Self::Zstd => 1,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Uncompressed),
            1 => Some(Self::Zstd),
            _ => None,
        }
    }
}

/// What the structure of an object says about it, established without any key: see
/// [`ChunkStore::peek`](super::ChunkStore::peek).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectInfo {
    /// Size of the object file in bytes.
    pub stored_len: u64,
    /// How the payload is stored.
    pub compression: Compression,
    /// Length of the original chunk, as the (authenticated, but here unchecked) header says.
    pub plaintext_len: u32,
}

/// The two secrets chunk storage needs, both derived from the master key.
pub(super) struct ChunkKeys {
    id: SecretKey,
    data: SecretKey,
}

impl ChunkKeys {
    pub(super) fn derive(keys: &RepositoryKeys) -> Result<Self> {
        Ok(Self {
            id: keys.derive(KeyPurpose::ContentId)?,
            data: keys.derive(KeyPurpose::DataEncryption)?,
        })
    }

    /// The keyed BLAKE3 hash of `plaintext`.
    pub(super) fn chunk_id(&self, plaintext: &[u8]) -> ChunkId {
        ChunkId::from_bytes(*blake3::keyed_hash(&self.id, plaintext).as_bytes())
    }
}

fn header(compression: Compression, plaintext_len: u32) -> [u8; HEADER_LEN] {
    let mut header = [0_u8; HEADER_LEN];
    header[..4].copy_from_slice(&MAGIC);
    header[4] = OBJECT_FORMAT_VERSION;
    header[5] = KIND_CHUNK;
    header[6] = compression.code();
    header[7] = 0;
    header[8..].copy_from_slice(&plaintext_len.to_le_bytes());
    header
}

fn aad(header: &[u8; HEADER_LEN], id: &ChunkId) -> [u8; AAD_LEN] {
    let mut aad = [0_u8; AAD_LEN];
    aad[..HEADER_LEN].copy_from_slice(header);
    aad[HEADER_LEN..].copy_from_slice(id.as_bytes());
    aad
}

/// Builds the object for `plaintext`, which the caller has already hashed to `id`. The payload is
/// compressed with zstd only if that makes it smaller.
pub(super) fn encode(
    keys: &ChunkKeys,
    id: &ChunkId,
    plaintext: &[u8],
) -> Result<(Vec<u8>, Compression)> {
    let too_large = || Error::ChunkTooLarge {
        len: plaintext.len(),
        max: MAX_CHUNK_BYTES,
    };
    if plaintext.len() > MAX_CHUNK_BYTES {
        return Err(too_large());
    }
    let plaintext_len = u32::try_from(plaintext.len()).map_err(|_| too_large())?;

    let compressed = zstd::bulk::compress(plaintext, ZSTD_LEVEL)
        .map_err(|err| Error::Compression(err.to_string()))?;
    let (compression, payload) = if compressed.len() < plaintext.len() {
        (Compression::Zstd, compressed.as_slice())
    } else {
        (Compression::Uncompressed, plaintext)
    };

    let header = header(compression, plaintext_len);
    let sealed = crypto::seal(&keys.data, &aad(&header, id), payload)?;
    let mut object = Vec::with_capacity(HEADER_LEN + sealed.len());
    object.extend_from_slice(&header);
    object.extend_from_slice(&sealed);
    Ok((object, compression))
}

/// Checks everything about an object that can be checked without a key: magic, version, header
/// fields, and that the size of the file is possible for what the header declares. `prefix` holds
/// at least the first `min(object_len, 12)` bytes of the object.
///
/// This is the first half of [`decode`], so the cheap check and the full verification agree on
/// what is structurally valid. It cannot see damage inside the encrypted payload, or a truncated
/// compressed object that is still larger than the minimum.
pub(super) fn inspect(id: &ChunkId, prefix: &[u8], object_len: u64) -> Result<ObjectInfo> {
    let corrupt = |reason: &str| Error::CorruptObject {
        id: id.to_hex(),
        reason: reason.to_owned(),
    };
    let header_len = HEADER_LEN as u64;

    if object_len < 5 {
        return Err(corrupt("truncated before the end of the header"));
    }
    if (prefix.len() as u64) < object_len.min(header_len) {
        return Err(corrupt("changed while it was being read"));
    }
    if prefix[..4] != MAGIC {
        return Err(corrupt("not a VaultSnap chunk object (bad magic)"));
    }
    if prefix[4] != OBJECT_FORMAT_VERSION {
        return Err(Error::UnsupportedObjectVersion {
            found: prefix[4],
            supported: OBJECT_FORMAT_VERSION,
        });
    }
    if object_len < header_len + (NONCE_LEN + TAG_LEN) as u64 {
        return Err(corrupt("truncated"));
    }
    if prefix[5] != KIND_CHUNK {
        return Err(corrupt("unknown object kind"));
    }
    let compression =
        Compression::from_code(prefix[6]).ok_or_else(|| corrupt("unknown compression method"))?;
    if prefix[7] != 0 {
        return Err(corrupt("unknown flags"));
    }
    let plaintext_len = u32::from_le_bytes([prefix[8], prefix[9], prefix[10], prefix[11]]);
    if usize::try_from(plaintext_len).map_or(true, |len| len > MAX_CHUNK_BYTES) {
        return Err(corrupt("declared length exceeds the maximum chunk size"));
    }

    let body_len = object_len - header_len;
    let largest_body = (NONCE_LEN + TAG_LEN) as u64 + u64::from(plaintext_len);
    match compression {
        Compression::Uncompressed if body_len != largest_body => {
            return Err(corrupt(
                "length does not match the header (truncated or extended)",
            ));
        }
        Compression::Zstd if body_len > largest_body => {
            return Err(corrupt("compressed payload is larger than the original"));
        }
        _ => {}
    }
    Ok(ObjectInfo {
        stored_len: object_len,
        compression,
        plaintext_len,
    })
}

/// Verifies and decodes the object stored under `id`.
///
/// In order: magic and version, structure and lengths (see [`inspect`]), authentication of the
/// header, id and ciphertext, decompression bounded by the authenticated length, the exact
/// length, and finally that the keyed hash of the result is `id`. Nothing is returned unless
/// every step passes.
pub(super) fn decode(keys: &ChunkKeys, id: &ChunkId, object: &[u8]) -> Result<Vec<u8>> {
    let corrupt = |reason: &str| Error::CorruptObject {
        id: id.to_hex(),
        reason: reason.to_owned(),
    };

    let info = inspect(id, object, object.len() as u64)?;
    let compression = info.compression;
    let plaintext_len = usize::try_from(info.plaintext_len)
        .map_err(|_| corrupt("length does not fit in memory"))?;
    let (header, body) = object.split_at(HEADER_LEN);
    let header: &[u8; HEADER_LEN] = header
        .try_into()
        .map_err(|_| corrupt("header has the wrong size"))?;

    let opened = crypto::open(&keys.data, &aad(header, id), body).map_err(|err| match err {
        Error::Decryption => corrupt(
            "authentication failed: the object is damaged or was modified, is stored under the \
             wrong id, or the key is wrong",
        ),
        other => other,
    })?;
    let plaintext = match compression {
        Compression::Uncompressed => {
            let mut opened: Zeroizing<Vec<u8>> = opened;
            std::mem::take(&mut *opened)
        }
        Compression::Zstd => zstd::bulk::decompress(&opened, plaintext_len)
            .map_err(|err| corrupt(&format!("decompression failed: {err}")))?,
    };
    if plaintext.len() != plaintext_len {
        return Err(corrupt("decoded size does not match the header"));
    }
    // The id is public (it is the file name), so an ordinary comparison leaks nothing.
    if keys.chunk_id(&plaintext) != *id {
        return Err(corrupt(
            "content does not hash to the id it is stored under",
        ));
    }
    Ok(plaintext)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunking::test_util::pseudo_random;

    fn keys() -> ChunkKeys {
        let master = RepositoryKeys::generate().expect("generate");
        ChunkKeys::derive(&master).expect("derive")
    }

    fn compressible(len: usize) -> Vec<u8> {
        b"all work and no play makes a dull backup "
            .iter()
            .copied()
            .cycle()
            .take(len)
            .collect()
    }

    fn encode_chunk(keys: &ChunkKeys, plaintext: &[u8]) -> (ChunkId, Vec<u8>, Compression) {
        let id = keys.chunk_id(plaintext);
        let (object, compression) = encode(keys, &id, plaintext).expect("encode");
        (id, object, compression)
    }

    fn assert_rejected(result: Result<Vec<u8>>, context: &str) {
        assert!(
            matches!(
                result,
                Err(Error::CorruptObject { .. } | Error::UnsupportedObjectVersion { .. })
            ),
            "{context}: {result:?}"
        );
    }

    #[test]
    fn round_trips_chunks_of_every_kind() {
        let keys = keys();
        let mut inputs = vec![Vec::new(), vec![0], vec![7; 100], compressible(70_000)];
        inputs.push(pseudo_random(1, 1));
        inputs.push(pseudo_random(5000, 2));
        inputs.push(pseudo_random(300_000, 3));
        for plaintext in inputs {
            let (id, object, _) = encode_chunk(&keys, &plaintext);
            assert_eq!(decode(&keys, &id, &object).expect("decode"), plaintext);
        }
    }

    #[test]
    fn compresses_only_when_that_is_smaller() {
        let keys = keys();
        let text = compressible(100_000);
        let (_, object, compression) = encode_chunk(&keys, &text);
        assert_eq!(compression, Compression::Zstd);
        assert!(
            object.len() < 5000,
            "compressible data took {} bytes",
            object.len()
        );

        let noise = pseudo_random(100_000, 9);
        let (_, object, compression) = encode_chunk(&keys, &noise);
        assert_eq!(compression, Compression::Uncompressed);
        assert_eq!(object.len(), HEADER_LEN + NONCE_LEN + noise.len() + TAG_LEN);

        let (_, object, compression) = encode_chunk(&keys, b"x");
        assert_eq!(compression, Compression::Uncompressed);
        assert_eq!(object.len(), HEADER_LEN + NONCE_LEN + 1 + TAG_LEN);
    }

    #[test]
    fn the_header_layout_is_the_documented_one() {
        let keys = keys();
        let (_, object, _) = encode_chunk(&keys, &pseudo_random(300, 4));
        assert_eq!(&object[..4], b"VSCO");
        assert_eq!(object[4], 1, "format version");
        assert_eq!(object[5], 1, "object kind");
        assert_eq!(object[6], 0, "compression");
        assert_eq!(object[7], 0, "flags");
        assert_eq!(&object[8..12], &300_u32.to_le_bytes());
    }

    #[test]
    fn objects_contain_no_plaintext() {
        let keys = keys();
        let marker = b"TOP-SECRET-PLAINTEXT-MARKER-0123456789";
        for compress in [false, true] {
            let mut plaintext = Vec::new();
            if compress {
                for _ in 0..200 {
                    plaintext.extend_from_slice(marker);
                }
            } else {
                plaintext.extend_from_slice(&pseudo_random(2000, 5));
                plaintext.extend_from_slice(marker);
                plaintext.extend_from_slice(&pseudo_random(2000, 6));
            }
            let (_, object, _) = encode_chunk(&keys, &plaintext);
            let found = object
                .windows(16)
                .any(|window| marker.windows(16).any(|m| m == window));
            assert!(
                !found,
                "plaintext visible in the object (compressed: {compress})"
            );
        }
    }

    #[test]
    fn every_encoding_uses_a_fresh_nonce() {
        let keys = keys();
        let plaintext = pseudo_random(500, 8);
        let (id, first, _) = encode_chunk(&keys, &plaintext);
        let (_, second, _) = encode_chunk(&keys, &plaintext);
        assert_ne!(first, second);
        assert_ne!(
            first[HEADER_LEN..HEADER_LEN + NONCE_LEN],
            second[HEADER_LEN..HEADER_LEN + NONCE_LEN]
        );
        assert_eq!(decode(&keys, &id, &first).expect("decode"), plaintext);
        assert_eq!(decode(&keys, &id, &second).expect("decode"), plaintext);
    }

    #[test]
    fn flipping_any_single_bit_is_detected() {
        let keys = keys();
        for plaintext in [pseudo_random(40, 10), compressible(400)] {
            let (id, object, _) = encode_chunk(&keys, &plaintext);
            for index in 0..object.len() {
                for bit in 0..8 {
                    let mut damaged = object.clone();
                    damaged[index] ^= 1 << bit;
                    assert_rejected(
                        decode(&keys, &id, &damaged),
                        &format!("byte {index} bit {bit}"),
                    );
                }
            }
        }
    }

    #[test]
    fn truncation_at_every_length_is_detected() {
        let keys = keys();
        for plaintext in [pseudo_random(60, 11), compressible(900)] {
            let (id, object, _) = encode_chunk(&keys, &plaintext);
            for len in 0..object.len() {
                let result = decode(&keys, &id, &object[..len]);
                assert!(
                    matches!(result, Err(Error::CorruptObject { .. })),
                    "len {len}: {result:?}"
                );
            }
        }
    }

    #[test]
    fn appended_data_is_detected() {
        let keys = keys();
        for plaintext in [pseudo_random(60, 12), compressible(900)] {
            let (id, mut object, _) = encode_chunk(&keys, &plaintext);
            object.extend_from_slice(b"trailing garbage");
            assert_rejected(decode(&keys, &id, &object), "appended");
        }
    }

    #[test]
    fn an_object_cannot_be_used_under_another_id() {
        let keys = keys();
        let (_, object, _) = encode_chunk(&keys, &pseudo_random(200, 13));
        let (other_id, _, _) = encode_chunk(&keys, &pseudo_random(200, 14));
        assert_rejected(decode(&keys, &other_id, &object), "swapped id");
    }

    #[test]
    fn the_wrong_key_is_rejected() {
        let (id, object, _) = encode_chunk(&keys(), &pseudo_random(200, 15));
        assert_rejected(decode(&keys(), &id, &object), "other repository's key");
    }

    #[test]
    fn authentic_content_that_does_not_match_its_id_is_rejected() {
        let keys = keys();
        let plaintext = pseudo_random(300, 16);
        let wrong_id = keys.chunk_id(b"some other chunk");
        let (object, _) = encode(&keys, &wrong_id, &plaintext).expect("encode");
        let result = decode(&keys, &wrong_id, &object);
        match result {
            Err(Error::CorruptObject { reason, .. }) => {
                assert!(reason.contains("does not hash"), "{reason}");
            }
            other => panic!("expected a content mismatch, got {other:?}"),
        }
    }

    #[test]
    fn unsupported_versions_are_reported_before_anything_else() {
        let keys = keys();
        let (id, mut object, _) = encode_chunk(&keys, &pseudo_random(100, 17));
        object[4] = 2;
        let result = decode(&keys, &id, &object);
        assert!(
            matches!(
                result,
                Err(Error::UnsupportedObjectVersion {
                    found: 2,
                    supported: 1
                })
            ),
            "{result:?}"
        );
    }

    #[test]
    fn a_declared_length_beyond_the_maximum_is_rejected() {
        let keys = keys();
        let (id, mut object, _) = encode_chunk(&keys, &pseudo_random(100, 18));
        let too_big = u32::try_from(MAX_CHUNK_BYTES + 1).expect("fits");
        object[8..12].copy_from_slice(&too_big.to_le_bytes());
        assert_rejected(decode(&keys, &id, &object), "oversized declared length");
    }

    /// An authentic object whose compressed payload expands far beyond the authenticated length
    /// must be rejected without allocating the expansion.
    #[test]
    fn decompression_bombs_are_rejected() {
        let keys = keys();
        let bomb =
            zstd::bulk::compress(&vec![0_u8; 4 * 1024 * 1024], ZSTD_LEVEL).expect("compress");
        let declared = 1000_u32;
        let header = header(Compression::Zstd, declared);
        let id = keys.chunk_id(b"irrelevant");
        let sealed = crypto::seal(&keys.data, &aad(&header, &id), &bomb).expect("seal");
        let mut object = header.to_vec();
        object.extend_from_slice(&sealed);
        assert_rejected(decode(&keys, &id, &object), "bomb");
    }

    #[test]
    fn a_compressed_payload_of_the_wrong_size_is_rejected() {
        let keys = keys();
        let payload = zstd::bulk::compress(&vec![1_u8; 1000], ZSTD_LEVEL).expect("compress");
        let header = header(Compression::Zstd, 2000);
        let id = keys.chunk_id(&[1_u8; 1000]);
        let sealed = crypto::seal(&keys.data, &aad(&header, &id), &payload).expect("seal");
        let mut object = header.to_vec();
        object.extend_from_slice(&sealed);
        assert_rejected(decode(&keys, &id, &object), "size mismatch");
    }

    #[test]
    fn oversized_chunks_are_refused_when_encoding() {
        let keys = keys();
        let big = vec![0_u8; MAX_CHUNK_BYTES + 1];
        let id = keys.chunk_id(b"unused");
        let result = encode(&keys, &id, &big);
        assert!(matches!(result, Err(Error::ChunkTooLarge { .. })));
    }

    fn inspected(id: &ChunkId, object: &[u8]) -> Result<ObjectInfo> {
        let prefix = &object[..object.len().min(HEADER_LEN)];
        inspect(id, prefix, object.len() as u64)
    }

    #[test]
    fn inspection_reports_what_the_header_declares_without_a_key() {
        let keys = keys();
        let data = pseudo_random(5000, 4);
        let (id, object, compression) = encode_chunk(&keys, &data);
        let info = inspected(&id, &object).expect("valid");
        assert_eq!(info.stored_len, object.len() as u64);
        assert_eq!(info.plaintext_len, 5000);
        assert_eq!(info.compression, compression);
        assert_eq!(compression, Compression::Uncompressed);

        let text = compressible(70_000);
        let (id, object, compression) = encode_chunk(&keys, &text);
        assert_eq!(compression, Compression::Zstd);
        let info = inspected(&id, &object).expect("valid");
        assert_eq!(
            (info.plaintext_len, info.compression),
            (70_000, Compression::Zstd)
        );
        assert!(info.stored_len < 70_000);
    }

    #[test]
    fn inspection_catches_what_can_be_seen_without_a_key() {
        let keys = keys();
        let (id, object, _) = encode_chunk(&keys, &pseudo_random(5000, 5));

        // Every truncation of an uncompressed object changes its size.
        for length in 0..object.len() {
            let result = inspected(&id, &object[..length]);
            assert!(
                matches!(result, Err(Error::CorruptObject { .. })),
                "length {length}"
            );
        }
        // So does appending.
        let mut longer = object.clone();
        longer.push(0);
        assert!(matches!(
            inspected(&id, &longer),
            Err(Error::CorruptObject { .. })
        ));

        // Header fields: magic, kind, compression, flags, declared length.
        for (position, value) in [(0, b'X'), (5, 9), (6, 7), (7, 1)] {
            let mut damaged = object.clone();
            damaged[position] = value;
            let result = inspected(&id, &damaged);
            assert!(
                matches!(result, Err(Error::CorruptObject { .. })),
                "byte {position}"
            );
        }
        let mut damaged = object.clone();
        damaged[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            inspected(&id, &damaged),
            Err(Error::CorruptObject { .. })
        ));
        let mut damaged = object;
        damaged[8] ^= 1;
        assert!(
            matches!(inspected(&id, &damaged), Err(Error::CorruptObject { .. })),
            "a wrong declared length contradicts the size of an uncompressed object"
        );
    }

    #[test]
    fn inspection_reports_unsupported_versions_before_anything_else() {
        let keys = keys();
        let (id, mut object, _) = encode_chunk(&keys, &pseudo_random(100, 6));
        object[4] = 2;
        let result = inspected(&id, &object);
        assert!(
            matches!(
                result,
                Err(Error::UnsupportedObjectVersion { found: 2, .. })
            ),
            "{result:?}"
        );
    }

    #[test]
    fn inspection_is_blind_to_damage_inside_the_payload_and_a_cut_compressed_object() {
        let keys = keys();
        let (id, mut object, _) = encode_chunk(&keys, &pseudo_random(5000, 7));
        let middle = object.len() / 2;
        object[middle] ^= 1;
        assert!(
            inspected(&id, &object).is_ok(),
            "only the full verification can see this"
        );
        assert_rejected(decode(&keys, &id, &object), "the full verification does");

        let (id, object, compression) = encode_chunk(&keys, &compressible(70_000));
        assert_eq!(compression, Compression::Zstd);
        let cut = &object[..object.len() - 1];
        assert!(
            inspected(&id, cut).is_ok(),
            "a shorter compressed object is still possible"
        );
        assert_rejected(decode(&keys, &id, cut), "but it does not authenticate");
    }

    #[test]
    fn inspection_notices_a_file_that_changed_while_it_was_read() {
        let keys = keys();
        let (id, object, _) = encode_chunk(&keys, &pseudo_random(100, 8));
        let result = inspect(&id, &object[..4], object.len() as u64);
        assert!(
            matches!(result, Err(Error::CorruptObject { .. })),
            "{result:?}"
        );
    }
}
