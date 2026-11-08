//! Passphrase handling. The secret lives in a buffer that is zeroed on drop and is never shown
//! by `Debug`, so it cannot leak through `tracing` fields or error messages.

use std::fmt;

use zeroize::Zeroizing;

use crate::error::{Error, Result};

/// Longest accepted passphrase, in bytes.
pub const MAX_PASSPHRASE_BYTES: usize = 4096;

/// A repository passphrase: arbitrary non-empty bytes, not required to be UTF-8.
#[derive(Clone)]
pub struct Passphrase(Zeroizing<Vec<u8>>);

impl Passphrase {
    /// Wraps `bytes`, taking ownership so no extra copy of the secret is made.
    pub fn new(bytes: Vec<u8>) -> Result<Self> {
        let bytes = Zeroizing::new(bytes);
        if bytes.is_empty() {
            return Err(Error::InvalidPassphrase("must not be empty"));
        }
        if bytes.len() > MAX_PASSPHRASE_BYTES {
            return Err(Error::InvalidPassphrase("is longer than 4096 bytes"));
        }
        Ok(Self(bytes))
    }

    /// Like [`Passphrase::new`] for the contents of a passphrase file: a single trailing newline
    /// (`\n` or `\r\n`), as added by editors and `echo`, is not part of the passphrase.
    pub fn from_file_contents(mut bytes: Vec<u8>) -> Result<Self> {
        if bytes.ends_with(b"\n") {
            bytes.pop();
            if bytes.ends_with(b"\r") {
                bytes.pop();
            }
        }
        Self::new(bytes)
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for Passphrase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Passphrase(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_arbitrary_bytes() {
        let passphrase = Passphrase::new(vec![0xff, 0x00, b'a']).expect("valid");
        assert_eq!(passphrase.as_bytes(), [0xff, 0x00, b'a']);
    }

    #[test]
    fn rejects_empty_and_oversized() {
        assert!(matches!(
            Passphrase::new(Vec::new()),
            Err(Error::InvalidPassphrase(_))
        ));
        let too_long = vec![b'a'; MAX_PASSPHRASE_BYTES + 1];
        assert!(matches!(
            Passphrase::new(too_long),
            Err(Error::InvalidPassphrase(_))
        ));
        assert!(Passphrase::new(vec![b'a'; MAX_PASSPHRASE_BYTES]).is_ok());
    }

    #[test]
    fn file_contents_lose_exactly_one_trailing_newline() {
        let read = |text: &[u8]| Passphrase::from_file_contents(text.to_vec());
        assert_eq!(read(b"secret").expect("valid").as_bytes(), b"secret");
        assert_eq!(read(b"secret\n").expect("valid").as_bytes(), b"secret");
        assert_eq!(read(b"secret\r\n").expect("valid").as_bytes(), b"secret");
        assert_eq!(read(b"secret\n\n").expect("valid").as_bytes(), b"secret\n");
        assert_eq!(read(b" secret ").expect("valid").as_bytes(), b" secret ");
        assert!(read(b"\n").is_err());
        assert!(read(b"").is_err());
    }

    #[test]
    fn debug_output_never_contains_the_secret() {
        let passphrase = Passphrase::new(b"hunter2-unique-marker".to_vec()).expect("valid");
        let shown = format!("{passphrase:?} {:#?}", passphrase);
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(shown.contains("redacted"));
    }
}
