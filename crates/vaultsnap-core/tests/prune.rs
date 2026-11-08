//! Integration tests of prune on repositories made by real backups: which snapshots and which
//! chunk objects go, that chunks shared with a kept snapshot stay, that every kept snapshot still
//! restores byte for byte and passes a full check, that a dry run and a refused prune change
//! nothing, and what happens when a prune is interrupted or races another operation.
//!
//! Retention by calendar (days, weeks, months, years) needs snapshots with chosen times, which a
//! real backup cannot produce; those rules are tested in `src/retention.rs` and in
//! `src/prune/engine.rs`. Here the rule is "keep the newest N", which does not depend on the clock.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;
use vaultsnap_core::snapshot::NodeKind;
use vaultsnap_core::{
    BackupOptions, BackupReport, CheckOptions, CheckReport, ChunkId, ChunkerConfig, Error,
    KdfParams, KeepReason, LockKind, Passphrase, PruneEvent, PruneMode, PruneOptions, PrunePhase,
    PruneReport, Repository, RepositoryKeys, RestoreOptions, RetentionPolicy, SnapshotId,
    SnapshotRef,
};

type Tree = BTreeMap<String, Vec<u8>>;

fn passphrase() -> Passphrase {
    Passphrase::new(b"correct horse battery staple".to_vec()).expect("valid passphrase")
}

fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

fn write(root: &Path, rel: &str, contents: &[u8]) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parents");
    }
    fs::write(path, contents).expect("write file");
}

fn pause() {
    std::thread::sleep(Duration::from_millis(10));
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("read_dir")
        .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Every regular file below `root` with its bytes, by path relative to `root`.
fn read_tree(root: &Path) -> Tree {
    let mut tree = Tree::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).expect("read_dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let relative = path.strip_prefix(root).expect("inside");
                let bytes = fs::read(&path).expect("read");
                tree.insert(relative.to_string_lossy().into_owned(), bytes);
            }
        }
    }
    tree
}

fn keep_last(count: u32) -> RetentionPolicy {
    RetentionPolicy {
        keep_last: Some(count),
        ..RetentionPolicy::default()
    }
}

fn assert_intact(report: &CheckReport) {
    let stats = &report.stats;
    assert_eq!(
        (stats.chunks_missing, stats.objects_damaged, stats.snapshots_damaged),
        (0, 0, 0),
        "{:?}",
        report.findings
    );
}

fn only_lock_conflicts<T>(result: Result<T, Error>, what: &str) {
    if let Err(err) = result {
        assert!(matches!(err, Error::Locked { .. }), "{what}: {err}");
    }
}

// ---------------------------------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------------------------------

struct Env {
    dir: TempDir,
    repo: Repository,
    keys: RepositoryKeys,
}

impl Env {
    async fn new() -> Self {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let repo = Repository::at(dir.path().join("repo"));
        let kdf = KdfParams {
            m_cost_kib: 64,
            t_cost: 1,
            p_cost: 1,
        };
        repo.init_with_kdf(&passphrase(), kdf).await.expect("init");
        let keys = repo.unlock(&passphrase()).await.expect("unlock");
        Self { dir, repo, keys }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.dir.path().join(relative)
    }

    /// The whole repository: every file with its bytes.
    fn repo_state(&self) -> Tree {
        read_tree(&self.path("repo"))
    }

    fn assert_unlocked(&self) {
        let locks = names(&self.path("repo/locks"));
        assert!(locks.is_empty(), "locks left behind: {locks:?}");
    }

    async fn try_backup(&self) -> Result<BackupReport, Error> {
        let options = BackupOptions {
            sources: vec![self.path("src")],
            chunker: ChunkerConfig::new(1024, 4096, 16_384).expect("valid config"),
            host: "test-host".to_owned(),
            jobs: 2,
            racy_margin: Duration::ZERO,
            ..BackupOptions::default()
        };
        self.repo.backup(&self.keys, options).await
    }

    async fn backup_report(&self) -> BackupReport {
        pause();
        self.try_backup().await.expect("backup succeeds")
    }

    async fn backup(&self) -> SnapshotId {
        self.backup_report().await.snapshot
    }

    async fn prune(&self, policy: RetentionPolicy, mode: PruneMode) -> Result<PruneReport, Error> {
        let options = PruneOptions {
            mode,
            ..PruneOptions::new(policy)
        };
        self.repo.prune(&self.keys, options).await
    }

    async fn check(&self) -> CheckReport {
        let options = CheckOptions {
            read_data: true,
            ..CheckOptions::default()
        };
        self.repo.check(&self.keys, options).await.expect("the check runs")
    }

    fn chunk_file(&self, id: &ChunkId) -> PathBuf {
        let hex = id.to_hex();
        self.path("repo/data").join(&hex[..2]).join(hex)
    }

    fn snapshot_file(&self, id: &SnapshotId) -> PathBuf {
        self.path("repo/snapshots").join(id.as_str())
    }

    /// The names of the snapshot files that exist.
    fn snapshot_names(&self) -> Vec<String> {
        names(&self.path("repo/snapshots"))
    }

    /// Every chunk the snapshot needs.
    async fn chunks(&self, id: &SnapshotId) -> HashSet<ChunkId> {
        let manifest = self.repo.snapshot(&self.keys, &SnapshotRef::from(id)).await.expect("load");
        let mut chunks = HashSet::new();
        for listed in vaultsnap_core::history::entries(&manifest, None) {
            if let NodeKind::File { chunks: list } = &listed.entry.kind {
                chunks.extend(list.iter().map(|chunk| chunk.id));
            }
        }
        chunks
    }

    /// Restores the snapshot and compares the files with `expected`.
    async fn assert_restores(&self, id: &SnapshotId, expected: &Tree) {
        let target = self.path(&format!("restored-{}", id.short()));
        let options = RestoreOptions::new(SnapshotRef::from(id), &target);
        let report = self.repo.restore(&self.keys, options).await.expect("restore succeeds");
        assert!(report.is_complete(), "{report:?}");
        let source = fs::canonicalize(self.path("src")).expect("the source exists");
        let root = report.target.join(source.strip_prefix("/").expect("absolute source"));
        assert_eq!(&read_tree(&root), expected, "snapshot {id} restores differently");
        fs::remove_dir_all(&target).expect("remove the restored copy");
    }
}

/// A repository with four snapshots of a source that changes between them, and the files each
/// snapshot was taken from.
///
/// * 1: `shared.bin`, `old.bin`, `notes.txt`
/// * 2: `shared.bin`, `middle.bin`, `notes.txt` (edited)
/// * 3: `shared.bin`, `new.bin`, `notes.txt` (as in 2)
/// * 4: `shared.bin`, `new.bin`, `notes.txt` (edited again)
struct History {
    env: Env,
    ids: Vec<SnapshotId>,
    trees: Vec<Tree>,
}

impl History {
    async fn take(&mut self) {
        let id = self.env.backup().await;
        self.trees.push(read_tree(&self.env.path("src")));
        self.ids.push(id);
    }

    /// Every snapshot that still exists restores to the files it was taken from.
    async fn assert_remaining_restore(&self) {
        for name in self.env.snapshot_names() {
            let index = self.ids.iter().position(|id| id.as_str() == name).expect("a known id");
            self.env.assert_restores(&self.ids[index], &self.trees[index]).await;
        }
    }
}

async fn history() -> History {
    let mut h = History {
        env: Env::new().await,
        ids: Vec::new(),
        trees: Vec::new(),
    };
    let src = h.env.path("src");
    write(&src, "shared.bin", &pseudo_random(200_000, 1));
    write(&src, "old.bin", &pseudo_random(100_000, 2));
    write(&src, "notes.txt", b"notes: first");
    h.take().await;

    fs::remove_file(src.join("old.bin")).expect("remove");
    write(&src, "middle.bin", &pseudo_random(80_000, 3));
    write(&src, "notes.txt", b"notes: second edit");
    h.take().await;

    fs::remove_file(src.join("middle.bin")).expect("remove");
    write(&src, "new.bin", &pseudo_random(60_000, 4));
    h.take().await;

    write(&src, "notes.txt", b"notes: the fourth edit, longer");
    h.take().await;
    h
}

fn cancel_after_deletions(options: &mut PruneOptions, deletions: usize) {
    let cancel = options.cancel.clone();
    let seen = Arc::new(AtomicUsize::new(0));
    options.observer = Some(Arc::new(move |event: &PruneEvent| {
        let deleting = matches!(event, PruneEvent::Step { phase } if *phase != PrunePhase::Reading);
        if deleting && seen.fetch_add(1, Ordering::SeqCst) + 1 == deletions {
            cancel.cancel();
        }
    }));
}

// ---------------------------------------------------------------------------------------------
// What goes and what stays
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_dry_run_is_the_default_and_changes_nothing() {
    let h = history().await;
    let before = h.env.repo_state();

    let options = PruneOptions::new(keep_last(2));
    assert_eq!(options.mode, PruneMode::DryRun);
    let report = h.env.repo.prune(&h.env.keys, options).await.expect("plan");

    assert!(!report.executed);
    assert_eq!(report.snapshots_to_remove(), 2);
    assert!(report.chunks_unreferenced > 0 && report.bytes_unreferenced > 0);
    assert_eq!((report.snapshots_removed, report.chunks_removed, report.bytes_removed), (0, 0, 0));
    assert_eq!(h.env.repo_state(), before, "nothing in the repository changed");
    h.env.assert_unlocked();
}

#[tokio::test]
async fn pruning_removes_old_snapshots_and_exactly_the_chunks_no_kept_snapshot_needs() {
    let h = history().await;
    let (s1, s2, s3, s4) = (&h.ids[0], &h.ids[1], &h.ids[2], &h.ids[3]);
    let (c1, c2) = (h.env.chunks(s1).await, h.env.chunks(s2).await);
    let (c3, c4) = (h.env.chunks(s3).await, h.env.chunks(s4).await);
    let kept: HashSet<ChunkId> = c3.union(&c4).copied().collect();
    let doomed: HashSet<ChunkId> =
        c1.union(&c2).copied().filter(|id| !kept.contains(id)).collect();
    let shared_with_kept: Vec<ChunkId> = c1.intersection(&kept).copied().collect();
    assert!(!doomed.is_empty() && !shared_with_kept.is_empty(), "the history must share chunks");

    let report = h.env.prune(keep_last(2), PruneMode::Unattended).await.expect("prune");

    assert!(report.executed && report.is_complete());
    assert_eq!(report.decisions.iter().map(|d| d.id.clone()).collect::<Vec<_>>(), h.ids);
    let reasons: Vec<Vec<KeepReason>> = report.decisions.iter().map(|d| d.keep.clone()).collect();
    assert_eq!(reasons, [vec![], vec![], vec![KeepReason::Last], vec![KeepReason::Last]]);
    assert_eq!(report.snapshots_removed, 2);
    assert_eq!(report.chunks_removed, doomed.len() as u64);
    assert_eq!(report.chunks_unreferenced, doomed.len() as u64);
    assert_eq!(report.chunks_in_use, kept.len() as u64);
    assert_eq!(report.chunks_verified, kept.len() as u64);

    assert_eq!(h.env.snapshot_names(), [s3.to_string(), s4.to_string()]);
    for id in &doomed {
        assert!(!h.env.chunk_file(id).exists(), "{id:?} should be gone");
    }
    for id in &kept {
        assert!(h.env.chunk_file(id).exists(), "{id:?} is needed by a kept snapshot");
    }
    for id in &shared_with_kept {
        assert!(h.env.chunk_file(id).exists(), "{id:?} was shared with a removed snapshot");
    }
    h.assert_remaining_restore().await;
    assert_intact(&h.env.check().await);
    h.env.assert_unlocked();
}

#[tokio::test]
async fn the_newest_snapshot_survives_the_strictest_policy() {
    let h = history().await;
    let report = h.env.prune(keep_last(1), PruneMode::Unattended).await.expect("prune");
    assert_eq!(report.snapshots_removed, 3);
    assert_eq!(h.env.snapshot_names(), [h.ids[3].to_string()]);
    h.assert_remaining_restore().await;
    let check = h.env.check().await;
    assert!(check.is_clean(), "{:?}", check.findings);
}

#[tokio::test]
async fn a_policy_that_keeps_everything_deletes_nothing_and_a_second_prune_has_nothing_to_do() {
    let h = history().await;
    let before = h.env.repo_state();
    let report = h.env.prune(keep_last(10), PruneMode::Unattended).await.expect("prune");
    assert!(report.executed && report.is_noop(), "{report:?}");
    assert_eq!(h.env.repo_state(), before);

    h.env.prune(keep_last(1), PruneMode::Unattended).await.expect("prune");
    let after = h.env.repo_state();
    let again = h.env.prune(keep_last(1), PruneMode::Unattended).await.expect("prune again");
    assert!(again.is_noop(), "{again:?}");
    assert_eq!((again.snapshots_removed, again.chunks_removed, again.bytes_removed), (0, 0, 0));
    assert_eq!(h.env.repo_state(), after);
    h.env.assert_unlocked();
}

#[tokio::test]
async fn policies_that_could_delete_everything_are_refused_before_anything_happens() {
    let h = history().await;
    let before = h.env.repo_state();
    let policies = [
        RetentionPolicy::default(),
        keep_last(0),
        RetentionPolicy {
            keep_last: Some(3),
            keep_daily: Some(0),
            ..RetentionPolicy::default()
        },
    ];
    for policy in policies {
        for mode in [PruneMode::DryRun, PruneMode::Unattended] {
            let err = h.env.prune(policy.clone(), mode).await.expect_err("must refuse");
            assert!(matches!(err, Error::InvalidRetention(_)), "{policy:?}: {err}");
        }
    }
    assert_eq!(h.env.repo_state(), before);
    h.env.assert_unlocked();
}

#[tokio::test]
async fn objects_no_snapshot_uses_are_collected_and_strangers_in_the_repository_are_left_alone() {
    let h = history().await;
    let chunks = h.env.repo.chunk_store(&h.env.keys).expect("chunk store");
    let orphans: Vec<ChunkId> = [10, 11]
        .map(|seed| chunks.put(&pseudo_random(5000, seed)).expect("store").id)
        .to_vec();
    write(&h.env.path("repo/data"), "ab/notes.txt", b"not a chunk");
    write(&h.env.path("repo/data"), "stray/file", b"not a shard");
    let strays: Tree = read_tree(&h.env.path("repo/data"))
        .into_iter()
        .filter(|(name, _)| name.ends_with("notes.txt") || name.starts_with("stray"))
        .collect();
    assert_eq!(strays.len(), 2);

    let report = h.env.prune(keep_last(10), PruneMode::Unattended).await.expect("prune");

    assert_eq!(report.snapshots_removed, 0);
    assert_eq!(report.chunks_unreferenced, 2);
    assert_eq!(report.chunks_removed, 2);
    for id in &orphans {
        assert!(!h.env.chunk_file(id).exists());
    }
    let after = read_tree(&h.env.path("repo/data"));
    for (name, bytes) in &strays {
        assert_eq!(after.get(name), Some(bytes), "{name} must not be touched");
    }
    assert_eq!(h.env.snapshot_names().len(), 4);
    h.assert_remaining_restore().await;
}

#[tokio::test]
async fn data_deleted_by_a_prune_is_stored_again_by_the_next_backup() {
    let env = Env::new().await;
    let src = env.path("src");
    let old = pseudo_random(120_000, 7);
    write(&src, "old.bin", &old);
    env.backup().await;
    fs::remove_file(src.join("old.bin")).expect("remove");
    write(&src, "now.txt", b"only this remains");
    env.backup().await;
    let report = env.prune(keep_last(1), PruneMode::Unattended).await.expect("prune");
    assert!(report.chunks_removed > 0, "{report:?}");

    write(&src, "old.bin", &old);
    let backup = env.backup_report().await;
    assert!(backup.transfer.chunks_new > 0, "the deleted chunks must be stored again");
    env.assert_restores(&backup.snapshot, &read_tree(&src)).await;
    assert_intact(&env.check().await);
}

// ---------------------------------------------------------------------------------------------
// Confirmation by plan
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_confirmed_plan_runs_exactly_as_shown() {
    let h = history().await;
    let plan = h.env.prune(keep_last(1), PruneMode::DryRun).await.expect("plan");
    let report = h
        .env
        .prune(keep_last(1), PruneMode::Confirmed(plan.digest))
        .await
        .expect("confirmed");
    assert!(report.executed);
    assert_eq!(report.digest, plan.digest);
    assert_eq!(report.snapshots_removed, plan.snapshots_to_remove());
    assert_eq!(report.chunks_removed, plan.chunks_unreferenced);
    h.assert_remaining_restore().await;
}

#[tokio::test]
async fn a_plan_confirmed_before_the_snapshots_changed_deletes_nothing() {
    let h = history().await;
    let plan = h.env.prune(keep_last(1), PruneMode::DryRun).await.expect("plan");
    h.env.backup().await;
    let before = h.env.repo_state();

    let err = h
        .env
        .prune(keep_last(1), PruneMode::Confirmed(plan.digest))
        .await
        .expect_err("the plan is stale");
    assert!(matches!(err, Error::PlanChanged), "{err}");
    assert_eq!(h.env.repo_state(), before);

    let fresh = h.env.prune(keep_last(1), PruneMode::DryRun).await.expect("new plan");
    assert_ne!(fresh.digest, plan.digest);
    h.env.prune(keep_last(1), PruneMode::Confirmed(fresh.digest)).await.expect("and now it runs");
    assert_eq!(h.env.snapshot_names().len(), 1);
}

// ---------------------------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_snapshot_that_cannot_be_read_stops_the_prune_before_anything_is_deleted() {
    let h = history().await;
    let victim = h.env.snapshot_file(&h.ids[1]);
    let mut bytes = fs::read(&victim).expect("read");
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0x01;
    fs::write(&victim, bytes).expect("write");
    let before = h.env.repo_state();

    for mode in [PruneMode::DryRun, PruneMode::Unattended] {
        let err = h.env.prune(keep_last(1), mode).await.expect_err("must refuse");
        match err {
            Error::UnreadableSnapshots { count, first, .. } => {
                assert_eq!(count, 1);
                assert_eq!(first, h.ids[1].to_string());
            }
            other => panic!("expected UnreadableSnapshots, got {other}"),
        }
        assert_eq!(h.env.repo_state(), before);
    }
    h.env.assert_unlocked();
}

#[tokio::test]
async fn the_wrong_keys_cannot_prune() {
    let h = history().await;
    let stranger = Env::new().await;
    let before = h.env.repo_state();
    let options = PruneOptions {
        mode: PruneMode::Unattended,
        ..PruneOptions::new(keep_last(1))
    };
    let err = h.env.repo.prune(&stranger.keys, options).await.expect_err("must refuse");
    assert!(matches!(err, Error::UnreadableSnapshots { count: 4, .. }), "{err}");
    assert_eq!(h.env.repo_state(), before);
}

#[tokio::test]
async fn a_kept_snapshot_that_lacks_a_chunk_stops_the_prune() {
    let h = history().await;
    let (c1, c2) = (h.env.chunks(&h.ids[0]).await, h.env.chunks(&h.ids[1]).await);
    let c3 = h.env.chunks(&h.ids[2]).await;
    let lost = *c3
        .iter()
        .find(|id| !c1.contains(*id) && !c2.contains(*id))
        .expect("snapshot 3 has chunks of its own");
    fs::remove_file(h.env.chunk_file(&lost)).expect("remove chunk");
    let before = h.env.repo_state();

    for mode in [PruneMode::DryRun, PruneMode::Unattended] {
        let err = h.env.prune(keep_last(2), mode).await.expect_err("must refuse");
        match err {
            Error::KeptSnapshotsDamaged { count, first } => {
                assert_eq!(count, 1);
                assert_eq!(first, lost.to_hex());
            }
            other => panic!("expected KeptSnapshotsDamaged, got {other}"),
        }
        assert_eq!(h.env.repo_state(), before);
    }
    h.env.assert_unlocked();
}

#[tokio::test]
async fn a_chunk_missing_from_a_snapshot_that_is_going_anyway_does_not_stop_the_prune() {
    let h = history().await;
    let others: Vec<HashSet<ChunkId>> = vec![
        h.env.chunks(&h.ids[1]).await,
        h.env.chunks(&h.ids[2]).await,
        h.env.chunks(&h.ids[3]).await,
    ];
    let c1 = h.env.chunks(&h.ids[0]).await;
    let lost = *c1
        .iter()
        .find(|id| others.iter().all(|other| !other.contains(*id)))
        .expect("snapshot 1 has chunks of its own");
    fs::remove_file(h.env.chunk_file(&lost)).expect("remove chunk");

    let report = h.env.prune(keep_last(2), PruneMode::Unattended).await.expect("prune");
    assert_eq!(report.snapshots_removed, 2);
    h.assert_remaining_restore().await;
    assert_intact(&h.env.check().await);
}

// ---------------------------------------------------------------------------------------------
// Locking and concurrency
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_deleting_prune_refuses_while_the_repository_is_in_use_and_a_dry_run_does_not() {
    let h = history().await;
    let reader = h.env.repo.lock(LockKind::Shared).await.expect("lock");
    let before = h.env.repo_state();

    h.env.prune(keep_last(1), PruneMode::DryRun).await.expect("a dry run reads next to a reader");
    let err = h.env.prune(keep_last(1), PruneMode::Unattended).await.expect_err("must refuse");
    assert!(matches!(err, Error::Locked { .. }), "{err}");
    assert_eq!(h.env.repo_state(), before, "the refused prune left no trace");
    drop(reader);
    h.env.assert_unlocked();

    let report = h.env.prune(keep_last(1), PruneMode::Unattended).await.expect("and then it runs");
    assert_eq!(report.snapshots_removed, 3);
}

#[tokio::test]
async fn nothing_else_starts_while_a_prune_holds_the_repository() {
    let h = history().await;
    let holder = h.env.repo.lock(LockKind::Exclusive).await.expect("lock");
    let before = h.env.repo_state();

    let err = h.env.try_backup().await.expect_err("a backup must wait");
    assert!(matches!(err, Error::Locked { .. }), "{err}");
    let err = h.env.prune(keep_last(1), PruneMode::DryRun).await.expect_err("even a dry run");
    assert!(matches!(err, Error::Locked { .. }), "{err}");
    let err = h.env.repo.check(&h.env.keys, CheckOptions::default()).await.expect_err("a check");
    assert!(matches!(err, Error::Locked { .. }), "{err}");
    assert_eq!(h.env.repo_state(), before);
    drop(holder);
    h.env.assert_unlocked();
}

#[tokio::test]
async fn a_prune_racing_a_backup_or_another_prune_never_damages_a_snapshot() {
    let mut h = history().await;
    for round in 0..6 {
        if round % 2 == 0 {
            let (backup, prune) = tokio::join!(
                h.env.try_backup(),
                h.env.prune(keep_last(1), PruneMode::Unattended)
            );
            match backup {
                Ok(report) => {
                    let last = h.trees.last().expect("a tree").clone();
                    h.ids.push(report.snapshot);
                    h.trees.push(last);
                }
                Err(err) => only_lock_conflicts::<()>(Err(err), "backup"),
            }
            only_lock_conflicts(prune, "prune");
        } else {
            let (first, second) = tokio::join!(
                h.env.prune(keep_last(1), PruneMode::Unattended),
                h.env.prune(keep_last(1), PruneMode::Unattended)
            );
            only_lock_conflicts(first, "first prune");
            only_lock_conflicts(second, "second prune");
        }
        h.env.assert_unlocked();
        h.assert_remaining_restore().await;
        assert_intact(&h.env.check().await);
    }
    h.env.prune(keep_last(1), PruneMode::Unattended).await.expect("a lone prune always runs");
    assert_eq!(h.env.snapshot_names().len(), 1);
}

// ---------------------------------------------------------------------------------------------
// Interruptions
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_interrupted_prune_never_endangers_a_snapshot_and_a_rerun_finishes_the_job() {
    let plan = history().await.env.prune(keep_last(2), PruneMode::DryRun).await.expect("plan");
    let total = (plan.snapshots_to_remove() + plan.chunks_unreferenced) as usize;
    assert!(total > 8, "the scenario must have a few deletions to interrupt: {total}");

    // Early in the removal of snapshots, between the two, and at several points in the chunks.
    for stop in [1, 2, 3, total / 2, total - 1] {
        let h = history().await;
        let mut options = PruneOptions {
            mode: PruneMode::Unattended,
            ..PruneOptions::new(keep_last(2))
        };
        cancel_after_deletions(&mut options, stop);
        let err = h.env.repo.prune(&h.env.keys, options).await.expect_err("cancelled");
        assert!(matches!(err, Error::PruneCancelled), "stop {stop}: {err}");
        h.env.assert_unlocked();

        for kept in [&h.ids[2], &h.ids[3]] {
            assert!(h.env.snapshot_file(kept).exists(), "stop {stop}: a kept snapshot is gone");
        }
        h.assert_remaining_restore().await;
        let report = h.env.check().await;
        assert_intact(&report);

        let done = h.env.prune(keep_last(2), PruneMode::Unattended).await.expect("rerun");
        assert!(done.executed && done.is_complete(), "stop {stop}: {done:?}");
        assert_eq!(h.env.snapshot_names(), [h.ids[2].to_string(), h.ids[3].to_string()]);
        let rest = h.env.prune(keep_last(2), PruneMode::DryRun).await.expect("plan");
        assert!(rest.is_noop(), "stop {stop}: {rest:?}");
        h.assert_remaining_restore().await;
        let clean = h.env.check().await;
        assert!(clean.is_clean(), "stop {stop}: {:?}", clean.findings);
    }
}

#[tokio::test]
async fn a_prune_cancelled_before_it_starts_changes_nothing() {
    let h = history().await;
    let before = h.env.repo_state();
    let options = PruneOptions {
        mode: PruneMode::Unattended,
        ..PruneOptions::new(keep_last(1))
    };
    options.cancel.cancel();
    let err = h.env.repo.prune(&h.env.keys, options).await.expect_err("cancelled");
    assert!(matches!(err, Error::PruneCancelled), "{err}");
    assert_eq!(h.env.repo_state(), before);
    h.env.assert_unlocked();
}
