//! Backup logic and repository operations for VaultSnap.
//!
//! The repository format and security boundaries are specified in `docs/FORMAT.md` and
//! `docs/SECURITY.md`. Implemented: repository creation (`init`), configuration, the local
//! store, locking, and key management (a random master key protected by an Argon2id-derived key,
//! per-purpose HKDF keys, XChaCha20-Poly1305 sealing), a streaming content-defined chunker
//! ([`chunking`]), an encrypted, deduplicating chunk store ([`chunk_store`]), a filesystem scanner
//! ([`scan`]), encrypted, atomically committed snapshot manifests ([`snapshot`]), and the
//! incremental backup that ties them together ([`backup`], [`Repository::backup`]), and the
//! inspection of snapshot history: listing, details, file listings, and comparison ([`history`]),
//! and the safe restore of snapshots into a directory ([`restore`], [`Repository::restore`]).
//! The repository check ([`check`], [`Repository::check`]) reports damage without ever changing
//! anything, and retention with mark-and-sweep garbage collection ([`prune`],
//! [`Repository::prune`]), the only operation that deletes data.

pub mod backup;
mod blocking;
pub mod check;
pub mod chunk_store;
pub mod chunking;
pub mod config;
pub mod crypto;
pub mod error;
pub mod history;
pub mod keys;
mod kv;
pub mod lock;
pub mod ops;
pub mod prune;
pub mod repository;
pub mod restore;
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
pub use check::{
    AffectedSnapshot, CheckEvent, CheckMode, CheckObserver, CheckOptions, CheckPhase, CheckReport,
    CheckStats, Finding, FindingKind, Severity,
};
pub use chunk_store::{ChunkId, ChunkStore, Compression, PutOutcome, PutResult};
pub use chunking::{Chunker, ChunkerConfig};
pub use config::RepositoryConfig;
pub use crypto::{KdfParams, KeyPurpose, SecretKey};
pub use error::{Error, Result};
pub use history::{Diff, SnapshotDetails, SnapshotList, SnapshotSizes};
pub use keys::RepositoryKeys;
pub use lock::{LockKind, RepositoryLock};
pub use prune::{
    PlanDigest, PruneEvent, PruneFailure, PruneMode, PruneObserver, PruneOptions, PrunePhase,
    PruneReport,
};
pub use repository::{Repository, RepositoryLayout};
pub use restore::{
    ConflictPolicy, RestoreEvent, RestoreObserver, RestoreOptions, RestoreProblem, RestoreReport,
    RestoreSymlinks,
};
pub use retention::{Candidate, Decision, KeepReason, RetentionPolicy};
pub use scan::{ScanOptions, Scanner};
pub use secret::Passphrase;
pub use snapshot::{
    Manifest, ManifestBuilder, SnapshotId, SnapshotListing, SnapshotRef, SnapshotStore,
    SnapshotSummary,
};
pub use store::LocalStore;
