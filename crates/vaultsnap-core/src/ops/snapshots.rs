use crate::error::{Error, Result};
use crate::repository::Repository;
use crate::snapshot::SnapshotId;

/// One row of the snapshot listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotSummary {
    /// Snapshot identifier.
    pub id: SnapshotId,
    /// Creation time, seconds since the Unix epoch.
    pub created_at_unix: i64,
    /// Labels stored with the snapshot.
    pub tags: Vec<String>,
}

impl Repository {
    /// Lists the snapshots in the repository, oldest first.
    pub async fn snapshots(&self) -> Result<Vec<SnapshotSummary>> {
        Err(Error::not_implemented("snapshots"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn snapshots_is_not_implemented() {
        let err = Repository::at("/unused")
            .snapshots()
            .await
            .expect_err("must fail");
        assert!(matches!(err, Error::NotImplemented { operation: "snapshots" }));
    }
}
