use crate::error::{Error, Result};
use crate::repository::Repository;
use crate::snapshot::SnapshotSummary;

impl Repository {
    /// Lists the snapshots in the repository, oldest first.
    ///
    /// Not implemented yet: reading snapshots needs the repository keys, which the command line
    /// does not obtain for this command so far. The building blocks exist in
    /// [`crate::snapshot::SnapshotStore`] (see [`Repository::snapshot_store`]).
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
