//! Backup logic and repository operations for VaultSnap.
//!
//! The repository format and security boundaries are specified in `docs/FORMAT.md` and
//! `docs/SECURITY.md`. Implemented: repository creation (`init`), configuration, the local
//! store, locking, and key management (a random master key protected by an Argon2id-derived key,
//! per-purpose HKDF keys, XChaCha20-Poly1305 sealing), a streaming content-defined chunker
//! ([`chunking`]), an encrypted, deduplicating chunk store ([`chunk_store`]), a filesystem scanner
//! ([`scan`]), encrypted, atomically committed snapshot manifests ([`snapshot`]), and the
//! incremental backup that ties them together ([`backup`], [`Repository::backup`]). Restore,
//! snapshot listing, check, and prune validate their arguments and then return
//! [`Error::NotImplemented`].

pub mod backup;
mod blocking;
pub mod chunk_store;
pub mod chunking;
pub mod config;
pub mod crypto;
pub mod error;
pub mod keys;
mod kv;
pub mod lock;
pub mod ops;
pub mod repository;
pub mod retention;
pub mod scan;
pub mod secret;
pub mod snapshot;
pub mod store;
#[cfg(test)]
mod testing;
mod util;

pub use backup::{
    BackupEvent, BackupObserver, BackupOptions, BackupReport, CancelToken, ParentPolicy,
};
pub use chunk_store::{ChunkId, ChunkStore, Compression, PutOutcome, PutResult};
pub use chunking::{Chunker, ChunkerConfig};
pub use config::RepositoryConfig;
pub use crypto::{KdfParams, KeyPurpose, SecretKey};
pub use error::{Error, Result};
pub use keys::RepositoryKeys;
pub use lock::{LockKind, RepositoryLock};
pub use ops::{CheckOptions, CheckReport, PruneOptions, PruneReport, RestoreOptions};
pub use repository::{Repository, RepositoryLayout};
pub use retention::RetentionPolicy;
pub use scan::{ScanOptions, Scanner};
pub use secret::Passphrase;
pub use snapshot::{
    Manifest, ManifestBuilder, SnapshotId, SnapshotListing, SnapshotStore, SnapshotSummary,
};
pub use store::LocalStore;
