//! The container of an encrypted snapshot: one file, `snapshots/<id>` (see `docs/FORMAT.md`).
//!
//! ```text
//! header (12 bytes, plaintext but authenticated)
//!   0..4  magic "VSSN"
//!   4     container format version (1)
//!   5     object kind (2 = snapshot)
//!   6     compression (0 = none, 1 = zstd)
//!   7     flags (reserved, must be 0)
//!   8..12 length of the manifest bytes, u32 little endian
//! body
//!   nonce (24 random bytes) || XChaCha20-Poly1305 ciphertext || tag (16)
//! ```
//!
//! The key is the repository's `MetadataEncryption` key. The associated data is the 12 header
//! bytes followed by the 32 raw bytes of the snapshot id, so the header cannot be altered and a
//! snapshot cannot be filed under another id (or one snapshot swapped for another) undetected.
//! The plaintext is the manifest encoding, zstd-compressed when that is smaller. Everything
//! sensitive, including every file name, is inside the ciphertext.

use zeroize::Zeroizing;

use super::id::SnapshotId;
use crate::crypto::{self, SecretKey, NONCE_LEN, TAG_LEN};
use crate::error::{Error, Result};

/// Version of the snapshot container written by this build.
pub const SNAPSHOT_OBJECT_VERSION: u8 = 1;
/// Largest manifest the format accepts (1 GiB). Bounds memory when reading a damaged file.
pub const MAX_MANIFEST_BYTES: usize = 1 << 30;

const MAGIC: [u8; 4] = *b"VSSN";
const KIND_SNAPSHOT: u8 = 2;
const COMPRESSION_NONE: u8 = 0;
const COMPRESSION_ZSTD: u8 = 1;
const HEADER_LEN: usize = 12;
const AAD_LEN: usize = HEADER_LEN + 32;
const ZSTD_LEVEL: i32 = 3;
/// A file larger than this cannot be a valid snapshot and is never read.
pub(super) const MAX_OBJECT_BYTES: u64 =
    (HEADER_LEN + NONCE_LEN + MAX_MANIFEST_BYTES + TAG_LEN) as u64;

fn header(compression: u8, manifest_len: u32) -> [u8; HEADER_LEN] {
    let mut header = [0_u8; HEADER_LEN];
    header[..4].copy_from_slice(&MAGIC);
    header[4] = SNAPSHOT_OBJECT_VERSION;
    header[5] = KIND_SNAPSHOT;
    header[6] = compression;
    header[7] = 0;
    header[8..].copy_from_slice(&manifest_len.to_le_bytes());
    header
}

fn aad(header: &[u8; HEADER_LEN], id: &SnapshotId) -> [u8; AAD_LEN] {
    let mut aad = [0_u8; AAD_LEN];
    aad[..HEADER_LEN].copy_from_slice(header);
    aad[HEADER_LEN..].copy_from_slice(&id.to_bytes());
    aad
}

/// Builds the file contents for the manifest bytes `body` of snapshot `id`.
pub(super) fn seal(key: &SecretKey, id: &SnapshotId, body: &[u8]) -> Result<Vec<u8>> {
    if body.len() > MAX_MANIFEST_BYTES {
        return Err(Error::InvalidManifest(
            "the manifest is larger than the format allows".to_owned(),
        ));
    }
    let manifest_len = u32::try_from(body.len())
        .map_err(|_| Error::InvalidManifest("the manifest is too large".to_owned()))?;
    let compressed = zstd::bulk::compress(body, ZSTD_LEVEL)
        .map_err(|err| Error::Compression(err.to_string()))?;
    let (compression, payload) = if compressed.len() < body.len() {
        (COMPRESSION_ZSTD, compressed.as_slice())
    } else {
        (COMPRESSION_NONE, body)
    };
    let header = header(compression, manifest_len);
    let sealed = crypto::seal(key, &aad(&header, id), payload)?;
    let mut object = Vec::with_capacity(HEADER_LEN + sealed.len());
    object.extend_from_slice(&header);
    object.extend_from_slice(&sealed);
    Ok(object)
}

/// Verifies and decrypts the file contents of snapshot `id`, returning the manifest bytes.
///
/// In order: magic and version (an unknown container version is reported as such), structure and
/// lengths, the AEAD tag over header, id, and ciphertext, decompression capped at the
/// authenticated length, and the exact length. Nothing is returned unless all of it passes.
pub(super) fn open(key: &SecretKey, id: &SnapshotId, object: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    let corrupt = |reason: &str| Error::CorruptSnapshot {
        id: id.to_string(),
        reason: reason.to_owned(),
    };

    if object.len() < 5 {
        return Err(corrupt("truncated before the end of the header"));
    }
    if object[..4] != MAGIC {
        return Err(corrupt("not a VaultSnap snapshot (bad magic)"));
    }
    if object[4] != SNAPSHOT_OBJECT_VERSION {
        return Err(Error::UnsupportedSnapshotVersion {
            found: object[4],
            supported: SNAPSHOT_OBJECT_VERSION,
        });
    }
    if object.len() < HEADER_LEN + NONCE_LEN + TAG_LEN {
        return Err(corrupt("truncated"));
    }
    let (header, body) = object.split_at(HEADER_LEN);
    let header: &[u8; HEADER_LEN] = header
        .try_into()
        .map_err(|_| corrupt("header has the wrong size"))?;
    if header[5] != KIND_SNAPSHOT {
        return Err(corrupt("unknown object kind"));
    }
    let compression = header[6];
    if compression != COMPRESSION_NONE && compression != COMPRESSION_ZSTD {
        return Err(corrupt("unknown compression method"));
    }
    if header[7] != 0 {
        return Err(corrupt("unknown flags"));
    }
    let manifest_len = usize::try_from(u32::from_le_bytes([
        header[8], header[9], header[10], header[11],
    ]))
    .map_err(|_| corrupt("length does not fit in memory"))?;
    if manifest_len > MAX_MANIFEST_BYTES {
        return Err(corrupt("declared length exceeds the maximum manifest size"));
    }

    let largest_body = NONCE_LEN + manifest_len + TAG_LEN;
    if compression == COMPRESSION_NONE && body.len() != largest_body {
        return Err(corrupt(
            "length does not match the header (truncated or extended)",
        ));
    }
    if compression == COMPRESSION_ZSTD && body.len() > largest_body {
        return Err(corrupt("compressed payload is larger than the original"));
    }

    let opened = crypto::open(key, &aad(header, id), body).map_err(|err| match err {
        Error::Decryption => corrupt(
            "authentication failed: the snapshot is damaged or was modified, is filed under the \
             wrong id, or the key is wrong",
        ),
        other => other,
    })?;
    let manifest = if compression == COMPRESSION_ZSTD {
        let decompressed = zstd::bulk::decompress(&opened, manifest_len)
            .map_err(|err| corrupt(&format!("decompression failed: {err}")))?;
        Zeroizing::new(decompressed)
    } else {
        opened
    };
    if manifest.len() != manifest_len {
        return Err(corrupt("decoded size does not match the header"));
    }
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{derive_key, KeyPurpose, KEY_LEN};

    fn key(byte: u8) -> SecretKey {
        derive_key(&[byte; KEY_LEN], KeyPurpose::MetadataEncryption).expect("derive")
    }

    fn id(byte: char) -> SnapshotId {
        byte.to_string().repeat(64).parse().expect("valid id")
    }

    fn noise(len: usize) -> Vec<u8> {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect()
    }

    fn compressible(len: usize) -> Vec<u8> {
        b"/home/me/Documents/projects/vaultsnap/"
            .iter()
            .copied()
            .cycle()
            .take(len)
            .collect()
    }

    fn rejected(result: Result<Zeroizing<Vec<u8>>>, context: &str) {
        assert!(
            matches!(
                result,
                Err(Error::CorruptSnapshot { .. } | Error::UnsupportedSnapshotVersion { .. })
            ),
            "{context}: {result:?}"
        );
    }

    #[test]
    fn round_trips_compressible_and_incompressible_manifests() {
        let (key, id) = (key(1), id('a'));
        for body in [Vec::new(), vec![1], noise(5000), compressible(50_000)] {
            let object = seal(&key, &id, &body).expect("seal");
            assert_eq!(
                open(&key, &id, &object).expect("open").as_slice(),
                body.as_slice()
            );
        }
    }

    #[test]
    fn the_header_layout_is_the_documented_one() {
        let object = seal(&key(1), &id('a'), &noise(300)).expect("seal");
        assert_eq!(&object[..4], b"VSSN");
        assert_eq!(object[4], 1, "container version");
        assert_eq!(object[5], 2, "object kind");
        assert_eq!(object[6], 0, "compression");
        assert_eq!(object[7], 0, "flags");
        assert_eq!(&object[8..12], &300_u32.to_le_bytes());
        assert_eq!(object.len(), 12 + 24 + 300 + 16);
        let compressed = seal(&key(1), &id('a'), &compressible(10_000)).expect("seal");
        assert_eq!(compressed[6], 1, "compression");
        assert!(compressed.len() < 1000);
    }

    #[test]
    fn paths_are_not_visible_in_the_file() {
        let marker = b"/secret/payroll-2024/confidential-name.xlsx";
        let mut body = Vec::new();
        for _ in 0..50 {
            body.extend_from_slice(marker);
        }
        let object = seal(&key(1), &id('a'), &body).expect("seal");
        let leaked = object
            .windows(12)
            .any(|w| marker.windows(12).any(|m| m == w));
        assert!(!leaked);
    }

    #[test]
    fn every_seal_uses_a_fresh_nonce() {
        let body = noise(100);
        let first = seal(&key(1), &id('a'), &body).expect("seal");
        let second = seal(&key(1), &id('a'), &body).expect("seal");
        assert_ne!(first, second);
    }

    #[test]
    fn a_wrong_key_or_a_wrong_id_is_rejected() {
        let object = seal(&key(1), &id('a'), &noise(200)).expect("seal");
        rejected(open(&key(2), &id('a'), &object), "wrong key");
        rejected(
            open(&key(1), &id('b'), &object),
            "snapshot filed under another id",
        );
    }

    #[test]
    fn flipping_any_single_bit_is_detected() {
        let (key, id) = (key(1), id('a'));
        for body in [noise(40), compressible(500)] {
            let object = seal(&key, &id, &body).expect("seal");
            for index in 0..object.len() {
                for bit in 0..8 {
                    let mut damaged = object.clone();
                    damaged[index] ^= 1 << bit;
                    rejected(
                        open(&key, &id, &damaged),
                        &format!("byte {index} bit {bit}"),
                    );
                }
            }
        }
    }

    #[test]
    fn truncation_at_every_length_and_trailing_data_are_detected() {
        let (key, id) = (key(1), id('a'));
        for body in [noise(60), compressible(900)] {
            let object = seal(&key, &id, &body).expect("seal");
            for len in 0..object.len() {
                let result = open(&key, &id, &object[..len]);
                assert!(
                    matches!(result, Err(Error::CorruptSnapshot { .. })),
                    "len {len}"
                );
            }
            let mut extended = object.clone();
            extended.extend_from_slice(b"extra");
            rejected(open(&key, &id, &extended), "trailing data");
        }
    }

    #[test]
    fn container_versions_other_than_the_supported_one_are_reported() {
        let (key, id) = (key(1), id('a'));
        let object = seal(&key, &id, &noise(100)).expect("seal");
        for version in [0_u8, 2, 255] {
            let mut changed = object.clone();
            changed[4] = version;
            let result = open(&key, &id, &changed);
            assert!(
                matches!(result, Err(Error::UnsupportedSnapshotVersion { found, supported: 1 }) if found == version),
                "{version}: {result:?}"
            );
        }
    }

    #[test]
    fn an_oversized_declared_length_is_rejected() {
        let (key, id) = (key(1), id('a'));
        let mut object = seal(&key, &id, &noise(100)).expect("seal");
        let too_big = u32::try_from(MAX_MANIFEST_BYTES + 1).expect("fits");
        object[8..12].copy_from_slice(&too_big.to_le_bytes());
        rejected(open(&key, &id, &object), "declared length");
    }

    #[test]
    fn decompression_bombs_are_rejected_without_expanding() {
        let (key, id) = (key(1), id('a'));
        let bomb =
            zstd::bulk::compress(&vec![0_u8; 8 * 1024 * 1024], ZSTD_LEVEL).expect("compress");
        let header = header(COMPRESSION_ZSTD, 1000);
        let sealed = crypto::seal(&key, &aad(&header, &id), &bomb).expect("seal");
        let mut object = header.to_vec();
        object.extend_from_slice(&sealed);
        rejected(open(&key, &id, &object), "bomb");
    }

    #[test]
    fn an_authentic_payload_of_the_wrong_size_is_rejected() {
        let (key, id) = (key(1), id('a'));
        let payload = zstd::bulk::compress(&[1_u8; 1000], ZSTD_LEVEL).expect("compress");
        let header = header(COMPRESSION_ZSTD, 2000);
        let sealed = crypto::seal(&key, &aad(&header, &id), &payload).expect("seal");
        let mut object = header.to_vec();
        object.extend_from_slice(&sealed);
        rejected(open(&key, &id, &object), "declared 2000, decoded 1000");
    }
}
