//! Integration tests of restore on repositories made by real backups: the restored tree is
//! compared byte for byte (and by permissions and times) with the tree that was backed up; selected
//! paths; conflicts with things already in the target, including links an attacker planted;
//! damaged chunks; tampered snapshots; and interrupted restores.
//!
//! Hostile manifests (paths that climb, names with separators, broken trees) cannot be produced by
//! a real backup, so they are tested against the engine directly in `src/restore/engine/tests.rs`
//! and `src/ops/restore.rs`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;
use vaultsnap_core::snapshot::NodeKind;
use vaultsnap_core::{
    BackupOptions, BackupReport, ChunkId, ChunkerConfig, ConflictPolicy, Error, KdfParams,
    Passphrase, Repository, RepositoryKeys, RestoreEvent, RestoreOptions, RestoreReport,
    RestoreSymlinks, SnapshotId, SnapshotRef,
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

fn reference(id: &SnapshotId) -> SnapshotRef {
    SnapshotRef::from(id)
}

fn pause() {
    std::thread::sleep(Duration::from_millis(10));
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

    /// The source as the snapshot records it (symbolic links in the temporary path resolved).
    fn source(&self, relative: &str) -> PathBuf {
        fs::canonicalize(self.path(relative)).expect("the source exists")
    }

    async fn backup(&self, sources: &[&str]) -> BackupReport {
        let options = BackupOptions {
            sources: sources.iter().map(|source| self.path(source)).collect(),
            chunker: ChunkerConfig::new(1024, 4096, 16_384).expect("valid config"),
            host: "test-host".to_owned(),
            jobs: 2,
            racy_margin: Duration::ZERO,
            ..BackupOptions::default()
        };
        self.repo
            .backup(&self.keys, options)
            .await
            .expect("backup succeeds")
    }

    fn options(&self, id: &SnapshotId, target: &Path) -> RestoreOptions {
        RestoreOptions::new(reference(id), target)
    }

    async fn restore(&self, options: RestoreOptions) -> Result<RestoreReport, Error> {
        self.repo.restore(&self.keys, options).await
    }

    /// Restores a snapshot with the default (safest) options and expects success.
    async fn restore_ok(&self, id: &SnapshotId, target: &Path) -> RestoreReport {
        let report = self
            .restore(self.options(id, target))
            .await
            .expect("restore succeeds");
        assert!(report.is_complete(), "{report:?}");
        report
    }

    /// The path of the object file of a chunk, from the documented layout.
    fn chunk_file(&self, id: &ChunkId) -> PathBuf {
        let hex = id.to_hex();
        self.path("repo/data").join(&hex[..2]).join(hex)
    }

    fn snapshot_file(&self, id: &SnapshotId) -> PathBuf {
        self.path("repo/snapshots").join(id.as_str())
    }

    fn lock_files(&self) -> usize {
        fs::read_dir(self.path("repo/locks"))
            .expect("read locks")
            .count()
    }

    /// The chunks of the file `rel` (relative to the source) in a snapshot.
    async fn chunks_of(&self, id: &SnapshotId, source: &Path, rel: &str) -> Vec<ChunkId> {
        let manifest = self
            .repo
            .snapshot(&self.keys, &reference(id))
            .await
            .expect("load");
        let want = format!("{}/{rel}", source.display());
        for listed in vaultsnap_core::history::entries(&manifest, None) {
            if listed.path == want.as_bytes() {
                if let NodeKind::File { chunks } = &listed.entry.kind {
                    return chunks.iter().map(|chunk| chunk.id).collect();
                }
            }
        }
        panic!("{want} is not a file in the snapshot");
    }
}

// ---------------------------------------------------------------------------------------------
// Capturing a tree to compare
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Dir,
    File(Vec<u8>),
    Link(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Captured {
    kind: Kind,
    /// Permission bits; 0 for links, whose permissions are not restored.
    mode: u32,
    /// Modification time as (seconds, nanoseconds); zero for links.
    modified: (u64, u32),
}

type Tree = BTreeMap<String, Captured>;

#[cfg(unix)]
fn mode_of(meta: &fs::Metadata, directory: bool) -> u32 {
    use std::os::unix::fs::PermissionsExt;

    meta.permissions().mode() & if directory { 0o1777 } else { 0o777 }
}

#[cfg(not(unix))]
fn mode_of(_meta: &fs::Metadata, _directory: bool) -> u32 {
    0
}

/// Every entry below `root` (and `root` itself, as the empty path) with content, permissions, and
/// modification time.
fn capture(root: &Path) -> Tree {
    fn walk(base: &Path, path: &Path, out: &mut Tree) {
        let relative = path
            .strip_prefix(base)
            .expect("inside")
            .to_string_lossy()
            .into_owned();
        let meta = fs::symlink_metadata(path).expect("stat");
        if meta.is_dir() {
            let captured = Captured {
                kind: Kind::Dir,
                mode: mode_of(&meta, true),
                modified: mtime(&meta),
            };
            out.insert(relative, captured);
            let mut names: Vec<_> = fs::read_dir(path)
                .expect("read_dir")
                .map(|entry| entry.expect("entry").file_name())
                .collect();
            names.sort();
            for name in names {
                walk(base, &path.join(name), out);
            }
        } else if meta.file_type().is_symlink() {
            let target = fs::read_link(path)
                .expect("read_link")
                .to_string_lossy()
                .into_owned();
            let captured = Captured {
                kind: Kind::Link(target),
                mode: 0,
                modified: (0, 0),
            };
            out.insert(relative, captured);
        } else {
            let captured = Captured {
                kind: Kind::File(fs::read(path).expect("read file")),
                mode: mode_of(&meta, false),
                modified: mtime(&meta),
            };
            out.insert(relative, captured);
        }
    }
    let mut out = Tree::new();
    walk(root, root, &mut out);
    out
}

fn mtime(meta: &fs::Metadata) -> (u64, u32) {
    let elapsed = meta
        .modified()
        .expect("modification time")
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after the epoch");
    (elapsed.as_secs(), elapsed.subsec_nanos())
}

/// Where a source that was backed up as `source` is restored below `target`.
fn restored_root(report: &RestoreReport, source: &Path) -> PathBuf {
    let relative = source.strip_prefix("/").expect("absolute source");
    report.target.join(relative)
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("read_dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

fn no_temporary_files(dir: &Path) {
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in fs::read_dir(&current).expect("read_dir") {
            let path = entry.expect("entry").path();
            assert!(
                !path.to_string_lossy().contains(".vaultsnap-tmp-"),
                "left behind: {path:?}"
            );
            if fs::symlink_metadata(&path).expect("stat").is_dir() {
                pending.push(path);
            }
        }
    }
}

/// A tree with most of what a restore has to get right.
fn sample_tree(root: &Path) {
    write(root, "readme.txt", b"hello restore");
    write(root, "empty.bin", b"");
    write(
        root,
        "docs/report.txt",
        &b"quarterly numbers\n".repeat(3000),
    );
    write(root, "docs/archive/data.bin", &pseudo_random(300_000, 1));
    write(
        root,
        "docs/archive/copy-of-data.bin",
        &pseudo_random(300_000, 1),
    );
    write(root, "docs/archive/notes", b"x");
    write(root, "unicode/日本語 ファイル.txt", "こんにちは".as_bytes());
    write(root, "with space/new\nline", b"odd names");
    write(root, "-dash-first", b"starts with a dash");
    fs::create_dir(root.join("empty-dir")).expect("mkdir");
}

#[cfg(unix)]
fn special_parts(root: &Path) {
    use std::os::unix::fs::PermissionsExt;

    write(root, "bin/run.sh", b"#!/bin/sh\necho hi\n");
    fs::set_permissions(root.join("bin/run.sh"), fs::Permissions::from_mode(0o755)).expect("chmod");
    write(root, "secret.key", b"private");
    fs::set_permissions(root.join("secret.key"), fs::Permissions::from_mode(0o600)).expect("chmod");
    write(root, "readonly.txt", b"look, don't touch");
    fs::set_permissions(root.join("readonly.txt"), fs::Permissions::from_mode(0o444))
        .expect("chmod");
    fs::set_permissions(root.join("bin"), fs::Permissions::from_mode(0o750)).expect("chmod");
    std::os::unix::fs::symlink("readme.txt", root.join("readme.link")).expect("symlink");
    std::os::unix::fs::symlink("docs/archive", root.join("archive.link")).expect("symlink");
    std::os::unix::fs::symlink("../elsewhere/up.txt", root.join("docs/up.link")).expect("symlink");
    fs::hard_link(root.join("readme.txt"), root.join("docs/readme-hardlink")).expect("hard link");
}

// ---------------------------------------------------------------------------------------------
// Byte-for-byte round trips
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_restored_tree_equals_the_original_in_content_permissions_and_times() {
    let env = Env::new().await;
    let src = env.path("src");
    sample_tree(&src);
    #[cfg(unix)]
    special_parts(&src);
    let backup = env.backup(&["src"]).await;
    let source = env.source("src");
    let original = capture(&source);
    assert!(original.len() > 15, "{}", original.len());

    let target = env.path("restored");
    let report = env.restore_ok(&backup.snapshot, &target).await;

    let restored = capture(&restored_root(&report, &source));
    assert_eq!(
        restored.keys().collect::<Vec<_>>(),
        original.keys().collect::<Vec<_>>()
    );
    for (path, expected) in &original {
        assert_eq!(restored[path], *expected, "{path:?} differs");
    }
    assert!(
        report.skipped.is_empty() && report.warnings.is_empty(),
        "{report:?}"
    );
    let files = original
        .values()
        .filter(|item| matches!(item.kind, Kind::File(_)))
        .count() as u64;
    #[cfg(unix)]
    assert_eq!(report.files + report.hard_links, files);
    #[cfg(not(unix))]
    assert_eq!(report.files, files);
    assert_eq!(report.snapshot, backup.snapshot);
    no_temporary_files(&report.target);
    assert_eq!(env.lock_files(), 0, "the lock was released");
}

#[cfg(unix)]
#[tokio::test]
async fn hard_links_come_back_as_hard_links_and_links_keep_their_targets() {
    use std::os::unix::fs::MetadataExt;

    let env = Env::new().await;
    let src = env.path("src");
    sample_tree(&src);
    special_parts(&src);
    let backup = env.backup(&["src"]).await;
    let source = env.source("src");

    let report = env
        .restore_ok(&backup.snapshot, &env.path("restored"))
        .await;
    let root = restored_root(&report, &source);
    let (a, b) = (
        fs::metadata(root.join("readme.txt")).expect("metadata"),
        fs::metadata(root.join("docs/readme-hardlink")).expect("metadata"),
    );
    assert_eq!(
        (a.dev(), a.ino()),
        (b.dev(), b.ino()),
        "one file with two names"
    );
    assert_eq!(a.nlink(), 2);
    assert_eq!(
        fs::read_link(root.join("readme.link")).expect("link"),
        Path::new("readme.txt")
    );
    assert_eq!(
        fs::read(root.join("readme.link")).expect("read"),
        b"hello restore"
    );
    assert_eq!(
        fs::read_link(root.join("docs/up.link")).expect("link"),
        Path::new("../elsewhere/up.txt")
    );
    assert_eq!(report.hard_links, 1);
    assert_eq!(report.symlinks, 3);
}

#[tokio::test]
async fn several_sources_are_restored_side_by_side() {
    let env = Env::new().await;
    write(&env.path("one"), "a.txt", b"from one");
    write(&env.path("two"), "a.txt", b"from two");
    fs::write(env.path("single-file.txt"), b"a source that is a file").expect("write");
    let backup = env.backup(&["one", "two", "single-file.txt"]).await;

    let report = env
        .restore_ok(&backup.snapshot, &env.path("restored"))
        .await;
    let one = restored_root(&report, &env.source("one"));
    let two = restored_root(&report, &env.source("two"));
    assert_eq!(fs::read(one.join("a.txt")).expect("read"), b"from one");
    assert_eq!(fs::read(two.join("a.txt")).expect("read"), b"from two");
    let single = restored_root(&report, &env.source("single-file.txt"));
    assert_eq!(fs::read(single).expect("read"), b"a source that is a file");
}

#[tokio::test]
async fn each_snapshot_restores_the_state_it_was_taken_in() {
    let env = Env::new().await;
    let src = env.path("src");
    write(&src, "keep.txt", b"never changes");
    write(&src, "edit.txt", b"first version");
    write(&src, "remove.txt", b"will be deleted");
    write(&src, "big.bin", &pseudo_random(100_000, 2));
    let first = env.backup(&["src"]).await;
    let source = env.source("src");
    let first_state = capture(&source);
    pause();

    write(&src, "edit.txt", b"second version, which is longer");
    fs::remove_file(src.join("remove.txt")).expect("remove");
    write(&src, "added.txt", b"new in the second snapshot");
    let mut big = pseudo_random(100_000, 2);
    big.splice(50_000..50_000, b"<<< inserted >>>".iter().copied());
    write(&src, "big.bin", &big);
    let second = env.backup(&["src"]).await;
    let second_state = capture(&source);
    pause();

    // The source changes again after the second snapshot; neither restore may notice.
    write(&src, "keep.txt", b"changed after both backups");
    fs::remove_file(src.join("added.txt")).expect("remove");

    let report = env
        .restore_ok(&first.snapshot, &env.path("restore-1"))
        .await;
    assert_eq!(capture(&restored_root(&report, &source)), first_state);
    let report = env
        .restore_ok(&second.snapshot, &env.path("restore-2"))
        .await;
    assert_eq!(capture(&restored_root(&report, &source)), second_state);
    assert_eq!(
        fs::read(restored_root(&report, &source).join("big.bin")).expect("read"),
        big
    );
}

#[tokio::test]
async fn a_snapshot_can_be_named_by_the_start_of_its_id() {
    let env = Env::new().await;
    write(&env.path("src"), "a.txt", b"one");
    let backup = env.backup(&["src"]).await;
    let prefix: SnapshotRef = backup.snapshot.as_str()[..10].parse().expect("syntax");

    let report = env
        .restore(RestoreOptions::new(prefix, env.path("out")))
        .await
        .expect("restore");
    assert_eq!(report.snapshot, backup.snapshot);
    assert_eq!(report.files, 1);
}

// ---------------------------------------------------------------------------------------------
// Selected files
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn selected_files_and_directories_are_restored_and_nothing_else() {
    let env = Env::new().await;
    let src = env.path("src");
    sample_tree(&src);
    let backup = env.backup(&["src"]).await;
    let source = env.source("src");
    let original = capture(&source);

    let mut options = env.options(&backup.snapshot, &env.path("partial"));
    options.includes = vec![
        source.join("docs/archive"),
        source.join("readme.txt"),
        source.join("with space/new\nline"),
    ];
    let report = env.restore(options).await.expect("restore");
    assert!(report.is_complete(), "{report:?}");
    let root = restored_root(&report, &source);

    let restored = capture(&root);
    let wanted: Vec<&str> = vec![
        "",
        "docs",
        "docs/archive",
        "docs/archive/copy-of-data.bin",
        "docs/archive/data.bin",
        "docs/archive/notes",
        "readme.txt",
        "with space",
        "with space/new\nline",
    ];
    assert_eq!(
        restored.keys().map(String::as_str).collect::<Vec<_>>(),
        wanted
    );
    for path in wanted {
        if matches!(original[path].kind, Kind::File(_)) {
            assert_eq!(restored[path].kind, original[path].kind, "{path:?}");
            assert_eq!(restored[path].modified, original[path].modified, "{path:?}");
        }
    }
    assert_eq!(report.files, 5);
    assert!(!root.join("unicode").exists());
}

#[tokio::test]
async fn an_unknown_selection_restores_nothing() {
    let env = Env::new().await;
    write(&env.path("src"), "a.txt", b"one");
    let backup = env.backup(&["src"]).await;
    let source = env.source("src");
    let target = env.path("out");

    let mut options = env.options(&backup.snapshot, &target);
    options.includes = vec![source.join("a.txt"), source.join("missing.txt")];
    let err = env.restore(options).await.expect_err("must fail");
    assert!(matches!(err, Error::IncludeNotFound { .. }), "{err}");
    assert!(!target.exists());
    assert_eq!(env.lock_files(), 0);
}

// ---------------------------------------------------------------------------------------------
// Conflicts
// ---------------------------------------------------------------------------------------------

/// A target that already holds an older version of some files, and a file of its own.
async fn target_with_old_files(env: &Env, backup: &BackupReport) -> (PathBuf, PathBuf) {
    let source = env.source("src");
    let target = env.path("out");
    let first = env.restore_ok(&backup.snapshot, &target).await;
    let root = restored_root(&first, &source);
    fs::write(root.join("readme.txt"), b"my own version").expect("write");
    fs::write(root.join("docs/mine.txt"), b"only here").expect("write");
    fs::remove_file(root.join("docs/report.txt")).expect("remove");
    (target, root)
}

#[tokio::test]
async fn fail_mode_changes_nothing_when_anything_is_in_the_way() {
    let env = Env::new().await;
    sample_tree(&env.path("src"));
    let backup = env.backup(&["src"]).await;
    let (target, root) = target_with_old_files(&env, &backup).await;
    let before = capture(&root);

    let err = env
        .restore(env.options(&backup.snapshot, &target))
        .await
        .expect_err("conflict");
    match err {
        // Nine files in the tree; one was removed from the target. Directories merge.
        Error::RestoreConflict { count, .. } => assert_eq!(count, 8),
        other => panic!("expected a conflict, got {other}"),
    }
    assert_eq!(
        capture(&root),
        before,
        "not a byte of the target changed, not even a time"
    );
    assert!(
        !root.join("docs/report.txt").exists(),
        "the missing file was not restored either"
    );
    no_temporary_files(&target);
}

#[tokio::test]
async fn skip_mode_keeps_what_exists_and_fills_in_the_rest() {
    let env = Env::new().await;
    sample_tree(&env.path("src"));
    let backup = env.backup(&["src"]).await;
    let source = env.source("src");
    let original = capture(&source);
    let (target, root) = target_with_old_files(&env, &backup).await;

    let mut options = env.options(&backup.snapshot, &target);
    options.on_conflict = ConflictPolicy::Skip;
    let report = env.restore(options).await.expect("restore");
    assert!(report.is_complete(), "{report:?}");
    assert_eq!(report.files, 1, "only the file that was missing");
    assert_eq!(report.skipped.len(), 8, "{:?}", report.skipped);
    assert_eq!(
        fs::read(root.join("readme.txt")).expect("read"),
        b"my own version"
    );
    assert_eq!(
        fs::read(root.join("docs/mine.txt")).expect("read"),
        b"only here"
    );
    assert_eq!(
        fs::read(root.join("docs/report.txt")).expect("read"),
        b"quarterly numbers\n".repeat(3000)
    );
    assert_eq!(
        capture(&root)["docs/archive/data.bin"].kind,
        original["docs/archive/data.bin"].kind
    );
}

#[tokio::test]
async fn overwrite_mode_replaces_files_merges_directories_and_keeps_strangers() {
    let env = Env::new().await;
    sample_tree(&env.path("src"));
    let backup = env.backup(&["src"]).await;
    let source = env.source("src");
    let original = capture(&source);
    let (target, root) = target_with_old_files(&env, &backup).await;

    let mut options = env.options(&backup.snapshot, &target);
    options.on_conflict = ConflictPolicy::Overwrite;
    let report = env.restore(options).await.expect("restore");
    assert!(
        report.is_complete() && report.skipped.is_empty(),
        "{report:?}"
    );

    let restored = capture(&root);
    for (path, expected) in &original {
        assert_eq!(restored[path].kind, expected.kind, "{path:?}");
    }
    assert_eq!(
        fs::read(root.join("docs/mine.txt")).expect("read"),
        b"only here",
        "merged"
    );
    no_temporary_files(&target);
}

#[tokio::test]
async fn a_directory_is_never_replaced_by_a_file() {
    let env = Env::new().await;
    write(&env.path("src"), "name", b"a file in the snapshot");
    let backup = env.backup(&["src"]).await;
    let source = env.source("src");
    let target = env.path("out");
    let root = target.join(source.strip_prefix("/").expect("absolute"));
    fs::create_dir_all(root.join("name")).expect("mkdir");
    fs::write(root.join("name/precious"), b"keep").expect("write");

    let mut options = env.options(&backup.snapshot, &target);
    options.on_conflict = ConflictPolicy::Overwrite;
    let report = env.restore(options).await.expect("restore runs");
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert!(!report.is_complete());
    assert_eq!(fs::read(root.join("name/precious")).expect("read"), b"keep");
    no_temporary_files(&target);
}

#[cfg(unix)]
#[tokio::test]
async fn a_link_planted_in_the_target_is_never_followed_whatever_the_mode() {
    for policy in [
        ConflictPolicy::Fail,
        ConflictPolicy::Skip,
        ConflictPolicy::Overwrite,
    ] {
        let env = Env::new().await;
        sample_tree(&env.path("src"));
        let backup = env.backup(&["src"]).await;
        let source = env.source("src");

        // The attacker prepared the target: the first directory of where the files go is a link
        // to a place outside of it.
        let outside = env.path("outside");
        fs::create_dir(&outside).expect("mkdir");
        fs::write(outside.join("sentinel"), b"untouched").expect("write");
        let target = env.path("out");
        fs::create_dir(&target).expect("mkdir");
        let first = source
            .components()
            .nth(1)
            .expect("a first component")
            .as_os_str()
            .to_owned();
        std::os::unix::fs::symlink(&outside, target.join(&first)).expect("symlink");

        let mut options = env.options(&backup.snapshot, &target);
        options.on_conflict = policy;
        let result = env.restore(options).await;

        assert_eq!(
            names(&outside),
            ["sentinel"],
            "{policy:?}: something was written through the link"
        );
        assert_eq!(
            fs::read(outside.join("sentinel")).expect("read"),
            b"untouched"
        );
        match policy {
            ConflictPolicy::Fail => assert!(
                matches!(result, Err(Error::RestoreConflict { .. })),
                "{result:?}"
            ),
            ConflictPolicy::Skip => {
                let report = result.expect("restore");
                assert_eq!((report.files, report.directories), (0, 0));
                assert!(report.skipped.len() > 10);
            }
            ConflictPolicy::Overwrite => {
                let report = result.expect("restore");
                assert!(report.is_complete(), "{report:?}");
                let meta = fs::symlink_metadata(target.join(&first)).expect("metadata");
                assert!(
                    meta.is_dir() && !meta.file_type().is_symlink(),
                    "the link itself was replaced"
                );
                assert_eq!(
                    fs::read(restored_root(&report, &source).join("readme.txt")).expect("read"),
                    b"hello restore"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Symbolic link policy
// ---------------------------------------------------------------------------------------------

#[cfg(unix)]
#[tokio::test]
async fn links_leading_out_of_the_restore_root_follow_the_policy() {
    let env = Env::new().await;
    let src = env.path("src");
    write(&src, "inside.txt", b"inside");
    std::os::unix::fs::symlink("inside.txt", src.join("good.link")).expect("symlink");
    std::os::unix::fs::symlink("/etc/passwd", src.join("absolute.link")).expect("symlink");
    // Far more levels than any depth the restore root has below the source's path.
    let climbing = "../".repeat(60) + "etc/passwd";
    std::os::unix::fs::symlink(&climbing, src.join("climbing.link")).expect("symlink");
    let backup = env.backup(&["src"]).await;
    let source = env.source("src");

    let default = env
        .restore_ok(&backup.snapshot, &env.path("contained"))
        .await;
    let root = restored_root(&default, &source);
    assert_eq!(names(&root), ["good.link", "inside.txt"]);
    assert_eq!(default.skipped.len(), 2, "{:?}", default.skipped);

    let mut options = env.options(&backup.snapshot, &env.path("none"));
    options.symlinks = RestoreSymlinks::Skip;
    let report = env.restore(options).await.expect("restore");
    assert_eq!(names(&restored_root(&report, &source)), ["inside.txt"]);

    let mut options = env.options(&backup.snapshot, &env.path("all"));
    options.symlinks = RestoreSymlinks::AsRecorded;
    let report = env.restore(options).await.expect("restore");
    let root = restored_root(&report, &source);
    assert_eq!(
        fs::read_link(root.join("absolute.link")).expect("link"),
        Path::new("/etc/passwd")
    );
    assert_eq!(
        fs::read_link(root.join("climbing.link")).expect("link"),
        Path::new(&climbing)
    );
    assert_eq!(
        names(&env.path("")),
        ["all", "contained", "none", "repo", "src"],
        "nothing was created next to the restore targets"
    );
}

// ---------------------------------------------------------------------------------------------
// Damaged data
// ---------------------------------------------------------------------------------------------

fn flip_a_byte(path: &Path) {
    let mut bytes = fs::read(path).expect("read object");
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0x01;
    fs::write(path, bytes).expect("write object");
}

#[tokio::test]
async fn a_damaged_chunk_fails_that_file_only_and_everything_else_is_exact() {
    let env = Env::new().await;
    let src = env.path("src");
    write(&src, "a.txt", b"before the damage");
    write(&src, "damaged.bin", &pseudo_random(60_000, 3));
    write(&src, "z.txt", b"after the damage");
    let backup = env.backup(&["src"]).await;
    let source = env.source("src");
    let original = capture(&source);

    let chunks = env
        .chunks_of(&backup.snapshot, &source, "damaged.bin")
        .await;
    assert!(chunks.len() > 5, "{}", chunks.len());
    flip_a_byte(&env.chunk_file(&chunks[chunks.len() / 2]));

    let target = env.path("out");
    let report = env
        .restore(env.options(&backup.snapshot, &target))
        .await
        .expect("restore runs");
    assert!(!report.is_complete());
    assert_eq!(report.failed.len(), 1, "{:?}", report.failed);
    assert!(report.failed[0].path.ends_with("damaged.bin"));
    assert!(
        report.failed[0].reason.contains("corrupt"),
        "{:?}",
        report.failed
    );

    let root = restored_root(&report, &source);
    assert_eq!(
        names(&root),
        ["a.txt", "z.txt"],
        "no partial file, no temporary file"
    );
    for name in ["a.txt", "z.txt"] {
        assert_eq!(capture(&root)[name], original[name], "{name}");
    }
    no_temporary_files(&target);
    assert_eq!(env.lock_files(), 0);
}

#[tokio::test]
async fn a_chunk_that_was_truncated_or_removed_is_caught_too() {
    for damage in ["truncate", "remove"] {
        let env = Env::new().await;
        let src = env.path("src");
        write(&src, "damaged.bin", &pseudo_random(30_000, 4));
        write(&src, "fine.txt", b"fine");
        let backup = env.backup(&["src"]).await;
        let source = env.source("src");
        let chunks = env
            .chunks_of(&backup.snapshot, &source, "damaged.bin")
            .await;
        let object = env.chunk_file(&chunks[0]);
        match damage {
            "truncate" => {
                let bytes = fs::read(&object).expect("read");
                fs::write(&object, &bytes[..bytes.len() - 1]).expect("write");
            }
            _ => fs::remove_file(&object).expect("remove"),
        }

        let report = env
            .restore(env.options(&backup.snapshot, &env.path("out")))
            .await
            .expect("runs");
        assert_eq!(report.failed.len(), 1, "{damage}: {:?}", report.failed);
        assert_eq!(
            names(&restored_root(&report, &source)),
            ["fine.txt"],
            "{damage}"
        );
    }
}

#[tokio::test]
async fn a_chunk_replaced_by_another_valid_chunk_is_refused() {
    let env = Env::new().await;
    let src = env.path("src");
    write(&src, "target.bin", &pseudo_random(10_000, 5));
    write(&src, "other.bin", &pseudo_random(10_000, 6));
    let backup = env.backup(&["src"]).await;
    let source = env.source("src");
    let wanted = env.chunks_of(&backup.snapshot, &source, "target.bin").await;
    let other = env.chunks_of(&backup.snapshot, &source, "other.bin").await;
    // A perfectly valid object, authentic in every byte, under the wrong id.
    fs::copy(env.chunk_file(&other[0]), env.chunk_file(&wanted[0])).expect("copy");

    let report = env
        .restore(env.options(&backup.snapshot, &env.path("out")))
        .await
        .expect("runs");
    assert_eq!(report.failed.len(), 1, "{:?}", report.failed);
    assert!(report.failed[0].path.ends_with("target.bin"));
    assert_eq!(names(&restored_root(&report, &source)), ["other.bin"]);
}

#[tokio::test]
async fn a_tampered_snapshot_restores_nothing() {
    let env = Env::new().await;
    write(&env.path("src"), "a.txt", b"one");
    let backup = env.backup(&["src"]).await;
    let path = env.snapshot_file(&backup.snapshot);
    let mut bytes = fs::read(&path).expect("read snapshot file");
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0x01;
    fs::write(&path, bytes).expect("write snapshot file");

    let target = env.path("out");
    let err = env
        .restore(env.options(&backup.snapshot, &target))
        .await
        .expect_err("must fail");
    assert!(matches!(err, Error::CorruptSnapshot { .. }), "{err}");
    assert!(!target.exists());
    assert_eq!(env.lock_files(), 0);
}

#[tokio::test]
async fn the_wrong_keys_restore_nothing() {
    let env = Env::new().await;
    write(&env.path("src"), "a.txt", b"one");
    let backup = env.backup(&["src"]).await;
    let stranger = Env::new().await;

    let target = env.path("out");
    let err = env
        .repo
        .restore(&stranger.keys, env.options(&backup.snapshot, &target))
        .await
        .expect_err("must fail");
    assert!(matches!(err, Error::CorruptSnapshot { .. }), "{err}");
    assert!(!target.exists());
}

// ---------------------------------------------------------------------------------------------
// The target
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_repository_itself_is_never_a_place_to_restore_to() {
    let env = Env::new().await;
    write(&env.path("src"), "a.txt", b"one");
    let backup = env.backup(&["src"]).await;
    let before = names(&env.path("repo"));

    for target in [
        env.path("repo"),
        env.path("repo/data"),
        env.path("repo/fresh/place"),
    ] {
        let err = env
            .restore(env.options(&backup.snapshot, &target))
            .await
            .expect_err("refused");
        assert!(matches!(err, Error::InvalidTarget(_)), "{target:?}: {err}");
    }
    assert_eq!(names(&env.path("repo")), before);
}

#[tokio::test]
async fn restoring_reads_but_never_changes_the_repository() {
    let env = Env::new().await;
    sample_tree(&env.path("src"));
    let backup = env.backup(&["src"]).await;
    let snapshot = fs::read(env.snapshot_file(&backup.snapshot)).expect("read");
    let objects = |env: &Env| {
        let mut files = Vec::new();
        let mut pending = vec![env.path("repo/data")];
        while let Some(dir) = pending.pop() {
            for entry in fs::read_dir(&dir).expect("read_dir") {
                let path = entry.expect("entry").path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    files.push((path.clone(), fs::read(&path).expect("read")));
                }
            }
        }
        files.sort();
        files
    };
    let before = objects(&env);

    env.restore_ok(&backup.snapshot, &env.path("out")).await;
    assert_eq!(
        fs::read(env.snapshot_file(&backup.snapshot)).expect("read"),
        snapshot
    );
    assert_eq!(objects(&env), before);
}

// ---------------------------------------------------------------------------------------------
// Interruptions
// ---------------------------------------------------------------------------------------------

fn cancel_after_chunks(options: &mut RestoreOptions, chunks: usize) {
    let cancel = options.cancel.clone();
    let seen = Arc::new(AtomicUsize::new(0));
    options.observer = Some(Arc::new(move |event: &RestoreEvent<'_>| {
        if matches!(event, RestoreEvent::BytesWritten { .. })
            && seen.fetch_add(1, Ordering::SeqCst) + 1 == chunks
        {
            cancel.cancel();
        }
    }));
}

#[tokio::test]
async fn an_interrupted_restore_leaves_whole_files_and_resuming_completes_it_exactly() {
    let env = Env::new().await;
    let src = env.path("src");
    sample_tree(&src);
    #[cfg(unix)]
    special_parts(&src);
    let backup = env.backup(&["src"]).await;
    let source = env.source("src");
    let original = capture(&source);

    let target = env.path("out");
    let mut options = env.options(&backup.snapshot, &target);
    cancel_after_chunks(&mut options, 40);
    let err = env.restore(options).await.expect_err("cancelled");
    assert!(matches!(err, Error::RestoreCancelled), "{err}");

    // What is there is exactly what the snapshot says, nothing is half written.
    let root = target.join(source.strip_prefix("/").expect("absolute"));
    let partial = capture(&root);
    assert!(partial.len() < original.len(), "the restore stopped early");
    for (path, item) in &partial {
        if let Kind::File(_) = item.kind {
            assert_eq!(item.kind, original[path].kind, "{path:?} is partly written");
        }
    }
    no_temporary_files(&target);
    assert_eq!(env.lock_files(), 0, "the lock was released");

    // Without a way to merge, the same command refuses to run over what is there ...
    let err = env
        .restore(env.options(&backup.snapshot, &target))
        .await
        .expect_err("conflict");
    assert!(matches!(err, Error::RestoreConflict { .. }), "{err}");

    // ... and with skip it finishes the job.
    let mut options = env.options(&backup.snapshot, &target);
    options.on_conflict = ConflictPolicy::Skip;
    let report = env.restore(options).await.expect("resume");
    assert!(report.is_complete(), "{report:?}");
    let finished = capture(&root);
    assert_eq!(
        finished.keys().collect::<Vec<_>>(),
        original.keys().collect::<Vec<_>>()
    );
    for (path, expected) in &original {
        assert_eq!(finished[path].kind, expected.kind, "{path:?}");
        if let Kind::File(_) = expected.kind {
            // Directories that already existed keep the time the resumed run gave them by writing
            // into them; only a run in overwrite mode sets them again.
            assert_eq!(finished[path].modified, expected.modified, "{path:?}");
            assert_eq!(finished[path].mode, expected.mode, "{path:?}");
        }
    }
    no_temporary_files(&target);

    let mut options = env.options(&backup.snapshot, &target);
    options.on_conflict = ConflictPolicy::Overwrite;
    let report = env.restore(options).await.expect("overwrite");
    assert!(report.is_complete(), "{report:?}");
    let after = capture(&root);
    for (path, expected) in &original {
        assert_eq!(after[path], *expected, "{path:?} after an overwrite run");
    }
}

#[tokio::test]
async fn a_restore_cancelled_before_it_starts_does_nothing() {
    let env = Env::new().await;
    write(&env.path("src"), "a.txt", b"one");
    let backup = env.backup(&["src"]).await;
    let target = env.path("out");
    let options = env.options(&backup.snapshot, &target);
    options.cancel.cancel();

    let err = env.restore(options).await.expect_err("cancelled");
    assert!(matches!(err, Error::RestoreCancelled));
    assert!(!target.exists());
}

#[tokio::test]
async fn a_stale_temporary_file_from_a_killed_restore_is_neither_trusted_nor_deleted() {
    let env = Env::new().await;
    write(&env.path("src"), "a.txt", b"the real content");
    let backup = env.backup(&["src"]).await;
    let source = env.source("src");
    let target = env.path("out");
    let root = target.join(source.strip_prefix("/").expect("absolute"));
    fs::create_dir_all(&root).expect("mkdir");
    let stale = root.join(".vaultsnap-tmp-00000000deadbeef");
    fs::write(&stale, b"half a file").expect("write");

    let report = env.restore_ok(&backup.snapshot, &target).await;
    assert_eq!(
        fs::read(restored_root(&report, &source).join("a.txt")).expect("read"),
        b"the real content"
    );
    assert_eq!(fs::read(&stale).expect("still there"), b"half a file");
}
