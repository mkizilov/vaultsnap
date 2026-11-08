//! Snapshot identifiers.

use std::fmt;
use std::str::FromStr;

use crate::error::{Error, Result};
use crate::repository::RepositoryLayout;
use crate::util::{decode_hex, hex, random_hex};

const ID_LEN: usize = 64;
const SHORT_LEN: usize = 8;

/// Identifier of a snapshot: 64 lowercase hexadecimal characters, 256 random bits from the
/// operating system. Ids are not secret; they are the snapshot file names.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SnapshotId(String);

impl SnapshotId {
    /// A new random identifier. Two calls collide with negligible probability, and committing a
    /// snapshot never overwrites an existing file, so a collision would be an error, not damage.
    pub fn generate() -> Result<Self> {
        Ok(Self(random_hex(ID_LEN / 2)?))
    }

    /// The identifier as 32 raw bytes.
    pub(crate) fn to_bytes(&self) -> [u8; ID_LEN / 2] {
        let mut bytes = [0_u8; ID_LEN / 2];
        if let Some(decoded) = decode_hex(&self.0) {
            bytes.copy_from_slice(&decoded);
        }
        bytes
    }

    /// An identifier from its 32 raw bytes.
    pub(crate) fn from_bytes(bytes: &[u8; ID_LEN / 2]) -> Self {
        Self(hex(bytes))
    }

    /// The full identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Repository-relative name of this snapshot's file: `snapshots/<id>`.
    pub(crate) fn object_name(&self) -> String {
        format!("{}/{}", RepositoryLayout::SNAPSHOTS_DIR, self.0)
    }

    /// An abbreviated form for display; not unique and never valid input.
    pub fn short(&self) -> &str {
        &self.0[..SHORT_LEN]
    }
}

impl FromStr for SnapshotId {
    type Err = Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let valid = s.len() == ID_LEN && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if valid {
            Ok(Self(s.to_owned()))
        } else {
            Err(Error::InvalidSnapshotId(s.to_owned()))
        }
    }
}

impl fmt::Display for SnapshotId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_are_valid_and_distinct() {
        let a = SnapshotId::generate().expect("randomness available");
        let b = SnapshotId::generate().expect("randomness available");
        assert_ne!(a, b);
        assert_eq!(a.as_str().parse::<SnapshotId>().expect("valid"), a);
        assert_eq!(a.as_str().len(), 64);
    }

    #[test]
    fn raw_bytes_round_trip() {
        let id: SnapshotId = "0123456789abcdef".repeat(4).parse().expect("valid");
        let bytes = id.to_bytes();
        assert_eq!(bytes[0], 0x01);
        assert_eq!(bytes[31], 0xef);
        assert_eq!(SnapshotId::from_bytes(&bytes), id);
    }

    #[test]
    fn parses_and_displays_a_valid_id() {
        let text = "0123456789abcdef".repeat(4);
        let id: SnapshotId = text.parse().expect("valid id");
        assert_eq!(id.to_string(), text);
        assert_eq!(id.as_str(), text);
        assert_eq!(id.short(), "01234567");
    }

    #[test]
    fn the_object_name_is_a_path_inside_the_snapshots_directory() {
        let text = "0123456789abcdef".repeat(4);
        let id: SnapshotId = text.parse().expect("valid id");
        assert_eq!(id.object_name(), format!("snapshots/{text}"));
    }

    #[test]
    fn rejects_wrong_length() {
        assert!("abc".parse::<SnapshotId>().is_err());
        assert!("a".repeat(65).parse::<SnapshotId>().is_err());
        assert!("".parse::<SnapshotId>().is_err());
    }

    #[test]
    fn rejects_uppercase_and_non_hex() {
        assert!("A".repeat(64).parse::<SnapshotId>().is_err());
        assert!("g".repeat(64).parse::<SnapshotId>().is_err());
        assert!(format!("{}/", "a".repeat(63)).parse::<SnapshotId>().is_err());
    }

    #[test]
    fn rejects_path_traversal_attempts() {
        assert!("../".repeat(22).parse::<SnapshotId>().is_err());
    }
}
