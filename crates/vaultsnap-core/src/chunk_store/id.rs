//! Chunk identifiers.

use std::fmt;
use std::str::FromStr;

use crate::error::Error;
use crate::repository::RepositoryLayout;
use crate::util::{decode_hex, hex};

const ID_LEN: usize = 32;

/// Identifier of a chunk: the keyed BLAKE3 hash (256 bits) of its plaintext, keyed with a secret
/// derived from the repository master key.
///
/// The same bytes always get the same id within one repository, which is what makes
/// deduplication work. Without the key the id cannot be computed, so someone who can read the
/// repository cannot test whether a file they already have is stored in it. Ids are not secret:
/// they are the object file names.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChunkId([u8; ID_LEN]);

impl ChunkId {
    /// Length of an id in bytes.
    pub const LEN: usize = ID_LEN;

    /// Wraps raw id bytes.
    pub fn from_bytes(bytes: [u8; ID_LEN]) -> Self {
        Self(bytes)
    }

    /// The raw id bytes.
    pub fn as_bytes(&self) -> &[u8; ID_LEN] {
        &self.0
    }

    /// The id as 64 lowercase hexadecimal characters.
    pub fn to_hex(&self) -> String {
        hex(&self.0)
    }

    /// Repository-relative directory holding this chunk's object: `data/<first two hex digits>`.
    pub(crate) fn shard_dir(&self) -> String {
        format!("{}/{}", RepositoryLayout::DATA_DIR, &self.to_hex()[..2])
    }

    /// Repository-relative name of this chunk's object: `data/<xx>/<id>`.
    pub(crate) fn object_name(&self) -> String {
        let hex = self.to_hex();
        format!("{}/{}/{hex}", RepositoryLayout::DATA_DIR, &hex[..2])
    }
}

impl FromStr for ChunkId {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let bytes = decode_hex(text)
            .filter(|bytes| bytes.len() == ID_LEN)
            .ok_or_else(|| Error::InvalidChunkId(text.to_owned()))?;
        let mut id = [0_u8; ID_LEN];
        id.copy_from_slice(&bytes);
        Ok(Self(id))
    }
}

impl fmt::Display for ChunkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for ChunkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ChunkId({})", self.to_hex())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ChunkId {
        let mut bytes = [0_u8; ID_LEN];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = index as u8 * 8 + 1;
        }
        ChunkId::from_bytes(bytes)
    }

    #[test]
    fn hex_round_trips() {
        let id = sample();
        let text = id.to_hex();
        assert_eq!(text.len(), 64);
        assert_eq!(text, id.to_string());
        assert_eq!(text.parse::<ChunkId>().expect("parses"), id);
        assert_eq!(id.as_bytes()[0], 1);
        assert_eq!(format!("{id:?}"), format!("ChunkId({text})"));
    }

    #[test]
    fn rejects_malformed_text() {
        let valid = sample().to_hex();
        let cases = [
            String::new(),
            "ab".repeat(31),
            "ab".repeat(33),
            valid.to_uppercase(),
            format!("{}g", &valid[..63]),
            format!("../{}", &valid[3..]),
            format!(" {}", &valid[1..]),
        ];
        for case in cases {
            let result = case.parse::<ChunkId>();
            assert!(matches!(result, Err(Error::InvalidChunkId(_))), "{case:?}");
        }
    }

    #[test]
    fn object_names_are_sharded_by_the_first_byte() {
        let id: ChunkId = format!("ab{}", "0".repeat(62)).parse().expect("valid");
        assert_eq!(id.shard_dir(), "data/ab");
        assert_eq!(id.object_name(), format!("data/ab/ab{}", "0".repeat(62)));
    }
}
