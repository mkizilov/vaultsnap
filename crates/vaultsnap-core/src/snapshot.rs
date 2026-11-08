//! Snapshot identifiers.

use std::fmt;
use std::str::FromStr;

use crate::error::Error;

const ID_LEN: usize = 64;
const SHORT_LEN: usize = 8;

/// Identifier of a snapshot: 64 lowercase hexadecimal characters (a 256-bit digest).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SnapshotId(String);

impl SnapshotId {
    /// The full identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// An abbreviated form for display; not unique and never valid input.
    pub fn short(&self) -> &str {
        &self.0[..SHORT_LEN]
    }
}

impl FromStr for SnapshotId {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
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
    fn parses_and_displays_a_valid_id() {
        let text = "0123456789abcdef".repeat(4);
        let id: SnapshotId = text.parse().expect("valid id");
        assert_eq!(id.to_string(), text);
        assert_eq!(id.as_str(), text);
        assert_eq!(id.short(), "01234567");
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
