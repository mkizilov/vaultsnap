//! The check pipeline.
//!
//! 1. **Walk** the repository ([`scan`]): structure, stray files, key files, locks; an inventory of
//!    snapshot files and chunk objects. Snapshot files are listed before objects (see the module
//!    documentation of [`check`](super)).
//! 2. **Read the snapshots**, one at a time, through authenticated decryption. Each is folded into
//!    a map of the chunks it needs and then dropped, so memory is one manifest plus one small
//!    record per distinct chunk, not the size of the history.
//! 3. **Compare**: chunks needed but not stored are missing; objects stored but not needed are
//!    orphans.
//! 4. **Examine every object** with a small pool of threads, in id order: its structure (quick) or
//!    its whole content (full). The results are collected by position, so the report is the same
//!    whatever the threads' timing.
//! 5. If chunks are damaged or missing, read the snapshots once more to say which of them cannot
//!    be restored completely.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use super::scan::{self, Inventory};
use super::{
    AffectedSnapshot, CheckEvent, CheckMode, CheckOptions, CheckPhase, CheckReport, CheckStats,
    Finding, FindingKind, Severity,
};
use crate::backup::effective_jobs;
use crate::chunk_store::{ChunkId, ChunkStore};
use crate::config::RepositoryConfig;
use crate::error::{Error, Result};
use crate::snapshot::{Manifest, NodeKind, SnapshotId, SnapshotStore};
use crate::store::Store;

/// Collects findings: every one is counted, but at most `cap` of each severity are kept, so a
/// repository with a million orphans does not need a million records, and errors are never
/// crowded out by warnings.
pub(super) struct Findings {
    cap: usize,
    errors: Vec<Finding>,
    warnings: Vec<Finding>,
    counts: BTreeMap<FindingKind, u64>,
}

/// What [`Findings::finish`] hands over.
pub(super) struct Finished {
    pub findings: Vec<Finding>,
    pub errors: u64,
    pub warnings: u64,
    pub suppressed: u64,
    pub counts: BTreeMap<FindingKind, u64>,
}

impl Findings {
    /// A collector listing at most `max_findings` findings (0 = all).
    pub(super) fn new(max_findings: usize) -> Self {
        Self {
            cap: if max_findings == 0 {
                usize::MAX
            } else {
                max_findings
            },
            errors: Vec::new(),
            warnings: Vec::new(),
            counts: BTreeMap::new(),
        }
    }

    pub(super) fn add(&mut self, finding: Finding) {
        *self.counts.entry(finding.kind).or_insert(0) += 1;
        let list = match finding.severity() {
            Severity::Error => &mut self.errors,
            Severity::Warning => &mut self.warnings,
        };
        if list.len() < self.cap {
            list.push(finding);
        }
    }

    /// Errors first, each group ordered by kind (and otherwise as found), at most `cap` in all.
    pub(super) fn finish(self) -> Finished {
        let Self {
            cap,
            mut errors,
            mut warnings,
            counts,
        } = self;
        let total: u64 = counts.values().sum();
        let error_total: u64 = counts
            .iter()
            .filter(|(kind, _)| kind.severity() == Severity::Error)
            .map(|(_, count)| *count)
            .sum();
        errors.sort_by_key(|finding| finding.kind);
        warnings.sort_by_key(|finding| finding.kind);
        warnings.truncate(cap.saturating_sub(errors.len()));
        let mut findings = errors;
        findings.extend(warnings);
        let suppressed = total - findings.len() as u64;
        Finished {
            findings,
            errors: error_total,
            warnings: total - error_total,
            suppressed,
            counts,
        }
    }
}

fn notify(options: &CheckOptions, event: CheckEvent) {
    if let Some(observer) = &options.observer {
        observer.event(&event);
    }
}

fn stop_if_cancelled(options: &CheckOptions) -> Result<()> {
    if options.cancel.is_cancelled() {
        Err(Error::CheckCancelled)
    } else {
        Ok(())
    }
}

/// An object to examine.
struct Item {
    id: ChunkId,
    /// The length snapshots record for the chunk; `None` if no readable snapshot refers to it.
    recorded: Option<u32>,
    /// Size of the object file.
    stored: u64,
}

/// What is wrong with an object, as a kind and a description.
type Failure = (FindingKind, String);

fn failure_of(err: &Error) -> Failure {
    match err {
        Error::ChunkNotFound(_) => (
            FindingKind::ChunkMissing,
            "the object disappeared while it was being checked".to_owned(),
        ),
        Error::CorruptObject { reason, .. } => (FindingKind::ChunkCorrupt, reason.clone()),
        Error::UnsupportedObjectVersion { .. } => {
            (FindingKind::UnsupportedVersion, err.to_string())
        }
        other => (FindingKind::IoError, other.to_string()),
    }
}

/// Examines one object: its structure in quick mode, all of it in full mode, and in both whether
/// it holds a chunk of the length the snapshots record.
fn verify_one(chunks: &ChunkStore, item: &Item, full: bool) -> Option<Failure> {
    let length: Result<u32> = if full {
        chunks
            .get(&item.id)
            .map(|data| u32::try_from(data.len()).unwrap_or(u32::MAX))
    } else {
        chunks.peek(&item.id).map(|info| info.plaintext_len)
    };
    match (length, item.recorded) {
        (Ok(found), Some(recorded)) if found != recorded => {
            let detail = format!(
                "the object holds a chunk of {found} bytes but snapshots record {recorded} bytes"
            );
            Some((FindingKind::ChunkLengthMismatch, detail))
        }
        (Ok(_), _) => None,
        (Err(err), _) => Some(failure_of(&err)),
    }
}

/// Examines every item with a pool of threads and returns, for each, what is wrong with it. Items
/// are handed out in order and results are stored by position, so the answer does not depend on
/// scheduling.
fn verify(
    chunks: &ChunkStore,
    items: &[Item],
    full: bool,
    options: &CheckOptions,
) -> Result<Vec<Option<Failure>>> {
    notify(
        options,
        CheckEvent::Started {
            phase: CheckPhase::Objects,
            total: items.len() as u64,
        },
    );
    let jobs = effective_jobs(options.jobs).min(items.len().max(1));
    let next = AtomicUsize::new(0);
    let found: Vec<Vec<(usize, Failure)>> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..jobs)
            .map(|_| {
                scope.spawn(|| {
                    let mut local = Vec::new();
                    while !options.cancel.is_cancelled() {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(item) = items.get(index) else {
                            break;
                        };
                        if let Some(failure) = verify_one(chunks, item, full) {
                            local.push((index, failure));
                        }
                        notify(
                            options,
                            CheckEvent::Step {
                                phase: CheckPhase::Objects,
                            },
                        );
                    }
                    local
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|worker| {
                worker
                    .join()
                    .map_err(|_| Error::Task("a verification thread panicked".to_owned()))
            })
            .collect::<Result<Vec<_>>>()
    })?;
    stop_if_cancelled(options)?;

    let mut aligned: Vec<Option<Failure>> = items.iter().map(|_| None).collect();
    for (index, failure) in found.into_iter().flatten() {
        aligned[index] = Some(failure);
    }
    Ok(aligned)
}

/// Adds the chunks `manifest` needs to `referenced`, with the length it records for each.
fn note_references(manifest: &Manifest, referenced: &mut HashMap<ChunkId, u32>) {
    for entry in manifest.roots.iter().flat_map(|root| root.entries.iter()) {
        if let NodeKind::File { chunks } = &entry.kind {
            for chunk in chunks {
                referenced.entry(chunk.id).or_insert(chunk.len);
            }
        }
    }
}

/// How a snapshot that could not be read is reported.
fn snapshot_finding(id: &SnapshotId, err: &Error) -> Finding {
    let kind = match err {
        Error::UnsupportedSnapshotVersion { .. } | Error::UnsupportedManifestVersion { .. } => {
            FindingKind::UnsupportedVersion
        }
        Error::Io { .. } | Error::Backend { .. } => FindingKind::IoError,
        _ => FindingKind::SnapshotDamaged,
    };
    Finding {
        kind,
        subject: format!("snapshots/{id}"),
        detail: err.to_string(),
    }
}

/// Reads the readable snapshots again and says which need damaged or missing chunks.
fn affected_snapshots(
    snapshots: &SnapshotStore,
    readable: &[SnapshotId],
    damaged: &HashSet<ChunkId>,
    options: &CheckOptions,
) -> Result<Vec<AffectedSnapshot>> {
    let mut affected = Vec::new();
    for id in readable {
        stop_if_cancelled(options)?;
        // It was readable a moment ago; if it no longer is, that is for the next check to say.
        let Ok(manifest) = snapshots.load(id) else {
            continue;
        };
        let mut files = 0_u64;
        let mut distinct: HashSet<ChunkId> = HashSet::new();
        for entry in manifest.roots.iter().flat_map(|root| root.entries.iter()) {
            let NodeKind::File { chunks } = &entry.kind else {
                continue;
            };
            let mut hit = false;
            for chunk in chunks.iter().filter(|chunk| damaged.contains(&chunk.id)) {
                hit = true;
                distinct.insert(chunk.id);
            }
            if hit {
                files += 1;
            }
        }
        if files > 0 {
            affected.push(AffectedSnapshot {
                snapshot: id.clone(),
                damaged_chunks: distinct.len() as u64,
                files,
            });
        }
    }
    Ok(affected)
}

/// The parts of a repository a check works with.
pub(crate) struct Subject<'a> {
    pub store: &'a Store,
    pub snapshots: &'a SnapshotStore,
    pub chunks: &'a ChunkStore,
    pub config: &'a RepositoryConfig,
}

/// Runs the check. `own_lock` is the name of the shared lock the caller holds, if any.
pub(crate) fn run(
    subject: &Subject<'_>,
    options: &CheckOptions,
    own_lock: Option<&str>,
) -> Result<CheckReport> {
    let Subject {
        store,
        snapshots,
        chunks,
        config,
    } = subject;
    let started = Instant::now();
    let full = options.read_data;
    let mut findings = Findings::new(options.max_findings);
    let mut stats = CheckStats::default();

    notify(
        options,
        CheckEvent::Started {
            phase: CheckPhase::Structure,
            total: 0,
        },
    );
    let Inventory {
        snapshot_ids,
        objects,
        key_files,
    } = scan::scan(store, own_lock, &mut findings)?;
    stop_if_cancelled(options)?;
    stats.key_files = key_files;
    stats.objects = objects.len() as u64;
    stats.object_bytes = objects.values().copied().sum();

    // Snapshots: authenticate each one, remember which chunks it needs.
    notify(
        options,
        CheckEvent::Started {
            phase: CheckPhase::Snapshots,
            total: snapshot_ids.len() as u64,
        },
    );
    let mut referenced: HashMap<ChunkId, u32> = HashMap::new();
    let mut readable: Vec<SnapshotId> = Vec::new();
    for id in snapshot_ids {
        stop_if_cancelled(options)?;
        match snapshots.load(&id) {
            Ok(manifest) => {
                note_references(&manifest, &mut referenced);
                readable.push(id);
            }
            // Removed since the walk; nothing to say about a snapshot that is gone.
            Err(Error::SnapshotNotFound(_)) => {}
            Err(err) => {
                stats.snapshots_damaged += 1;
                findings.add(snapshot_finding(&id, &err));
            }
        }
        notify(
            options,
            CheckEvent::Step {
                phase: CheckPhase::Snapshots,
            },
        );
    }
    stats.snapshots = readable.len() as u64;
    stats.chunks_referenced = referenced.len() as u64;
    stats.orphans_exact = stats.snapshots_damaged == 0;

    // Chunks: compare what is needed with what is there, then examine what is there.
    let mut missing: Vec<ChunkId> = referenced
        .keys()
        .filter(|id| !objects.contains_key(*id))
        .copied()
        .collect();
    missing.sort();
    stats.chunks_missing = missing.len() as u64;
    for id in &missing {
        findings.add(Finding {
            kind: FindingKind::ChunkMissing,
            subject: id.object_name(),
            detail: "no object exists for a chunk that snapshots refer to".to_owned(),
        });
    }

    let mut items: Vec<Item> = objects
        .iter()
        .map(|(id, stored)| Item {
            id: *id,
            recorded: referenced.get(id).copied(),
            stored: *stored,
        })
        .collect();
    items.sort_by_key(|item| item.id);
    let failures = verify(chunks, &items, full, options)?;

    let mut damaged: HashSet<ChunkId> = missing.iter().copied().collect();
    for (item, failure) in items.iter().zip(failures) {
        let orphan = item.recorded.is_none();
        stats.objects_checked += 1;
        if orphan {
            stats.orphans += 1;
            stats.orphan_bytes += item.stored;
        }
        match failure {
            Some((kind, mut detail)) => {
                stats.objects_damaged += 1;
                if orphan {
                    detail.push_str(
                        " (no snapshot refers to this object, but a later backup would reuse it \
                         as it is)",
                    );
                } else {
                    damaged.insert(item.id);
                }
                findings.add(Finding {
                    kind,
                    subject: item.id.object_name(),
                    detail,
                });
            }
            None => {
                if full {
                    stats.bytes_authenticated += item.stored;
                }
                if orphan {
                    let detail = if stats.orphans_exact {
                        "no snapshot refers to this object".to_owned()
                    } else {
                        "no readable snapshot refers to this object; it may belong to a snapshot \
                         that could not be read"
                            .to_owned()
                    };
                    findings.add(Finding {
                        kind: FindingKind::OrphanObject,
                        subject: item.id.object_name(),
                        detail,
                    });
                }
            }
        }
    }

    let affected = if damaged.is_empty() {
        Vec::new()
    } else {
        affected_snapshots(snapshots, &readable, &damaged, options)?
    };
    let finished = findings.finish();
    Ok(CheckReport {
        repository_id: config.repository_id.clone(),
        format_version: config.format_version,
        mode: if full {
            CheckMode::Full
        } else {
            CheckMode::Quick
        },
        stats,
        affected,
        findings: finished.findings,
        errors: finished.errors,
        warnings: finished.warnings,
        suppressed: finished.suppressed,
        counts: finished.counts,
        duration: started.elapsed(),
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;

    use tempfile::TempDir;

    use super::*;
    use crate::chunking::test_util::pseudo_random;
    use crate::keys::RepositoryKeys;

    struct Fx {
        dir: TempDir,
        chunks: ChunkStore,
    }

    impl Fx {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("create temporary directory");
            Store::local(dir.path()).create_dir("data").expect("mkdir");
            let keys = RepositoryKeys::generate().expect("generate");
            let chunks = ChunkStore::new(Store::local(dir.path()), &keys).expect("chunk store");
            Self { dir, chunks }
        }

        fn item(&self, data: &[u8], recorded: Option<u32>) -> Item {
            let put = self.chunks.put(data).expect("put");
            let stored = self.chunks.stored_len(&put.id).expect("length");
            Item {
                id: put.id,
                recorded,
                stored,
            }
        }

        fn path_of(&self, item: &Item) -> PathBuf {
            self.dir.path().join(item.id.object_name())
        }
    }

    fn finding(kind: FindingKind, subject: &str) -> Finding {
        Finding {
            kind,
            subject: subject.to_owned(),
            detail: String::new(),
        }
    }

    #[test]
    fn a_limit_keeps_errors_before_warnings_and_the_counts_exact() {
        let mut findings = Findings::new(3);
        for index in 0..5 {
            findings.add(finding(
                FindingKind::OrphanObject,
                &format!("orphan-{index}"),
            ));
        }
        findings.add(finding(FindingKind::ChunkMissing, "gone-1"));
        findings.add(finding(FindingKind::ChunkCorrupt, "bad"));
        findings.add(finding(FindingKind::ChunkMissing, "gone-2"));
        let finished = findings.finish();

        let listed: Vec<(FindingKind, &str)> = finished
            .findings
            .iter()
            .map(|f| (f.kind, f.subject.as_str()))
            .collect();
        assert_eq!(
            listed,
            [
                (FindingKind::ChunkMissing, "gone-1"),
                (FindingKind::ChunkMissing, "gone-2"),
                (FindingKind::ChunkCorrupt, "bad"),
            ],
            "the errors fill the limit, ordered by kind and then as found"
        );
        assert_eq!(
            (finished.errors, finished.warnings, finished.suppressed),
            (3, 5, 5)
        );
        assert_eq!(finished.counts[&FindingKind::OrphanObject], 5);
        assert_eq!(finished.counts[&FindingKind::ChunkMissing], 2);
    }

    #[test]
    fn warnings_fill_what_the_errors_leave_free_and_zero_means_no_limit() {
        let mut findings = Findings::new(4);
        findings.add(finding(FindingKind::IoError, "e"));
        for index in 0..6 {
            findings.add(finding(FindingKind::UnexpectedFile, &format!("w{index}")));
        }
        let finished = findings.finish();
        assert_eq!(finished.findings.len(), 4);
        assert_eq!(finished.findings[0].kind, FindingKind::IoError);
        assert_eq!(
            (finished.errors, finished.warnings, finished.suppressed),
            (1, 6, 3)
        );

        let mut unlimited = Findings::new(0);
        for index in 0..2500 {
            unlimited.add(finding(FindingKind::OrphanObject, &format!("o{index}")));
        }
        let finished = unlimited.finish();
        assert_eq!((finished.findings.len(), finished.suppressed), (2500, 0));
    }

    #[test]
    fn errors_are_classified_by_what_they_say() {
        let cases = [
            (
                Error::ChunkNotFound("x".to_owned()),
                FindingKind::ChunkMissing,
            ),
            (
                Error::CorruptObject {
                    id: "x".to_owned(),
                    reason: "truncated".to_owned(),
                },
                FindingKind::ChunkCorrupt,
            ),
            (
                Error::UnsupportedObjectVersion {
                    found: 9,
                    supported: 1,
                },
                FindingKind::UnsupportedVersion,
            ),
            (Error::InvalidName("x".to_owned()), FindingKind::IoError),
        ];
        for (error, expected) in cases {
            assert_eq!(failure_of(&error).0, expected, "{error}");
        }
        let (_, detail) = failure_of(&Error::CorruptObject {
            id: "x".to_owned(),
            reason: "truncated".to_owned(),
        });
        assert_eq!(detail, "truncated");
    }

    #[test]
    fn an_intact_object_passes_in_both_modes_and_a_length_disagreement_is_caught_in_both() {
        let fx = Fx::new();
        let data = pseudo_random(5000, 1);
        let fine = fx.item(&data, Some(5000));
        let orphan = fx.item(&pseudo_random(3000, 2), None);
        let wrong = fx.item(&pseudo_random(4000, 3), Some(4001));
        for full in [false, true] {
            assert_eq!(verify_one(&fx.chunks, &fine, full), None, "full={full}");
            assert_eq!(verify_one(&fx.chunks, &orphan, full), None, "full={full}");
            let (kind, detail) = verify_one(&fx.chunks, &wrong, full).expect("a failure");
            assert_eq!(kind, FindingKind::ChunkLengthMismatch, "full={full}");
            assert_eq!(
                detail,
                "the object holds a chunk of 4000 bytes but snapshots record 4001 bytes"
            );
        }
    }

    #[test]
    fn damage_is_classified_and_only_the_full_read_sees_a_flipped_payload_bit() {
        let fx = Fx::new();
        let flipped = fx.item(&pseudo_random(5000, 4), Some(5000));
        let cut = fx.item(&pseudo_random(5000, 5), Some(5000));
        let gone = fx.item(&pseudo_random(5000, 6), Some(5000));

        let path = fx.path_of(&flipped);
        let mut bytes = fs::read(&path).expect("read");
        let middle = bytes.len() / 2;
        bytes[middle] ^= 1;
        fs::write(&path, bytes).expect("write");
        let path = fx.path_of(&cut);
        let bytes = fs::read(&path).expect("read");
        fs::write(&path, &bytes[..bytes.len() - 3]).expect("write");
        fs::remove_file(fx.path_of(&gone)).expect("remove");

        assert_eq!(
            verify_one(&fx.chunks, &flipped, false),
            None,
            "quick mode cannot see this"
        );
        let (kind, detail) = verify_one(&fx.chunks, &flipped, true).expect("full mode does");
        assert_eq!(kind, FindingKind::ChunkCorrupt);
        assert!(detail.contains("authentication failed"), "{detail}");
        for full in [false, true] {
            let (kind, detail) = verify_one(&fx.chunks, &cut, full).expect("a failure");
            assert_eq!(kind, FindingKind::ChunkCorrupt, "full={full}");
            assert!(detail.contains("truncated or extended"), "{detail}");
            let (kind, _) = verify_one(&fx.chunks, &gone, full).expect("a failure");
            assert_eq!(kind, FindingKind::ChunkMissing, "full={full}");
        }
    }

    #[test]
    fn the_result_does_not_depend_on_the_number_of_threads() {
        let fx = Fx::new();
        let mut items = Vec::new();
        for seed in 0..40_u64 {
            items.push(fx.item(
                &pseudo_random(2000 + seed as usize, seed + 100),
                Some(2000 + seed as u32),
            ));
        }
        items.sort_by_key(|item| item.id);
        for index in [3, 10, 11, 25, 39] {
            let path = fx.path_of(&items[index]);
            let mut bytes = fs::read(&path).expect("read");
            bytes[20] ^= 0x40;
            fs::write(&path, bytes).expect("write");
        }

        let outcome = |jobs: usize, full: bool| {
            let options = CheckOptions {
                jobs,
                ..CheckOptions::default()
            };
            verify(&fx.chunks, &items, full, &options).expect("verify")
        };
        let single = outcome(1, true);
        assert_eq!(single.iter().filter(|failure| failure.is_some()).count(), 5);
        for jobs in [2, 3, 8] {
            assert_eq!(outcome(jobs, true), single, "{jobs} threads");
        }
        let positions: Vec<usize> = single
            .iter()
            .enumerate()
            .filter_map(|(index, failure)| failure.as_ref().map(|_| index))
            .collect();
        assert_eq!(
            positions,
            [3, 10, 11, 25, 39],
            "the failures are at their own positions"
        );
    }

    #[test]
    fn progress_is_reported_once_per_object_and_a_cancelled_check_fails() {
        let fx = Fx::new();
        let items: Vec<Item> = (0..12_u64)
            .map(|seed| fx.item(&pseudo_random(1500, seed + 7), None))
            .collect();

        let steps = Arc::new(AtomicUsize::new(0));
        let total = Arc::new(AtomicUsize::new(0));
        let (steps_in, total_in) = (Arc::clone(&steps), Arc::clone(&total));
        let options = CheckOptions {
            jobs: 3,
            observer: Some(Arc::new(move |event: &CheckEvent| match event {
                CheckEvent::Started { total, .. } => {
                    total_in.store(*total as usize, Ordering::SeqCst);
                }
                CheckEvent::Step { .. } => {
                    steps_in.fetch_add(1, Ordering::SeqCst);
                }
            })),
            ..CheckOptions::default()
        };
        verify(&fx.chunks, &items, false, &options).expect("verify");
        assert_eq!(
            (total.load(Ordering::SeqCst), steps.load(Ordering::SeqCst)),
            (12, 12)
        );

        let options = CheckOptions::default();
        options.cancel.cancel();
        let result = verify(&fx.chunks, &items, true, &options);
        assert!(matches!(result, Err(Error::CheckCancelled)), "{result:?}");
    }

    #[test]
    fn an_empty_inventory_is_verified_without_starting_threads_for_nothing() {
        let fx = Fx::new();
        let failures = verify(&fx.chunks, &[], true, &CheckOptions::default()).expect("verify");
        assert!(failures.is_empty());
    }
}
