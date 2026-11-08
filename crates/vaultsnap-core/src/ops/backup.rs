use std::path::PathBuf;

use crate::error::{Error, Result};
use crate::repository::Repository;
use crate::snapshot::SnapshotId;

/// Inputs of a backup run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackupOptions {
    /// Files and directories to back up.
    pub sources: Vec<PathBuf>,
    /// Glob patterns to skip.
    pub excludes: Vec<String>,
    /// Labels stored with the snapshot.
    pub tags: Vec<String>,
}

impl Repository {
    /// Creates a snapshot of `options.sources` and returns its identifier.
    pub async fn backup(&self, options: &BackupOptions) -> Result<SnapshotId> {
        if options.sources.is_empty() {
            return Err(Error::InvalidArgument("at least one source path is required"));
        }
        Err(Error::not_implemented("backup"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_empty_sources() {
        let repo = Repository::at("/unused");
        let err = repo
            .backup(&BackupOptions::default())
            .await
            .expect_err("must fail");
        assert!(matches!(err, Error::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn valid_options_reach_the_unimplemented_storage_layer() {
        let repo = Repository::at("/unused");
        let options = BackupOptions {
            sources: vec![PathBuf::from("data")],
            ..Default::default()
        };
        let err = repo.backup(&options).await.expect_err("must fail");
        assert!(matches!(err, Error::NotImplemented { operation: "backup" }));
    }
}
