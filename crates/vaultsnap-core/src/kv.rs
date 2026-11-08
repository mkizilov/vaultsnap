//! Strict `key=value` text files, shared by the repository `config` and key files.
//!
//! The first line is a fixed header, every other line is `key=value`, keys are unique. Callers
//! take the fields they know with the `take_*` helpers and then call [`reject_unknown`], so
//! unexpected fields are errors rather than silently ignored. Errors are plain messages that the
//! caller wraps in its own error variant.

use std::collections::BTreeMap;
use std::str::FromStr;

use crate::util::decode_hex;

pub(crate) type Fields<'a> = BTreeMap<&'a str, &'a str>;
pub(crate) type ParseResult<T> = std::result::Result<T, String>;

pub(crate) fn parse<'a>(text: &'a str, magic: &str) -> ParseResult<Fields<'a>> {
    let mut lines = text.lines();
    if lines.next() != Some(magic) {
        return Err(format!("missing `{magic}` header"));
    }
    let mut fields = Fields::new();
    for (index, line) in lines.enumerate() {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("line {} is not `key=value`", index + 2))?;
        if fields.insert(key, value).is_some() {
            return Err(format!("duplicate key `{key}`"));
        }
    }
    Ok(fields)
}

pub(crate) fn take<'a>(fields: &mut Fields<'a>, key: &str) -> ParseResult<&'a str> {
    fields.remove(key).ok_or_else(|| format!("missing `{key}`"))
}

pub(crate) fn take_number<T: FromStr>(fields: &mut Fields<'_>, key: &str) -> ParseResult<T> {
    let value = take(fields, key)?;
    value
        .parse()
        .map_err(|_| format!("`{key}` is not a valid number: {value:?}"))
}

/// Takes `key` and requires it to equal `expected`.
pub(crate) fn take_exact(fields: &mut Fields<'_>, key: &str, expected: &str) -> ParseResult<()> {
    let value = take(fields, key)?;
    if value == expected {
        Ok(())
    } else {
        Err(format!(
            "unsupported `{key}`: {value:?} (expected {expected:?})"
        ))
    }
}

/// Takes `key` and decodes it as exactly `byte_len` bytes of lowercase hex.
pub(crate) fn take_hex(
    fields: &mut Fields<'_>,
    key: &str,
    byte_len: usize,
) -> ParseResult<Vec<u8>> {
    let value = take(fields, key)?;
    decode_hex(value)
        .filter(|bytes| bytes.len() == byte_len)
        .ok_or_else(|| format!("`{key}` must be {} lowercase hex characters", byte_len * 2))
}

pub(crate) fn reject_unknown(fields: &Fields<'_>) -> ParseResult<()> {
    match fields.keys().next() {
        Some(key) => Err(format!("unknown key `{key}`")),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fields_after_the_header() {
        let fields = parse("magic\na=1\nb=two=2\n", "magic").expect("parses");
        assert_eq!(fields.get("a"), Some(&"1"));
        assert_eq!(fields.get("b"), Some(&"two=2"));
    }

    #[test]
    fn rejects_bad_headers_lines_and_duplicates() {
        assert!(parse("", "magic").is_err());
        assert!(parse("other\na=1\n", "magic").is_err());
        assert!(parse("magic\nno equals\n", "magic").is_err());
        assert!(parse("magic\n\n", "magic").is_err());
        assert!(parse("magic\na=1\na=2\n", "magic").is_err());
    }

    #[test]
    fn take_helpers_consume_fields() {
        let mut fields = parse("m\nn=7\nh=0aff\nk=v\nbad=x\n", "m").expect("parses");
        assert_eq!(take_number::<u32>(&mut fields, "n"), Ok(7));
        assert_eq!(take_hex(&mut fields, "h", 2), Ok(vec![0x0a, 0xff]));
        assert_eq!(take_exact(&mut fields, "k", "v"), Ok(()));
        assert!(take_number::<u32>(&mut fields, "bad").is_err());
        assert!(take(&mut fields, "n").is_err(), "already taken");
        assert_eq!(reject_unknown(&fields), Ok(()));
    }

    #[test]
    fn take_helpers_report_problems() {
        let mut fields = parse("m\nh=0aff\nk=v\nextra=1\n", "m").expect("parses");
        assert!(take_hex(&mut fields, "h", 3).is_err());
        assert!(take_exact(&mut fields, "k", "w").is_err());
        assert!(take_number::<u32>(&mut fields, "missing").is_err());
        assert!(reject_unknown(&fields).is_err());
    }
}
