//! Encrypted, versioned snapshot manifests.
//!
//! A snapshot records one backup: when it was taken, which roots were backed up, the directory
//! structure with every entry's attributes, for every file the ordered list of chunks that make up
//! its content, how the backup was configured, and what was skipped. It is a single immutable file,
//! `snapshots/<id>`, encrypted and authenticated together with its id (so every file name and path
//! is protected), and committed atomically: a snapshot either exists completely or not at all.
//!
//! * [`Manifest`] is the plaintext model, with [`Manifest::validate`] as the single definition of
//!   a well-formed manifest (safe single-component names, parents before children, chunk lengths
//!   adding up to file sizes, consistent totals).
//! * [`ManifestBuilder`] assembles a manifest from [`crate::scan`] entries and chunk references.
//! * [`SnapshotStore`] commits, loads, and lists snapshots.
//!
//! The byte-level formats and the compatibility rules are in `docs/FORMAT.md`. Restoring a
//! snapshot is not implemented yet; loading one only reads and verifies its manifest.

mod builder;
mod codec;
mod id;
mod model;
mod object;
mod store;

pub(crate) use builder::{os_bytes, relative_path_bytes};
pub use builder::ManifestBuilder;
pub use id::SnapshotId;
pub use model::{
    is_valid_name, Attributes, BackupConfig, ChunkRef, Manifest, ManifestEntry, ManifestStats,
    NodeKind, Root, SkippedReason, SkippedRecord, SnapshotSummary, MANIFEST_FORMAT_VERSION,
    MAX_NAME_LEN, MAX_SOURCE_LEN,
};
pub use object::{MAX_MANIFEST_BYTES, SNAPSHOT_OBJECT_VERSION};
pub use store::{SnapshotListing, SnapshotStore};
