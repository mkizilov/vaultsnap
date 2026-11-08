//! Integration tests of the incremental backup: repeated runs, modified, inserted-into, deleted,
//! and added files, interruptions and failures, partial backups, concurrency, and a check that the
//! repository holds enough to reconstruct every completed snapshot byte for byte.
//!
//! Restoring is not implemented, so "reconstruct" here reads each file's chunk list from the
//! manifest and fetches the chunks from the chunk store, exactly what a restore will do.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tempfile::TempDir;
use vaultsnap_core::snapshot::NodeKind;
use vaultsnap_core::{
    BackupEvent, BackupOptions, BackupReport, ChunkStore, ChunkerConfig, Error, KdfParams,
    Manifest, ParentPolicy, Passphrase, Repository, RepositoryKeys, SnapshotId, SnapshotStore,
};

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

// ---------------------------------------------------------------------------------------------
// Test environment
// ---------------------------------------------------------------------------------------------

struct Env {
    dir: TempDir,
    repo: Repository,
    keys: RepositoryKeys,
    chunks: ChunkStore,
    snapshots: SnapshotStore,
}

impl Env {
    async fn new() -> Self {
        Self::with_repository_at("repo").await
    }

    async fn with_repository_at(relative: &str) -> Self {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let path = dir.path().join(relative);
        fs::create_dir_all(path.parent().expect("has a parent")).expect("create parents");
        let repo = Repository::at(path);
        let kdf = KdfParams {
            m_cost_kib: 64,
            t_cost: 1,
            p_cost: 1,
        };
        repo.init_with_kdf(&passphrase(), kdf).await.expect("init");
        let keys = repo.unlock(&passphrase()).await.expect("unlock");
        Self {
            chunks: repo.chunk_store(&keys).expect("chunk store"),
            snapshots: repo.snapshot_store(&keys).await.expect("snapshot store"),
            dir,
            repo,
            keys,
        }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.dir.path().join(relative)
    }

    /// Small chunks so that modest files have many; no safety margin because the test
    /// filesystem has fine-grained timestamps; two workers.
    fn options(&self, sources: &[&str]) -> BackupOptions {
        BackupOptions {
            sources: sources.iter().map(|source| self.path(source)).collect(),
            chunker: ChunkerConfig::new(1024, 4096, 16_384).expect("valid config"),
            host: "test-host".to_owned(),
            jobs: 2,
            racy_margin: Duration::ZERO,
            ..BackupOptions::default()
        }
    }

    async fn backup(&self, options: BackupOptions) -> BackupReport {
        self.repo.backup(&self.keys, options).await.expect("backup succeeds")
    }

    fn load(&self, id: &SnapshotId) -> Manifest {
        self.snapshots.load(id).expect("snapshot loads")
    }

    fn snapshot_count(&self) -> usize {
        self.snapshots.ids().expect("ids").len()
    }

    fn lock_files(&self) -> usize {
        fs::read_dir(self.path("repo/locks")).expect("read locks").count()
    }

    fn object_count(&self) -> usize {
        count_files(&self.path("repo/data"))
    }
}

fn count_files(dir: &Path) -> usize {
    let mut count = 0;
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).expect("read_dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                pending.push(path);
            } else {
                count += 1;
            }
        }
    }
    count
}

// ---------------------------------------------------------------------------------------------
// Reconstruction: what a restore will do, compared with what was on disk at backup time
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Item {
    Dir,
    File(Vec<u8>),
    Link(String),
}

type Tree = BTreeMap<String, Item>;

/// The tree under `root` as it is on disk right now, skipping `skip` (a repository kept inside).
fn capture(root: &Path, skip: Option<&Path>) -> Tree {
    fn walk(base: &Path, path: &Path, skip: Option<&Path>, out: &mut Tree) {
        if skip == Some(path) {
            return;
        }
        let relative = path.strip_prefix(base).expect("inside").to_string_lossy().into_owned();
        let meta = fs::symlink_metadata(path).expect("stat");
        if meta.is_dir() {
            out.insert(relative, Item::Dir);
            let mut names: Vec<_> = fs::read_dir(path)
                .expect("read_dir")
                .map(|entry| entry.expect("entry").file_name())
                .collect();
            names.sort();
            for name in names {
                walk(base, &path.join(name), skip, out);
            }
        } else if meta.file_type().is_symlink() {
            let target = fs::read_link(path).expect("read_link");
            out.insert(relative, Item::Link(target.to_string_lossy().into_owned()));
        } else {
            out.insert(relative, Item::File(fs::read(path).expect("read file")));
        }
    }
    let mut out = Tree::new();
    walk(root, root, skip, &mut out);
    out
}

/// The content of one file entry, reassembled from its ordered chunk list.
fn file_content(env: &Env, chunks: &[vaultsnap_core::snapshot::ChunkRef], size: u64) -> Vec<u8> {
    let mut data = Vec::new();
    for chunk in chunks {
        let bytes = env.chunks.get(&chunk.id).expect("every referenced chunk is stored");
        assert_eq!(bytes.len(), chunk.len as usize, "recorded chunk length");
        data.extend_from_slice(&bytes);
    }
    assert_eq!(data.len() as u64, size, "recorded file size");
    data
}

/// Every root of snapshot `id`, rebuilt purely from the repository.
fn reconstruct(env: &Env, id: &SnapshotId) -> Vec<Tree> {
    let manifest = env.load(id);
    manifest
        .roots
        .iter()
        .map(|root| {
            let mut tree = Tree::new();
            for (index, entry) in root.entries.iter().enumerate() {
                let relative = String::from_utf8_lossy(&root.relative_path(index)).into_owned();
                let item = match &entry.kind {
                    NodeKind::Directory => Item::Dir,
                    NodeKind::File { chunks } => {
                        Item::File(file_content(env, chunks, entry.attributes.size))
                    }
                    NodeKind::Symlink { target } => {
                        Item::Link(String::from_utf8_lossy(target).into_owned())
                    }
                    NodeKind::HardLink { target } => {
                        let NodeKind::File { chunks } = &root.entries[*target as usize].kind else {
                            panic!("a hard link must name a file");
                        };
                        Item::File(file_content(env, chunks, entry.attributes.size))
                    }
                    other => panic!("unexpected entry kind {other:?}"),
                };
                tree.insert(relative, item);
            }
            tree
        })
        .collect()
}

fn file_count(tree: &Tree) -> u64 {
    tree.values().filter(|item| matches!(item, Item::File(_))).count() as u64
}

fn sample_tree(root: &Path) {
    write(root, "readme.txt", b"hello backup");
    write(root, "empty.bin", b"");
    write(root, "docs/report.txt", &b"quarterly numbers\n".repeat(3000));
    write(root, "docs/archive/data.bin", &pseudo_random(150_000, 1));
    write(root, "docs/archive/notes", b"x");
    write(root, "unicode/日本語 ファイル.txt", "こんにちは".as_bytes());
    write(root, "with space/new\nline", b"odd names");
    fs::create_dir(root.join("empty-dir")).expect("mkdir");
}

// ---------------------------------------------------------------------------------------------
// Repeated, incremental backups
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_first_backup_stores_everything_and_can_be_reconstructed() {
    let env = Env::new().await;
    sample_tree(&env.path("src"));
    let expected = capture(&env.path("src"), None);

    let report = env.backup(env.options(&["src"])).await;
    assert!(!report.is_partial());
    assert_eq!(report.parents, [None]);
    assert_eq!(report.transfer.files_read, file_count(&expected));
    assert_eq!(report.transfer.files_unchanged, 0);
    assert!(report.transfer.chunks_new > 20, "{:?}", report.transfer);
    assert!(report.transfer.bytes_stored > 0);
    assert_eq!(report.totals.files, file_count(&expected));
    assert_eq!(report.sources.len(), 1);

    assert_eq!(env.snapshot_count(), 1);
    assert_eq!(reconstruct(&env, &report.snapshot), [expected]);
    let manifest = env.load(&report.snapshot);
    assert_eq!(manifest.host, "test-host");
    assert_eq!(manifest.created, report.started);
    assert_eq!(manifest.config.chunker, ChunkerConfig::new(1024, 4096, 16_384).expect("valid"));
    assert_eq!(env.lock_files(), 0, "the lock was released");
}

#[tokio::test]
async fn repeating_a_backup_of_an_unchanged_tree_reads_nothing_and_stores_nothing() {
    let env = Env::new().await;
    sample_tree(&env.path("src"));
    let expected = capture(&env.path("src"), None);

    let first = env.backup(env.options(&["src"])).await;
    let objects = env.object_count();
    let second = env.backup(env.options(&["src"])).await;

    assert_ne!(first.snapshot, second.snapshot);
    assert_eq!(second.parents, [Some(first.snapshot.clone())]);
    assert_eq!(second.transfer.files_read, 0, "{:?}", second.transfer);
    assert_eq!(second.transfer.bytes_read, 0);
    assert_eq!(second.transfer.files_unchanged, file_count(&expected));
    assert_eq!(second.transfer.chunks_new, 0);
    assert_eq!(second.transfer.bytes_stored, 0);
    assert!(second.transfer.bytes_unchanged > 150_000);
    assert_eq!(env.object_count(), objects, "no new chunk objects");

    // Two snapshots, both complete, identical in content.
    assert_eq!(env.snapshot_count(), 2);
    assert_eq!(reconstruct(&env, &first.snapshot), [expected.clone()]);
    assert_eq!(reconstruct(&env, &second.snapshot), [expected]);
    let (a, b) = (env.load(&first.snapshot), env.load(&second.snapshot));
    assert_eq!(a.roots[0].entries.len(), b.roots[0].entries.len());
}

#[tokio::test]
async fn modified_inserted_deleted_and_added_files_are_handled_and_history_is_kept() {
    let env = Env::new().await;
    let src = env.path("src");
    write(&src, "big.bin", &pseudo_random(400_000, 7));
    write(&src, "notes.txt", b"first version of the notes\n");
    write(&src, "keep.txt", &pseudo_random(20_000, 8));
    write(&src, "dir/old.txt", b"this file will be deleted");
    write(&src, "dir/same.txt", b"unchanged");
    let before = capture(&src, None);
    let first = env.backup(env.options(&["src"])).await;
    let big_chunks = match &env.load(&first.snapshot).roots[0].entries.iter().find(|e| e.name == b"big.bin").expect("big.bin").kind {
        NodeKind::File { chunks } => chunks.len(),
        _ => panic!("not a file"),
    };
    assert!(big_chunks > 40);

    // Modify (append), insert into the middle, delete, add.
    let mut notes = fs::read(src.join("notes.txt")).expect("read");
    notes.extend_from_slice(b"a second line\n");
    fs::write(src.join("notes.txt"), notes).expect("write");
    let mut big = fs::read(src.join("big.bin")).expect("read");
    big.splice(200_000..200_000, b"<<< inserted in the middle of the file >>>".iter().copied());
    fs::write(src.join("big.bin"), big).expect("write");
    fs::remove_file(src.join("dir/old.txt")).expect("remove");
    write(&src, "dir/new.txt", b"a brand new file");
    let after = capture(&src, None);
    assert_ne!(before, after);

    let second = env.backup(env.options(&["src"])).await;
    assert_eq!(second.parents, [Some(first.snapshot.clone())]);
    // Only the three touched files are read; keep.txt and same.txt are reused.
    assert_eq!(second.transfer.files_read, 3, "{:?}", second.transfer);
    assert_eq!(second.transfer.files_unchanged, 2, "{:?}", second.transfer);
    // The insertion shifts the data but content-defined chunking re-synchronizes: nearly every
    // chunk of the big file already exists.
    assert!(second.transfer.chunks_new <= 8, "{:?}", second.transfer);
    assert!(second.transfer.chunks_existing as usize + 8 >= big_chunks, "{:?}", second.transfer);

    assert_eq!(reconstruct(&env, &first.snapshot), [before], "history is intact");
    assert_eq!(reconstruct(&env, &second.snapshot), [after.clone()]);
    assert!(!after.contains_key("dir/old.txt") && after.contains_key("dir/new.txt"));
}

#[tokio::test]
async fn a_rewrite_with_the_same_size_and_a_restored_modification_time_is_still_noticed() {
    use std::os::unix::fs::MetadataExt;

    let env = Env::new().await;
    let src = env.path("src");
    write(&src, "victim.bin", &vec![b'a'; 8192]);
    write(&src, "other.bin", &pseudo_random(8192, 3));
    let original = fs::metadata(src.join("victim.bin")).expect("stat");
    let first = env.backup(env.options(&["src"])).await;

    // Let the filesystem clock tick, then rewrite the content with the same size and put the old
    // modification time back. Only the inode change time reveals this.
    std::thread::sleep(Duration::from_millis(50));
    fs::write(src.join("victim.bin"), vec![b'b'; 8192]).expect("rewrite");
    let file = fs::OpenOptions::new().write(true).open(src.join("victim.bin")).expect("open");
    file.set_modified(original.modified().expect("mtime")).expect("restore mtime");
    drop(file);
    let rewritten = fs::metadata(src.join("victim.bin")).expect("stat");
    assert_eq!(rewritten.mtime(), original.mtime(), "the test really restored the mtime");

    let second = env.backup(env.options(&["src"])).await;
    assert_eq!(second.transfer.files_read, 1, "{:?}", second.transfer);
    assert_eq!(second.transfer.files_unchanged, 1);
    let tree = reconstruct(&env, &second.snapshot);
    assert_eq!(tree[0]["victim.bin"], Item::File(vec![b'b'; 8192]));
    let old = reconstruct(&env, &first.snapshot);
    assert_eq!(old[0]["victim.bin"], Item::File(vec![b'a'; 8192]));
}

#[tokio::test]
async fn unchanged_files_are_read_again_when_their_chunks_have_gone_missing() {
    let env = Env::new().await;
    let src = env.path("src");
    write(&src, "damaged.bin", &pseudo_random(30_000, 11));
    write(&src, "fine.bin", &pseudo_random(30_000, 12));
    let first = env.backup(env.options(&["src"])).await;

    let manifest = env.load(&first.snapshot);
    let entry = manifest.roots[0].entries.iter().find(|e| e.name == b"damaged.bin").expect("entry");
    let NodeKind::File { chunks } = &entry.kind else {
        panic!("not a file");
    };
    let hex = chunks[2].id.to_hex();
    fs::remove_file(env.path("repo/data").join(&hex[..2]).join(&hex)).expect("remove a chunk object");

    let second = env.backup(env.options(&["src"])).await;
    assert_eq!(second.transfer.files_read, 1, "only the damaged file is read again");
    assert_eq!(second.transfer.files_unchanged, 1);
    assert_eq!(second.transfer.chunks_new, 1, "just the missing chunk is stored again");
    assert_eq!(reconstruct(&env, &second.snapshot), [capture(&src, None)]);
}

#[tokio::test]
async fn the_default_safety_margin_distrusts_freshly_written_files() {
    let env = Env::new().await;
    sample_tree(&env.path("src"));
    let mut options = env.options(&["src"]);
    options.racy_margin = Duration::from_secs(2);
    env.backup(options.clone()).await;
    let second = env.backup(options).await;
    // Everything was written moments before the first backup began, well inside the margin, so
    // nothing may be assumed unchanged.
    assert_eq!(second.transfer.files_unchanged, 0, "{:?}", second.transfer);
    assert!(second.transfer.files_read > 0);
    assert_eq!(second.transfer.chunks_new, 0, "everything is deduplicated though");
}

#[tokio::test]
async fn the_parent_policy_selects_what_a_backup_is_compared_against() {
    let env = Env::new().await;
    sample_tree(&env.path("src"));
    let first = env.backup(env.options(&["src"])).await;
    let second = env.backup(env.options(&["src"])).await;

    let mut options = env.options(&["src"]);
    options.parent = ParentPolicy::Disabled;
    let disabled = env.backup(options).await;
    assert_eq!(disabled.parents, [None]);
    assert_eq!(disabled.transfer.files_unchanged, 0);
    assert_eq!(disabled.transfer.chunks_new, 0);

    let mut options = env.options(&["src"]);
    options.parent = ParentPolicy::Snapshot(first.snapshot.clone());
    let explicit = env.backup(options).await;
    assert_eq!(explicit.parents, [Some(first.snapshot.clone())]);
    assert_eq!(explicit.transfer.files_read, 0);

    let mut options = env.options(&["src"]);
    options.parent = ParentPolicy::Snapshot(SnapshotId::generate().expect("randomness"));
    let err = env.repo.backup(&env.keys, options).await.expect_err("unknown parent");
    assert!(matches!(err, Error::SnapshotNotFound(_)), "{err}");
    assert_eq!(env.snapshot_count(), 4, "{} {}", first.snapshot, second.snapshot);
}

#[tokio::test]
async fn several_sources_become_several_roots_of_one_consistent_snapshot() {
    let env = Env::new().await;
    write(&env.path("one"), "a/file.txt", b"in the first source");
    write(&env.path("two"), "b/file.txt", b"in the second source");
    write(&env.path("three.txt"), "", b"");
    fs::write(env.path("three.txt"), b"a single file as a source").expect("write");

    let first = env.backup(env.options(&["one", "two", "three.txt"])).await;
    let manifest = env.load(&first.snapshot);
    assert_eq!(manifest.roots.len(), 3);
    assert_eq!(first.sources.len(), 3);
    let trees = reconstruct(&env, &first.snapshot);
    assert_eq!(trees[0], capture(&env.path("one"), None));
    assert_eq!(trees[1], capture(&env.path("two"), None));
    assert_eq!(trees[2][""], Item::File(b"a single file as a source".to_vec()));
    assert_eq!(first.totals.files, 3);

    // Roots are compared with their own earlier versions.
    write(&env.path("two"), "b/extra.txt", b"new");
    let second = env.backup(env.options(&["one", "two", "three.txt"])).await;
    assert_eq!(second.parents, vec![Some(first.snapshot.clone()); 3]);
    assert_eq!(second.transfer.files_read, 1);
    assert_eq!(second.transfer.files_unchanged, 3);

    // A different set of sources only finds parents for the roots it shares.
    let third = env.backup(env.options(&["two", "one"])).await;
    assert_eq!(third.parents, vec![Some(second.snapshot.clone()); 2]);
    assert_eq!(env.load(&third.snapshot).roots[0].source, env.load(&second.snapshot).roots[1].source);
}

#[tokio::test]
async fn patterns_are_applied_and_recorded_in_the_snapshot() {
    let env = Env::new().await;
    let src = env.path("src");
    write(&src, "keep.txt", b"keep");
    write(&src, "drop.tmp", b"scratch");
    write(&src, "cache/data", b"cached");
    write(&src, "sub/also.tmp", b"scratch");
    let mut options = env.options(&["src"]);
    options.scan.excludes = vec!["*.tmp".to_owned(), "cache/".to_owned()];
    let report = env.backup(options).await;
    let tree = &reconstruct(&env, &report.snapshot)[0];
    let names: Vec<&str> = tree.keys().map(String::as_str).collect();
    assert_eq!(names, ["", "keep.txt", "sub"]);
    let manifest = env.load(&report.snapshot);
    assert_eq!(manifest.config.excludes, ["*.tmp", "cache/"]);
}

#[tokio::test]
async fn bad_sources_fail_cleanly_before_anything_is_stored() {
    let env = Env::new().await;
    let missing = env.options(&["does-not-exist"]);
    let err = env.repo.backup(&env.keys, missing).await.expect_err("must fail");
    assert!(matches!(err, Error::Io { .. }), "{err}");

    sample_tree(&env.path("src"));
    let twice = env.options(&["src", "src"]);
    assert!(matches!(env.repo.backup(&env.keys, twice).await, Err(Error::InvalidArgument(_))));

    assert_eq!(env.snapshot_count(), 0);
    assert_eq!(env.object_count(), 0);
    assert_eq!(env.lock_files(), 0);
}

#[tokio::test]
async fn the_repository_is_never_backed_up_into_itself() {
    let env = Env::with_repository_at("src/backup-repo").await;
    let src = env.path("src");
    write(&src, "file.txt", b"payload");
    write(&src, "dir/inner.txt", b"more");

    let first = env.backup(BackupOptions {
        sources: vec![src.clone()],
        ..env.options(&[])
    })
    .await;
    let second = env.backup(BackupOptions {
        sources: vec![src.clone()],
        ..env.options(&[])
    })
    .await;
    for report in [&first, &second] {
        let tree = &reconstruct(&env, &report.snapshot)[0];
        assert!(tree.keys().all(|path| !path.contains("backup-repo")), "{:?}", tree.keys());
        assert_eq!(tree.len(), 4, "{:?}", tree.keys());
    }
    assert_eq!(second.transfer.files_read, 0, "the growing repository did not disturb the parent");
}

// ---------------------------------------------------------------------------------------------
// Progress, concurrency
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn progress_events_account_for_everything() {
    let env = Env::new().await;
    sample_tree(&env.path("src"));
    write(&env.path("other"), "x.txt", b"x");
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let bytes = Arc::new(AtomicUsize::new(0));
    let committing = Arc::new(AtomicUsize::new(0));
    let (log_in, bytes_in, committing_in) = (Arc::clone(&log), Arc::clone(&bytes), Arc::clone(&committing));
    let mut options = env.options(&["src", "other"]);
    options.observer = Some(Arc::new(move |event: &BackupEvent<'_>| match event {
        BackupEvent::SourceStarted { source, .. } => {
            log_in.lock().expect("lock").push(format!("source {source}"));
        }
        BackupEvent::FileFinished { path, result, .. } => {
            log_in.lock().expect("lock").push(format!("done {}", path.display()));
            assert!(!result.unchanged);
        }
        BackupEvent::BytesRead { bytes } => {
            bytes_in.fetch_add(*bytes as usize, Ordering::SeqCst);
        }
        BackupEvent::Committing => {
            committing_in.fetch_add(1, Ordering::SeqCst);
        }
        BackupEvent::FileStarted { .. } | BackupEvent::Skipped { .. } => {}
    }));
    let report = env.backup(options).await;

    assert_eq!(bytes.load(Ordering::SeqCst) as u64, report.transfer.bytes_read);
    assert_eq!(committing.load(Ordering::SeqCst), 1);
    let log = log.lock().expect("lock");
    assert_eq!(log.iter().filter(|line| line.starts_with("source")).count(), 2);
    assert_eq!(log.iter().filter(|line| line.starts_with("done")).count() as u64, report.transfer.files_read);
    assert!(log.contains(&"done docs/archive/data.bin".to_owned()), "{log:?}");
}

#[tokio::test]
async fn concurrency_is_bounded_and_does_not_change_the_result() {
    let env = Env::new().await;
    let src = env.path("src");
    for i in 0..40 {
        write(&src, &format!("dir{}/file{i:02}.bin", i % 5), &pseudo_random(20_000 + i * 100, i as u64));
    }

    let run = |jobs: usize| {
        let in_flight = Arc::new(AtomicIsize::new(0));
        let peak = Arc::new(AtomicIsize::new(0));
        let (flight, high) = (Arc::clone(&in_flight), Arc::clone(&peak));
        let mut options = env.options(&["src"]);
        options.jobs = jobs;
        options.parent = ParentPolicy::Disabled;
        options.observer = Some(Arc::new(move |event: &BackupEvent<'_>| match event {
            BackupEvent::FileStarted { .. } => {
                let now = flight.fetch_add(1, Ordering::SeqCst) + 1;
                high.fetch_max(now, Ordering::SeqCst);
            }
            BackupEvent::FileFinished { .. } => {
                flight.fetch_sub(1, Ordering::SeqCst);
            }
            _ => {}
        }));
        (options, peak)
    };

    let (options, peak) = run(1);
    let serial = env.backup(options).await;
    assert_eq!(peak.load(Ordering::SeqCst), 1);
    let (options, peak) = run(3);
    let parallel = env.backup(options).await;
    assert!(peak.load(Ordering::SeqCst) <= 3, "peak {}", peak.load(Ordering::SeqCst));

    let shape = |id: &SnapshotId| -> Vec<(Vec<u8>, NodeKind)> {
        env.load(id).roots[0].entries.iter().map(|e| (e.name.clone(), e.kind.clone())).collect()
    };
    assert_eq!(shape(&serial.snapshot), shape(&parallel.snapshot), "same manifest whatever the scheduling");
    assert_eq!(parallel.transfer.chunks_new, 0);
    assert_eq!(reconstruct(&env, &parallel.snapshot), [capture(&src, None)]);
}

#[tokio::test]
async fn simultaneous_backups_share_the_repository_safely() {
    let env = Env::new().await;
    let shared = pseudo_random(60_000, 99);
    write(&env.path("a"), "shared.bin", &shared);
    write(&env.path("a"), "only-a.bin", &pseudo_random(30_000, 100));
    write(&env.path("b"), "shared.bin", &shared);
    write(&env.path("b"), "only-b.bin", &pseudo_random(30_000, 101));

    let (first, second) = tokio::join!(
        env.repo.backup(&env.keys, env.options(&["a"])),
        env.repo.backup(&env.keys, env.options(&["b"])),
    );
    let (first, second) = (first.expect("first backup"), second.expect("second backup"));
    assert_eq!(env.snapshot_count(), 2);
    assert_eq!(env.lock_files(), 0);
    assert_eq!(reconstruct(&env, &first.snapshot), [capture(&env.path("a"), None)]);
    assert_eq!(reconstruct(&env, &second.snapshot), [capture(&env.path("b"), None)]);
    // The shared file's chunks exist exactly once.
    let unique: usize = {
        let mut ids = std::collections::HashSet::new();
        for id in [&first.snapshot, &second.snapshot] {
            for root in &env.load(id).roots {
                for entry in &root.entries {
                    if let NodeKind::File { chunks } = &entry.kind {
                        ids.extend(chunks.iter().map(|c| c.id));
                    }
                }
            }
        }
        ids.len()
    };
    assert_eq!(env.object_count(), unique);
}

// ---------------------------------------------------------------------------------------------
// Interruptions and failures: nothing uncommitted is ever visible
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn cancelling_midway_leaves_nothing_visible_and_a_rerun_reuses_the_stored_chunks() {
    let env = Env::new().await;
    let src = env.path("src");
    for i in 0..20 {
        write(&src, &format!("f{i:02}.bin"), &pseudo_random(30_000, 200 + i as u64));
    }
    let expected = capture(&src, None);

    let mut options = env.options(&["src"]);
    options.jobs = 1;
    let cancel = options.cancel.clone();
    let finished = Arc::new(AtomicUsize::new(0));
    options.observer = Some(Arc::new(move |event: &BackupEvent<'_>| {
        if matches!(event, BackupEvent::FileFinished { .. }) && finished.fetch_add(1, Ordering::SeqCst) + 1 == 3 {
            cancel.cancel();
        }
    }));
    let err = env.repo.backup(&env.keys, options).await.expect_err("cancelled");
    assert!(matches!(err, Error::Cancelled), "{err}");

    assert_eq!(env.snapshot_count(), 0, "no snapshot is visible");
    assert!(env.snapshots.list().expect("list").is_empty());
    assert_eq!(env.lock_files(), 0, "the lock was released");
    let stored = env.object_count();
    assert!(stored >= 3 * 5, "chunks of the finished files stay: {stored}");

    let report = env.backup(env.options(&["src"])).await;
    assert_eq!(report.parents, [None], "an interrupted run is not a parent");
    assert!(report.transfer.chunks_existing as usize >= stored, "{:?} vs {stored}", report.transfer);
    assert_eq!(reconstruct(&env, &report.snapshot), [expected]);
}

#[tokio::test]
async fn cancelling_never_damages_snapshots_that_were_already_complete() {
    let env = Env::new().await;
    let src = env.path("src");
    for i in 0..10 {
        write(&src, &format!("f{i}.bin"), &pseudo_random(20_000, 300 + i as u64));
    }
    let before = capture(&src, None);
    let complete = env.backup(env.options(&["src"])).await;

    write(&src, "f3.bin", &pseudo_random(25_000, 999));
    write(&src, "g.bin", &pseudo_random(25_000, 998));
    let mut options = env.options(&["src"]);
    options.jobs = 1;
    let cancel = options.cancel.clone();
    options.observer = Some(Arc::new(move |event: &BackupEvent<'_>| {
        if matches!(event, BackupEvent::FileStarted { .. }) {
            cancel.cancel();
        }
    }));
    let err = env.repo.backup(&env.keys, options).await.expect_err("cancelled");
    assert!(matches!(err, Error::Cancelled), "{err}");

    assert_eq!(env.snapshot_count(), 1);
    assert_eq!(reconstruct(&env, &complete.snapshot), [before], "the earlier snapshot is intact");
}

#[tokio::test]
async fn a_failure_midway_leaves_nothing_visible_and_the_next_run_succeeds() {
    let env = Env::new().await;
    let src = env.path("src");
    for i in 0..12 {
        write(&src, &format!("f{i:02}.bin"), &pseudo_random(20_000, 400 + i as u64));
    }

    // The storage disappears under the running backup (think: a disk that fills up or is
    // unmounted). The backup must fail as a whole.
    let mut options = env.options(&["src"]);
    options.jobs = 1;
    let data_dir = env.path("repo/data");
    let removed = Arc::new(AtomicBool::new(false));
    options.observer = Some(Arc::new(move |event: &BackupEvent<'_>| {
        if matches!(event, BackupEvent::FileFinished { .. }) && !removed.swap(true, Ordering::SeqCst) {
            fs::remove_dir_all(&data_dir).expect("remove the chunk storage");
        }
    }));
    let err = env.repo.backup(&env.keys, options).await.expect_err("the backup must fail");
    assert!(matches!(err, Error::Io { .. }), "{err}");

    assert_eq!(env.snapshot_count(), 0, "a failed backup leaves no snapshot");
    assert_eq!(env.lock_files(), 0, "the lock was released");

    fs::create_dir(env.path("repo/data")).expect("repair the storage");
    let report = env.backup(env.options(&["src"])).await;
    assert_eq!(reconstruct(&env, &report.snapshot), [capture(&src, None)]);
}

#[tokio::test]
async fn a_file_that_changes_once_while_being_read_is_retried_and_captured_consistently() {
    let env = Env::new().await;
    let src = env.path("src");
    write(&src, "growing.bin", &pseudo_random(100_000, 500));
    write(&src, "steady.bin", &pseudo_random(10_000, 501));

    let target = src.join("growing.bin");
    let armed = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let mut options = env.options(&["src"]);
    options.jobs = 1;
    options.observer = Some(Arc::new(move |event: &BackupEvent<'_>| match event {
        BackupEvent::FileStarted { path, .. } if *path == Path::new("growing.bin") => {
            armed.store(true, Ordering::SeqCst);
        }
        BackupEvent::BytesRead { .. } if armed.load(Ordering::SeqCst) && !done.swap(true, Ordering::SeqCst) => {
            let mut file = fs::OpenOptions::new().append(true).open(&target).expect("open");
            std::io::Write::write_all(&mut file, b"APPENDED WHILE BEING BACKED UP").expect("append");
        }
        _ => {}
    }));
    let report = env.backup(options).await;
    assert!(!report.is_partial(), "{:?}", report.skipped);
    let tree = &reconstruct(&env, &report.snapshot)[0];
    let content = &tree["growing.bin"];
    assert_eq!(*content, Item::File(fs::read(src.join("growing.bin")).expect("read")));
    match content {
        Item::File(bytes) => assert!(bytes.ends_with(b"APPENDED WHILE BEING BACKED UP")),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_file_that_keeps_changing_is_given_up_on_and_reported() {
    let env = Env::new().await;
    let src = env.path("src");
    write(&src, "restless.bin", &pseudo_random(100_000, 600));
    write(&src, "steady.bin", &pseudo_random(10_000, 601));

    let target = src.join("restless.bin");
    let armed = Arc::new(AtomicBool::new(false));
    let mut options = env.options(&["src"]);
    options.jobs = 1;
    options.observer = Some(Arc::new(move |event: &BackupEvent<'_>| match event {
        BackupEvent::FileStarted { path, .. } if *path == Path::new("restless.bin") => {
            armed.store(true, Ordering::SeqCst);
        }
        BackupEvent::BytesRead { .. } if armed.load(Ordering::SeqCst) => {
            let mut file = fs::OpenOptions::new().append(true).open(&target).expect("open");
            std::io::Write::write_all(&mut file, b"more").expect("append");
        }
        _ => {}
    }));
    let report = env.backup(options.clone()).await;

    assert!(report.is_partial());
    assert_eq!(report.skipped.len(), 1, "{:?}", report.skipped);
    assert_eq!(report.skipped[0].path, "restless.bin");
    assert!(report.skipped[0].failure);
    assert!(report.skipped[0].reason.contains("kept changing"), "{}", report.skipped[0].reason);
    assert_eq!(report.totals.skipped, 1);
    let tree = &reconstruct(&env, &report.snapshot)[0];
    assert!(!tree.contains_key("restless.bin") && tree.contains_key("steady.bin"));
    let manifest = env.load(&report.snapshot);
    assert_eq!(manifest.skipped.len(), 1, "the snapshot itself records the gap");

    // With the stop-on-error policy the same situation fails the whole backup.
    options.scan.on_error = vaultsnap_core::scan::ErrorPolicy::Fail;
    let before = env.snapshot_count();
    let err = env.repo.backup(&env.keys, options).await.expect_err("must fail");
    assert!(matches!(err, Error::SourceUnreadable { .. }), "{err}");
    assert_eq!(env.snapshot_count(), before);
}

#[tokio::test]
async fn a_file_that_disappears_during_the_backup_is_noted_but_is_not_a_failure() {
    let env = Env::new().await;
    let src = env.path("src");
    for i in 0..30 {
        write(&src, &format!("f{i:02}.txt"), format!("file number {i}").as_bytes());
    }
    write(&src, "zz-vanishes.txt", b"here now, gone soon");

    let mut options = env.options(&["src"]);
    options.jobs = 1;
    let victim = src.join("zz-vanishes.txt");
    let removed = Arc::new(AtomicBool::new(false));
    options.observer = Some(Arc::new(move |event: &BackupEvent<'_>| {
        if matches!(event, BackupEvent::FileFinished { .. }) && !removed.swap(true, Ordering::SeqCst) {
            let _ = fs::remove_file(&victim);
        }
    }));
    let report = env.backup(options).await;
    assert!(!report.is_partial());
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].path, "zz-vanishes.txt");
    assert!(!report.skipped[0].failure);
    assert!(!reconstruct(&env, &report.snapshot)[0].contains_key("zz-vanishes.txt"));
}

#[cfg(unix)]
struct Unlock(Vec<PathBuf>);

#[cfg(unix)]
impl Drop for Unlock {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;

        for path in &self.0 {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn unreadable_files_make_the_backup_partial_and_say_so() {
    use std::os::unix::fs::PermissionsExt;

    let env = Env::new().await;
    let src = env.path("src");
    write(&src, "readable.txt", b"fine");
    write(&src, "secret.txt", b"cannot be read");
    write(&src, "locked/inside.txt", b"hidden by its directory");
    let secret = src.join("secret.txt");
    let locked = src.join("locked");
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o000)).expect("chmod");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod");
    let _guard = Unlock(vec![secret.clone(), locked.clone()]);
    if fs::File::open(&secret).is_ok() {
        eprintln!("note: running with privileges that bypass permissions; case skipped");
        return;
    }

    let report = env.backup(env.options(&["src"])).await;
    assert!(report.is_partial());
    let mut paths: Vec<&str> = report.skipped.iter().map(|s| s.path.as_str()).collect();
    paths.sort_unstable();
    assert_eq!(paths, ["locked", "secret.txt"]);
    assert!(report.skipped.iter().all(|s| s.failure));
    assert!(report.skipped.iter().all(|s| s.reason.to_lowercase().contains("permission denied")), "{:?}", report.skipped);
    let tree = &reconstruct(&env, &report.snapshot)[0];
    assert_eq!(tree.get("readable.txt"), Some(&Item::File(b"fine".to_vec())));
    assert!(!tree.contains_key("secret.txt") && !tree.contains_key("locked"));
    assert_eq!(env.load(&report.snapshot).stats.skipped, 2);

    let mut options = env.options(&["src"]);
    options.scan.on_error = vaultsnap_core::scan::ErrorPolicy::Fail;
    let count = env.snapshot_count();
    assert!(env.repo.backup(&env.keys, options).await.is_err());
    assert_eq!(env.snapshot_count(), count, "a failed run adds nothing");
}

#[cfg(unix)]
#[tokio::test]
async fn links_are_recorded_and_hard_linked_data_is_stored_once() {
    use std::os::unix::fs::symlink;

    let env = Env::new().await;
    let src = env.path("src");
    write(&src, "a", &pseudo_random(30_000, 700));
    fs::hard_link(src.join("a"), src.join("b")).expect("hard link");
    symlink("a", src.join("link")).expect("symlink");
    symlink("/outside/the/tree", src.join("dangling")).expect("symlink");

    let report = env.backup(env.options(&["src"])).await;
    let manifest = env.load(&report.snapshot);
    let kinds: BTreeMap<String, &NodeKind> = manifest.roots[0]
        .entries
        .iter()
        .enumerate()
        .map(|(i, e)| (String::from_utf8_lossy(&manifest.roots[0].relative_path(i)).into_owned(), &e.kind))
        .collect();
    assert!(matches!(kinds["a"], NodeKind::File { .. }));
    assert!(matches!(kinds["b"], NodeKind::HardLink { .. }));
    assert!(matches!(kinds["link"], NodeKind::Symlink { .. }));
    assert_eq!(report.transfer.files_read, 1, "the linked data is read once");
    assert_eq!(report.totals.hard_links, 1);

    let tree = &reconstruct(&env, &report.snapshot)[0];
    assert_eq!(tree["a"], tree["b"]);
    assert_eq!(tree["link"], Item::Link("a".to_owned()));
    assert_eq!(tree["dangling"], Item::Link("/outside/the/tree".to_owned()));

    let again = env.backup(env.options(&["src"])).await;
    assert_eq!(again.transfer.files_read, 0);
    assert_eq!(reconstruct(&env, &again.snapshot), [tree.clone()].to_vec());
}

#[tokio::test]
async fn every_completed_snapshot_can_be_reconstructed_after_a_long_history() {
    let env = Env::new().await;
    let src = env.path("src");
    write(&src, "stable.bin", &pseudo_random(50_000, 800));
    write(&src, "evolving.txt", b"version 0\n");
    write(&src, "dir/transient.txt", b"exists only at first");

    let mut history: Vec<(SnapshotId, Tree)> = Vec::new();
    for round in 0..6_u64 {
        match round {
            1 => {
                let mut text = fs::read(src.join("evolving.txt")).expect("read");
                text.extend_from_slice(format!("version {round}\n").as_bytes());
                fs::write(src.join("evolving.txt"), text).expect("write");
            }
            2 => fs::remove_file(src.join("dir/transient.txt")).expect("remove"),
            3 => write(&src, "dir/added.bin", &pseudo_random(40_000, 801)),
            4 => {
                let mut stable = fs::read(src.join("stable.bin")).expect("read");
                stable.splice(10..10, vec![7_u8; 300]);
                fs::write(src.join("stable.bin"), stable).expect("write");
            }
            5 => fs::remove_dir_all(src.join("dir")).expect("remove"),
            _ => {}
        }
        let tree = capture(&src, None);
        let report = env.backup(env.options(&["src"])).await;
        history.push((report.snapshot, tree));
    }

    assert_eq!(env.snapshot_count(), 6);
    for (id, expected) in &history {
        assert_eq!(&reconstruct(&env, id)[0], expected, "snapshot {id}");
    }
}
