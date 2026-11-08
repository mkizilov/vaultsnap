use crate::error::{Error, Result};
use crate::repository::Repository;
use crate::retention::RetentionPolicy;
use crate::snapshot::SnapshotId;

/// Inputs of a prune run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneOptions {
    /// Which snapshots to keep.
    pub policy: RetentionPolicy,
    /// Report what would be removed without removing anything.
    pub dry_run: bool,
}

/// Outcome of a prune run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneReport {
    /// Snapshots removed, or that would be removed in a dry run.
    pub removed: Vec<SnapshotId>,
}

impl Repository {
    /// Removes snapshots not selected by the retention policy, then unreferenced data.
    pub async fn prune(&self, options: &PruneOptions) -> Result<PruneReport> {
        options.policy.validate()?;
        Err(Error::not_implemented("prune"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_a_policy_that_keeps_nothing() {
        let err = Repository::at("/unused")
            .prune(&PruneOptions::default())
            .await
            .expect_err("must fail");
        assert!(matches!(err, Error::InvalidRetention(_)));
    }

    #[tokio::test]
    async fn valid_policy_reaches_the_unimplemented_storage_layer() {
        let options = PruneOptions {
            policy: RetentionPolicy {
                keep_last: Some(1),
                ..Default::default()
            },
            dry_run: true,
        };
        let err = Repository::at("/unused")
            .prune(&options)
            .await
            .expect_err("must fail");
        assert!(matches!(err, Error::NotImplemented { operation: "prune" }));
    }
}
