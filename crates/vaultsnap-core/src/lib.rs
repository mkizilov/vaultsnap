//! Backup logic and repository operations for VaultSnap.
//!
//! The repository format and security boundaries are specified in `docs/FORMAT.md` and
//! `docs/SECURITY.md`. Repository creation (`init`), configuration, the local store, and locking
//! are implemented. Backup, restore, and the remaining operations validate their arguments and
//! then return [`Error::NotImplemented`]. Encryption is not implemented yet.

mod blocking;
pub mod config;
pub mod error;
pub mod lock;
pub mod ops;
pub mod repository;
pub mod retention;
pub mod snapshot;
pub mod store;
mod util;

pub use config::RepositoryConfig;
pub use error::{Error, Result};
pub use lock::{LockKind, RepositoryLock};
pub use ops::{
    BackupOptions, CheckOptions, CheckReport, PruneOptions, PruneReport, RestoreOptions,
    SnapshotSummary,
};
pub use repository::{Repository, RepositoryLayout};
pub use retention::RetentionPolicy;
pub use snapshot::SnapshotId;
pub use store::LocalStore;
