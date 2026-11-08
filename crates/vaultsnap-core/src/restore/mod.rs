//! Restoring a snapshot into a directory, safely.
//!
//! [`Repository::restore`](crate::Repository::restore) rebuilds the files of a snapshot under a
//! directory chosen by the caller. A snapshot is treated as untrusted even after it has been
//! authenticated, and the target as possibly hostile, so every step is defensive:
//!
//! * **Authenticated data only.** The snapshot is loaded through authenticated decryption and
//!   validated. Every chunk is read with [`ChunkStore::get`](crate::ChunkStore::get), which
//!   authenticates and decrypts it and checks that its content matches its id; its length is
//!   then checked against what the snapshot records, and a file's chunks must add up to its size.
//! * **Paths cannot escape.** Each destination is built from the restore root and individually
//!   checked path components: no `/`, no `.` or `..`, no empty or NUL-containing names, also for
//!   the source path of each root. Plans are made, and everything is checked, before the first
//!   byte is written. A symbolic link anywhere along a destination path is never followed.
//! * **Atomic files.** A file is written to a private temporary file (`0600`, created exclusively
//!   without following links) next to its destination and moved into place with a single rename
//!   (or, when nothing may be replaced, a hard link that fails if the name exists). Nobody ever
//!   sees a half-written file under its final name, and a stopped restore leaves none.
//! * **Conflicts are explicit.** [`ConflictPolicy`] decides what happens when something exists
//!   already: fail before writing anything, skip, or overwrite.
//! * **Symbolic links are restored only under a safe policy** ([`RestoreSymlinks`]).
//!
//! The layout under the target is the absolute path each source had when it was backed up, so
//! several sources never collide and `--include` paths read like the paths `ls` prints.
//! Details, guarantees, and limits are in `docs/RESTORE.md`.

mod dest;
mod engine;
mod plan;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::backup::CancelToken;
use crate::snapshot::{SnapshotId, SnapshotRef};

pub(crate) use engine::run;

/// What to do when something already exists where an entry would be restored.
///
/// A directory in the snapshot that meets a directory in the target is never a conflict: their
/// contents are merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConflictPolicy {
    /// Check everything first and, if anything would be overwritten, restore nothing and report
    /// the conflicts. This is the default.
    #[default]
    Fail,
    /// Leave what exists untouched, restore everything else, and report what was skipped. A
    /// directory that cannot be created because a file is in the way takes everything below it
    /// along.
    Skip,
    /// Replace files and symbolic links that are in the way, atomically. A file, link, or special
    /// file in the way of a directory is removed first. A directory is never removed or replaced:
    /// an entry that meets a directory is reported as failed. The attributes of a directory that
    /// already existed are updated too.
    Overwrite,
}

/// What to do with symbolic links in the snapshot.
///
/// A link is only data: its target is arbitrary text. Restoring a link is safe in itself, because
/// the restore never follows links, but a link that points out of the restored tree can lead
/// other programs (or the user) into places they did not mean to enter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RestoreSymlinks {
    /// Restore a link only if it can never lead out of the restore root: its target is relative,
    /// goes up (`..`) only at the start and not further than the root, and then only down. A link
    /// that does not qualify, such as `/etc/passwd` or `../../x` or `a/../../x`, is skipped and
    /// reported. This is the default.
    #[default]
    Contained,
    /// Restore no symbolic links.
    Skip,
    /// Restore every link exactly as recorded, including absolute and escaping ones. Opt in only
    /// for snapshots you trust.
    AsRecorded,
}

/// Progress notifications. Paths are in the restore target.
#[derive(Debug, Clone, Copy)]
pub enum RestoreEvent<'a> {
    /// The plan is made: how many regular files (hard links included) will be written, and
    /// their total size. Sent once, before anything is written.
    Planned {
        /// Files to restore.
        files: u64,
        /// Their total size in bytes.
        bytes: u64,
    },
    /// A chunk was written; `bytes` is its length.
    BytesWritten {
        /// Length of the chunk.
        bytes: u64,
    },
    /// An entry (file, directory, link) is in place.
    EntryRestored {
        /// Where it is.
        path: &'a Path,
    },
}

/// Receives [`RestoreEvent`]s. Any `Fn(&RestoreEvent<'_>) + Send + Sync` closure is an observer.
pub trait RestoreObserver: Send + Sync {
    /// Called for every event.
    fn event(&self, event: &RestoreEvent<'_>);
}

impl<F> RestoreObserver for F
where
    F: Fn(&RestoreEvent<'_>) + Send + Sync,
{
    fn event(&self, event: &RestoreEvent<'_>) {
        self(event);
    }
}

/// Everything that controls a restore.
#[derive(Clone)]
pub struct RestoreOptions {
    /// The snapshot: a full id, or the start of one that matches exactly one snapshot.
    pub snapshot: SnapshotRef,
    /// The directory to restore into. Created if it does not exist; it must not be inside the
    /// repository. If it is or contains symbolic links they are resolved once, here; below it
    /// nothing is followed.
    pub target: PathBuf,
    /// Logical paths (as `ls` prints them) to restore, each with everything below it. Empty
    /// means the whole snapshot. A path that matches nothing is an error. The directories above
    /// a selected path are created too, with the attributes the snapshot records for them.
    pub includes: Vec<PathBuf>,
    /// What to do about things that already exist.
    pub on_conflict: ConflictPolicy,
    /// What to do with symbolic links.
    pub symlinks: RestoreSymlinks,
    /// Receives progress events.
    pub observer: Option<Arc<dyn RestoreObserver>>,
    /// Asks the restore to stop. Files already restored stay, complete; nothing half-written
    /// remains.
    pub cancel: CancelToken,
}

impl RestoreOptions {
    /// Options to restore the whole of `snapshot` into `target` with the safest defaults:
    /// [`ConflictPolicy::Fail`] and [`RestoreSymlinks::Contained`].
    pub fn new(snapshot: SnapshotRef, target: impl Into<PathBuf>) -> Self {
        Self {
            snapshot,
            target: target.into(),
            includes: Vec::new(),
            on_conflict: ConflictPolicy::default(),
            symlinks: RestoreSymlinks::default(),
            observer: None,
            cancel: CancelToken::new(),
        }
    }
}

impl fmt::Debug for RestoreOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RestoreOptions")
            .field("snapshot", &self.snapshot)
            .field("target", &self.target)
            .field("includes", &self.includes)
            .field("on_conflict", &self.on_conflict)
            .field("symlinks", &self.symlinks)
            .field("observer", &self.observer.is_some())
            .field("cancel", &self.cancel)
            .finish()
    }
}

/// Something that was not restored, or not completely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreProblem {
    /// Where the entry would be or is, in the restore target.
    pub path: PathBuf,
    /// Why, in words.
    pub reason: String,
}

/// What a finished restore did. Whether the restore is complete is up to
/// [`RestoreReport::is_complete`]; `skipped` entries were left out on purpose.
#[derive(Debug, Clone)]
pub struct RestoreReport {
    /// The snapshot that was restored.
    pub snapshot: SnapshotId,
    /// The restore root, with symbolic links resolved.
    pub target: PathBuf,
    /// Regular files written (hard links that had to be written as files included).
    pub files: u64,
    /// Directories created or merged into.
    pub directories: u64,
    /// Symbolic links created.
    pub symlinks: u64,
    /// Hard links created.
    pub hard_links: u64,
    /// Bytes of file content written.
    pub bytes: u64,
    /// Entries left out on purpose: they already existed ([`ConflictPolicy::Skip`]), a symbolic
    /// link was refused by the [`RestoreSymlinks`] policy, or the entry is a special file
    /// (FIFO, socket, device), which is never restored.
    pub skipped: Vec<RestoreProblem>,
    /// Entries that should have been restored but could not be: a chunk is missing or damaged,
    /// the disk refused, a directory was in the way. Nothing half-written is left for them.
    pub failed: Vec<RestoreProblem>,
    /// Attributes (permissions, times) that could not be applied to entries that were restored.
    pub warnings: Vec<RestoreProblem>,
    /// Wall-clock time taken.
    pub duration: Duration,
}

impl RestoreReport {
    /// True if everything that should have been restored was. Skipped entries do not count
    /// against this; failures do.
    pub fn is_complete(&self) -> bool {
        self.failed.is_empty()
    }

    pub(crate) fn new(snapshot: SnapshotId, target: PathBuf) -> Self {
        Self {
            snapshot,
            target,
            files: 0,
            directories: 0,
            symlinks: 0,
            hard_links: 0,
            bytes: 0,
            skipped: Vec::new(),
            failed: Vec::new(),
            warnings: Vec::new(),
            duration: Duration::ZERO,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference() -> SnapshotRef {
        "abcd".parse().expect("valid reference")
    }

    #[test]
    fn the_defaults_are_the_safest_choices() {
        assert_eq!(ConflictPolicy::default(), ConflictPolicy::Fail);
        assert_eq!(RestoreSymlinks::default(), RestoreSymlinks::Contained);
        let options = RestoreOptions::new(reference(), "out");
        assert_eq!(options.on_conflict, ConflictPolicy::Fail);
        assert_eq!(options.symlinks, RestoreSymlinks::Contained);
        assert!(options.includes.is_empty() && options.observer.is_none());
        assert!(!options.cancel.is_cancelled());
        assert!(format!("{options:?}").contains("RestoreOptions"));
    }

    #[test]
    fn only_failures_make_a_report_incomplete() {
        let id = SnapshotId::generate().expect("randomness");
        let mut report = RestoreReport::new(id, PathBuf::from("out"));
        assert!(report.is_complete());
        let problem = RestoreProblem {
            path: PathBuf::from("out/x"),
            reason: "why".to_owned(),
        };
        report.skipped.push(problem.clone());
        report.warnings.push(problem.clone());
        assert!(report.is_complete(), "skipped entries and warnings are not failures");
        report.failed.push(problem);
        assert!(!report.is_complete());
    }

    #[test]
    fn closures_are_observers() {
        let seen = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counter = Arc::clone(&seen);
        let observer: Arc<dyn RestoreObserver> = Arc::new(move |event: &RestoreEvent<'_>| {
            if let RestoreEvent::BytesWritten { bytes } = event {
                counter.fetch_add(*bytes, std::sync::atomic::Ordering::SeqCst);
            }
        });
        observer.event(&RestoreEvent::BytesWritten { bytes: 7 });
        observer.event(&RestoreEvent::Planned { files: 1, bytes: 7 });
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 7);
    }
}
