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
    /// Chunk size limits are out of range or inconsistent.
    #[error("invalid chunker configuration: {0}")]
    InvalidChunkerConfig(String),
    /// A chunk identifier was not 64 lowercase hexadecimal characters.
    #[error("invalid chunk id {0:?}: expected 64 lowercase hexadecimal characters")]
    InvalidChunkId(String),
    /// The repository has no object for this chunk.
    #[error("chunk {0} is not in the repository")]
    ChunkNotFound(String),
    /// A chunk is larger than the largest chunk the format supports.
    #[error("chunk of {len} bytes exceeds the maximum of {max} bytes")]
    ChunkTooLarge {
        /// Size of the offered chunk.
        len: usize,
        /// The limit.
        max: usize,
    },
    /// A stored object failed verification: it is damaged, truncated, tampered with, stored
    /// under the wrong id, or the key is wrong. These cases cannot be told apart reliably.
    #[error("object {id} is corrupt: {reason}")]
    CorruptObject {
        /// The chunk id the object was read for.
        id: String,
        /// What failed.
        reason: String,
    },
    /// An object was written with a format version this build does not understand.
    #[error(
        "object format version {found} is not supported (this build supports version \
         {supported}); nothing was changed"
    )]
    UnsupportedObjectVersion {
        /// Version found in the object.
        found: u8,
        /// Version this build reads and writes.
        supported: u8,
    },
    /// Compressing a chunk failed.
    #[error("compression failed: {0}")]
    Compression(String),
    /// An include or exclude pattern could not be compiled.
    #[error("invalid pattern {pattern:?}: {reason}")]
    InvalidPattern {
        /// The pattern as given.
        pattern: String,
        /// Why it was rejected.
        reason: String,
    },
    /// A file was replaced, removed, or modified between being scanned and being read.
    #[error("file {} changed while it was being backed up: {}", .path.display(), .reason)]
    FileChanged {
        /// The file.
        path: PathBuf,
        /// What changed.
        reason: String,
    },
    /// Text naming a snapshot was not 4 to 64 lowercase hexadecimal characters.
    #[error(
        "invalid snapshot reference {0:?}: expected 4 to 64 lowercase hexadecimal characters \
         (the start of a snapshot id)"
    )]
    InvalidSnapshotRef(String),
    /// The start of a snapshot id matches more than one snapshot.
    #[error(
        "snapshot reference {prefix} matches {count} snapshots; use more characters of the id"
    )]
    AmbiguousSnapshot {
        /// The text that was given.
        prefix: String,
        /// How many snapshots start with it.
        count: usize,
    },
    /// There is no snapshot with this id.
    #[error("snapshot {0} does not exist")]
    SnapshotNotFound(String),
    /// A snapshot with this id was already committed; snapshots are never overwritten.
    #[error("snapshot {0} already exists")]
    SnapshotExists(String),
    /// A snapshot file failed verification: it is damaged, truncated, tampered with, filed under
    /// the wrong id, or the key is wrong. These cases cannot be told apart reliably.
    #[error("snapshot {id} is corrupt: {reason}")]
    CorruptSnapshot {
        /// The snapshot id it was read for.
        id: String,
        /// What failed.
        reason: String,
    },
    /// A snapshot file uses a container format version this build does not understand.
    #[error(
        "snapshot file format version {found} is not supported (this build supports version \
         {supported}); nothing was changed"
    )]
    UnsupportedSnapshotVersion {
        /// Version found in the file.
        found: u8,
        /// Version this build reads and writes.
        supported: u8,
    },
    /// A manifest uses a schema version this build does not understand.
    #[error(
        "snapshot manifest version {found} is not supported (this build supports version \
         {supported}); nothing was changed"
    )]
    UnsupportedManifestVersion {
        /// Version found in the manifest.
        found: u32,
        /// Version this build reads and writes.
        supported: u32,
    },
    /// A manifest is malformed or breaks the rules of a well-formed manifest.
    #[error("invalid snapshot manifest: {0}")]
    InvalidManifest(String),
    /// A snapshot refers to chunks that are not in the repository, so it cannot be complete.
    #[error(
        "snapshot is incomplete: {missing} referenced chunk(s) are missing from the repository \
         (first: {first}); nothing was committed"
    )]
    IncompleteSnapshot {
        /// How many distinct chunks are missing.
        missing: usize,
        /// The first missing chunk id.
        first: String,
    },
    /// The operation was cancelled; nothing was committed.
    #[error("cancelled: no snapshot was created (chunks already stored are kept and will be reused)")]
    Cancelled,
    /// A file could not be backed up and the error policy is to stop.
    #[error("cannot back up {}: {}", .path.display(), .reason)]
    SourceUnreadable {
        /// The file, relative to its backup root.
        path: PathBuf,
        /// Why.
        reason: String,
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
