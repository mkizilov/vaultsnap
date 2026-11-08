//! The prune pipeline: read, decide, mark, check, sweep, verify.
//!
//! The order of the sweep is the whole point. Snapshot files go first and their directory is made
//! durable; chunk objects go second. A snapshot file that exists therefore always has all of its
//! chunks, whatever happens in between (a crash, a power cut, a cancellation, a failed deletion).
//! The objects deleted are exactly the ones that no *kept* snapshot needs, computed before
//! anything is deleted from the manifests of the kept snapshots, so no kept snapshot can lose a
//! chunk to the sweep.

use std::collections::HashSet;
use std::time::Instant;

use super::{
    PlanDigest, PruneEvent, PruneFailure, PruneMode, PruneOptions, PruneReport, PrunePhase,
    MAX_LISTED_FAILURES,
};
use crate::chunk_store::{ChunkId, ChunkStore};
use crate::error::{Error, Result};
use crate::retention::Candidate;
use crate::snapshot::{Manifest, NodeKind, SnapshotId, SnapshotStore};

/// The stores a prune works on.
pub(crate) struct Subject<'a> {
    pub snapshots: &'a SnapshotStore,
    pub chunks: &'a ChunkStore,
}

fn notify(options: &PruneOptions, event: PruneEvent) {
    if let Some(observer) = &options.observer {
        observer.event(&event);
    }
}

fn stop_if_cancelled(options: &PruneOptions) -> Result<()> {
    if options.cancel.is_cancelled() {
        Err(Error::PruneCancelled)
    } else {
        Ok(())
    }
}

/// Adds the chunks `manifest` needs to `needed`.
fn mark(manifest: &Manifest, needed: &mut HashSet<ChunkId>) {
    for entry in manifest.roots.iter().flat_map(|root| root.entries.iter()) {
        if let NodeKind::File { chunks } = &entry.kind {
            needed.extend(chunks.iter().map(|chunk| chunk.id));
        }
    }
}

/// Runs a prune. The caller holds the lock: shared for a dry run, exclusive otherwise.
pub(crate) fn run(subject: &Subject<'_>, options: &PruneOptions) -> Result<PruneReport> {
    let Subject { snapshots, chunks } = subject;
    let started = Instant::now();
    options.policy.validate()?;
    stop_if_cancelled(options)?;

    // Read every snapshot. One that cannot be read means the set of chunks in use is unknown.
    let ids = snapshots.ids()?;
    notify(
        options,
        PruneEvent::Started {
            phase: PrunePhase::Reading,
            total: ids.len() as u64,
        },
    );
    let mut candidates: Vec<Candidate> = Vec::with_capacity(ids.len());
    let mut unreadable: Vec<(SnapshotId, String)> = Vec::new();
    for id in ids {
        stop_if_cancelled(options)?;
        match snapshots.load(&id) {
            Ok(manifest) => candidates.push(Candidate {
                id,
                created: manifest.created,
            }),
            // Removed by someone else since the listing; there is nothing to keep or remove.
            Err(Error::SnapshotNotFound(_)) => {}
            Err(err) => unreadable.push((id, err.to_string())),
        }
        notify(options, PruneEvent::Step { phase: PrunePhase::Reading });
    }
    if let Some((first, reason)) = unreadable.first() {
        return Err(Error::UnreadableSnapshots {
            count: unreadable.len(),
            first: first.to_string(),
            reason: reason.clone(),
        });
    }

    // Decide. The newest snapshot is kept by every rule; a decision that says otherwise is a bug
    // and must not be acted on.
    let decisions = options.policy.select(&candidates)?;
    if decisions.last().is_some_and(|newest| !newest.is_kept()) {
        return Err(Error::InvalidRetention("the newest snapshot would be removed"));
    }
    let digest = PlanDigest::of(&decisions);
    if let PruneMode::Confirmed(confirmed) = options.mode {
        if confirmed != digest {
            return Err(Error::PlanChanged);
        }
    }

    // Mark: the chunks that the kept snapshots need.
    let mut needed: HashSet<ChunkId> = HashSet::new();
    for decision in decisions.iter().filter(|decision| decision.is_kept()) {
        stop_if_cancelled(options)?;
        mark(&snapshots.load(&decision.id)?, &mut needed);
    }

    // Check: every marked chunk exists, and find what is not marked.
    let objects = chunks.objects()?;
    let mut missing: Vec<ChunkId> = needed
        .iter()
        .filter(|id| objects.binary_search_by_key(*id, |(object, _)| *object).is_err())
        .copied()
        .collect();
    if !missing.is_empty() {
        missing.sort();
        return Err(Error::KeptSnapshotsDamaged {
            count: missing.len(),
            first: missing[0].to_hex(),
        });
    }
    let garbage: Vec<(ChunkId, u64)> = objects
        .iter()
        .filter(|(id, _)| !needed.contains(id))
        .copied()
        .collect();

    let mut report = PruneReport {
        executed: false,
        decisions,
        digest,
        chunks_in_use: needed.len() as u64,
        objects: objects.len() as u64,
        chunks_unreferenced: garbage.len() as u64,
        bytes_unreferenced: garbage.iter().map(|(_, len)| *len).sum(),
        snapshots_removed: 0,
        chunks_removed: 0,
        bytes_removed: 0,
        failures: Vec::new(),
        failure_count: 0,
        chunks_verified: 0,
        duration: started.elapsed(),
    };
    if options.mode == PruneMode::DryRun {
        return Ok(report);
    }

    // Sweep, part one: snapshot files, oldest first. Each deletion leaves a consistent
    // repository, and a failed one stops everything before any chunk is touched.
    let doomed: Vec<SnapshotId> = report.to_remove().map(|decision| decision.id.clone()).collect();
    notify(
        options,
        PruneEvent::Started {
            phase: PrunePhase::RemovingSnapshots,
            total: doomed.len() as u64,
        },
    );
    for id in &doomed {
        stop_if_cancelled(options)?;
        snapshots.remove(id)?;
        report.snapshots_removed += 1;
        notify(options, PruneEvent::Step { phase: PrunePhase::RemovingSnapshots });
    }
    if !doomed.is_empty() {
        // The barrier: the removals are on disk before any chunk goes, so a crash cannot bring
        // back a snapshot whose chunks are gone.
        snapshots.sync_removals()?;
    }

    // Sweep, part two: the objects no kept snapshot needs. A failure here leaves a harmless
    // leftover and does not stop the rest.
    notify(
        options,
        PruneEvent::Started {
            phase: PrunePhase::RemovingChunks,
            total: garbage.len() as u64,
        },
    );
    for (id, len) in &garbage {
        stop_if_cancelled(options)?;
        match chunks.remove(id) {
            Ok(()) => {
                report.chunks_removed += 1;
                report.bytes_removed += len;
            }
            // Already gone: the goal is reached.
            Err(err) if err.is_not_found() => {}
            Err(err) => {
                report.failure_count += 1;
                if report.failures.len() < MAX_LISTED_FAILURES {
                    report.failures.push(PruneFailure {
                        subject: id.object_name(),
                        reason: err.to_string(),
                    });
                }
            }
        }
        notify(options, PruneEvent::Step { phase: PrunePhase::RemovingChunks });
    }

    // Verify: everything a kept snapshot needs is still there.
    let mut in_use: Vec<&ChunkId> = needed.iter().collect();
    in_use.sort();
    for id in in_use {
        if !chunks.contains(id)? {
            return Err(Error::RetainedChunkLost {
                first: id.to_hex(),
            });
        }
        report.chunks_verified += 1;
    }
    report.executed = true;
    report.duration = started.elapsed();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use tempfile::TempDir;

    use super::*;
    use crate::chunking::test_util::pseudo_random;
    use crate::keys::RepositoryKeys;
    use crate::retention::RetentionPolicy;
    use crate::scan::Timestamp;
    use crate::snapshot::testing::{file, minimal, REPO_ID};
    use crate::snapshot::ChunkRef;
    use crate::store::LocalStore;

    /// A repository of hand-made snapshots at chosen times over real chunks, so that retention can
    /// be exercised on calendar dates and chunks can be shared in exact, known ways.
    struct Fx {
        dir: TempDir,
        chunks: ChunkStore,
        snapshots: SnapshotStore,
        next_id: u8,
    }

    impl Fx {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("create temporary directory");
            let local = LocalStore::new(dir.path());
            local.create_dir("data").expect("mkdir");
            local.create_dir("snapshots").expect("mkdir");
            let keys = RepositoryKeys::generate().expect("generate");
            Self {
                chunks: ChunkStore::new(local.clone(), &keys).expect("chunk store"),
                snapshots: SnapshotStore::new(local, &keys, REPO_ID).expect("snapshot store"),
                dir,
                next_id: 1,
            }
        }

        fn subject(&self) -> Subject<'_> {
            Subject {
                snapshots: &self.snapshots,
                chunks: &self.chunks,
            }
        }

        /// A chunk with content chosen by `seed`; equal seeds are the same chunk.
        fn chunk(&self, seed: u64) -> ChunkRef {
            let data = pseudo_random(700 + seed as usize, seed);
            let put = self.chunks.put(&data).expect("put");
            ChunkRef {
                id: put.id,
                len: u32::try_from(data.len()).expect("small"),
            }
        }

        /// Commits a snapshot taken at `secs` whose files are the chunks of the given seeds, one
        /// file per seed. Returns its id.
        fn snapshot(&mut self, secs: i64, seeds: &[u64]) -> SnapshotId {
            let mut manifest = minimal();
            manifest.id = format!("{:02x}", self.next_id).repeat(32).parse().expect("valid id");
            self.next_id += 1;
            manifest.created = Timestamp { secs, nanos: 0 };
            for (index, seed) in seeds.iter().enumerate() {
                let chunk = self.chunk(*seed);
                manifest.roots[0]
                    .entries
                    .push(file(0, format!("f{index}").as_bytes(), vec![chunk]));
            }
            manifest.stats = manifest.compute_stats();
            self.snapshots.commit(&manifest).expect("commit");
            manifest.id
        }

        fn object_count(&self) -> usize {
            self.chunks.objects().expect("objects").len()
        }

        fn ids(&self) -> Vec<SnapshotId> {
            self.snapshots.ids().expect("ids")
        }

        /// Every file below the repository with its content.
        fn dump(&self) -> Vec<(PathBuf, Vec<u8>)> {
            let mut found = Vec::new();
            let mut pending = vec![self.dir.path().to_path_buf()];
            while let Some(dir) = pending.pop() {
                for entry in fs::read_dir(&dir).expect("read_dir") {
                    let path = entry.expect("entry").path();
                    if path.is_dir() {
                        pending.push(path);
                    } else {
                        found.push((path.clone(), fs::read(&path).expect("read")));
                    }
                }
            }
            found.sort();
            found
        }

        /// True if every chunk of snapshot `id` exists and verifies.
        fn restorable(&self, id: &SnapshotId) -> bool {
            let manifest = self.snapshots.load(id).expect("load");
            manifest
                .roots
                .iter()
                .flat_map(|root| root.entries.iter())
                .filter_map(|entry| match &entry.kind {
                    NodeKind::File { chunks } => Some(chunks),
                    _ => None,
                })
                .flatten()
                .all(|chunk| self.chunks.get(&chunk.id).is_ok())
        }
    }

    const DAY: i64 = 86_400;

    fn keep_last(count: u32) -> RetentionPolicy {
        RetentionPolicy {
            keep_last: Some(count),
            ..RetentionPolicy::default()
        }
    }

    fn options(policy: RetentionPolicy, mode: PruneMode) -> PruneOptions {
        PruneOptions {
            mode,
            ..PruneOptions::new(policy)
        }
    }

    #[test]
    fn shared_chunks_survive_and_only_unshared_ones_go() {
        let mut fx = Fx::new();
        // Chunk 1 is in all three snapshots, 2 in the first two, 3 in the second, 4 in the third.
        let oldest = fx.snapshot(DAY, &[1, 2]);
        let middle = fx.snapshot(2 * DAY, &[1, 2, 3]);
        let newest = fx.snapshot(3 * DAY, &[1, 4]);
        assert_eq!(fx.object_count(), 4);

        let report = run(&fx.subject(), &options(keep_last(1), PruneMode::Unattended)).expect("prune");
        assert!(report.executed && report.is_complete());
        assert_eq!(fx.ids(), [newest.clone()]);
        assert_eq!(fx.object_count(), 2, "chunks 1 and 4 remain; 2 and 3 were only in removed snapshots");
        assert_eq!((report.snapshots_removed, report.chunks_removed, report.chunks_unreferenced), (2, 2, 2));
        assert_eq!((report.chunks_in_use, report.chunks_verified, report.objects), (2, 2, 4));
        assert!(fx.restorable(&newest));
        assert!(matches!(fx.snapshots.load(&oldest), Err(Error::SnapshotNotFound(_))));
        assert!(matches!(fx.snapshots.load(&middle), Err(Error::SnapshotNotFound(_))));
    }

    #[test]
    fn removing_a_middle_snapshot_leaves_both_neighbours_whole() {
        // 2022-06-01, 2023-01-10, and 2023-06-01. Keeping one snapshot per year for two years
        // keeps the newest of 2023 and the one of 2022, and drops the one in between.
        let mut fx = Fx::new();
        let oldest = fx.snapshot(19_144 * DAY, &[1, 2, 3]);
        let middle = fx.snapshot(19_367 * DAY, &[2, 3, 4]);
        let newest = fx.snapshot(19_509 * DAY, &[3, 4, 5]);
        let policy = RetentionPolicy {
            keep_yearly: Some(2),
            ..RetentionPolicy::default()
        };

        let report = run(&fx.subject(), &options(policy, PruneMode::Unattended)).expect("prune");
        assert_eq!(report.snapshots_removed, 1);
        let remaining = fx.ids();
        assert!(remaining.contains(&oldest) && remaining.contains(&newest));
        assert!(!remaining.contains(&middle));
        // Everything the middle one used is also used by a neighbour, so no object goes.
        assert_eq!((report.chunks_removed, report.chunks_unreferenced), (0, 0));
        assert_eq!(fx.object_count(), 5);
        assert!(remaining.iter().all(|id| fx.restorable(id)));
    }

    #[test]
    fn a_dry_run_changes_nothing_and_describes_exactly_what_executing_does() {
        let mut fx = Fx::new();
        fx.snapshot(DAY, &[1, 2]);
        fx.snapshot(2 * DAY, &[2, 3]);
        fx.snapshot(3 * DAY, &[3, 4]);
        let orphan = fx.chunk(99);
        let before = fx.dump();

        let plan = run(&fx.subject(), &options(keep_last(1), PruneMode::DryRun)).expect("plan");
        assert!(!plan.executed);
        assert_eq!(fx.dump(), before, "not a byte changed");
        assert_eq!((plan.snapshots_to_remove(), plan.chunks_unreferenced), (2, 3));
        assert_eq!((plan.snapshots_removed, plan.chunks_removed, plan.bytes_removed), (0, 0, 0));
        assert!(plan.bytes_unreferenced > 0);
        assert!(fx.chunks.contains(&orphan.id).expect("contains"));

        let done = run(&fx.subject(), &options(keep_last(1), PruneMode::Unattended)).expect("prune");
        assert!(done.executed);
        assert_eq!(done.digest, plan.digest);
        assert_eq!(done.snapshots_removed, plan.snapshots_to_remove());
        assert_eq!(done.chunks_removed, plan.chunks_unreferenced);
        assert_eq!(done.bytes_removed, plan.bytes_unreferenced);
        assert!(!fx.chunks.contains(&orphan.id).expect("contains"), "unreferenced objects are swept too");
    }

    #[test]
    fn executing_needs_the_plan_that_was_confirmed() {
        let mut fx = Fx::new();
        fx.snapshot(DAY, &[1]);
        fx.snapshot(2 * DAY, &[2]);
        let plan = run(&fx.subject(), &options(keep_last(1), PruneMode::DryRun)).expect("plan");

        // A new snapshot arrives after the plan was shown: keep-last 1 would now remove another.
        fx.snapshot(3 * DAY, &[3]);
        let before = fx.dump();
        let err = run(&fx.subject(), &options(keep_last(1), PruneMode::Confirmed(plan.digest)))
            .expect_err("the plan changed");
        assert!(matches!(err, Error::PlanChanged), "{err}");
        assert_eq!(fx.dump(), before, "nothing was deleted");

        // The new plan, confirmed, is executed.
        let new_plan = run(&fx.subject(), &options(keep_last(1), PruneMode::DryRun)).expect("plan");
        assert_ne!(new_plan.digest, plan.digest);
        let done = run(&fx.subject(), &options(keep_last(1), PruneMode::Confirmed(new_plan.digest)))
            .expect("prune");
        assert_eq!(done.snapshots_removed, 2);
    }

    #[test]
    fn a_confirmed_plan_for_another_policy_is_not_executed() {
        let mut fx = Fx::new();
        fx.snapshot(DAY, &[1]);
        fx.snapshot(2 * DAY, &[2]);
        fx.snapshot(3 * DAY, &[3]);
        let plan = run(&fx.subject(), &options(keep_last(2), PruneMode::DryRun)).expect("plan");
        let err = run(&fx.subject(), &options(keep_last(1), PruneMode::Confirmed(plan.digest)))
            .expect_err("a different policy removes different snapshots");
        assert!(matches!(err, Error::PlanChanged));
        assert_eq!(fx.ids().len(), 3);
    }

    #[test]
    fn a_policy_that_keeps_everything_still_sweeps_unreferenced_objects() {
        let mut fx = Fx::new();
        let only = fx.snapshot(DAY, &[1, 2]);
        let orphans = [fx.chunk(50), fx.chunk(51)];
        assert_eq!(fx.object_count(), 4);

        let report = run(&fx.subject(), &options(keep_last(10), PruneMode::Unattended)).expect("prune");
        assert_eq!((report.snapshots_removed, report.chunks_removed), (0, 2));
        assert_eq!(fx.ids(), [only.clone()]);
        assert!(orphans.iter().all(|c| !fx.chunks.contains(&c.id).expect("contains")));
        assert!(fx.restorable(&only));
    }

    #[test]
    fn an_empty_repository_and_a_repeated_prune_have_nothing_to_do() {
        let mut fx = Fx::new();
        let report = run(&fx.subject(), &options(keep_last(1), PruneMode::Unattended)).expect("prune");
        assert!(report.is_noop() && report.executed && report.decisions.is_empty());

        fx.snapshot(DAY, &[1]);
        fx.snapshot(2 * DAY, &[2]);
        run(&fx.subject(), &options(keep_last(1), PruneMode::Unattended)).expect("prune");
        let again = run(&fx.subject(), &options(keep_last(1), PruneMode::Unattended)).expect("prune");
        assert!(again.is_noop(), "pruning twice is the same as pruning once");
        assert_eq!((again.snapshots_removed, again.chunks_removed), (0, 0));
    }

    #[test]
    fn calendar_rules_decide_which_snapshots_go() {
        // Daily snapshots over three months: 2024-01-01 .. 2024-03-31 is 91 days; take every day.
        let jan_first = 19_723 * DAY;
        let mut fx = Fx::new();
        for day in 0..91_i64 {
            fx.snapshot(jan_first + day * DAY + 12 * 3600, &[day as u64 + 1000]);
        }
        let policy = RetentionPolicy {
            keep_daily: Some(3),
            keep_monthly: Some(3),
            ..RetentionPolicy::default()
        };
        let report = run(&fx.subject(), &options(policy, PruneMode::Unattended)).expect("prune");
        // The last three days (Mar 29-31) and the last day of each month (Jan 31, Feb 29, Mar 31).
        assert_eq!(fx.ids().len(), 5);
        assert_eq!(report.snapshots_removed, 86);
        assert!(fx.ids().iter().all(|id| fx.restorable(id)));
        assert_eq!(fx.object_count(), 5, "each snapshot had a chunk of its own");
    }

    // --- refusing to guess -----------------------------------------------------------------

    #[test]
    fn an_unreadable_snapshot_stops_everything_and_nothing_is_changed() {
        for mode in [PruneMode::DryRun, PruneMode::Unattended] {
            let mut fx = Fx::new();
            fx.snapshot(DAY, &[1, 2]);
            let victim = fx.snapshot(2 * DAY, &[3]);
            fx.snapshot(3 * DAY, &[4]);
            let path = fx.dir.path().join(format!("snapshots/{victim}"));
            let mut bytes = fs::read(&path).expect("read");
            let middle = bytes.len() / 2;
            bytes[middle] ^= 1;
            fs::write(&path, bytes).expect("write");
            let before = fx.dump();

            let err = run(&fx.subject(), &options(keep_last(1), mode)).expect_err("must refuse");
            match err {
                Error::UnreadableSnapshots { count, first, .. } => {
                    assert_eq!((count, first), (1, victim.to_string()));
                }
                other => panic!("expected UnreadableSnapshots, got {other}"),
            }
            assert_eq!(fx.dump(), before, "{mode:?}: nothing was deleted");
        }
    }

    #[test]
    fn a_kept_snapshot_that_is_already_missing_chunks_blocks_the_prune() {
        let mut fx = Fx::new();
        let older = fx.snapshot(DAY, &[1, 2]);
        let newer = fx.snapshot(2 * DAY, &[3, 4]);
        // The newest snapshot loses a chunk; keeping only it would delete the intact older one.
        let lost = fx.chunk(3);
        fs::remove_file(fx.dir.path().join(lost.id.object_name())).expect("remove");
        let before = fx.dump();

        let err = run(&fx.subject(), &options(keep_last(1), PruneMode::Unattended)).expect_err("refused");
        match err {
            Error::KeptSnapshotsDamaged { count, first } => {
                assert_eq!((count, first), (1, lost.id.to_hex()));
            }
            other => panic!("expected KeptSnapshotsDamaged, got {other}"),
        }
        assert_eq!(fx.dump(), before);
        assert!(fx.restorable(&older));
        assert!(!fx.restorable(&newer));
    }

    #[test]
    fn a_damaged_snapshot_that_would_be_removed_anyway_does_not_block_the_prune() {
        let mut fx = Fx::new();
        let doomed = fx.snapshot(DAY, &[1, 2]);
        let kept = fx.snapshot(2 * DAY, &[3]);
        let lost = fx.chunk(1);
        fs::remove_file(fx.dir.path().join(lost.id.object_name())).expect("remove");

        let report = run(&fx.subject(), &options(keep_last(1), PruneMode::Unattended)).expect("prune");
        assert_eq!(report.snapshots_removed, 1);
        assert!(matches!(fx.snapshots.load(&doomed), Err(Error::SnapshotNotFound(_))));
        assert!(fx.restorable(&kept));
        assert_eq!(fx.object_count(), 1);
    }

    #[test]
    fn invalid_policies_fail_before_anything_is_read() {
        let mut fx = Fx::new();
        fx.snapshot(DAY, &[1]);
        let before = fx.dump();
        for policy in [RetentionPolicy::default(), keep_last(0)] {
            let err = run(&fx.subject(), &options(policy, PruneMode::Unattended)).expect_err("invalid");
            assert!(matches!(err, Error::InvalidRetention(_)), "{err}");
        }
        assert_eq!(fx.dump(), before);
    }

    #[test]
    fn strays_and_temporary_files_are_never_deleted() {
        let mut fx = Fx::new();
        fx.snapshot(DAY, &[1]);
        fx.snapshot(2 * DAY, &[2]);
        let root = fx.dir.path();
        let shard = root.join("data").join(&fx.chunk(1).id.to_hex()[..2]);
        let strays = [
            root.join("notes.txt"),
            root.join("snapshots/.tmp-0011223344556677"),
            root.join("snapshots/not-a-snapshot"),
            shard.join(".tmp-0011223344556677"),
            shard.join("not-an-object"),
        ];
        for stray in &strays {
            fs::write(stray, b"mine").expect("write");
        }

        run(&fx.subject(), &options(keep_last(1), PruneMode::Unattended)).expect("prune");
        for stray in &strays {
            assert_eq!(fs::read(stray).expect("still there"), b"mine", "{stray:?}");
        }
    }

    // --- interruption ----------------------------------------------------------------------

    /// Runs a prune that is cancelled once `deletions` deletions have happened.
    fn interrupted(fx: &Fx, policy: &RetentionPolicy, deletions: usize) -> Result<PruneReport> {
        let cancel = crate::backup::CancelToken::new();
        let done = Arc::new(AtomicUsize::new(0));
        let (token, counter) = (cancel.clone(), Arc::clone(&done));
        let mut options = options(policy.clone(), PruneMode::Unattended);
        options.cancel = cancel.clone();
        options.observer = Some(Arc::new(move |event: &PruneEvent| {
            let deletion = matches!(
                event,
                PruneEvent::Step {
                    phase: PrunePhase::RemovingSnapshots | PrunePhase::RemovingChunks
                }
            );
            if deletion && counter.fetch_add(1, Ordering::SeqCst) + 1 == deletions {
                token.cancel();
            }
        }));
        if deletions == 0 {
            cancel.cancel();
        }
        run(&fx.subject(), &options)
    }

    #[test]
    fn stopping_after_any_number_of_deletions_leaves_every_snapshot_whole_and_a_rerun_finishes() {
        // Count the deletions of a complete prune.
        let build = || {
            let mut fx = Fx::new();
            fx.snapshot(DAY, &[1, 2, 3]);
            fx.snapshot(2 * DAY, &[2, 3, 4]);
            fx.snapshot(3 * DAY, &[3, 4, 5]);
            fx.snapshot(4 * DAY, &[5, 6]);
            fx.chunk(77);
            fx
        };
        let policy = keep_last(1);
        let total = {
            let fx = build();
            let report = run(&fx.subject(), &options(policy.clone(), PruneMode::Unattended)).expect("prune");
            (report.snapshots_removed + report.chunks_removed) as usize
        };
        assert_eq!(total, 3 + 5, "three snapshots; chunks 1 to 4 and the orphan 77 are unreferenced");

        for deletions in 0..=total {
            let fx = build();
            let result = interrupted(&fx, &policy, deletions);
            if deletions < total {
                assert!(matches!(result, Err(Error::PruneCancelled)), "after {deletions}: {result:?}");
            }
            // Whatever was done, every snapshot that still exists can be restored in full.
            for id in fx.ids() {
                assert!(fx.restorable(&id), "after {deletions} deletions, {id} lost a chunk");
            }
            // And running the prune again reaches the same end state as an uninterrupted one.
            let finished = run(&fx.subject(), &options(policy.clone(), PruneMode::Unattended)).expect("rerun");
            assert!(finished.is_complete());
            assert_eq!(fx.ids().len(), 1, "after {deletions}");
            assert_eq!(fx.object_count(), 2, "after {deletions}: chunks 5 and 6 only");
            assert!(fx.restorable(&fx.ids()[0]));
        }
    }

    #[test]
    fn a_cancelled_prune_reports_that_it_was_stopped_and_not_that_it_failed() {
        let mut fx = Fx::new();
        fx.snapshot(DAY, &[1]);
        fx.snapshot(2 * DAY, &[2]);
        let options = options(keep_last(1), PruneMode::Unattended);
        options.cancel.cancel();
        let before = fx.dump();
        let err = run(&fx.subject(), &options).expect_err("cancelled");
        assert!(matches!(err, Error::PruneCancelled), "{err}");
        assert_eq!(fx.dump(), before);
    }

    #[test]
    fn progress_counts_every_deletion() {
        let mut fx = Fx::new();
        fx.snapshot(DAY, &[1, 2]);
        fx.snapshot(2 * DAY, &[3]);
        let snapshots = Arc::new(AtomicUsize::new(0));
        let chunks = Arc::new(AtomicUsize::new(0));
        let (s, c) = (Arc::clone(&snapshots), Arc::clone(&chunks));
        let mut options = options(keep_last(1), PruneMode::Unattended);
        options.observer = Some(Arc::new(move |event: &PruneEvent| match event {
            PruneEvent::Step { phase: PrunePhase::RemovingSnapshots } => {
                s.fetch_add(1, Ordering::SeqCst);
            }
            PruneEvent::Step { phase: PrunePhase::RemovingChunks } => {
                c.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        }));
        let report = run(&fx.subject(), &options).expect("prune");
        assert_eq!(snapshots.load(Ordering::SeqCst) as u64, report.snapshots_removed);
        assert_eq!(chunks.load(Ordering::SeqCst) as u64, report.chunks_removed);
    }

    // --- failures --------------------------------------------------------------------------

    #[cfg(unix)]
    #[test]
    fn an_object_that_cannot_be_deleted_is_reported_and_the_rest_goes_on() {
        use std::os::unix::fs::PermissionsExt;

        struct Unlock(PathBuf);
        impl Drop for Unlock {
            fn drop(&mut self) {
                let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
            }
        }

        let mut fx = Fx::new();
        fx.snapshot(DAY, &[1, 2, 3, 4, 5, 6]);
        let kept = fx.snapshot(2 * DAY, &[7]);
        // Make one shard directory read-only: its objects cannot be unlinked.
        let victim = fx.chunk(1);
        let shard = fx.dir.path().join(victim.id.shard_dir());
        let _guard = Unlock(shard.clone());
        fs::set_permissions(&shard, fs::Permissions::from_mode(0o500)).expect("chmod");
        if fs::write(shard.join("probe"), b"x").is_ok() {
            // Running with privileges that ignore permissions (root): the case cannot be set up.
            fs::remove_file(shard.join("probe")).expect("cleanup");
            return;
        }

        let report = run(&fx.subject(), &options(keep_last(1), PruneMode::Unattended)).expect("prune goes on");
        assert!(report.executed && !report.is_complete());
        assert!(report.failure_count >= 1 && report.failures.len() as u64 == report.failure_count);
        assert!(report.failures[0].subject.starts_with("data/"), "{:?}", report.failures[0]);
        assert_eq!(report.snapshots_removed, 1, "the snapshot is gone");
        assert_eq!(report.chunks_removed + report.failure_count, report.chunks_unreferenced);
        assert!(fx.restorable(&kept), "the kept snapshot is untouched");
        assert_eq!(report.chunks_verified, report.chunks_in_use);
    }

    #[test]
    fn the_newest_snapshot_is_never_removed_by_any_valid_policy() {
        let mut fx = Fx::new();
        let first = fx.snapshot(DAY, &[1]);
        let newest = fx.snapshot(400 * DAY, &[2]);
        for policy in [
            keep_last(1),
            RetentionPolicy { keep_daily: Some(1), ..Default::default() },
            RetentionPolicy { keep_weekly: Some(1), ..Default::default() },
            RetentionPolicy { keep_monthly: Some(1), ..Default::default() },
            RetentionPolicy { keep_yearly: Some(1), ..Default::default() },
        ] {
            let plan = run(&fx.subject(), &options(policy, PruneMode::DryRun)).expect("plan");
            let removed: Vec<&SnapshotId> = plan.to_remove().map(|d| &d.id).collect();
            assert_eq!(removed, [&first]);
            assert!(plan.kept().any(|d| d.id == newest));
        }
    }
}
