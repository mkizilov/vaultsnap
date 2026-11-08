//! Snapshot identifiers.

use std::fmt;
use std::str::FromStr;

use crate::error::{Error, Result};
use crate::repository::RepositoryLayout;
use crate::util::{decode_hex, hex, random_hex};

const ID_LEN: usize = 64;
const SHORT_LEN: usize = 8;
/// The shortest start of an id accepted as a [`SnapshotRef`].
pub const MIN_REFERENCE_LEN: usize = 4;

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

/// What a person types to name a snapshot: a full id, or the start of one as shown in listings
/// (at least [`MIN_REFERENCE_LEN`] characters). Only the syntax is checked here; whether it names
/// exactly one snapshot is decided against a repository by `SnapshotStore::resolve`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotRef(String);

impl SnapshotRef {
    /// The text as given.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// True if this is a complete id rather than a prefix.
    pub fn is_full(&self) -> bool {
        self.0.len() == ID_LEN
    }
}

impl FromStr for SnapshotRef {
    type Err = Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let valid = (MIN_REFERENCE_LEN..=ID_LEN).contains(&s.len())
            && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if valid {
            Ok(Self(s.to_owned()))
        } else {
            Err(Error::InvalidSnapshotRef(s.to_owned()))
        }
    }
}

impl fmt::Display for SnapshotRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&SnapshotId> for SnapshotRef {
    fn from(id: &SnapshotId) -> Self {
        Self(id.0.clone())
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
        assert!(format!("{}/", "a".repeat(63))
            .parse::<SnapshotId>()
            .is_err());
    }

    #[test]
    fn rejects_path_traversal_attempts() {
        assert!("../".repeat(22).parse::<SnapshotId>().is_err());
    }

    #[test]
    fn references_accept_four_to_sixty_four_lowercase_hex_digits() {
        for length in [4, 8, 63, 64] {
            let text = "0123456789abcdef".repeat(4)[..length].to_owned();
            let reference: SnapshotRef = text.parse().expect("valid reference");
            assert_eq!(reference.as_str(), text);
            assert_eq!(reference.to_string(), text);
            assert_eq!(reference.is_full(), length == 64);
        }
    }

    #[test]
    fn references_reject_everything_else() {
        let too_long = "a".repeat(65);
        let bad = [
            "",
            "abc",
            "latest",
            "ABCD",
            "ab cd",
            "../..",
            "abcd/",
            too_long.as_str(),
        ];
        for text in bad {
            let result = text.parse::<SnapshotRef>();
            assert!(
                matches!(result, Err(Error::InvalidSnapshotRef(_))),
                "{text:?}"
            );
        }
    }

    #[test]
    fn an_id_converts_to_a_full_reference() {
        let id = SnapshotId::generate().expect("randomness available");
        let reference = SnapshotRef::from(&id);
        assert!(reference.is_full());
        assert_eq!(reference.as_str(), id.as_str());
    }
}
