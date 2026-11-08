//! Repository `config` file: versioned, plaintext, strictly parsed (see `docs/FORMAT.md`).

use crate::error::{Error, Result};
use crate::kv;
use crate::repository::{RepositoryLayout, FORMAT_VERSION};
use crate::store::Store;

const MAGIC: &str = "vaultsnap-repository";
const REPOSITORY_ID_LEN: usize = 32;

/// Upper bound on the size of a `config` file; larger files are rejected unread.
pub const MAX_CONFIG_BYTES: u64 = 4096;

/// Contents of the repository `config` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryConfig {
    /// Repository format version.
    pub format_version: u32,
    /// Random identifier, 32 lowercase hexadecimal characters, unique per repository.
    pub repository_id: String,
    /// Creation time, seconds since the Unix epoch.
    pub created_at_unix: u64,
}

impl RepositoryConfig {
    /// A configuration for the current format version.
    pub fn new(repository_id: String, created_at_unix: u64) -> Self {
        Self {
            format_version: FORMAT_VERSION,
            repository_id,
            created_at_unix,
        }
    }

    /// The exact bytes stored in the `config` file.
    pub fn to_bytes(&self) -> Vec<u8> {
        format!(
            "{MAGIC}\nformat_version={}\nrepository_id={}\ncreated_at_unix={}\n",
            self.format_version, self.repository_id, self.created_at_unix
        )
        .into_bytes()
    }

    /// Parses a `config` file.
    ///
    /// The format version is checked before anything else, so a repository written by a newer
    /// release yields [`Error::UnsupportedVersion`] even if it uses fields unknown here.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let text = std::str::from_utf8(bytes)
            .map_err(|_| Error::InvalidConfig("file is not valid UTF-8".to_owned()))?;
        let invalid = Error::InvalidConfig;
        let mut fields = kv::parse(text, MAGIC).map_err(invalid)?;

        let format_version =
            kv::take_number::<u32>(&mut fields, "format_version").map_err(invalid)?;
        if format_version != FORMAT_VERSION {
            return Err(Error::UnsupportedVersion {
                found: format_version,
                supported: FORMAT_VERSION,
            });
        }
        let repository_id = kv::take(&mut fields, "repository_id").map_err(invalid)?;
        if !is_repository_id(repository_id) {
            return Err(Error::InvalidConfig(format!(
                "`repository_id` must be {REPOSITORY_ID_LEN} lowercase hexadecimal characters"
            )));
        }
        let created_at_unix =
            kv::take_number::<u64>(&mut fields, "created_at_unix").map_err(invalid)?;
        kv::reject_unknown(&fields).map_err(invalid)?;

        Ok(Self {
            format_version,
            repository_id: repository_id.to_owned(),
            created_at_unix,
        })
    }

    /// Reads and parses the `config` file of the repository in `store`.
    ///
    /// A missing file is reported as an I/O error for which `Error::is_not_found` is true.
    pub fn load(store: &Store) -> Result<Self> {
        Self::parse(&store.read(RepositoryLayout::CONFIG_FILE, MAX_CONFIG_BYTES)?)
    }
}

fn is_repository_id(value: &str) -> bool {
    let is_hex = |b: u8| matches!(b, b'0'..=b'9' | b'a'..=b'f');
    value.len() == REPOSITORY_ID_LEN && value.bytes().all(is_hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> String {
        "0123456789abcdef".repeat(2)
    }

    fn valid_text() -> String {
        format!(
            "{MAGIC}\nformat_version=1\nrepository_id={}\ncreated_at_unix=1700000000\n",
            id()
        )
    }

    #[test]
    fn round_trips() {
        let config = RepositoryConfig::new(id(), 1_700_000_000);
        assert_eq!(config.format_version, FORMAT_VERSION);
        assert_eq!(config.to_bytes(), valid_text().into_bytes());
        assert_eq!(
            RepositoryConfig::parse(&config.to_bytes()).expect("parses"),
            config
        );
    }

    #[test]
    fn newer_version_is_reported_even_with_unknown_fields() {
        let text = format!("{MAGIC}\nformat_version=2\nsomething_new=yes\nrepository_id=zz\n");
        let err = RepositoryConfig::parse(text.as_bytes()).expect_err("must fail");
        assert!(matches!(
            err,
            Error::UnsupportedVersion {
                found: 2,
                supported: 1
            }
        ));
    }

    #[test]
    fn version_zero_is_unsupported() {
        let text = valid_text().replace("format_version=1", "format_version=0");
        let err = RepositoryConfig::parse(text.as_bytes()).expect_err("must fail");
        assert!(matches!(err, Error::UnsupportedVersion { found: 0, .. }));
    }

    #[test]
    fn rejects_malformed_input() {
        let cases = [
            String::new(),
            "not a config\n".to_owned(),
            valid_text().replace(MAGIC, "other-program"),
            valid_text().replace("format_version=1\n", ""),
            valid_text().replace("format_version=1", "format_version=one"),
            valid_text().replace("format_version=1", "format_version=-1"),
            valid_text().replace("repository_id=", "repository_id=ABC"),
            valid_text().replace(&id(), "abcd"),
            valid_text().replace(&id(), &id().to_uppercase()),
            valid_text().replace("\ncreated_at_unix=1700000000", ""),
            valid_text().replace("1700000000", "later"),
            format!("{}extra=1\n", valid_text()),
            format!("{}format_version=1\n", valid_text()),
            format!("{}no equals sign\n", valid_text()),
            valid_text().replace("format_version=1", "format_version = 1"),
        ];
        for case in cases {
            let err = RepositoryConfig::parse(case.as_bytes()).expect_err(&case);
            assert!(
                matches!(err, Error::InvalidConfig(_)),
                "{case:?} gave {err}"
            );
        }
    }

    #[test]
    fn rejects_invalid_utf8() {
        let err = RepositoryConfig::parse(&[0xff, 0xfe, 0x00]).expect_err("must fail");
        assert!(matches!(err, Error::InvalidConfig(_)));
    }
}
