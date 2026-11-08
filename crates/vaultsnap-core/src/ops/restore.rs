use std::path::PathBuf;

use crate::error::{Error, Result};
use crate::repository::Repository;
use crate::snapshot::SnapshotId;

/// Inputs of a restore run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreOptions {
    /// Snapshot to restore.
    pub snapshot: SnapshotId,
    /// Directory to restore into.
    pub target: PathBuf,
    /// Paths inside the snapshot to restore; empty means everything.
    pub includes: Vec<PathBuf>,
}

impl Repository {
    /// Restores a snapshot into `options.target`.
    pub async fn restore(&self, options: &RestoreOptions) -> Result<()> {
        let _ = options;
        Err(Error::not_implemented("restore"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn restore_is_not_implemented() {
        let options = RestoreOptions {
            snapshot: "a".repeat(64).parse().expect("valid id"),
            target: PathBuf::from("out"),
            includes: Vec::new(),
        };
        let err = Repository::at("/unused")
            .restore(&options)
            .await
            .expect_err("must fail");
        assert!(matches!(err, Error::NotImplemented { operation: "restore" }));
    }
}
