//! The backup engine: scan, chunk, store, and commit a snapshot.
//!
//! [`Repository::backup`](crate::Repository::backup) ties the scanner, the chunker, the encrypted
//! chunk store, and the snapshot store together:
//!
//! 1. Every source is scanned (deterministically, never following symbolic links by default; the
//!    repository itself is never backed up).
//! 2. Regular files are read as streams, cut into content-defined chunks, and stored encrypted;
//!    a chunk that already exists is reused, so identical data is stored once.
//! 3. A file whose metadata is identical to the same file in a previous snapshot **and** whose
//!    chunks are all still in the repository is not read at all: the previous chunk list is
//!    reused. See [`BackupOptions::racy_margin`] for when this is considered safe.
//! 4. The manifest is built, validated, and committed atomically. A snapshot becomes visible only
//!    if every step succeeded and nobody cancelled; otherwise nothing is visible, and the chunks
//!    stored so far are simply reused by the next run.
//!
//! File processing uses a bounded pool of worker threads ([`BackupOptions::jobs`]); results are
//! assembled in scan order, so the manifest does not depend on timing. Work can be cancelled at
//! any time with a [`CancelToken`]. Progress is reported through a [`BackupObserver`]. Failures
//! of individual files are reported, and the snapshot records them, so a partial backup is never
//! mistaken for a complete one. Details and limits are in `docs/BACKUP.md`.

mod engine;
mod parent;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::chunking::ChunkerConfig;
use crate::scan::{ScanOptions, Timestamp};
use crate::snapshot::{ManifestStats, SnapshotId};

pub(crate) use engine::{effective_jobs, run, Services};

/// Which earlier snapshot an incremental backup compares against.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ParentPolicy {
    /// Use the most recent snapshots that contain each source (the default).
    #[default]
    Latest,
    /// Read every file: no comparison with earlier snapshots.
    Disabled,
    /// Compare against this snapshot.
    Snapshot(SnapshotId),
}

/// A shared flag for asking a running backup to stop.
///
/// Cancelling is safe at any moment: workers stop between chunks, no snapshot is committed, the
/// repository lock is released, and the chunks already stored stay valid and are reused by the
/// next backup. The backup returns [`Error::Cancelled`](crate::Error::Cancelled).
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// A new token that is not cancelled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation. Idempotent; may be called from any thread or a signal handler task.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether cancellation was requested.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// What happened to one file; part of [`BackupEvent::FileFinished`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileResult {
    /// Size of the file's content in bytes.
    pub bytes: u64,
    /// True if the file was unchanged since the parent snapshot and was not read.
    pub unchanged: bool,
    /// Chunks of this file that were new to the repository.
    pub chunks_new: u32,
    /// Chunks of this file that were already stored.
    pub chunks_existing: u32,
    /// Bytes of new chunk objects written for this file.
    pub bytes_stored: u64,
}

/// Progress notifications. Paths are relative to the source with the given index.
///
/// Events are delivered from worker threads and the coordinating thread, possibly concurrently,
/// so observers must be quick and thread safe.
#[derive(Debug, Clone, Copy)]
pub enum BackupEvent<'a> {
    /// A source is about to be scanned, with the parent snapshot chosen for it, if any.
    SourceStarted {
        /// Index of the source.
        source: usize,
        /// The resolved path being backed up.
        path: &'a Path,
        /// The earlier snapshot it is compared against.
        parent: Option<&'a SnapshotId>,
    },
    /// A worker began reading a file.
    FileStarted {
        /// Index of the source.
        source: usize,
        /// Path relative to the source.
        path: &'a Path,
        /// Size according to the scan.
        size: u64,
    },
    /// A chunk of a file was read and stored; `bytes` is its length.
    BytesRead {
        /// Length of the chunk.
        bytes: u64,
    },
    /// A file is complete (read, or reused from the parent).
    FileFinished {
        /// Index of the source.
        source: usize,
        /// Path relative to the source.
        path: &'a Path,
        /// What was done.
        result: FileResult,
    },
    /// A path could not be backed up and was left out of the snapshot.
    Skipped {
        /// Index of the source.
        source: usize,
        /// Path relative to the source.
        path: &'a Path,
        /// Why.
        reason: &'a str,
    },
    /// Everything is stored; the snapshot is being committed.
    Committing,
}

/// Receives [`BackupEvent`]s. Any `Fn(&BackupEvent<'_>) + Send + Sync` closure is an observer.
pub trait BackupObserver: Send + Sync {
    /// Called for every event.
    fn event(&self, event: &BackupEvent<'_>);
}

impl<F> BackupObserver for F
where
    F: Fn(&BackupEvent<'_>) + Send + Sync,
{
    fn event(&self, event: &BackupEvent<'_>) {
        self(event);
    }
}

/// Everything that controls a backup.
#[derive(Clone)]
pub struct BackupOptions {
    /// Files and directories to back up. At least one; duplicates and anything inside the
    /// repository are refused.
    pub sources: Vec<PathBuf>,
    /// Scan behavior: patterns and the symbolic link, hard link, error, special file, change,
    /// and filesystem policies. Defaults are conservative; see `docs/SCANNING.md`.
    pub scan: ScanOptions,
    /// Chunk size limits. Chunks cut with other limits do not deduplicate against these.
    pub chunker: ChunkerConfig,
    /// Labels stored with the snapshot.
    pub tags: Vec<String>,
    /// Host name stored with the snapshot (empty if unknown).
    pub host: String,
    /// Worker threads reading and storing files; `0` chooses automatically (the number of CPUs,
    /// at most 4). Memory in flight is bounded by roughly this many times twice the maximum
    /// chunk size, plus a small queue.
    pub jobs: usize,
    /// The snapshot to compare against.
    pub parent: ParentPolicy,
    /// A file counts as unchanged only if its modification and inode-change times are older than
    /// the start of the parent backup by at least this much. Filesystems store times coarsely
    /// (down to two seconds), so a file modified again within the same tick after being read
    /// would otherwise look unchanged. Two seconds is safe everywhere; zero is only appropriate
    /// on filesystems with nanosecond timestamps and for tests.
    pub racy_margin: Duration,
    /// Receives progress events.
    pub observer: Option<Arc<dyn BackupObserver>>,
    /// Asks the backup to stop.
    pub cancel: CancelToken,
}

impl Default for BackupOptions {
    fn default() -> Self {
        Self {
            sources: Vec::new(),
            scan: ScanOptions::default(),
            chunker: ChunkerConfig::default(),
            tags: Vec::new(),
            host: String::new(),
            jobs: 0,
            parent: ParentPolicy::default(),
            racy_margin: Duration::from_secs(2),
            observer: None,
            cancel: CancelToken::new(),
        }
    }
}

impl fmt::Debug for BackupOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BackupOptions")
            .field("sources", &self.sources)
            .field("scan", &self.scan)
            .field("chunker", &self.chunker)
            .field("tags", &self.tags)
            .field("host", &self.host)
            .field("jobs", &self.jobs)
            .field("parent", &self.parent)
            .field("racy_margin", &self.racy_margin)
            .field("observer", &self.observer.is_some())
            .field("cancel", &self.cancel)
            .finish()
    }
}

/// What was transferred. "Read" means read from the sources; "stored" means written to the
/// repository. Retried reads (a file that changed while being read) count every attempt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransferStats {
    /// Files whose content was read.
    pub files_read: u64,
    /// Files reused from the parent snapshot without being read.
    pub files_unchanged: u64,
    /// Bytes read from the sources.
    pub bytes_read: u64,
    /// Bytes of files reused without being read.
    pub bytes_unchanged: u64,
    /// Chunks that were new to the repository.
    pub chunks_new: u64,
    /// Chunks that were already stored (deduplicated).
    pub chunks_existing: u64,
    /// Bytes of new chunk objects written (compressed and encrypted).
    pub bytes_stored: u64,
}

/// A path that is not in the snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedFile {
    /// Index of the source.
    pub source: usize,
    /// Path relative to the source (lossy if not UTF-8).
    pub path: String,
    /// Why it was left out.
    pub reason: String,
    /// True if the path should have been backed up but could not be (unreadable, kept changing
    /// while being read). False for paths that are absent by policy or by circumstance: a mount
    /// point that was not entered, a special file excluded by policy, a file that disappeared
    /// during the backup.
    pub failure: bool,
}

/// The result of a completed (committed) backup.
#[derive(Debug, Clone)]
pub struct BackupReport {
    /// The id of the snapshot that was committed.
    pub snapshot: SnapshotId,
    /// When the backup started; also the snapshot's recorded creation time.
    pub started: Timestamp,
    /// The resolved source paths, in order.
    pub sources: Vec<PathBuf>,
    /// The parent snapshot used for each source, if any.
    pub parents: Vec<Option<SnapshotId>>,
    /// Totals of what the snapshot contains.
    pub totals: ManifestStats,
    /// What was read and written.
    pub transfer: TransferStats,
    /// Everything that was left out. Non-empty means the snapshot is partial.
    pub skipped: Vec<SkippedFile>,
    /// Wall-clock time taken.
    pub duration: Duration,
}

impl BackupReport {
    /// True if some paths that should have been backed up could not be: the snapshot exists and
    /// is valid, but it does not contain everything that was asked for. Paths left out by policy
    /// or because they disappeared do not make a backup partial.
    pub fn is_partial(&self) -> bool {
        self.skipped.iter().any(|skipped| skipped.failure)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    #[test]
    fn a_cancel_token_is_shared_between_clones() {
        let token = CancelToken::new();
        let other = token.clone();
        assert!(!token.is_cancelled());
        other.cancel();
        assert!(token.is_cancelled());
        other.cancel();
        assert!(token.is_cancelled(), "cancelling twice is harmless");
        assert!(
            !CancelToken::new().is_cancelled(),
            "fresh tokens are independent"
        );
    }

    #[test]
    fn closures_are_observers() {
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&count);
        let observer: Arc<dyn BackupObserver> = Arc::new(move |event: &BackupEvent<'_>| {
            if matches!(event, BackupEvent::Committing) {
                seen.fetch_add(1, Ordering::SeqCst);
            }
        });
        observer.event(&BackupEvent::Committing);
        observer.event(&BackupEvent::BytesRead { bytes: 1 });
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn defaults_are_conservative() {
        let options = BackupOptions::default();
        assert!(options.sources.is_empty());
        assert_eq!(options.parent, ParentPolicy::Latest);
        assert_eq!(options.racy_margin, Duration::from_secs(2));
        assert_eq!(options.jobs, 0);
        assert!(options.observer.is_none());
        assert!(!options.cancel.is_cancelled());
        assert!(format!("{options:?}").contains("BackupOptions"));
    }

    #[test]
    fn partial_reports_say_so() {
        let mut report = BackupReport {
            snapshot: SnapshotId::generate().expect("randomness"),
            started: Timestamp { secs: 0, nanos: 0 },
            sources: Vec::new(),
            parents: Vec::new(),
            totals: ManifestStats::default(),
            transfer: TransferStats::default(),
            skipped: Vec::new(),
            duration: Duration::ZERO,
        };
        assert!(!report.is_partial());
        let skip = |failure| SkippedFile {
            source: 0,
            path: "x".to_owned(),
            reason: "why".to_owned(),
            failure,
        };
        report.skipped.push(skip(false));
        assert!(
            !report.is_partial(),
            "policy skips and vanished files are not failures"
        );
        report.skipped.push(skip(true));
        assert!(report.is_partial());
    }
}
