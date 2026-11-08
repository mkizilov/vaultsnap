//! Integration tests of the storage abstraction through the public API: that every reference
//! backend passes the contract, that a whole repository (init, backup, restore, check, prune)
//! works on a backend that is not a directory and on one that fails now and then, that a
//! repository moves between backends object by object without any conversion, and that a
//! backend that cannot guarantee what the format needs is refused before anything is written.
//!
//! Backend behavior in detail is tested beside the backends (`src/storage/`) and by the contract
//! itself (`src/storage/contract.rs`).

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;
use vaultsnap_core::storage::contract::{self, Fixture};
use vaultsnap_core::storage::{
    Capabilities, DirEntryKind, Fault, FlakyBackend, Guarantee, ImmediateSleeper, LocalBackend,
    MemoryBackend, PutMode,
};
use vaultsnap_core::{
    Backend, BackupOptions, CheckOptions, ChunkerConfig, Error, KdfParams, Passphrase, PruneMode,
    PruneOptions, Repository, RepositoryKeys, RestoreOptions, RetentionPolicy, RetryPolicy,
    RetryingBackend, SnapshotId, SnapshotRef,
};

type Tree = BTreeMap<String, Vec<u8>>;

fn passphrase() -> Passphrase {
    Passphrase::new(b"correct horse battery staple".to_vec()).expect("valid passphrase")
}

fn kdf() -> KdfParams {
    KdfParams {
        m_cost_kib: 64,
        t_cost: 1,
        p_cost: 1,
    }
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

async fn backup(repo: &Repository, keys: &RepositoryKeys, src: &Path) -> SnapshotId {
    std::thread::sleep(Duration::from_millis(10));
    let options = BackupOptions {
        sources: vec![src.to_path_buf()],
        chunker: ChunkerConfig::new(1024, 4096, 16_384).expect("valid config"),
        host: "test-host".to_owned(),
        jobs: 2,
        racy_margin: Duration::ZERO,
        ..BackupOptions::default()
    };
    repo.backup(keys, options)
        .await
        .expect("backup succeeds")
        .snapshot
}

/// Restores a snapshot of `src` into `out` and returns the restored files.
async fn restored_files(
    repo: &Repository,
    keys: &RepositoryKeys,
    id: &SnapshotId,
    src: &Path,
    out: &Path,
) -> Tree {
    let options = RestoreOptions::new(SnapshotRef::from(id), out);
    let report = repo.restore(keys, options).await.expect("restore succeeds");
    assert!(report.is_complete(), "{report:?}");
    let canonical = fs::canonicalize(src).expect("the source exists");
    read_tree(
        &report
            .target
            .join(canonical.strip_prefix("/").expect("absolute")),
    )
}

async fn assert_healthy(repo: &Repository, keys: &RepositoryKeys) {
    let options = CheckOptions {
        read_data: true,
        ..CheckOptions::default()
    };
    let report = repo.check(keys, options).await.expect("the check runs");
    assert!(report.is_clean(), "{:?}", report.findings);
}

/// Everything a repository does, on whatever storage `repo` was made with: two backups of a
/// changing source, a listing, restores that are compared with the sources byte for byte, a full
/// check, a prune, and a check and a restore after it.
async fn full_cycle(repo: &Repository, work: &Path) {
    let passphrase = passphrase();
    repo.init_with_kdf(&passphrase, kdf()).await.expect("init");
    let keys = repo.unlock(&passphrase).await.expect("unlock");

    let src = work.join("src");
    write(&src, "readme.txt", b"hello storage");
    write(
        &src,
        "docs/report.txt",
        &b"quarterly numbers\n".repeat(2000),
    );
    write(&src, "docs/data.bin", &pseudo_random(200_000, 1));
    write(&src, "old.bin", &pseudo_random(30_000, 2));
    write(&src, "empty.bin", b"");
    let first_files = read_tree(&src);
    let first = backup(repo, &keys, &src).await;

    fs::remove_file(src.join("old.bin")).expect("remove");
    write(&src, "readme.txt", b"hello storage, edited");
    write(&src, "new.bin", &pseudo_random(60_000, 3));
    let second_files = read_tree(&src);
    let second = backup(repo, &keys, &src).await;
    assert_ne!(first, second);

    let listing = repo.snapshots(&keys).await.expect("list");
    assert!(listing.damaged.is_empty());
    let listed: Vec<SnapshotId> = listing
        .snapshots
        .iter()
        .map(|info| info.summary.id.clone())
        .collect();
    assert_eq!(listed, [first.clone(), second.clone()], "oldest first");

    assert_eq!(
        restored_files(repo, &keys, &first, &src, &work.join("out1")).await,
        first_files
    );
    assert_eq!(
        restored_files(repo, &keys, &second, &src, &work.join("out2")).await,
        second_files
    );
    assert_healthy(repo, &keys).await;

    let policy = RetentionPolicy {
        keep_last: Some(1),
        ..RetentionPolicy::default()
    };
    let options = PruneOptions {
        mode: PruneMode::Unattended,
        ..PruneOptions::new(policy)
    };
    let pruned = repo.prune(&keys, options).await.expect("prune");
    assert_eq!(pruned.snapshots_removed, 1);
    assert!(
        pruned.chunks_removed > 0,
        "the removed snapshot had chunks of its own"
    );
    assert_healthy(repo, &keys).await;
    assert_eq!(
        restored_files(repo, &keys, &second, &src, &work.join("out3")).await,
        second_files
    );
    let listing = repo.snapshots(&keys).await.expect("list");
    assert_eq!(listing.snapshots.len(), 1);
}

fn work() -> TempDir {
    tempfile::tempdir().expect("create temporary directory")
}

// ---------------------------------------------------------------------------------------------
// The contract
// ---------------------------------------------------------------------------------------------

#[test]
fn every_reference_backend_passes_the_contract() {
    contract::run(&|| {
        let dir = tempfile::tempdir().expect("create temporary directory");
        let backend = Arc::new(LocalBackend::new(dir.path()));
        Fixture::keeping(backend, dir)
    });
    contract::run(&|| Fixture::new(Arc::new(MemoryBackend::new())));
    contract::run(&|| {
        let flaky = FlakyBackend::periodic(Arc::new(MemoryBackend::new()), 4);
        let policy = RetryPolicy {
            max_attempts: 12,
            ..RetryPolicy::none()
        };
        let retrying =
            RetryingBackend::with_sleeper(Arc::new(flaky), policy, Arc::new(ImmediateSleeper));
        Fixture::new(Arc::new(retrying))
    });
}

// ---------------------------------------------------------------------------------------------
// A whole repository on each kind of storage
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_repository_works_end_to_end_on_a_directory() {
    let work = work();
    let repo = Repository::with_backend(Arc::new(LocalBackend::new(work.path().join("repo"))));
    full_cycle(&repo, work.path()).await;
}

#[tokio::test]
async fn a_repository_works_end_to_end_on_object_store_semantics() {
    let work = work();
    let memory = Arc::new(MemoryBackend::new());
    let repo = Repository::with_backend(memory.clone());
    full_cycle(&repo, work.path()).await;

    let names = memory.object_names();
    assert!(names.iter().any(|name| name == "config"));
    assert!(names.iter().any(|name| name.starts_with("keys/")));
    assert!(names.iter().any(|name| name.starts_with("snapshots/")));
    assert!(names.iter().any(|name| name.starts_with("data/")));
    assert!(
        names.iter().all(|name| !name.starts_with("locks/")),
        "no lock is left behind"
    );
}

#[tokio::test]
async fn a_repository_works_on_storage_that_fails_now_and_then_behind_retries() {
    let work = work();
    let flaky = Arc::new(FlakyBackend::periodic(Arc::new(MemoryBackend::new()), 5));
    let policy = RetryPolicy {
        max_attempts: 12,
        ..RetryPolicy::none()
    };
    let retrying = RetryingBackend::with_sleeper(flaky.clone(), policy, Arc::new(ImmediateSleeper));
    let repo = Repository::with_backend(Arc::new(retrying));

    full_cycle(&repo, work.path()).await;

    assert!(
        flaky.injected() > 20,
        "the storage must really have failed: {}",
        flaky.injected()
    );
}

#[tokio::test]
async fn without_retries_a_failure_of_the_storage_reaches_the_caller_as_a_transient_error() {
    let flaky =
        FlakyBackend::scripted_for(Arc::new(MemoryBackend::new()), &["probe"], [Fault::Before]);
    let repo = Repository::with_backend(Arc::new(flaky));
    let err = repo.load_config().await.expect_err("the first probe fails");
    assert!(err.is_transient(), "{err}");
    let err = repo
        .load_config()
        .await
        .expect_err("and then the repository is simply not there");
    assert!(matches!(err, Error::NotARepository { .. }), "{err}");
}

// ---------------------------------------------------------------------------------------------
// Moving a repository between backends
// ---------------------------------------------------------------------------------------------

/// Copies every object below `dir` from one backend to another, creating directories on the way.
fn copy_objects(from: &dyn Backend, to: &dyn Backend, dir: &str) {
    for entry in from.list_dir(dir).expect("list") {
        let name = if dir.is_empty() {
            entry.name.clone()
        } else {
            format!("{dir}/{}", entry.name)
        };
        match entry.kind {
            DirEntryKind::Directory => {
                to.create_dir(&name).expect("create directory");
                copy_objects(from, to, &name);
            }
            DirEntryKind::File => {
                let object = from.get(&name, 1 << 30).expect("get");
                to.put(&name, &object.data, PutMode::Create).expect("put");
            }
            other => panic!("unexpected {other:?} at {name}"),
        }
    }
}

#[tokio::test]
async fn a_repository_moves_between_backends_object_by_object_without_conversion() {
    let work = work();
    let passphrase = passphrase();
    let src = work.path().join("src");
    write(&src, "a.txt", b"first version");
    write(&src, "dir/b.bin", &pseudo_random(100_000, 9));
    let files = read_tree(&src);

    // Made on a directory ...
    let directory = LocalBackend::new(work.path().join("repo"));
    directory.create_root().expect("create the directory");
    let original = Repository::with_backend(Arc::new(directory));
    original
        .init_with_kdf(&passphrase, kdf())
        .await
        .expect("init");
    let keys = original.unlock(&passphrase).await.expect("unlock");
    let id = backup(&original, &keys, &src).await;

    // ... copied into an object store ...
    let memory = Arc::new(MemoryBackend::new());
    copy_objects(original.backend().as_ref(), memory.as_ref(), "");
    let in_memory = Repository::with_backend(memory.clone());
    let keys = in_memory
        .unlock(&passphrase)
        .await
        .expect("the same passphrase unlocks it");
    assert_eq!(
        restored_files(&in_memory, &keys, &id, &src, &work.path().join("out1")).await,
        files
    );
    assert_healthy(&in_memory, &keys).await;

    // ... and copied back into a new directory. The four directories of the format are
    // created first, as `init` does: an object store has no empty ones to copy.
    let back = LocalBackend::new(work.path().join("repo2"));
    back.create_root().expect("create the directory");
    for dir in ["data", "snapshots", "locks", "keys"] {
        back.create_dir(dir).expect("create directory");
    }
    copy_objects(memory.as_ref(), &back, "");
    let on_disk = Repository::at(work.path().join("repo2"));
    let keys = on_disk.unlock(&passphrase).await.expect("unlocks");
    assert_eq!(
        restored_files(&on_disk, &keys, &id, &src, &work.path().join("out2")).await,
        files
    );
    assert_healthy(&on_disk, &keys).await;
    assert_eq!(
        read_tree(&work.path().join("repo")),
        read_tree(&work.path().join("repo2")),
        "the same bytes in the same places"
    );
}

// ---------------------------------------------------------------------------------------------
// What a backend must guarantee
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_backend_that_cannot_guarantee_what_the_format_needs_is_refused_untouched() {
    let sound = MemoryBackend::new().capabilities();
    let weak = [
        Capabilities {
            read_after_write: false,
            ..sound.clone()
        },
        Capabilities {
            create_if_absent: Guarantee::Process,
            ..sound.clone()
        },
        Capabilities {
            create_if_absent: Guarantee::None,
            ..sound.clone()
        },
        Capabilities {
            atomic_put: false,
            ..sound
        },
    ];
    for capabilities in weak {
        let memory = Arc::new(MemoryBackend::with_capabilities(capabilities.clone()));
        let repo = Repository::with_backend(memory.clone());
        let err = repo
            .init_with_kdf(&passphrase(), kdf())
            .await
            .expect_err("must be refused");
        assert!(
            matches!(err, Error::UnsupportedBackend(_)),
            "{capabilities:?}: {err}"
        );
        assert!(memory.object_names().is_empty(), "nothing was written");
        let err = repo.load_config().await.expect_err("must be refused");
        assert!(matches!(err, Error::UnsupportedBackend(_)), "{err}");
    }
}

#[tokio::test]
async fn a_directory_given_as_a_backend_still_keeps_restores_out_of_itself() {
    let work = work();
    let passphrase = passphrase();
    let src = work.path().join("src");
    write(&src, "a.txt", b"data");
    let repo_dir = work.path().join("repo");
    let backend = LocalBackend::new(&repo_dir);
    backend.create_root().expect("create the directory");
    let repo = Repository::with_backend(Arc::new(backend));
    repo.init_with_kdf(&passphrase, kdf()).await.expect("init");
    let keys = repo.unlock(&passphrase).await.expect("unlock");
    let id = backup(&repo, &keys, &src).await;
    assert_eq!(repo.backend().local_path(), Some(repo_dir.as_path()));

    for target in [repo_dir.join("inside"), repo_dir.join("data")] {
        let options = RestoreOptions::new(SnapshotRef::from(&id), &target);
        let err = repo
            .restore(&keys, options)
            .await
            .expect_err("must be refused");
        assert!(matches!(err, Error::InvalidTarget(_)), "{target:?}: {err}");
    }
    assert!(!repo_dir.join("inside").exists());
}

#[tokio::test]
async fn a_repository_that_is_not_a_directory_has_nowhere_to_keep_a_restore_out_of() {
    let work = work();
    let memory = Arc::new(MemoryBackend::new());
    let repo = Repository::with_backend(memory);
    assert!(repo.backend().local_path().is_none());
    let passphrase = passphrase();
    repo.init_with_kdf(&passphrase, kdf()).await.expect("init");
    let keys = repo.unlock(&passphrase).await.expect("unlock");
    let src = work.path().join("src");
    write(&src, "a.txt", b"data");
    let id = backup(&repo, &keys, &src).await;
    // Restoring next to the sources works, because the repository is not on this disk at all.
    let files = restored_files(&repo, &keys, &id, &src, &work.path().join("restored")).await;
    assert_eq!(files["a.txt"], b"data");
}
