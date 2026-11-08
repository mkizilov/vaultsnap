//! The backup pipeline: scan on the calling thread, read and store files on a bounded pool of
//! workers, assemble the manifest in scan order, commit.
//!
//! Ordering and boundedness: the coordinating thread walks the scan and hands each regular file
//! that needs reading to a worker through a bounded queue, keeping a short queue of "slots" in
//! scan order. A slot is settled (added to the manifest) strictly in order, so the manifest is the
//! same however the workers are scheduled. At most `4 x jobs` files are in flight, which bounds
//! memory; a worker holds one file's chunker buffers (at most twice the maximum chunk size) at a
//! time.

use std::collections::{HashSet, VecDeque};
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use super::parent::ParentRoot;
use super::{
    BackupEvent, BackupOptions, BackupReport, FileResult, ParentPolicy, SkippedFile, TransferStats,
};
use crate::chunk_store::{ChunkStore, PutOutcome};
use crate::chunking::Chunker;
use crate::error::{Error, Result};
use crate::scan::{
    Entry, EntryKind, ErrorPolicy, Metadata, ScanItem, Scanner, SkipReason, Skipped, Timestamp,
};
use crate::snapshot::{
    os_bytes, BackupConfig, ChunkRef, Manifest, ManifestBuilder, SkippedReason, SnapshotStore,
};

/// How often a file that changes while being read is retried before it is given up on.
const ATTEMPTS: usize = 3;
/// How many recent snapshots are examined when looking for a parent.
const PARENT_SEARCH_LIMIT: usize = 8;
const MAX_JOBS: usize = 64;

/// The repository pieces a backup needs.
pub(crate) struct Services {
    pub chunks: ChunkStore,
    pub snapshots: SnapshotStore,
    pub repository_id: String,
    pub repository_root: PathBuf,
}

#[derive(Default)]
struct Counters {
    files_read: AtomicU64,
    files_unchanged: AtomicU64,
    bytes_read: AtomicU64,
    bytes_unchanged: AtomicU64,
    chunks_new: AtomicU64,
    chunks_existing: AtomicU64,
    bytes_stored: AtomicU64,
}

impl Counters {
    fn snapshot(&self) -> TransferStats {
        TransferStats {
            files_read: self.files_read.load(Ordering::Relaxed),
            files_unchanged: self.files_unchanged.load(Ordering::Relaxed),
            bytes_read: self.bytes_read.load(Ordering::Relaxed),
            bytes_unchanged: self.bytes_unchanged.load(Ordering::Relaxed),
            chunks_new: self.chunks_new.load(Ordering::Relaxed),
            chunks_existing: self.chunks_existing.load(Ordering::Relaxed),
            bytes_stored: self.bytes_stored.load(Ordering::Relaxed),
        }
    }
}

fn add(counter: &AtomicU64, amount: u64) {
    counter.fetch_add(amount, Ordering::Relaxed);
}

/// A unit of work for a worker: read one file and store its chunks.
struct Job {
    source: usize,
    entry: Entry,
    reply: mpsc::Sender<Reply>,
}

/// What a worker reports back.
enum Reply {
    Done(Box<FileDone>),
    /// The file could not be read; the rest of the backup can go on.
    Failed(String),
    Cancelled,
    /// Something is wrong with the repository or the program; the backup cannot go on.
    Fatal(Error),
}

struct FileDone {
    chunks: Vec<ChunkRef>,
    /// The file's attributes at the moment it was opened, which are the ones that describe the
    /// content that was read.
    metadata: Metadata,
}

/// The outcome of one read attempt.
enum Attempt {
    Done(Box<FileDone>, FileResult),
    Changed(String),
    Failed(String),
    Cancelled,
    Fatal(Error),
}

/// A manifest entry waiting for its turn, in scan order. At most a few dozen exist at a time, so
/// the size difference between variants does not matter.
#[allow(clippy::large_enum_variant)]
enum Slot {
    Ready {
        entry: Entry,
        chunks: Vec<ChunkRef>,
    },
    Failed {
        path: PathBuf,
        reason: String,
    },
    Waiting {
        entry: Entry,
        receiver: Receiver<Reply>,
        received: Option<Reply>,
    },
}

impl Slot {
    fn is_ready(&mut self) -> bool {
        let Slot::Waiting {
            receiver, received, ..
        } = self
        else {
            return true;
        };
        if received.is_none() {
            match receiver.try_recv() {
                Ok(reply) => *received = Some(reply),
                Err(TryRecvError::Empty) => return false,
                Err(TryRecvError::Disconnected) => *received = Some(lost_worker()),
            }
        }
        true
    }
}

fn lost_worker() -> Reply {
    Reply::Fatal(Error::Task("a backup worker stopped unexpectedly".to_owned()))
}

struct Context<'a> {
    services: &'a Services,
    options: &'a BackupOptions,
    scanners: &'a [Scanner],
    parents: &'a [Option<ParentRoot>],
    counters: Counters,
    /// Set when the coordinator fails, so workers stop promptly.
    abort: AtomicBool,
    /// Maximum number of files in flight.
    limit: usize,
}

/// Runs a backup and commits the snapshot. Returns only after every worker has stopped.
pub(crate) fn run(services: &Services, options: &BackupOptions) -> Result<BackupReport> {
    let wall_clock = Instant::now();
    // The snapshot's recorded creation time is the *start* of the backup: a file changed after
    // this moment is never trusted as unchanged by a later backup.
    let started = Timestamp::now();

    if options.sources.is_empty() {
        return Err(Error::InvalidArgument("at least one source path is required"));
    }
    if options.cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let jobs = effective_jobs(options.jobs);
    let scanners = build_scanners(services, options)?;
    let parents = select_parents(services, options, &scanners)?;
    let context = Context {
        services,
        options,
        scanners: &scanners,
        parents: &parents,
        counters: Counters::default(),
        abort: AtomicBool::new(false),
        limit: jobs * 4,
    };

    let (job_tx, job_rx) = mpsc::sync_channel::<Job>(jobs * 2);
    let job_rx = Mutex::new(job_rx);
    let built = std::thread::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(|| context.worker(&job_rx));
        }
        let built = context.build_manifest(&job_tx, started);
        if built.is_err() {
            context.abort.store(true, Ordering::SeqCst);
        }
        // Closing the queue lets the workers finish and exit.
        drop(job_tx);
        built
    });
    let manifest = built?;

    // The last chance to stop: once the snapshot is published it is complete and valid.
    context.check_stopped()?;
    context.emit(&BackupEvent::Committing);
    services.snapshots.commit(&manifest)?;

    let skipped = manifest.skipped.iter().map(describe_skipped).collect();
    Ok(BackupReport {
        snapshot: manifest.id.clone(),
        started,
        sources: scanners.iter().map(|s| s.root().to_path_buf()).collect(),
        parents: parents
            .iter()
            .map(|parent| parent.as_ref().map(|p| p.snapshot_id().clone()))
            .collect(),
        totals: manifest.stats.clone(),
        transfer: context.counters.snapshot(),
        skipped,
        duration: wall_clock.elapsed(),
    })
}

fn effective_jobs(requested: usize) -> usize {
    let jobs = if requested == 0 {
        std::thread::available_parallelism().map_or(2, std::num::NonZeroUsize::get).min(4)
    } else {
        requested
    };
    jobs.clamp(1, MAX_JOBS)
}

fn build_scanners(services: &Services, options: &BackupOptions) -> Result<Vec<Scanner>> {
    let repository = fs::canonicalize(&services.repository_root)
        .unwrap_or_else(|_| services.repository_root.clone());
    // The repository must never become part of its own backup.
    let mut scan = options.scan.clone();
    scan.exclude_paths.push(repository.clone());

    let mut scanners: Vec<Scanner> = Vec::new();
    for source in &options.sources {
        let scanner = Scanner::new(source, scan.clone())?;
        if scanner.root().starts_with(&repository) {
            return Err(Error::InvalidArgument("a source cannot be inside the repository"));
        }
        if scanners.iter().any(|other| other.root() == scanner.root()) {
            return Err(Error::InvalidArgument("the same source was given twice"));
        }
        scanners.push(scanner);
    }
    Ok(scanners)
}

/// For each source, the root of an earlier snapshot to compare against, if there is one.
fn select_parents(
    services: &Services,
    options: &BackupOptions,
    scanners: &[Scanner],
) -> Result<Vec<Option<ParentRoot>>> {
    let candidates: Vec<Arc<Manifest>> = match &options.parent {
        ParentPolicy::Disabled => Vec::new(),
        ParentPolicy::Snapshot(id) => vec![Arc::new(services.snapshots.load(id)?)],
        ParentPolicy::Latest => {
            let mut loaded = Vec::new();
            for id in services
                .snapshots
                .ids_newest_first()?
                .into_iter()
                .take(PARENT_SEARCH_LIMIT)
            {
                match services.snapshots.load(&id) {
                    Ok(manifest) => loaded.push(Arc::new(manifest)),
                    // A damaged snapshot is not a reason to refuse a new backup; it is merely
                    // not useful as a parent. `check` is where it gets reported.
                    Err(err) => {
                        tracing::warn!(snapshot = %id, error = %err, "ignoring an unreadable snapshot as parent");
                    }
                }
            }
            loaded
        }
    };
    Ok(scanners
        .iter()
        .map(|scanner| {
            let source = os_bytes(scanner.root().as_os_str());
            candidates.iter().find_map(|manifest| {
                let root = manifest.roots.iter().position(|root| root.source == source)?;
                Some(ParentRoot::new(Arc::clone(manifest), root))
            })
        })
        .collect())
}

fn describe_skip(reason: &SkipReason) -> String {
    match reason {
        SkipReason::Vanished => "disappeared during the backup".to_owned(),
        SkipReason::Unreadable {
            operation, message, ..
        } => format!("{operation}: {message}"),
        SkipReason::OtherFilesystem => "on another filesystem, not entered".to_owned(),
        SkipReason::SpecialFile(kind) => format!("special file ({kind:?}) skipped by policy"),
    }
}

fn describe_skipped(record: &crate::snapshot::SkippedRecord) -> SkippedFile {
    let (reason, failure) = match &record.reason {
        SkippedReason::Vanished => ("disappeared during the backup".to_owned(), false),
        SkippedReason::Unreadable { detail } => (detail.clone(), true),
        SkippedReason::OtherFilesystem => ("on another filesystem, not entered".to_owned(), false),
        SkippedReason::SpecialFile { kind } => {
            (format!("special file ({kind}) skipped by policy"), false)
        }
    };
    SkippedFile {
        source: usize::try_from(record.root).unwrap_or(usize::MAX),
        path: String::from_utf8_lossy(&record.path).into_owned(),
        reason,
        failure,
    }
}

/// Whether a read error means "the file changed under us" (worth a retry).
fn is_change(err: &io::Error) -> bool {
    err.get_ref()
        .and_then(|inner| inner.downcast_ref::<Error>())
        .is_some_and(|inner| matches!(inner, Error::FileChanged { .. }))
}

impl Context<'_> {
    fn emit(&self, event: &BackupEvent<'_>) {
        if let Some(observer) = &self.options.observer {
            observer.event(event);
        }
    }

    fn stopped(&self) -> bool {
        self.options.cancel.is_cancelled() || self.abort.load(Ordering::SeqCst)
    }

    fn check_stopped(&self) -> Result<()> {
        if self.options.cancel.is_cancelled() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }

    // ----- coordinating thread -------------------------------------------------------------

    fn build_manifest(&self, job_tx: &SyncSender<Job>, started: Timestamp) -> Result<Manifest> {
        let config = BackupConfig::new(self.options.chunker, &self.options.scan);
        let mut builder = ManifestBuilder::new(self.services.repository_id.clone(), config)
            .host(self.options.host.clone())
            .tags(self.options.tags.clone());

        for (source, scanner) in self.scanners.iter().enumerate() {
            let root = builder.add_root(scanner.root());
            self.emit(&BackupEvent::SourceStarted {
                source,
                path: scanner.root(),
                parent: self.parents[source].as_ref().map(ParentRoot::snapshot_id),
            });

            let mut pending: VecDeque<Slot> = VecDeque::new();
            let mut failed: HashSet<PathBuf> = HashSet::new();
            for item in scanner.scan() {
                self.check_stopped()?;
                match item? {
                    ScanItem::Skipped(skipped) => {
                        let reason = describe_skip(&skipped.reason);
                        self.emit(&BackupEvent::Skipped {
                            source,
                            path: &skipped.path,
                            reason: &reason,
                        });
                        builder.add_skipped(root, &skipped)?;
                    }
                    ScanItem::Entry(entry) => {
                        let slot = self.dispatch(source, entry, job_tx, &failed)?;
                        pending.push_back(slot);
                    }
                }
                self.drain(&mut builder, root, &mut pending, &mut failed, false)?;
            }
            self.drain(&mut builder, root, &mut pending, &mut failed, true)?;
        }
        self.check_stopped()?;
        builder.finish(started)
    }

    /// Turns a scanned entry into a slot: immediately ready if it needs no reading, otherwise
    /// queued for a worker.
    fn dispatch(
        &self,
        source: usize,
        entry: Entry,
        job_tx: &SyncSender<Job>,
        failed: &HashSet<PathBuf>,
    ) -> Result<Slot> {
        if entry.kind == EntryKind::File {
            if let Some(chunks) = self.reuse(source, &entry)? {
                let size = entry.metadata.size;
                add(&self.counters.files_unchanged, 1);
                add(&self.counters.bytes_unchanged, size);
                self.emit(&BackupEvent::FileFinished {
                    source,
                    path: &entry.path,
                    result: FileResult {
                        bytes: size,
                        unchanged: true,
                        chunks_new: 0,
                        chunks_existing: u32::try_from(chunks.len()).unwrap_or(u32::MAX),
                        bytes_stored: 0,
                    },
                });
                return Ok(Slot::Ready { entry, chunks });
            }
            let (reply, receiver) = mpsc::channel();
            let job = Job {
                source,
                entry: entry.clone(),
                reply,
            };
            job_tx
                .send(job)
                .map_err(|_| Error::Task("the backup workers stopped unexpectedly".to_owned()))?;
            return Ok(Slot::Waiting {
                entry,
                receiver,
                received: None,
            });
        }
        if let EntryKind::HardLink { target } = &entry.kind {
            if failed.contains(target) {
                return Ok(Slot::Failed {
                    path: entry.path.clone(),
                    reason: "the file it is a hard link to could not be backed up".to_owned(),
                });
            }
        }
        Ok(Slot::Ready {
            entry,
            chunks: Vec::new(),
        })
    }

    /// The chunk list of `entry` from the parent snapshot, if the file is unchanged and every
    /// one of those chunks is still in the repository.
    fn reuse(&self, source: usize, entry: &Entry) -> Result<Option<Vec<ChunkRef>>> {
        let Some(parent) = self.parents[source].as_ref() else {
            return Ok(None);
        };
        let Some(chunks) = parent.reusable(&entry.path, &entry.metadata, self.options.racy_margin)
        else {
            return Ok(None);
        };
        for chunk in chunks {
            if !self.services.chunks.contains(&chunk.id)? {
                return Ok(None);
            }
        }
        Ok(Some(chunks.to_vec()))
    }

    /// Settles slots from the front, in order: all of them when `all`, otherwise those that are
    /// ready, plus as many more (waiting for them) as needed to stay within the in-flight limit.
    fn drain(
        &self,
        builder: &mut ManifestBuilder,
        root: usize,
        pending: &mut VecDeque<Slot>,
        failed: &mut HashSet<PathBuf>,
        all: bool,
    ) -> Result<()> {
        loop {
            let ready = match pending.front_mut() {
                Some(front) => front.is_ready(),
                None => return Ok(()),
            };
            if !all && !ready && pending.len() < self.limit {
                return Ok(());
            }
            if let Some(slot) = pending.pop_front() {
                self.settle(builder, root, slot, failed)?;
            }
        }
    }

    fn settle(
        &self,
        builder: &mut ManifestBuilder,
        root: usize,
        slot: Slot,
        failed: &mut HashSet<PathBuf>,
    ) -> Result<()> {
        match slot {
            Slot::Ready { entry, chunks } => builder.add_entry(root, &entry, chunks),
            Slot::Failed { path, reason } => self.record_failure(builder, root, path, &reason, failed),
            Slot::Waiting {
                entry,
                receiver,
                received,
            } => {
                let reply = match received {
                    Some(reply) => reply,
                    None => receiver.recv().unwrap_or_else(|_| lost_worker()),
                };
                match reply {
                    Reply::Done(done) => {
                        let mut entry = entry;
                        entry.metadata = done.metadata;
                        builder.add_entry(root, &entry, done.chunks)
                    }
                    Reply::Failed(reason) => {
                        self.record_failure(builder, root, entry.path, &reason, failed)
                    }
                    Reply::Cancelled => Err(Error::Cancelled),
                    Reply::Fatal(err) => Err(err),
                }
            }
        }
    }

    /// A file could not be backed up: stop, or record it and carry on, per the error policy.
    fn record_failure(
        &self,
        builder: &mut ManifestBuilder,
        root: usize,
        path: PathBuf,
        reason: &str,
        failed: &mut HashSet<PathBuf>,
    ) -> Result<()> {
        if self.options.scan.on_error == ErrorPolicy::Fail {
            return Err(Error::SourceUnreadable {
                path,
                reason: reason.to_owned(),
            });
        }
        self.emit(&BackupEvent::Skipped {
            source: root,
            path: &path,
            reason,
        });
        let skipped = Skipped {
            path: path.clone(),
            reason: SkipReason::Unreadable {
                operation: "read file",
                kind: io::ErrorKind::Other,
                message: reason.to_owned(),
            },
        };
        builder.add_skipped(root, &skipped)?;
        failed.insert(path);
        Ok(())
    }

    // ----- worker threads ------------------------------------------------------------------

    fn worker(&self, jobs: &Mutex<Receiver<Job>>) {
        loop {
            let job = {
                let queue = jobs.lock().unwrap_or_else(PoisonError::into_inner);
                queue.recv()
            };
            let Ok(job) = job else {
                return;
            };
            let reply = self.process_file(job.source, &job.entry);
            // The coordinator may already have given up on this file.
            let _ = job.reply.send(reply);
        }
    }

    fn process_file(&self, source: usize, entry: &Entry) -> Reply {
        let scanner = &self.scanners[source];
        self.emit(&BackupEvent::FileStarted {
            source,
            path: &entry.path,
            size: entry.metadata.size,
        });
        let mut last_reason = String::new();
        for _ in 0..ATTEMPTS {
            if self.stopped() {
                return Reply::Cancelled;
            }
            match self.read_once(scanner, entry) {
                Attempt::Done(done, result) => {
                    add(&self.counters.files_read, 1);
                    self.emit(&BackupEvent::FileFinished {
                        source,
                        path: &entry.path,
                        result,
                    });
                    return Reply::Done(done);
                }
                Attempt::Changed(reason) => last_reason = reason,
                Attempt::Failed(reason) => return Reply::Failed(reason),
                Attempt::Cancelled => return Reply::Cancelled,
                Attempt::Fatal(err) => return Reply::Fatal(err),
            }
        }
        Reply::Failed(format!(
            "it kept changing while it was being read ({last_reason}); gave up after {ATTEMPTS} attempts"
        ))
    }

    fn read_once(&self, scanner: &Scanner, entry: &Entry) -> Attempt {
        let reader = match scanner.open(entry) {
            Ok(reader) => reader,
            Err(Error::FileChanged { reason, .. }) => return Attempt::Changed(reason),
            Err(err) => return Attempt::Failed(err.to_string()),
        };
        let metadata = reader.metadata().clone();
        let mut chunker = Chunker::new(reader, self.options.chunker);
        let mut chunks = Vec::new();
        let mut result = FileResult {
            bytes: 0,
            unchanged: false,
            chunks_new: 0,
            chunks_existing: 0,
            bytes_stored: 0,
        };
        loop {
            if self.stopped() {
                return Attempt::Cancelled;
            }
            let chunk = match chunker.next_chunk() {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(err) if is_change(&err) => return Attempt::Changed(err.to_string()),
                Err(err) => return Attempt::Failed(err.to_string()),
            };
            let length = chunk.data.len();
            let Ok(len) = u32::try_from(length) else {
                return Attempt::Fatal(Error::InvalidArgument("a chunk is larger than the format allows"));
            };
            match self.services.chunks.put(chunk.data) {
                Ok(put) => {
                    match put.outcome {
                        PutOutcome::Stored { object_len, .. } => {
                            result.chunks_new += 1;
                            result.bytes_stored += object_len;
                            add(&self.counters.chunks_new, 1);
                            add(&self.counters.bytes_stored, object_len);
                        }
                        PutOutcome::Duplicate => {
                            result.chunks_existing += 1;
                            add(&self.counters.chunks_existing, 1);
                        }
                    }
                    chunks.push(ChunkRef { id: put.id, len });
                }
                Err(err) => return Attempt::Fatal(err),
            }
            result.bytes += u64::from(len);
            add(&self.counters.bytes_read, u64::from(len));
            self.emit(&BackupEvent::BytesRead {
                bytes: u64::from(len),
            });
        }
        Attempt::Done(Box::new(FileDone { chunks, metadata }), result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_worker_count_is_automatic_or_clamped() {
        assert!((1..=4).contains(&effective_jobs(0)));
        assert_eq!(effective_jobs(1), 1);
        assert_eq!(effective_jobs(3), 3);
        assert_eq!(effective_jobs(10_000), MAX_JOBS);
    }

    #[test]
    fn skip_reasons_are_described_for_people() {
        let unreadable = SkipReason::Unreadable {
            operation: "read directory",
            kind: io::ErrorKind::PermissionDenied,
            message: "Permission denied".to_owned(),
        };
        assert_eq!(describe_skip(&unreadable), "read directory: Permission denied");
        assert!(describe_skip(&SkipReason::Vanished).contains("disappeared"));
        assert!(describe_skip(&SkipReason::SpecialFile(EntryKind::Fifo)).contains("Fifo"));
    }

    #[test]
    fn only_unreadable_paths_count_as_failures() {
        use crate::snapshot::SkippedRecord;

        let record = |reason| SkippedRecord {
            root: 1,
            path: b"a/b".to_vec(),
            reason,
        };
        let failure = describe_skipped(&record(SkippedReason::Unreadable {
            detail: "denied".to_owned(),
        }));
        assert!(failure.failure && failure.reason == "denied");
        assert_eq!((failure.source, failure.path.as_str()), (1, "a/b"));
        for benign in [
            SkippedReason::Vanished,
            SkippedReason::OtherFilesystem,
            SkippedReason::SpecialFile {
                kind: "Socket".to_owned(),
            },
        ] {
            assert!(!describe_skipped(&record(benign)).failure);
        }
    }

    #[test]
    fn a_read_error_is_recognized_as_a_change_only_if_it_carries_that_error() {
        let changed = io::Error::other(Error::FileChanged {
            path: PathBuf::from("x"),
            reason: "size".to_owned(),
        });
        assert!(is_change(&changed));
        assert!(!is_change(&io::Error::other("something else")));
        assert!(!is_change(&io::Error::from(io::ErrorKind::NotFound)));
    }

    #[test]
    fn slots_without_workers_are_ready() {
        let mut slot = Slot::Failed {
            path: PathBuf::from("x"),
            reason: "r".to_owned(),
        };
        assert!(slot.is_ready());
    }

    #[test]
    fn waiting_slots_become_ready_when_a_reply_arrives_or_the_worker_vanishes() {
        let entry = Entry {
            path: PathBuf::from("f"),
            kind: EntryKind::File,
            metadata: crate::scan::Metadata {
                size: 0,
                modified: None,
                accessed: None,
                changed: None,
                created: None,
                mode: None,
                uid: None,
                gid: None,
                readonly: false,
                device: None,
                inode: None,
                links: None,
            },
            followed_symlink: false,
        };
        let (reply, receiver) = mpsc::channel();
        let mut slot = Slot::Waiting {
            entry: entry.clone(),
            receiver,
            received: None,
        };
        assert!(!slot.is_ready(), "nothing sent yet");
        reply.send(Reply::Cancelled).expect("send");
        assert!(slot.is_ready());
        assert!(matches!(slot, Slot::Waiting { received: Some(Reply::Cancelled), .. }));

        let (reply, receiver) = mpsc::channel::<Reply>();
        let mut orphan = Slot::Waiting {
            entry,
            receiver,
            received: None,
        };
        drop(reply);
        assert!(orphan.is_ready(), "a disconnected worker settles the slot as a failure");
        assert!(matches!(orphan, Slot::Waiting { received: Some(Reply::Fatal(_)), .. }));
    }
}
