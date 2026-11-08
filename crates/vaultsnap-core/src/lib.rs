//! Backup logic and repository operations for VaultSnap.
//!
//! The repository format and security boundaries are specified in `docs/FORMAT.md` and
//! `docs/SECURITY.md`. Implemented: repository creation (`init`), configuration, the local
//! store, locking, and key management (a random master key protected by an Argon2id-derived key,
//! per-purpose HKDF keys, XChaCha20-Poly1305 sealing). Backup, restore, and the remaining
//! operations validate their arguments and then return [`Error::NotImplemented`]; nothing encrypts
//! file data yet.

mod blocking;
pub mod config;
pub mod crypto;
pub mod error;
pub mod keys;
mod kv;
pub mod lock;
pub mod ops;
pub mod repository;
pub mod retention;
pub mod secret;
pub mod snapshot;
pub mod store;
#[cfg(test)]
mod testing;
mod util;

pub use config::RepositoryConfig;
pub use crypto::{KdfParams, KeyPurpose, SecretKey};
pub use error::{Error, Result};
pub use keys::RepositoryKeys;
pub use lock::{LockKind, RepositoryLock};
pub use ops::{
    BackupOptions, CheckOptions, CheckReport, PruneOptions, PruneReport, RestoreOptions,
    SnapshotSummary,
};
pub use repository::{Repository, RepositoryLayout};
pub use retention::RetentionPolicy;
pub use secret::Passphrase;
pub use snapshot::SnapshotId;
pub use store::LocalStore;
