//! Crash recovery. A process that is killed does not get to clean up: every storage call after
//! some point simply never happens. These tests make exactly that happen, at **every** storage
//! call a backup or a prune makes, in both ways a call can fail to report back (it never arrived,
//! or it was carried out and the answer was lost), and then look at what a person would find:
//!
//! * no snapshot is ever visible that is not complete, and every visible snapshot restores to
//!   exactly the files it was made from;
//! * a full check finds no errors (leftovers such as orphaned chunks and a stale lock are
//!   warnings);
//! * the next run of the same command works and the repository is healthy afterwards.
//!
//! A prune that dies holds the exclusive lock; the lock file of a dead process is never removed
//! automatically, because nobody can tell a dead process from a slow one. Removing it is the one
//! manual step (`docs/DISASTER_RECOVERY.md`), and the tests take it the same way.
//!
//! The repository is in memory and copied for every run, so hundreds of crash points cost
//! seconds. `VAULTSNAP_CRASH_STRIDE=<n>` tries every n-th crash point only.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;
use vaultsnap_core::storage::{DirEntryKind, Fault, FlakyBackend, PutMode};
use vaultsnap_core::{
    Backend, BackupOptions, CheckOptions, ChunkerConfig, Error, KdfParams, MemoryBackend,
    Passphrase, PruneMode, PruneOptions, Repository, RepositoryConfig, RepositoryKeys,
    RestoreOptions, RetentionPolicy, SnapshotId, SnapshotRef,
};

type Tree = BTreeMap<String, Vec<u8>>;
type Objects = Vec<(String, Vec<u8>)>;

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
                tree.insert(
                    relative.to_string_lossy().into_owned(),
                    fs::read(&path).expect("read"),
                );
            }
        }
    }
    tree
}

fn stride() -> usize {
    std::env::var("VAULTSNAP_CRASH_STRIDE")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1)
        .max(1)
}

// ---------------------------------------------------------------------------------------------
// A repository that can be copied, so each crash starts from the same state
// ---------------------------------------------------------------------------------------------

/// Every object below `dir`, at any depth, with its bytes.
fn dump(backend: &dyn Backend, dir: &str) -> Objects {
    let mut out = Objects::new();
    for entry in backend.list_dir(dir).expect("list") {
        let name = if dir.is_empty() {
            entry.name
        } else {
            format!("{dir}/{}", entry.name)
        };
        if entry.kind == DirEntryKind::Directory {
            out.extend(dump(backend, &name));
        } else {
            let data = backend.get(&name, 1 << 28).expect("read the object").data;
            out.push((name, data));
        }
    }
    out
}

fn load(objects: &Objects) -> Arc<MemoryBackend> {
    let memory = Arc::new(MemoryBackend::new());
    for (name, data) in objects {
        memory
            .put(name, data, PutMode::Overwrite)
            .expect("load the object");
    }
    memory
}

/// The lock files of processes that died, as an operator would remove them. Returns how many.
fn remove_locks(backend: &dyn Backend) -> usize {
    let names: Vec<String> = backend
        .list_dir("locks")
        .expect("list the locks")
        .into_iter()
        .map(|entry| format!("locks/{}", entry.name))
        .collect();
    for name in &names {
        backend.delete(name).expect("remove the lock file");
    }
    names.len()
}

/// A repository whose storage stops answering after `after` calls: the next call fails the way
/// `variant` says, and so does every call after it.
fn crashing(objects: &Objects, after: u64, variant: Fault) -> (Arc<MemoryBackend>, Repository) {
    let memory = load(objects);
    let faults = std::iter::once(variant).chain(std::iter::repeat_n(Fault::Before, 100_000));
    let flaky = FlakyBackend::scripted(memory.clone(), faults).skipping(after);
    (memory, Repository::with_backend(Arc::new(flaky)))
}

// ---------------------------------------------------------------------------------------------
// A repository with history
// ---------------------------------------------------------------------------------------------

/// The source tree after `round` changes: the first round creates it, later ones rewrite a file
/// and add one.
fn change(src: &Path, round: u64) {
    std::thread::sleep(Duration::from_millis(10));
    if round == 0 {
        write(src, "notes.txt", &b"first line\n".repeat(400));
        write(src, "big/one.bin", &pseudo_random(30_000, 1));
        write(src, "big/two.bin", &pseudo_random(20_000, 2));
        write(src, "empty.bin", b"");
    } else {
        write(src, "big/one.bin", &pseudo_random(30_000, 100 + round));
        write(
            src,
            &format!("added-{round}.bin"),
            &pseudo_random(9_000, 200 + round),
        );
    }
}

fn options(src: &Path) -> BackupOptions {
    BackupOptions {
        sources: vec![src.to_path_buf()],
        chunker: ChunkerConfig::new(1024, 4096, 16_384).expect("valid config"),
        host: "crash-test".to_owned(),
        jobs: 1,
        racy_margin: Duration::ZERO,
        ..BackupOptions::default()
    }
}

struct Base {
    work: TempDir,
    src: PathBuf,
    keys: RepositoryKeys,
    /// The repository as it is before the command that will be killed.
    objects: Objects,
    /// The snapshots in it, oldest first, with the files each was made from.
    history: Vec<(SnapshotId, Tree)>,
}

impl Base {
    /// A repository with `rounds` snapshots of a source that changes in between.
    async fn new(rounds: u64) -> Self {
        let work = tempfile::tempdir().expect("create temporary directory");
        let src = work.path().join("src");
        let memory = Arc::new(MemoryBackend::new());
        let repo = Repository::with_backend(memory.clone());
        let kdf = KdfParams {
            m_cost_kib: 64,
            t_cost: 1,
            p_cost: 1,
        };
        repo.init_with_kdf(&passphrase(), kdf).await.expect("init");
        let keys = repo.unlock(&passphrase()).await.expect("unlock");

        let mut history = Vec::new();
        for round in 0..rounds {
            change(&src, round);
            let tree = read_tree(&src);
            let report = repo
                .backup(&keys, options(&src))
                .await
                .expect("backup succeeds");
            history.push((report.snapshot, tree));
        }
        let objects = dump(memory.as_ref(), "");
        Self {
            work,
            src,
            keys,
            objects,
            history,
        }
    }

    /// Restores `id` and returns the files that came back.
    async fn restore(&self, repo: &Repository, id: &SnapshotId, out: &str, context: &str) -> Tree {
        let options = RestoreOptions::new(SnapshotRef::from(id), self.work.path().join(out));
        let report = repo
            .restore(&self.keys, options)
            .await
            .unwrap_or_else(|err| panic!("{context}: the restore of {id:?} failed: {err}"));
        assert!(report.is_complete(), "{context}: {:?}", report.failed);
        let canonical = fs::canonicalize(&self.src).expect("the source exists");
        read_tree(
            &report
                .target
                .join(canonical.strip_prefix("/").expect("absolute")),
        )
    }

    /// The ids of the snapshots in `repo`, oldest first, which must all be good.
    async fn visible(&self, repo: &Repository, context: &str) -> Vec<SnapshotId> {
        let listing = repo
            .snapshots(&self.keys)
            .await
            .unwrap_or_else(|err| panic!("{context}: listing failed: {err}"));
        assert!(
            listing.damaged.is_empty(),
            "{context}: damaged: {:?}",
            listing.damaged
        );
        listing
            .snapshots
            .iter()
            .map(|info| info.summary.id.clone())
            .collect()
    }

    /// A full check, which must find no errors.
    async fn assert_no_errors(&self, repo: &Repository, context: &str) {
        let options = CheckOptions {
            read_data: true,
            ..CheckOptions::default()
        };
        let report = repo
            .check(&self.keys, options)
            .await
            .unwrap_or_else(|err| panic!("{context}: the check could not run: {err}"));
        assert!(report.is_ok(), "{context}: {:?}", report.findings);
    }
}

// ---------------------------------------------------------------------------------------------
// A backup that is killed
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_backup_killed_at_any_storage_call_leaves_a_repository_that_works() {
    let base = Base::new(1).await;
    let old_id = base.history[0].0.clone();
    let old_tree = base.history[0].1.clone();
    change(&base.src, 1);
    let new_tree = read_tree(&base.src);

    let counting = Arc::new(FlakyBackend::reliable(load(&base.objects)));
    Repository::with_backend(counting.clone())
        .backup(&base.keys, options(&base.src))
        .await
        .expect("an undisturbed backup succeeds");
    let total = counting.calls();
    assert!(
        total > 20,
        "a backup makes more storage calls than {total}?"
    );

    let mut out = 0;
    for point in (0..=total).step_by(stride()) {
        for variant in [Fault::Before, Fault::After] {
            out += 1;
            let context = format!("killed after {point} of {total} calls ({variant:?})");
            let (memory, doomed) = crashing(&base.objects, point, variant);
            let outcome = doomed.backup(&base.keys, options(&base.src)).await;

            // What a person finds afterwards, with the healthy storage and no cleanup.
            let healthy = Repository::with_backend(memory.clone());
            let ids = base.visible(&healthy, &context).await;
            assert!(
                ids.len() == 1 || ids.len() == 2,
                "{context}: {} snapshots",
                ids.len()
            );
            assert_eq!(ids[0], old_id, "{context}: the earlier snapshot is gone");
            if outcome.is_ok() {
                assert_eq!(
                    ids.len(),
                    2,
                    "{context}: a backup that succeeded left no snapshot"
                );
            }
            let first = base
                .restore(&healthy, &ids[0], &format!("a{out}"), &context)
                .await;
            assert!(
                first == old_tree,
                "{context}: the earlier snapshot restores wrongly"
            );
            if let Some(newer) = ids.get(1) {
                let second = base
                    .restore(&healthy, newer, &format!("b{out}"), &context)
                    .await;
                assert!(
                    second == new_tree,
                    "{context}: a visible snapshot is not complete"
                );
            }
            base.assert_no_errors(&healthy, &context).await;

            // And the next backup just works, with no manual step: shared locks do not clash.
            let again = healthy
                .backup(&base.keys, options(&base.src))
                .await
                .unwrap_or_else(|err| panic!("{context}: the next backup failed: {err}"));
            let name = format!("c{out}");
            let restored = base
                .restore(&healthy, &again.snapshot, &name, &context)
                .await;
            assert!(
                restored == new_tree,
                "{context}: the next backup restores wrongly"
            );
            base.assert_no_errors(&healthy, &context).await;
        }
    }
}

#[tokio::test]
async fn a_dead_backups_lock_blocks_prune_until_it_is_removed_by_hand() {
    let base = Base::new(2).await;
    change(&base.src, 2);

    let counting = Arc::new(FlakyBackend::reliable(load(&base.objects)));
    Repository::with_backend(counting.clone())
        .backup(&base.keys, options(&base.src))
        .await
        .expect("an undisturbed backup succeeds");
    let total = counting.calls();

    // The latest crash point that leaves the lock file behind and makes the backup fail. The new
    // snapshot may or may not have been committed by then; either way it is whole or absent.
    let mut found = None;
    for point in (0..=total).rev() {
        let (memory, doomed) = crashing(&base.objects, point, Fault::Before);
        let outcome = doomed.backup(&base.keys, options(&base.src)).await;
        if outcome.is_err() && !memory.list_dir("locks").expect("list").is_empty() {
            found = Some(memory);
            break;
        }
    }
    let memory = found.expect("some crash point leaves a lock file behind");
    let healthy = Repository::with_backend(memory.clone());
    let ids = base.visible(&healthy, "after the crash").await;
    assert!(ids.len() == 2 || ids.len() == 3, "{} snapshots", ids.len());

    let policy = RetentionPolicy {
        keep_last: Some(1),
        ..RetentionPolicy::default()
    };
    let prune = || PruneOptions {
        mode: PruneMode::Unattended,
        ..PruneOptions::new(policy.clone())
    };
    let err = healthy
        .prune(&base.keys, prune())
        .await
        .expect_err("the dead lock blocks prune");
    assert!(matches!(err, Error::Locked { .. }), "{err}");
    assert_eq!(
        base.visible(&healthy, "after the refused prune").await,
        ids,
        "nothing changed"
    );

    assert!(remove_locks(memory.as_ref()) > 0);
    let report = healthy
        .prune(&base.keys, prune())
        .await
        .expect("prune works once the lock is gone");
    assert_eq!(
        report.snapshots_removed,
        u64::try_from(ids.len() - 1).expect("small")
    );
    base.assert_no_errors(&healthy, "after the prune").await;
    assert_eq!(base.visible(&healthy, "after the prune").await.len(), 1);
}

// ---------------------------------------------------------------------------------------------
// A lost `config`
// ---------------------------------------------------------------------------------------------

/// The `config` file is the one object a person can lose that the passphrase cannot make up for:
/// key files are authenticated together with the repository id, which is written nowhere else in
/// the clear. The id is not secret (`init` prints it), so a recorded id is enough to rewrite the
/// file by hand, as `docs/DISASTER_RECOVERY.md` describes.
#[tokio::test]
async fn a_lost_config_file_is_rewritten_from_the_repository_id() {
    let base = Base::new(1).await;
    let memory = load(&base.objects);
    let healthy = Repository::with_backend(memory.clone());
    let original = memory.get("config", 4096).expect("read the config").data;
    let config = RepositoryConfig::parse(&original).expect("a valid config");

    memory.delete("config").expect("delete the config");
    assert!(
        healthy.unlock(&passphrase()).await.is_err(),
        "no config, no repository"
    );

    // A made-up id does not unlock anything: the key file is bound to the real one.
    let invented = RepositoryConfig::new("0123456789abcdef0123456789abcdef".to_owned(), 0);
    memory
        .put("config", &invented.to_bytes(), PutMode::Create)
        .expect("write a config");
    assert!(
        healthy.unlock(&passphrase()).await.is_err(),
        "a different id must not unlock"
    );

    // The real id does, whatever creation time is written: it is not authenticated.
    let rewritten = RepositoryConfig::new(config.repository_id.clone(), 0);
    memory
        .put("config", &rewritten.to_bytes(), PutMode::Overwrite)
        .expect("rewrite the config");
    healthy
        .unlock(&passphrase())
        .await
        .expect("the repository unlocks again");

    let (id, expected) = &base.history[0];
    let restored = base
        .restore(&healthy, id, "rewritten", "after rewriting the config")
        .await;
    assert!(restored == *expected, "the snapshot restores wrongly");
    base.assert_no_errors(&healthy, "after rewriting the config")
        .await;
}

// ---------------------------------------------------------------------------------------------
// A prune that is killed
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_prune_killed_at_any_storage_call_never_costs_a_kept_snapshot() {
    let base = Base::new(3).await;
    let newest = base.history[2].0.clone();
    let policy = RetentionPolicy {
        keep_last: Some(1),
        ..RetentionPolicy::default()
    };
    let prune = || PruneOptions {
        mode: PruneMode::Unattended,
        ..PruneOptions::new(policy.clone())
    };

    let counting = Arc::new(FlakyBackend::reliable(load(&base.objects)));
    let undisturbed = Repository::with_backend(counting.clone())
        .prune(&base.keys, prune())
        .await
        .expect("an undisturbed prune succeeds");
    assert_eq!(undisturbed.snapshots_removed, 2);
    assert!(
        undisturbed.chunks_removed > 0,
        "the removed snapshots had chunks of their own"
    );
    let total = counting.calls();

    let mut out = 0;
    for point in (0..=total).step_by(stride()) {
        for variant in [Fault::Before, Fault::After] {
            out += 1;
            let context = format!("killed after {point} of {total} calls ({variant:?})");
            let (memory, doomed) = crashing(&base.objects, point, variant);
            let outcome = doomed.prune(&base.keys, prune()).await;

            // The one manual step: the dead process's lock.
            let healthy = Repository::with_backend(memory.clone());
            remove_locks(memory.as_ref());

            let ids = base.visible(&healthy, &context).await;
            assert!(
                ids.contains(&newest),
                "{context}: the newest snapshot was removed"
            );
            if outcome.is_ok() {
                assert_eq!(
                    ids,
                    std::slice::from_ref(&newest),
                    "{context}: a prune that succeeded left more"
                );
            }
            // Whatever is left is a suffix of the history, and every part of it restores.
            for (position, id) in ids.iter().enumerate() {
                let (_, expected) = base
                    .history
                    .iter()
                    .find(|(known, _)| known == id)
                    .unwrap_or_else(|| panic!("{context}: an unknown snapshot appeared"));
                let got = base
                    .restore(&healthy, id, &format!("p{out}-{position}"), &context)
                    .await;
                assert!(
                    got == *expected,
                    "{context}: a kept snapshot restores wrongly"
                );
            }
            base.assert_no_errors(&healthy, &context).await;

            // Running the prune again finishes the job.
            let report = healthy
                .prune(&base.keys, prune())
                .await
                .unwrap_or_else(|err| panic!("{context}: the next prune failed: {err}"));
            assert!(report.is_complete(), "{context}: {:?}", report.failures);
            assert_eq!(
                base.visible(&healthy, &context).await,
                std::slice::from_ref(&newest)
            );
            let (_, expected) = &base.history[2];
            let got = base
                .restore(&healthy, &newest, &format!("q{out}"), &context)
                .await;
            assert!(
                got == *expected,
                "{context}: the newest snapshot restores wrongly"
            );
            base.assert_no_errors(&healthy, &context).await;
        }
    }
}
