//! Backup logic and repository operations for VaultSnap.
//!
//! The repository format and security boundaries are specified in `docs/FORMAT.md` and
//! `docs/SECURITY.md`. Storage behavior is not implemented yet: every repository operation
//! validates its arguments and then returns [`Error::NotImplemented`].

pub mod error;
pub mod ops;
pub mod repository;
pub mod retention;
pub mod snapshot;

pub use error::{Error, Result};
pub use ops::{
    BackupOptions, CheckOptions, CheckReport, PruneOptions, PruneReport, RestoreOptions,
    SnapshotSummary,
};
pub use repository::{Repository, RepositoryLayout};
pub use retention::RetentionPolicy;
pub use snapshot::SnapshotId;
