//! Repository operations. Each submodule adds one operation to [`Repository`](crate::Repository).

mod backup;
mod check;
mod init;
mod prune;
mod restore;
mod snapshots;

pub use backup::BackupOptions;
pub use check::{CheckOptions, CheckReport};
pub use prune::{PruneOptions, PruneReport};
pub use restore::RestoreOptions;
pub use snapshots::SnapshotSummary;
