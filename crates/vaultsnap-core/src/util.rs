//! Small helpers shared across the crate.

use std::fmt::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};

/// `byte_len` random bytes from the operating system, as lowercase hex.
pub(crate) fn random_hex(byte_len: usize) -> Result<String> {
    let mut bytes = vec![0_u8; byte_len];
    getrandom::getrandom(&mut bytes).map_err(|err| Error::Random(err.to_string()))?;
    Ok(hex(&bytes))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(out, "{byte:02x}").expect("writing to a String cannot fail");
    }
    out
}

/// Seconds since the Unix epoch; 0 if the clock is set before it.
pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_encodes_lowercase_with_padding() {
        assert_eq!(hex(&[0x00, 0x0f, 0xab, 0xff]), "000fabff");
        assert_eq!(hex(&[]), "");
    }

    #[test]
    fn random_hex_has_the_requested_length_and_alphabet() {
        let value = random_hex(16).expect("randomness available");
        assert_eq!(value.len(), 32);
        assert!(value.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
    }

    #[test]
    fn random_values_differ() {
        let a = random_hex(16).expect("randomness available");
        let b = random_hex(16).expect("randomness available");
        assert_ne!(a, b);
    }

    #[test]
    fn unix_now_is_after_2020() {
        assert!(unix_now() > 1_577_836_800);
    }
}
