//! Error type shared by all repository operations.

use std::path::{Path, PathBuf};

/// Errors returned by `vaultsnap-core`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The operation is specified but its storage behavior has not been built yet.
    #[error("{operation} is not implemented yet: repository storage has not been built")]
    NotImplemented {
        /// Name of the requested operation.
        operation: &'static str,
    },
    /// A snapshot identifier was not 64 lowercase hexadecimal characters.
    #[error("invalid snapshot id {0:?}: expected 64 lowercase hexadecimal characters")]
    InvalidSnapshotId(String),
    /// A retention policy would be unsafe or meaningless to apply.
    #[error("invalid retention policy: {0}")]
    InvalidRetention(&'static str),
    /// An operation was called with arguments it cannot act on.
    #[error("invalid argument: {0}")]
    InvalidArgument(&'static str),
    /// A filesystem operation failed.
    #[error("failed to {} {}: {}", .operation, .path.display(), .source)]
    Io {
        /// What was being attempted, e.g. `read`.
        operation: &'static str,
        /// The path involved.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The repository location cannot be used.
    #[error("invalid repository path: {0}")]
    InvalidPath(String),
    /// A store object name was empty, absolute, hidden, or contained unsafe characters.
    #[error("invalid object name {0:?}")]
    InvalidName(String),
    /// A repository already exists at the location; nothing was changed.
    #[error("a VaultSnap repository already exists at {}", .path.display())]
    AlreadyInitialized {
        /// The repository root.
        path: PathBuf,
    },
    /// The location does not contain a VaultSnap repository.
    #[error("{} is not a VaultSnap repository", .path.display())]
    NotARepository {
        /// The location that was inspected.
        path: PathBuf,
    },
    /// The repository was written with a format this build does not understand.
    #[error(
        "repository format version {found} is not supported (this build supports version \
         {supported}); nothing was changed"
    )]
    UnsupportedVersion {
        /// Version found in the repository.
        found: u32,
        /// Version this build reads and writes.
        supported: u32,
    },
    /// The repository `config` file is malformed.
    #[error("invalid repository config: {0}")]
    InvalidConfig(String),
    /// The repository is missing required parts.
    #[error("repository is damaged: {0}")]
    Corrupt(String),
    /// A file that must not be overwritten already exists.
    #[error("{} already exists", .path.display())]
    AlreadyExists {
        /// The existing file.
        path: PathBuf,
    },
    /// A symbolic link was found inside the repository where only real entries are allowed.
    #[error("refusing to follow symbolic link {}", .path.display())]
    Symlink {
        /// The symbolic link.
        path: PathBuf,
    },
    /// A file exceeded the size limit for its kind.
    #[error("{} is larger than the allowed {} bytes", .path.display(), .max_len)]
    TooLarge {
        /// The oversized file.
        path: PathBuf,
        /// The limit that was exceeded.
        max_len: u64,
    },
    /// Another process holds a conflicting repository lock.
    #[error(
        "repository is locked by {holder}; if no other VaultSnap process is running, remove \
         that lock file"
    )]
    Locked {
        /// Repository-relative name of the conflicting lock file.
        holder: String,
    },
    /// A passphrase was rejected before use.
    #[error("invalid passphrase: {0}")]
    InvalidPassphrase(&'static str),
    /// No key slot could be opened with the given passphrase.
    #[error("incorrect passphrase, or the key file was modified")]
    WrongPassphrase,
    /// Authenticated decryption failed.
    #[error("decryption failed: wrong key, or the data was corrupted or tampered with")]
    Decryption,
    /// A cryptographic primitive reported an internal failure.
    #[error("cryptographic operation failed: {0}")]
    Crypto(&'static str),
    /// Argon2id could not derive a key.
    #[error("key derivation failed: {0}")]
    Kdf(String),
    /// Key derivation parameters are out of the accepted range.
    #[error("invalid key derivation parameters: {0}")]
    InvalidKdfParams(String),
    /// A key file is malformed.
    #[error("invalid key file: {0}")]
    InvalidKeyFile(String),
    /// A key file was written with a version this build does not understand.
    #[error(
        "key file version {found} is not supported (this build supports version {supported}); \
         nothing was changed"
    )]
    UnsupportedKeyVersion {
        /// Version found in the key file.
        found: u32,
        /// Version this build reads and writes.
        supported: u32,
    },
    /// The operating system could not provide random bytes.
    #[error("failed to gather randomness: {0}")]
    Random(String),
    /// A background task failed to complete.
    #[error("background task failed: {0}")]
    Task(String),
}

/// Result alias using [`enum@Error`].
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub(crate) fn not_implemented(operation: &'static str) -> Self {
        tracing::debug!(operation, "operation requested before storage is implemented");
        Self::NotImplemented { operation }
    }

    pub(crate) fn io(operation: &'static str, path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            operation,
            path: path.to_path_buf(),
            source,
        }
    }

    pub(crate) fn is_not_found(&self) -> bool {
        matches!(self, Self::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_implemented_names_the_operation() {
        let message = Error::not_implemented("backup").to_string();
        assert!(message.starts_with("backup is not implemented"), "{message}");
    }

    #[test]
    fn invalid_snapshot_id_quotes_the_input() {
        let message = Error::InvalidSnapshotId("xyz".to_owned()).to_string();
        assert!(message.contains("\"xyz\""), "{message}");
    }

    #[test]
    fn io_error_names_operation_path_and_cause() {
        let cause = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let err = Error::io("read", Path::new("/repo/config"), cause);
        assert_eq!(err.to_string(), "failed to read /repo/config: denied");
        assert!(!err.is_not_found());
    }

    #[test]
    fn not_found_is_detected() {
        let cause = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert!(Error::io("open", Path::new("x"), cause).is_not_found());
        assert!(!Error::InvalidPath("x".to_owned()).is_not_found());
    }

    #[test]
    fn unsupported_version_message_is_single_line_and_clear() {
        let message = Error::UnsupportedVersion {
            found: 9,
            supported: 1,
        }
        .to_string();
        assert_eq!(
            message,
            "repository format version 9 is not supported (this build supports version 1); \
             nothing was changed"
        );
    }
}
