//! Error type shared by all repository operations.

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
}

/// Result alias using [`enum@Error`].
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub(crate) fn not_implemented(operation: &'static str) -> Self {
        tracing::debug!(operation, "operation requested before storage is implemented");
        Self::NotImplemented { operation }
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
}
