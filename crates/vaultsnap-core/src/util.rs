//! Small helpers shared across the crate.

use std::fmt::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};

/// Fills `buf` with random bytes from the operating system.
pub(crate) fn fill_random(buf: &mut [u8]) -> Result<()> {
    getrandom::getrandom(buf).map_err(|err| Error::Random(err.to_string()))
}

/// `N` random bytes from the operating system.
pub(crate) fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0_u8; N];
    fill_random(&mut bytes)?;
    Ok(bytes)
}

/// `byte_len` random bytes from the operating system, as lowercase hex.
pub(crate) fn random_hex(byte_len: usize) -> Result<String> {
    let mut bytes = vec![0_u8; byte_len];
    fill_random(&mut bytes)?;
    Ok(hex(&bytes))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(out, "{byte:02x}").expect("writing to a String cannot fail");
    }
    out
}

/// Decodes lowercase hex; `None` for odd length or any other character.
pub(crate) fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    text.as_bytes()
        .chunks(2)
        .map(|pair| Some((nibble(pair[0])? << 4) | nibble(pair[1])?))
        .collect()
}

fn nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
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
    fn decode_hex_round_trips_and_rejects_bad_input() {
        assert_eq!(decode_hex("000fabff"), Some(vec![0x00, 0x0f, 0xab, 0xff]));
        assert_eq!(decode_hex(""), Some(Vec::new()));
        assert_eq!(decode_hex("abc"), None);
        assert_eq!(decode_hex("AB"), None);
        assert_eq!(decode_hex("zz"), None);
        assert_eq!(decode_hex("0x"), None);
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
        let c = random_bytes::<24>().expect("randomness available");
        let d = random_bytes::<24>().expect("randomness available");
        assert_ne!(c, d);
    }

    #[test]
    fn unix_now_is_after_2020() {
        assert!(unix_now() > 1_577_836_800);
    }
}
