//! Version information: what `vaultsnap --version` prints in one line, and what `vaultsnap
//! version` prints in full, including the repository formats this binary reads and writes.
//!
//! The build facts come from `build.rs`. The format versions come from `vaultsnap-core`, so they
//! cannot drift from what the code does: they are the constants the code compares against.

use std::io::{self, Write};

use vaultsnap_core::chunk_store::OBJECT_FORMAT_VERSION;
use vaultsnap_core::keys::KEY_FILE_VERSION;
use vaultsnap_core::repository::FORMAT_VERSION;
use vaultsnap_core::snapshot::{MANIFEST_FORMAT_VERSION, SNAPSHOT_OBJECT_VERSION};

/// The text after `vaultsnap ` in `--version`.
pub const SHORT: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("VAULTSNAP_BUILD_COMMIT"),
    ")"
);

/// The multi-line description printed by `vaultsnap version`.
pub fn render() -> String {
    let rows = [
        ("version", env!("CARGO_PKG_VERSION").to_owned()),
        ("commit", env!("VAULTSNAP_BUILD_COMMIT").to_owned()),
        ("target", env!("VAULTSNAP_BUILD_TARGET").to_owned()),
        ("profile", env!("VAULTSNAP_BUILD_PROFILE").to_owned()),
        ("repository format", FORMAT_VERSION.to_string()),
        ("snapshot schema", MANIFEST_FORMAT_VERSION.to_string()),
        ("snapshot container", SNAPSHOT_OBJECT_VERSION.to_string()),
        ("chunk object format", OBJECT_FORMAT_VERSION.to_string()),
        ("key file version", KEY_FILE_VERSION.to_string()),
        ("encryption", "XChaCha20-Poly1305".to_owned()),
        (
            "key derivation",
            "Argon2id (passphrase), HKDF-SHA256 (purpose keys)".to_owned(),
        ),
        ("chunk ids", "BLAKE3, keyed".to_owned()),
    ];
    let width = rows.iter().map(|(name, _)| name.len()).max().unwrap_or(0);
    let mut text = String::from("vaultsnap\n");
    for (name, value) in rows {
        text.push_str(&format!("{name:<width$}  {value}\n"));
    }
    text
}

/// Prints [`render`] on standard output.
pub fn print() -> io::Result<()> {
    io::stdout().lock().write_all(render().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_short_version_names_the_crate_version_and_a_commit() {
        assert!(SHORT.starts_with(env!("CARGO_PKG_VERSION")), "{SHORT}");
        assert!(SHORT.ends_with(')') && SHORT.contains(" ("), "{SHORT}");
    }

    #[test]
    fn the_full_version_lists_the_formats_the_code_compares_against() {
        let text = render();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "vaultsnap");
        let value = |name: &str| {
            lines
                .iter()
                .find_map(|line| line.strip_prefix(name))
                .map(|rest| rest.trim().to_owned())
                .unwrap_or_default()
        };
        assert_eq!(value("version"), env!("CARGO_PKG_VERSION"));
        assert_eq!(value("repository format"), FORMAT_VERSION.to_string());
        assert_eq!(
            value("snapshot schema"),
            MANIFEST_FORMAT_VERSION.to_string()
        );
        assert_eq!(
            value("chunk object format"),
            OBJECT_FORMAT_VERSION.to_string()
        );
        assert_eq!(value("key file version"), KEY_FILE_VERSION.to_string());
        assert!(
            value("target").len() > 3 && value("profile").len() > 3,
            "{text}"
        );
    }
}
