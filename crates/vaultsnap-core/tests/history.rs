//! Integration tests of snapshot inspection against real repositories made by real backups:
//! ordering, what changed between backups, unusual file names, sizes checked against the object
//! files on disk, and the guarantee that everything shown was authenticated.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tempfile::TempDir;
use vaultsnap_core::history::{entries, Aspect, EntryType, Status};
use vaultsnap_core::{
    BackupOptions, BackupReport, ChunkerConfig, Diff, Error, KdfParams, Passphrase, Repository,
    RepositoryKeys, SnapshotId, SnapshotRef,
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

#[cfg(unix)]
fn bytes_of(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;

    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn bytes_of(path: &Path) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

fn reference(id: &SnapshotId) -> SnapshotRef {
    SnapshotRef::from(id)
}

fn pause() {
    // Creation times have nanosecond resolution, but a pause keeps consecutive backups apart even
    // on a coarse clock.
    std::thread::sleep(Duration::from_millis(10));
}

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

    /// The backed-up root as the snapshot records it (symbolic links in the temporary directory
    /// path are resolved).
    fn root(&self, relative: &str) -> PathBuf {
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

    /// Total size of every object file below `data/`.
    fn data_bytes(&self) -> u64 {
        let mut total = 0;
        let mut pending = vec![self.path("repo/data")];
        while let Some(dir) = pending.pop() {
            for entry in fs::read_dir(&dir).expect("read_dir") {
                let path = entry.expect("entry").path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    total += fs::metadata(&path).expect("metadata").len();
                }
            }
        }
        total
    }

    fn snapshot_file(&self, id: &SnapshotId) -> PathBuf {
        self.path("repo/snapshots").join(id.as_str())
    }

    fn corrupt(&self, id: &SnapshotId) {
        let path = self.snapshot_file(id);
        let mut bytes = fs::read(&path).expect("read snapshot file");
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0x01;
        fs::write(&path, bytes).expect("write snapshot file");
    }

    fn lock_files(&self) -> usize {
        fs::read_dir(self.path("repo/locks"))
            .expect("read locks")
            .count()
    }
}

// ---------------------------------------------------------------------------------------------
// Ordering and references
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn snapshots_are_listed_oldest_first_whatever_their_random_ids() {
    let env = Env::new().await;
    let src = env.path("src");
    let mut taken = Vec::new();
    for round in 0..5 {
        write(&src, "a.txt", format!("version {round}").as_bytes());
        taken.push(env.backup(&["src"]).await.snapshot);
        pause();
    }

    let list = env.repo.snapshots(&env.keys).await.expect("list");
    let listed: Vec<SnapshotId> = list
        .snapshots
        .iter()
        .map(|i| i.summary.id.clone())
        .collect();
    assert_eq!(listed, taken, "the order of creation, not of the ids");
    for pair in list.snapshots.windows(2) {
        assert!(pair[0].summary.created < pair[1].summary.created);
    }
    assert!(list.damaged.is_empty());
    assert_eq!(env.lock_files(), 0, "the lock was released");

    let again = env.repo.snapshots(&env.keys).await.expect("list");
    let relisted: Vec<SnapshotId> = again
        .snapshots
        .iter()
        .map(|i| i.summary.id.clone())
        .collect();
    assert_eq!(relisted, taken, "the same order every time");
}

#[tokio::test]
async fn a_snapshot_is_found_by_the_start_of_its_id() {
    let env = Env::new().await;
    write(&env.path("src"), "a.txt", b"one");
    let report = env.backup(&["src"]).await;
    let id = report.snapshot.as_str();

    for length in [8, 20, 64] {
        let prefix: SnapshotRef = id[..length].parse().expect("syntax");
        let manifest = env.repo.snapshot(&env.keys, &prefix).await.expect("found");
        assert_eq!(manifest.id, report.snapshot);
        let details = env
            .repo
            .snapshot_details(&env.keys, &prefix)
            .await
            .expect("found");
        assert_eq!(details.manifest.id, report.snapshot);
    }
    let other = if id.starts_with('0') { "1111" } else { "0000" };
    let missing: SnapshotRef = other.parse().expect("syntax");
    let err = env
        .repo
        .snapshot(&env.keys, &missing)
        .await
        .expect_err("no such snapshot");
    assert!(matches!(err, Error::SnapshotNotFound(_)), "{err}");
}

// ---------------------------------------------------------------------------------------------
// Changes between backups
// ---------------------------------------------------------------------------------------------

/// `path` relative to `root`, as text.
fn relative(root: &Path, path: &[u8]) -> String {
    let prefix = bytes_of(root);
    let rest = path
        .strip_prefix(prefix.as_slice())
        .expect("below the root");
    let rest = rest.strip_prefix(b"/").unwrap_or(rest);
    String::from_utf8_lossy(rest).into_owned()
}

#[tokio::test]
async fn the_changes_between_two_backups_are_added_deleted_and_modified_paths() {
    let env = Env::new().await;
    let src = env.path("src");
    write(&src, "keep.txt", b"never touched");
    write(&src, "edit.txt", b"original text");
    write(&src, "remove.txt", b"to be deleted");
    write(&src, "dir/inner.txt", b"stays");
    write(&src, "dir/gone.txt", b"also deleted");
    let big = pseudo_random(200_000, 1);
    write(&src, "big.bin", &big);
    let first = env.backup(&["src"]).await;
    pause();

    write(&src, "edit.txt", b"edited, and longer than before");
    fs::remove_file(src.join("remove.txt")).expect("remove");
    fs::remove_file(src.join("dir/gone.txt")).expect("remove");
    write(&src, "added.txt", b"brand new");
    write(&src, "newdir/file.txt", b"in a new directory");
    let mut inserted = big.clone();
    inserted.splice(
        100_000..100_000,
        b"<<< inserted into the middle >>>".iter().copied(),
    );
    write(&src, "big.bin", &inserted);
    let second = env.backup(&["src"]).await;

    let diff = env
        .repo
        .compare_snapshots(
            &env.keys,
            &reference(&first.snapshot),
            &reference(&second.snapshot),
        )
        .await
        .expect("compare");
    let root = env.root("src");
    let changes: BTreeMap<String, (Status, Vec<Aspect>)> = diff
        .changes
        .iter()
        .map(|change| {
            (
                relative(&root, &change.path),
                (change.status, change.aspects.clone()),
            )
        })
        .collect();

    let statuses: BTreeMap<&str, Status> = changes
        .iter()
        .map(|(path, (status, _))| (path.as_str(), *status))
        .collect();
    assert_eq!(
        statuses,
        BTreeMap::from([
            ("added.txt", Status::Added),
            ("big.bin", Status::Modified),
            ("dir/gone.txt", Status::Deleted),
            ("edit.txt", Status::Modified),
            ("newdir", Status::Added),
            ("newdir/file.txt", Status::Added),
            ("remove.txt", Status::Deleted),
        ]),
        "unchanged files and directories whose only change is a time are not listed"
    );
    assert!(changes["edit.txt"].1.contains(&Aspect::Content));
    assert!(changes["big.bin"].1.contains(&Aspect::Content));

    assert_eq!(diff.count(Status::Added), 3);
    assert_eq!(diff.count(Status::Deleted), 2);
    assert_eq!(diff.count(Status::Modified), 2);
    assert_eq!(diff.added_bytes, 9 + 18, "added.txt and newdir/file.txt");
    assert_eq!(diff.deleted_bytes, 13 + 12, "remove.txt and dir/gone.txt");
    assert_eq!(
        diff.unchanged, 4,
        "the root, keep.txt, dir, and dir/inner.txt"
    );
    assert_eq!(
        (diff.from.id.clone(), diff.to.id.clone()),
        (first.snapshot, second.snapshot)
    );
    assert!(!diff.chunker_differs);
    assert_eq!(diff.missing_chunks, 0);

    // Chunking is content-defined, so inserting into the middle of a big file leaves most of its
    // chunks shared between the two snapshots.
    let manifest = env
        .repo
        .snapshot(&env.keys, &reference(&diff.to.id))
        .await
        .expect("load");
    let big_chunks = entries(&manifest, None)
        .find(|listed| listed.path.ends_with(b"/big.bin"))
        .map(|listed| match &listed.entry.kind {
            vaultsnap_core::snapshot::NodeKind::File { chunks } => chunks.len(),
            other => panic!("big.bin is a file, not {other:?}"),
        })
        .expect("big.bin is listed");
    assert!(big_chunks > 20, "{big_chunks}");
    assert!(
        (diff.chunks_only_in_to as usize) < big_chunks / 2,
        "{} new chunks of {big_chunks} in the file alone",
        diff.chunks_only_in_to
    );
    assert!(diff.stored_only_in_to > 0 && diff.stored_only_in_from > 0);
    assert!(diff.chunks_only_in_from > 0);
}

#[tokio::test]
async fn comparing_in_the_other_direction_swaps_added_and_deleted() {
    let env = Env::new().await;
    let src = env.path("src");
    write(&src, "common.txt", b"same");
    write(&src, "only-first.txt", b"first");
    let first = env.backup(&["src"]).await;
    pause();
    fs::remove_file(src.join("only-first.txt")).expect("remove");
    write(&src, "only-second.txt", b"second");
    let second = env.backup(&["src"]).await;

    let forward = env
        .repo
        .compare_snapshots(
            &env.keys,
            &reference(&first.snapshot),
            &reference(&second.snapshot),
        )
        .await
        .expect("compare");
    let backward = env
        .repo
        .compare_snapshots(
            &env.keys,
            &reference(&second.snapshot),
            &reference(&first.snapshot),
        )
        .await
        .expect("compare");
    assert_eq!(
        (forward.count(Status::Added), forward.count(Status::Deleted)),
        (1, 1)
    );
    let file_name = |diff: &Diff, status: Status| -> String {
        let change = diff
            .changes
            .iter()
            .find(|c| c.status == status)
            .expect("a change");
        let text = String::from_utf8_lossy(&change.path).into_owned();
        text.rsplit('/').next().unwrap_or_default().to_owned()
    };
    assert_eq!(file_name(&forward, Status::Added), "only-second.txt");
    assert_eq!(file_name(&forward, Status::Deleted), "only-first.txt");
    assert_eq!(file_name(&backward, Status::Added), "only-first.txt");
    assert_eq!(file_name(&backward, Status::Deleted), "only-second.txt");
    assert_eq!(forward.stored_only_in_from, backward.stored_only_in_to);
    assert_eq!(forward.stored_only_in_to, backward.stored_only_in_from);
}

#[tokio::test]
async fn a_snapshot_compared_with_itself_has_no_changes() {
    let env = Env::new().await;
    write(&env.path("src"), "a.txt", b"one");
    let report = env.backup(&["src"]).await;
    let id = reference(&report.snapshot);

    let diff = env
        .repo
        .compare_snapshots(&env.keys, &id, &id)
        .await
        .expect("compare");
    assert!(diff.is_empty());
    assert_eq!(diff.unchanged, 2);
    assert_eq!((diff.stored_only_in_from, diff.stored_only_in_to), (0, 0));
}

#[cfg(unix)]
#[tokio::test]
async fn permission_and_link_changes_are_named() {
    use std::os::unix::fs::PermissionsExt;

    let env = Env::new().await;
    let src = env.path("src");
    write(&src, "script.sh", b"#!/bin/sh\n");
    write(&src, "target-a", b"a");
    write(&src, "target-b", b"b");
    std::os::unix::fs::symlink("target-a", src.join("link")).expect("symlink");
    let first = env.backup(&["src"]).await;
    pause();

    fs::set_permissions(src.join("script.sh"), fs::Permissions::from_mode(0o755)).expect("chmod");
    fs::remove_file(src.join("link")).expect("remove link");
    std::os::unix::fs::symlink("target-b", src.join("link")).expect("symlink");
    let second = env.backup(&["src"]).await;

    let diff = env
        .repo
        .compare_snapshots(
            &env.keys,
            &reference(&first.snapshot),
            &reference(&second.snapshot),
        )
        .await
        .expect("compare");
    let root = env.root("src");
    let find = |name: &str| {
        diff.changes
            .iter()
            .find(|change| relative(&root, &change.path) == name)
            .unwrap_or_else(|| panic!("{name} should have changed: {:?}", diff.changes))
    };
    assert!(find("script.sh").aspects.contains(&Aspect::Mode));
    assert!(!find("script.sh").aspects.contains(&Aspect::Content));
    assert!(find("link").aspects.contains(&Aspect::LinkTarget));
    assert_eq!(find("link").kind, EntryType::Symlink);
}

// ---------------------------------------------------------------------------------------------
// Unusual file names
// ---------------------------------------------------------------------------------------------

#[cfg(unix)]
fn unusual_names() -> Vec<Vec<u8>> {
    vec![
        b"plain.txt".to_vec(),
        b"with space.txt".to_vec(),
        b"new\nline.txt".to_vec(),
        b"tab\there".to_vec(),
        b"-dash-first".to_vec(),
        b"quote\"s'.txt".to_vec(),
        b"back\\slash".to_vec(),
        b"ansi\x1b[31mred".to_vec(),
        b".hidden".to_vec(),
        b"trailing dot.".to_vec(),
        "ünïcödé 日本語.txt".as_bytes().to_vec(),
        "emoji-😀".as_bytes().to_vec(),
        b"*.glob?[chars]".to_vec(),
    ]
}

#[cfg(unix)]
fn create(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    fs::write(path, contents)
}

#[cfg(unix)]
fn name_path(dir: &Path, name: &[u8]) -> PathBuf {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    dir.join(OsStr::from_bytes(name))
}

#[cfg(unix)]
#[tokio::test]
async fn unusual_names_come_back_byte_for_byte_and_can_be_selected_by_path() {
    let env = Env::new().await;
    let src = env.path("src");
    fs::create_dir_all(&src).expect("mkdir");
    let mut names = unusual_names();
    names.push(b"not-utf8-\xff\xfe".to_vec());
    let mut created = Vec::new();
    for name in &names {
        match create(&name_path(&src, name), b"content") {
            Ok(()) => created.push(name.clone()),
            // Some file systems refuse some names (macOS refuses names that are not UTF-8).
            Err(err) => eprintln!("skipped {:?}: {err}", String::from_utf8_lossy(name)),
        }
    }
    assert!(
        created.len() >= 10,
        "most names must be creatable: {}",
        created.len()
    );
    let report = env.backup(&["src"]).await;

    let manifest = env
        .repo
        .snapshot(&env.keys, &reference(&report.snapshot))
        .await
        .expect("load");
    let root = bytes_of(&env.root("src"));
    let listed: Vec<Vec<u8>> = entries(&manifest, None).map(|listed| listed.path).collect();
    for name in &created {
        let mut expected = root.clone();
        expected.push(b'/');
        expected.extend_from_slice(name);
        assert!(
            listed.contains(&expected),
            "{:?} is missing from the listing",
            String::from_utf8_lossy(name)
        );
    }
    assert_eq!(listed.len(), created.len() + 1, "the root and nothing else");

    // Selecting by path takes whole components, whatever characters they hold.
    for name in &created {
        let path = name_path(&env.root("src"), name);
        let selected: Vec<_> = entries(&manifest, Some(path.as_path())).collect();
        assert_eq!(selected.len(), 1, "{:?}", String::from_utf8_lossy(name));
        assert!(selected[0].path.ends_with(name));
    }

    // A change to such a file is reported under its exact name.
    pause();
    let target = created
        .iter()
        .find(|name| name.contains(&b'\n'))
        .expect("created");
    create(&name_path(&src, target), b"changed content").expect("rewrite");
    let second = env.backup(&["src"]).await;
    let diff = env
        .repo
        .compare_snapshots(
            &env.keys,
            &reference(&report.snapshot),
            &reference(&second.snapshot),
        )
        .await
        .expect("compare");
    assert_eq!(diff.changes.len(), 1);
    let mut expected = root;
    expected.push(b'/');
    expected.extend_from_slice(target);
    assert_eq!(diff.changes[0].path, expected);
    assert_eq!(diff.changes[0].status, Status::Modified);
}

#[cfg(unix)]
#[tokio::test]
async fn the_listing_does_not_depend_on_the_order_the_files_were_created_in() {
    let env = Env::new().await;
    let names = unusual_names();
    let reversed: Vec<Vec<u8>> = names.iter().rev().cloned().collect();
    for (directory, order) in [("one", &names), ("two", &reversed)] {
        let dir = env.path(directory);
        fs::create_dir_all(&dir).expect("mkdir");
        for name in order {
            create(&name_path(&dir, name), b"same").expect("create");
        }
    }
    let first = env.backup(&["one"]).await;
    let second = env.backup(&["two"]).await;

    let listing = |report: &BackupReport, root: PathBuf| {
        let repo = &env.repo;
        let keys = &env.keys;
        let id = reference(&report.snapshot);
        async move {
            let manifest = repo.snapshot(keys, &id).await.expect("load");
            let prefix = bytes_of(&root);
            entries(&manifest, None)
                .map(|listed| listed.path[prefix.len()..].to_vec())
                .collect::<Vec<_>>()
        }
    };
    let one = listing(&first, env.root("one")).await;
    let two = listing(&second, env.root("two")).await;
    assert_eq!(one, two);
}

// ---------------------------------------------------------------------------------------------
// Sizes
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn sizes_agree_with_the_files_and_the_objects_on_disk() {
    let env = Env::new().await;
    let src = env.path("src");
    let shared = pseudo_random(50_000, 5);
    write(&src, "copy1.bin", &shared);
    write(&src, "copy2.bin", &shared);
    write(&src, "other.bin", &pseudo_random(30_000, 6));
    write(&src, "empty", b"");
    let first = env.backup(&["src"]).await;

    let list = env.repo.snapshots(&env.keys).await.expect("list");
    let sizes = &list.snapshots[0].sizes;
    assert_eq!(sizes.logical_bytes, 130_000);
    assert_eq!(
        sizes.logical_bytes - sizes.deduplicated_bytes,
        50_000,
        "the second copy adds nothing after deduplication"
    );
    assert!(
        sizes.stored_bytes > 80_000 && sizes.stored_bytes < 100_000,
        "{}",
        sizes.stored_bytes
    );
    assert_eq!(
        sizes.stored_bytes,
        env.data_bytes(),
        "the only snapshot owns every object"
    );
    assert_eq!(sizes.unique_stored_bytes, sizes.stored_bytes);
    assert_eq!(sizes.unique_chunks, sizes.chunks);
    assert_eq!(sizes.missing_chunks, 0);
    assert_eq!(list.referenced_stored_bytes, env.data_bytes());

    // The same tree again stores nothing new, and then nothing is particular to either snapshot.
    pause();
    let second = env.backup(&["src"]).await;
    assert_eq!(second.transfer.chunks_new, 0);
    let list = env.repo.snapshots(&env.keys).await.expect("list");
    assert_eq!(list.snapshots.len(), 2);
    for info in &list.snapshots {
        assert_eq!(info.sizes.stored_bytes, env.data_bytes());
        assert_eq!(info.sizes.unique_stored_bytes, 0);
        assert_eq!(info.sizes.unique_chunks, 0);
    }
    assert_eq!(list.referenced_stored_bytes, env.data_bytes());

    let details = env
        .repo
        .snapshot_details(&env.keys, &reference(&first.snapshot))
        .await
        .expect("details");
    assert_eq!(details.sizes, list.snapshots[0].sizes);
    assert!(details.unique_sizes_exact);
    assert_eq!(details.roots.len(), 1);
    assert_eq!(details.roots[0].files, 4);
    assert_eq!(details.roots[0].bytes, 130_000);
}

#[tokio::test]
async fn data_added_by_a_later_backup_is_unique_to_it() {
    let env = Env::new().await;
    let src = env.path("src");
    write(&src, "base.bin", &pseudo_random(40_000, 7));
    env.backup(&["src"]).await;
    let before = env.data_bytes();
    pause();
    write(&src, "extra.bin", &pseudo_random(25_000, 8));
    env.backup(&["src"]).await;

    let list = env.repo.snapshots(&env.keys).await.expect("list");
    let (older, newer) = (&list.snapshots[0].sizes, &list.snapshots[1].sizes);
    assert_eq!(
        older.unique_stored_bytes, 0,
        "everything the first has, the second has too"
    );
    assert_eq!(newer.unique_stored_bytes, env.data_bytes() - before);
    assert_eq!(newer.stored_bytes, env.data_bytes());
    assert_eq!(newer.logical_bytes, 65_000);
}

// ---------------------------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_tampered_snapshot_is_never_shown() {
    let env = Env::new().await;
    write(&env.path("src"), "a.txt", b"one");
    let good = env.backup(&["src"]).await;
    pause();
    write(&env.path("src"), "a.txt", b"two");
    let bad = env.backup(&["src"]).await;
    env.corrupt(&bad.snapshot);

    let err = env
        .repo
        .snapshot(&env.keys, &reference(&bad.snapshot))
        .await
        .expect_err("tampered");
    assert!(matches!(err, Error::CorruptSnapshot { .. }), "{err}");
    let err = env
        .repo
        .snapshot_details(&env.keys, &reference(&bad.snapshot))
        .await;
    assert!(matches!(err, Err(Error::CorruptSnapshot { .. })));
    let err = env
        .repo
        .compare_snapshots(
            &env.keys,
            &reference(&good.snapshot),
            &reference(&bad.snapshot),
        )
        .await;
    assert!(matches!(err, Err(Error::CorruptSnapshot { .. })));

    let list = env
        .repo
        .snapshots(&env.keys)
        .await
        .expect("the listing itself succeeds");
    assert_eq!(list.snapshots.len(), 1);
    assert_eq!(list.snapshots[0].summary.id, good.snapshot);
    assert_eq!(list.damaged.len(), 1);
    assert_eq!(list.damaged[0].id, bad.snapshot);
    assert!(!list.unique_sizes_exact());

    let details = env
        .repo
        .snapshot_details(&env.keys, &reference(&good.snapshot))
        .await
        .expect("the good snapshot is still readable");
    assert!(
        !details.unique_sizes_exact,
        "a damaged neighbour makes unique sizes uncertain"
    );
}

#[tokio::test]
async fn one_snapshot_file_cannot_stand_in_for_another() {
    let env = Env::new().await;
    write(&env.path("src"), "a.txt", b"one");
    let first = env.backup(&["src"]).await;
    pause();
    write(&env.path("src"), "a.txt", b"two");
    let second = env.backup(&["src"]).await;
    // A valid, authentic snapshot file under the wrong name.
    fs::copy(
        env.snapshot_file(&second.snapshot),
        env.snapshot_file(&first.snapshot),
    )
    .expect("copy");

    let err = env
        .repo
        .snapshot(&env.keys, &reference(&first.snapshot))
        .await
        .expect_err("swapped");
    assert!(matches!(err, Error::CorruptSnapshot { .. }), "{err}");
    let list = env.repo.snapshots(&env.keys).await.expect("list");
    assert_eq!(list.damaged.len(), 1);
    assert_eq!(list.damaged[0].id, first.snapshot);
}

#[tokio::test]
async fn the_wrong_keys_open_nothing() {
    let env = Env::new().await;
    write(&env.path("src"), "a.txt", b"one");
    let report = env.backup(&["src"]).await;
    let stranger = Env::new().await;

    let err = env
        .repo
        .snapshot(&stranger.keys, &reference(&report.snapshot))
        .await;
    assert!(matches!(err, Err(Error::CorruptSnapshot { .. })), "{err:?}");
    let list = env
        .repo
        .snapshots(&stranger.keys)
        .await
        .expect("the listing itself succeeds");
    assert!(list.snapshots.is_empty());
    assert_eq!(list.damaged.len(), 1);
}

#[tokio::test]
async fn inspecting_never_changes_the_repository() {
    let env = Env::new().await;
    write(&env.path("src"), "a.txt", b"one");
    let first = env.backup(&["src"]).await;
    pause();
    write(&env.path("src"), "a.txt", b"two");
    let second = env.backup(&["src"]).await;

    let snapshot = |id: &SnapshotId| fs::read(env.snapshot_file(id)).expect("read");
    let before = (
        snapshot(&first.snapshot),
        snapshot(&second.snapshot),
        env.data_bytes(),
    );
    env.repo.snapshots(&env.keys).await.expect("list");
    env.repo
        .snapshot_details(&env.keys, &reference(&first.snapshot))
        .await
        .expect("details");
    env.repo
        .compare_snapshots(
            &env.keys,
            &reference(&first.snapshot),
            &reference(&second.snapshot),
        )
        .await
        .expect("compare");
    let after = (
        snapshot(&first.snapshot),
        snapshot(&second.snapshot),
        env.data_bytes(),
    );
    assert_eq!(before, after);
    assert_eq!(env.lock_files(), 0);
}
