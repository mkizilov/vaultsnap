//! Where passphrases come from: an explicit file for automation, otherwise an interactive prompt
//! with terminal echo disabled. Never a command-line argument or environment variable value,
//! both of which are visible to other processes.

use std::fs::File;
use std::io::{IsTerminal, Read, Write};
use std::path::Path;

use vaultsnap_core::{Passphrase, Repository, RepositoryKeys};
use zeroize::Zeroizing;

use crate::commands::CliError;

/// Larger files are certainly not passphrase files.
const MAX_PASSWORD_FILE_BYTES: u64 = 4096;

/// Obtains the passphrase for a repository that is about to be created. From a file when
/// `password_file` is given, otherwise by asking twice on the terminal.
pub fn read_new(password_file: Option<&Path>) -> Result<Passphrase, CliError> {
    match password_file {
        Some(path) => from_file(path),
        None => prompt_new(),
    }
}

/// Obtains the passphrase of an existing repository. From a file when `password_file` is given,
/// otherwise by asking once on the terminal.
pub fn read_existing(password_file: Option<&Path>) -> Result<Passphrase, CliError> {
    match password_file {
        Some(path) => from_file(path),
        None => prompt_existing(),
    }
}

/// Reads the passphrase of an existing repository and unlocks it. The passphrase is cleared
/// when this returns; only the derived keys live on.
pub async fn unlock(
    repo: &Repository,
    password_file: Option<&Path>,
) -> Result<RepositoryKeys, CliError> {
    let passphrase = read_existing(password_file)?;
    Ok(repo.unlock(&passphrase).await?)
}

fn from_file(path: &Path) -> Result<Passphrase, CliError> {
    let io_error = |source: std::io::Error| CliError::PasswordFile {
        path: path.to_path_buf(),
        source,
    };
    let file = File::open(path).map_err(io_error)?;
    warn_if_shared(&file, path);
    let mut bytes = Vec::new();
    file.take(MAX_PASSWORD_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() as u64 > MAX_PASSWORD_FILE_BYTES {
        bytes.fill(0);
        return Err(CliError::PasswordFileTooLarge {
            path: path.to_path_buf(),
        });
    }
    Ok(Passphrase::from_file_contents(bytes)?)
}

#[cfg(unix)]
fn warn_if_shared(file: &File, path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    if let Ok(metadata) = file.metadata() {
        if metadata.permissions().mode() & 0o077 != 0 {
            tracing::warn!(
                path = %path.display(),
                "password file is accessible by other users; restrict it with chmod 600"
            );
        }
    }
}

#[cfg(not(unix))]
fn warn_if_shared(_file: &File, _path: &Path) {}

fn prompt_new() -> Result<Passphrase, CliError> {
    if !std::io::stdin().is_terminal() {
        return Err(CliError::NoTerminal);
    }
    let first = prompt("Enter new passphrase: ")?;
    let second = prompt("Enter same passphrase again: ")?;
    if *first != *second {
        return Err(CliError::PassphraseMismatch);
    }
    Ok(Passphrase::new(first.as_bytes().to_vec())?)
}

fn prompt_existing() -> Result<Passphrase, CliError> {
    if !std::io::stdin().is_terminal() {
        return Err(CliError::NoTerminal);
    }
    let line = prompt("Enter passphrase: ")?;
    Ok(Passphrase::new(line.as_bytes().to_vec())?)
}

fn prompt(label: &str) -> Result<Zeroizing<String>, CliError> {
    let mut stderr = std::io::stderr();
    write!(stderr, "{label}")
        .and_then(|()| stderr.flush())
        .map_err(CliError::Terminal)?;
    let line = rpassword::read_password().map_err(CliError::Terminal)?;
    Ok(Zeroizing::new(line))
}
