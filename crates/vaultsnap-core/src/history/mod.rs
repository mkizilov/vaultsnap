//! Looking at what has been backed up: listing snapshots, describing one, walking its files, and
//! comparing two.
//!
//! Everything here works on snapshots that were read with
//! [`SnapshotStore::load`](crate::SnapshotStore::load), which authenticates the file against its
//! id before decrypting it and validates the manifest afterwards. A snapshot that fails is
//! reported as damaged and never shown. File data is never read: sizes come from the manifests
//! and from the lengths of the chunk object files.
//!
//! * [`SnapshotList`]: the snapshots oldest first (by the creation time inside the manifest) with
//!   their [`SnapshotSizes`].
//! * [`SnapshotDetails`]: one snapshot in full.
//! * [`entries()`]: the paths of a snapshot, optionally below one path.
//! * [`Diff`]: the paths added, deleted, and modified between two snapshots.
//!
//! The operations are methods of [`Repository`](crate::Repository); see `docs/CLI.md` for the
//! command-line view and `docs/FORMAT.md` for how sizes are defined.

mod diff;
mod entries;
mod list;
mod sizes;

pub use diff::{Aspect, Change, Diff, Status};
pub use entries::{content_size, entries, skipped_path, EntryType, ListedEntry};
pub use list::{unreadable_paths, DamagedSnapshot, SnapshotDetails, SnapshotInfo, SnapshotList};
pub use sizes::SnapshotSizes;

pub(crate) use diff::compare;
pub(crate) use list::{details, list};
