//! Verifying that a repository is intact.
//!
//! [`Repository::check`](crate::Repository::check) examines a repository and **reports**; it never
//! repairs, moves, or deletes anything. The only thing it writes is its own shared lock file,
//! which it removes when it is done. What it does not like it describes as a [`Finding`], with a
//! stable [`FindingKind`], and leaves exactly as it is.
//!
//! # What is checked
//!
//! In both modes:
//!
//! * **Metadata.** The `config` file is valid (that is a precondition: without it there is nothing
//!   to check); the required directories exist and are directories; key files can be parsed;
//!   every entry in the repository is something VaultSnap wrote (anything else is reported, and
//!   the leftovers of interrupted writes and of killed processes are recognized).
//! * **Snapshots.** Every snapshot file is read through authenticated decryption and its manifest
//!   is validated. A snapshot that fails is damaged and is never trusted.
//! * **Chunks.** Every chunk a readable snapshot refers to must exist as an object. Every object
//!   in `data/` is examined, whether a snapshot refers to it or not.
//! * **Orphans.** Objects that no readable snapshot refers to are reported.
//!
//! The two modes differ in how objects are examined ([`CheckMode`]):
//!
//! * [`CheckMode::Quick`] reads the first 12 bytes of each object and checks everything that can
//!   be checked without a key: magic, version, header fields, that the size of the file is
//!   possible for what the header declares, and that the length the header declares is the length
//!   the snapshots record. It finds missing objects, truncated or extended uncompressed objects,
//!   and damaged headers. **It cannot see damage inside the encrypted payload**, nor a truncated
//!   compressed object.
//! * [`CheckMode::Full`] reads every object completely: authenticates it, decrypts and
//!   decompresses it, and checks that its content hashes to the id it is stored under. Nothing
//!   that this finds can be missed by it; it costs one read of the whole repository.
//!
//! # Concurrency
//!
//! A check holds a shared lock, so it runs next to a backup, a restore, or another check, and
//! refuses to run next to a prune. The order of the steps (snapshot files are listed before
//! objects are) means a backup that commits while a check runs can show up as unreferenced
//! objects, never as missing ones.
//!
//! The behavior, the findings, and what to do about each of them are in `docs/CHECK.md`.

mod engine;
mod scan;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use crate::backup::CancelToken;
use crate::snapshot::SnapshotId;

pub(crate) use engine::{run, Subject};

/// How many findings a report lists unless asked otherwise.
pub const DEFAULT_MAX_FINDINGS: usize = 1000;

/// How thoroughly the chunk objects are examined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckMode {
    /// Structure only: see the [module documentation](self).
    Quick,
    /// Every object is read, authenticated, and decrypted.
    Full,
}

impl CheckMode {
    /// The name used in machine-readable output; part of the output contract.
    pub fn name(self) -> &'static str {
        match self {
            Self::Quick => "quick",
            Self::Full => "full",
        }
    }
}

/// How serious a finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    /// Data is damaged or missing, or the repository cannot be fully trusted or used.
    Error,
    /// Something is unusual or untidy, but no data is at risk.
    Warning,
}

impl Severity {
    /// The name used in machine-readable output; part of the output contract.
    pub fn name(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
        }
    }
}

/// What a finding is about. The order of the variants is the order findings are listed in:
/// errors first, then warnings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FindingKind {
    /// A required directory is missing, or is something other than a directory.
    MissingDirectory,
    /// A snapshot file failed authentication, decryption, or validation.
    SnapshotDamaged,
    /// Something was written by a newer VaultSnap than this one.
    UnsupportedVersion,
    /// A snapshot refers to a chunk that has no object.
    ChunkMissing,
    /// A chunk object is damaged: wrong structure, truncated, extended, modified, or stored under
    /// the wrong id.
    ChunkCorrupt,
    /// An authentic or well-formed object and the snapshots disagree about the chunk's length.
    ChunkLengthMismatch,
    /// The operating system refused a read.
    IoError,
    /// A chunk object that no readable snapshot refers to.
    OrphanObject,
    /// An entry that is not part of a VaultSnap repository.
    UnexpectedFile,
    /// A temporary file left behind by an interrupted write.
    LeftoverTempFile,
    /// A lock file exists besides the check's own.
    LockPresent,
    /// A key file that cannot be parsed.
    KeyFileInvalid,
}

impl FindingKind {
    /// Every kind, in listing order.
    pub const ALL: [FindingKind; 12] = [
        Self::MissingDirectory,
        Self::SnapshotDamaged,
        Self::UnsupportedVersion,
        Self::ChunkMissing,
        Self::ChunkCorrupt,
        Self::ChunkLengthMismatch,
        Self::IoError,
        Self::OrphanObject,
        Self::UnexpectedFile,
        Self::LeftoverTempFile,
        Self::LockPresent,
        Self::KeyFileInvalid,
    ];

    /// The name used in machine-readable output and in `docs/CHECK.md`; never changes.
    pub fn name(self) -> &'static str {
        match self {
            Self::MissingDirectory => "missing_directory",
            Self::SnapshotDamaged => "snapshot_damaged",
            Self::UnsupportedVersion => "unsupported_version",
            Self::ChunkMissing => "chunk_missing",
            Self::ChunkCorrupt => "chunk_corrupt",
            Self::ChunkLengthMismatch => "chunk_length_mismatch",
            Self::IoError => "io_error",
            Self::OrphanObject => "orphan_object",
            Self::UnexpectedFile => "unexpected_file",
            Self::LeftoverTempFile => "leftover_temp_file",
            Self::LockPresent => "lock_present",
            Self::KeyFileInvalid => "key_file_invalid",
        }
    }

    /// How serious findings of this kind are.
    pub fn severity(self) -> Severity {
        match self {
            Self::MissingDirectory
            | Self::SnapshotDamaged
            | Self::UnsupportedVersion
            | Self::ChunkMissing
            | Self::ChunkCorrupt
            | Self::ChunkLengthMismatch
            | Self::IoError => Severity::Error,
            Self::OrphanObject
            | Self::UnexpectedFile
            | Self::LeftoverTempFile
            | Self::LockPresent
            | Self::KeyFileInvalid => Severity::Warning,
        }
    }

    /// What to do about it, in a few sentences. The longer version is in `docs/CHECK.md`. None of
    /// it is done for the user: a check never changes the repository.
    pub fn advice(self) -> &'static str {
        match self {
            Self::MissingDirectory => {
                "Restore the directory and its contents from a copy of the repository. Creating \
                 an empty one only hides the loss."
            }
            Self::SnapshotDamaged => {
                "The snapshot cannot be trusted or restored. Put an intact copy of its file back \
                 from a copy of the repository; the other snapshots are not affected."
            }
            Self::UnsupportedVersion => {
                "This was written by a newer VaultSnap. Use that version or a newer one; do not \
                 modify the file."
            }
            Self::ChunkMissing => {
                "Snapshots that refer to it cannot be restored completely (see the affected \
                 snapshots). Copy the object back from a copy of the repository, or, if the \
                 original data is still unchanged, run `backup` again: it stores the missing \
                 chunk afresh and thereby repairs the snapshots that refer to it."
            }
            Self::ChunkCorrupt => {
                "The object is damaged and a restore will refuse it. Move it out of the \
                 repository (keep it for diagnosis), then copy an intact one back or run `backup` \
                 again so it is stored afresh. Until it is moved, a new backup would reuse the \
                 damaged object."
            }
            Self::ChunkLengthMismatch => {
                "The snapshots and the object disagree about the size of a chunk, which should \
                 be impossible. Treat the object as damaged, as for chunk_corrupt."
            }
            Self::IoError => {
                "The operating system refused a read: look at permissions, the disk, or the \
                 network drive, fix the cause, and run the check again. The data may be fine."
            }
            Self::OrphanObject => {
                "No snapshot refers to this object. It is normally left by an interrupted backup \
                 or a deleted snapshot, wastes space only, and is harmless. The check does not \
                 remove it; a future prune will."
            }
            Self::UnexpectedFile => {
                "VaultSnap did not write this and ignores it. Remove it by hand if you know it \
                 does not belong here."
            }
            Self::LeftoverTempFile => {
                "An interrupted write left this behind. It is ignored and can be removed by hand \
                 when no VaultSnap process is running."
            }
            Self::LockPresent => {
                "A process may be using the repository, which is fine. If none is running, the \
                 lock is stale (a process was killed) and blocks prune: remove the file by hand \
                 after making sure."
            }
            Self::KeyFileInvalid => {
                "Another key file opened the repository, so nothing is lost now. Keep a copy of \
                 the key file that works; this one can be removed by hand."
            }
        }
    }
}

impl fmt::Display for FindingKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// One thing the check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// What kind of finding it is.
    pub kind: FindingKind,
    /// What it is about: a repository-relative path such as `snapshots/<id>` or
    /// `data/ab/<id>`. Never a name from the backed-up data. For an unexpected entry it is the
    /// path of that entry, which can be anything.
    pub subject: String,
    /// What is wrong, in words. Contains no names from the backed-up data.
    pub detail: String,
}

impl Finding {
    /// How serious it is.
    pub fn severity(&self) -> Severity {
        self.kind.severity()
    }
}

/// A snapshot that cannot be restored completely because chunks it needs are missing or damaged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AffectedSnapshot {
    /// The snapshot.
    pub snapshot: SnapshotId,
    /// How many different damaged or missing chunks its files need.
    pub damaged_chunks: u64,
    /// How many regular file entries need at least one of them. Hard links to such a file are
    /// not counted separately.
    pub files: u64,
}

/// Counts of what was examined.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckStats {
    /// Key files that parse.
    pub key_files: u64,
    /// Snapshots that could be read.
    pub snapshots: u64,
    /// Snapshots that could not.
    pub snapshots_damaged: u64,
    /// Chunk objects found in `data/`.
    pub objects: u64,
    /// Their total size in bytes.
    pub object_bytes: u64,
    /// Distinct chunks that readable snapshots refer to.
    pub chunks_referenced: u64,
    /// Of those, how many have no object.
    pub chunks_missing: u64,
    /// Objects examined: all of `objects`.
    pub objects_checked: u64,
    /// Of those, how many failed.
    pub objects_damaged: u64,
    /// Bytes of objects that were read completely and authenticated (always 0 in quick mode).
    pub bytes_authenticated: u64,
    /// Objects that no readable snapshot refers to.
    pub orphans: u64,
    /// Their total size in bytes.
    pub orphan_bytes: u64,
    /// False if a snapshot could not be read: its chunks look like orphans, so the orphan counts
    /// can be too high.
    pub orphans_exact: bool,
}

/// The outcome of a check.
#[derive(Debug, Clone)]
pub struct CheckReport {
    /// The repository's id.
    pub repository_id: String,
    /// The repository's format version.
    pub format_version: u32,
    /// How thoroughly the objects were examined.
    pub mode: CheckMode,
    /// What was examined.
    pub stats: CheckStats,
    /// Snapshots that cannot be restored completely, in snapshot id order. Empty unless chunks
    /// are missing or damaged.
    pub affected: Vec<AffectedSnapshot>,
    /// The findings: errors first, each group in the order of [`FindingKind`] and then of the
    /// repository's own names. Limited to [`CheckOptions::max_findings`]; the counts below are
    /// always exact.
    pub findings: Vec<Finding>,
    /// How many errors were found (listed or not).
    pub errors: u64,
    /// How many warnings were found (listed or not).
    pub warnings: u64,
    /// How many findings were not listed because of the limit.
    pub suppressed: u64,
    /// Exact number of findings of each kind that occurred.
    pub counts: BTreeMap<FindingKind, u64>,
    /// Wall-clock time taken.
    pub duration: Duration,
}

impl CheckReport {
    /// True if no errors were found. Warnings do not count against this.
    pub fn is_ok(&self) -> bool {
        self.errors == 0
    }

    /// True if there is nothing at all to report.
    pub fn is_clean(&self) -> bool {
        self.errors == 0 && self.warnings == 0
    }

    /// How many findings of `kind` occurred.
    pub fn count(&self, kind: FindingKind) -> u64 {
        self.counts.get(&kind).copied().unwrap_or(0)
    }
}

/// The steps of a check, for progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckPhase {
    /// Walking the repository's directories.
    Structure,
    /// Reading the snapshots.
    Snapshots,
    /// Examining the chunk objects.
    Objects,
}

/// Progress notifications. Sent from several threads during the object phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckEvent {
    /// A phase begins; it has `total` steps (0 if unknown).
    Started {
        /// The phase.
        phase: CheckPhase,
        /// How many steps it will take.
        total: u64,
    },
    /// One step of the phase is done.
    Step {
        /// The phase.
        phase: CheckPhase,
    },
}

/// Receives [`CheckEvent`]s. Any `Fn(&CheckEvent) + Send + Sync` closure is an observer.
pub trait CheckObserver: Send + Sync {
    /// Called for every event.
    fn event(&self, event: &CheckEvent);
}

impl<F> CheckObserver for F
where
    F: Fn(&CheckEvent) + Send + Sync,
{
    fn event(&self, event: &CheckEvent) {
        self(event);
    }
}

/// Everything that controls a check.
#[derive(Clone)]
pub struct CheckOptions {
    /// Read and authenticate every object completely ([`CheckMode::Full`]) instead of examining
    /// its structure only ([`CheckMode::Quick`]).
    pub read_data: bool,
    /// Worker threads for the object phase; 0 picks a default (the number of CPUs, at most 4).
    pub jobs: usize,
    /// List at most this many findings; 0 lists all. Errors are listed before warnings, so a
    /// limit never hides an error behind a warning. Counts stay exact.
    pub max_findings: usize,
    /// Receives progress events.
    pub observer: Option<Arc<dyn CheckObserver>>,
    /// Asks the check to stop; it then fails with [`Error::CheckCancelled`](crate::Error).
    pub cancel: CancelToken,
}

impl Default for CheckOptions {
    fn default() -> Self {
        Self {
            read_data: false,
            jobs: 0,
            max_findings: DEFAULT_MAX_FINDINGS,
            observer: None,
            cancel: CancelToken::new(),
        }
    }
}

impl fmt::Debug for CheckOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CheckOptions")
            .field("read_data", &self.read_data)
            .field("jobs", &self.jobs)
            .field("max_findings", &self.max_findings)
            .field("observer", &self.observer.is_some())
            .field("cancel", &self.cancel)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_names_are_unique_stable_and_listed_in_order() {
        let names: Vec<&str> = FindingKind::ALL.iter().map(|kind| kind.name()).collect();
        assert_eq!(
            names,
            [
                "missing_directory",
                "snapshot_damaged",
                "unsupported_version",
                "chunk_missing",
                "chunk_corrupt",
                "chunk_length_mismatch",
                "io_error",
                "orphan_object",
                "unexpected_file",
                "leftover_temp_file",
                "lock_present",
                "key_file_invalid",
            ]
        );
        let mut sorted = FindingKind::ALL;
        sorted.sort();
        assert_eq!(
            sorted,
            FindingKind::ALL,
            "the declaration order is the listing order"
        );
        assert_eq!(FindingKind::ChunkMissing.to_string(), "chunk_missing");
    }

    #[test]
    fn errors_come_before_warnings_in_the_listing_order() {
        let first_warning = FindingKind::ALL
            .iter()
            .position(|kind| kind.severity() == Severity::Warning)
            .expect("there are warnings");
        assert!(FindingKind::ALL[..first_warning]
            .iter()
            .all(|kind| kind.severity() == Severity::Error));
        assert!(FindingKind::ALL[first_warning..]
            .iter()
            .all(|kind| kind.severity() == Severity::Warning));
        assert!(Severity::Error < Severity::Warning);
        assert_eq!(
            (Severity::Error.name(), Severity::Warning.name()),
            ("error", "warning")
        );
    }

    #[test]
    fn every_kind_has_one_line_of_advice_and_the_ones_about_data_loss_point_at_a_copy() {
        for kind in FindingKind::ALL {
            let advice = kind.advice();
            assert!(advice.len() > 40, "{kind}: {advice}");
            assert!(!advice.contains('\n'), "{kind}");
        }
        for kind in [
            FindingKind::MissingDirectory,
            FindingKind::SnapshotDamaged,
            FindingKind::ChunkMissing,
            FindingKind::ChunkCorrupt,
        ] {
            assert!(kind.advice().contains("copy"), "{kind}");
        }
    }

    #[test]
    fn a_report_is_ok_without_errors_and_clean_without_findings() {
        let mut report = CheckReport {
            repository_id: "0123456789abcdef0123456789abcdef".to_owned(),
            format_version: 1,
            mode: CheckMode::Quick,
            stats: CheckStats::default(),
            affected: Vec::new(),
            findings: Vec::new(),
            errors: 0,
            warnings: 0,
            suppressed: 0,
            counts: BTreeMap::new(),
            duration: Duration::ZERO,
        };
        assert!(report.is_ok() && report.is_clean());
        report.warnings = 2;
        report.counts.insert(FindingKind::OrphanObject, 2);
        assert!(
            report.is_ok() && !report.is_clean(),
            "warnings are not errors"
        );
        assert_eq!(
            (
                report.count(FindingKind::OrphanObject),
                report.count(FindingKind::IoError)
            ),
            (2, 0)
        );
        report.errors = 1;
        assert!(!report.is_ok());
    }

    #[test]
    fn the_defaults_are_a_quick_bounded_check() {
        let options = CheckOptions::default();
        assert!(!options.read_data);
        assert_eq!(
            (options.jobs, options.max_findings),
            (0, DEFAULT_MAX_FINDINGS)
        );
        assert!(options.observer.is_none() && !options.cancel.is_cancelled());
        assert!(format!("{options:?}").contains("CheckOptions"));
        assert_eq!(
            (CheckMode::Quick.name(), CheckMode::Full.name()),
            ("quick", "full")
        );
    }

    #[test]
    fn closures_are_observers() {
        let seen = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counter = Arc::clone(&seen);
        let observer: Arc<dyn CheckObserver> = Arc::new(move |event: &CheckEvent| {
            if matches!(event, CheckEvent::Step { .. }) {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });
        observer.event(&CheckEvent::Step {
            phase: CheckPhase::Objects,
        });
        observer.event(&CheckEvent::Started {
            phase: CheckPhase::Objects,
            total: 1,
        });
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
