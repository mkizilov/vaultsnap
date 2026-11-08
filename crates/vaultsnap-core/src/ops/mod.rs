//! Repository operations. Each submodule adds one operation to [`Repository`](crate::Repository).

mod backup;
mod check;
mod init;
mod prune;
mod restore;
mod snapshots;
mod unlock;

pub use prune::{PruneOptions, PruneReport};
