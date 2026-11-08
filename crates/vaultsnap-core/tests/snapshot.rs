//! End-to-end tests of snapshot manifests: a real repository, the real scanner, chunker, and chunk
//! store, and the snapshot store. These do everything a backup does short of the command itself;
//! restoring is not implemented, so file content is checked by reading chunks back directly.

use std::fs;
use std::path::Path;

use tempfile::TempDir;
use vaultsnap_core::scan::{EntryKind, ScanItem, ScanOptions, Scanner, Timestamp};
use vaultsnap_core::snapshot::{BackupConfig, ChunkRef, ManifestEntry, NodeKind};
use vaultsnap_core::{
    ChunkStore, Chunker, ChunkerConfig, Error, KdfParams, Manifest, ManifestBuilder, Passphrase,
    Repository, SnapshotStore,
};

fn passphrase() -> Passphrase {
    Passphrase::new(b"correct horse battery staple".to_vec()).expect("valid passphrase")
}

fn chunker_config() -> ChunkerConfig {
    ChunkerConfig::new(1024, 4096, 16_384).expect("valid config")
}

fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
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

struct Repo {
    _dir: TempDir,
    path: std::path::PathBuf,
    repository_id: String,
    chunks: ChunkStore,
    snapshots: SnapshotStore,
}

async fn repository() -> Repo {
    let dir = tempfile::tempdir().expect("create temporary directory");
    let path = dir.path().join("repo");
    let repo = Repository::at(&path);
    let kdf = KdfParams {
        m_cost_kib: 64,
        t_cost: 1,
        p_cost: 1,
    };
    let config = repo.init_with_kdf(&passphrase(), kdf).await.expect("init");
    let keys = repo.unlock(&passphrase()).await.expect("unlock");
    Repo {
        chunks: repo.chunk_store(&keys).expect("chunk store"),
        snapshots: repo.snapshot_store(&keys).await.expect("snapshot store"),
        repository_id: config.repository_id,
        path,
        _dir: dir,
    }
}

/// Does what a backup does: scan, chunk and store every file, and build the manifest.
fn take_snapshot(repo: &Repo, source: &Path, options: ScanOptions, tags: &[&str]) -> Manifest {
    let scanner = Scanner::new(source, options.clone()).expect("scanner");
    let mut builder = ManifestBuilder::new(
        repo.repository_id.clone(),
        BackupConfig::new(chunker_config(), &options),
    )
    .host("test-host")
    .tags(tags.iter().map(|tag| (*tag).to_owned()).collect());
    let root = builder.add_root(scanner.root());
    for item in scanner.scan() {
        match item.expect("scan") {
            ScanItem::Entry(entry) => {
                let mut refs = Vec::new();
                if entry.is_file() {
                    let reader = scanner.open(&entry).expect("open");
                    for chunk in Chunker::new(reader, chunker_config()) {
                        let chunk = chunk.expect("read chunk");
                        let put = repo.chunks.put(&chunk.data).expect("store chunk");
                        let len = u32::try_from(chunk.data.len()).expect("small chunk");
                        refs.push(ChunkRef { id: put.id, len });
                    }
                }
                builder.add_entry(root, &entry, refs).expect("add entry");
            }
            ScanItem::Skipped(skipped) => {
                builder.add_skipped(root, &skipped).expect("add skipped");
            }
        }
    }
    builder.finish(Timestamp::now()).expect("finish manifest")
}

/// The content of a file entry, read back from the chunk store.
fn content(repo: &Repo, entry: &ManifestEntry) -> Vec<u8> {
    let NodeKind::File { chunks } = &entry.kind else {
        panic!("not a file");
    };
    let mut data = Vec::new();
    for chunk in chunks {
        let bytes = repo.chunks.get(&chunk.id).expect("chunk is stored");
        assert_eq!(bytes.len(), chunk.len as usize);
        data.extend_from_slice(&bytes);
    }
    data
}

fn source_tree() -> TempDir {
    let dir = tempfile::tempdir().expect("create temporary directory");
    let root = dir.path();
    write(root, "readme.txt", b"hello snapshot");
    write(root, "empty.bin", b"");
    write(
        root,
        "docs/report-2024.txt",
        &b"quarterly numbers\n".repeat(2000),
    );
    write(root, "docs/archive/data.bin", &pseudo_random(120_000, 1));
    write(root, "docs/archive/notes", b"x");
    write(root, "unicode/日本語 ファイル.txt", "こんにちは".as_bytes());
    write(root, "with space/new\nline", b"odd names");
    fs::create_dir(root.join("empty-dir")).expect("mkdir");
    dir
}

#[tokio::test]
async fn a_backup_commits_and_loads_back_with_its_full_structure() {
    let repo = repository().await;
    let source = source_tree();
    let manifest = take_snapshot(&repo, source.path(), ScanOptions::default(), &["first"]);
    repo.snapshots.commit(&manifest).expect("commit");

    let loaded = repo.snapshots.load(&manifest.id).expect("load");
    assert_eq!(loaded, manifest);
    assert_eq!(loaded.tags, ["first"]);
    assert_eq!(loaded.host, "test-host");
    assert_eq!(loaded.repository_id, repo.repository_id);
    assert_eq!(loaded.config.chunker, chunker_config());
    assert_eq!(loaded.roots.len(), 1);
    assert!(loaded.stats.skipped == 0 && loaded.skipped.is_empty());

    // The recorded tree matches the scan, entry for entry and in the same order.
    let scanner = Scanner::new(source.path(), ScanOptions::default()).expect("scanner");
    let scanned: Vec<_> = scanner
        .scan()
        .map(|item| match item.expect("scan") {
            ScanItem::Entry(entry) => entry,
            ScanItem::Skipped(skipped) => panic!("unexpected skip {skipped:?}"),
        })
        .collect();
    let root = &loaded.roots[0];
    assert_eq!(root.entries.len(), scanned.len());
    for (index, entry) in scanned.iter().enumerate() {
        let recorded = root.relative_path(index);
        let expected = entry.path.to_string_lossy().replace('\\', "/");
        assert_eq!(
            String::from_utf8_lossy(&recorded),
            expected,
            "entry {index}"
        );
        let kind_matches = matches!(
            (&entry.kind, &root.entries[index].kind),
            (EntryKind::Directory, NodeKind::Directory) | (EntryKind::File, NodeKind::File { .. })
        );
        assert!(
            kind_matches,
            "entry {index}: {:?} vs {:?}",
            entry.kind, root.entries[index].kind
        );
        let attributes = &root.entries[index].attributes;
        assert_eq!(attributes.mode, entry.metadata.mode);
        assert_eq!(attributes.modified, entry.metadata.modified);
    }

    // Every file's ordered chunk list reproduces exactly the bytes on disk.
    let mut files = 0;
    for (index, entry) in root.entries.iter().enumerate() {
        if matches!(entry.kind, NodeKind::File { .. }) {
            let relative = String::from_utf8(root.relative_path(index)).expect("utf-8 names here");
            let on_disk = fs::read(source.path().join(&relative)).expect("read source file");
            assert_eq!(content(&repo, entry), on_disk, "{relative}");
            assert_eq!(entry.attributes.size, on_disk.len() as u64);
            files += 1;
        }
    }
    assert_eq!(files, 7);
    assert_eq!(loaded.stats.files, 7);
    assert_eq!(loaded.stats.directories, 6);
}

#[tokio::test]
async fn large_files_are_recorded_as_ordered_multi_chunk_lists() {
    let repo = repository().await;
    let source = source_tree();
    let manifest = take_snapshot(&repo, source.path(), ScanOptions::default(), &[]);
    let root = &manifest.roots[0];
    let (index, entry) = root
        .entries
        .iter()
        .enumerate()
        .find(|(index, _)| root.relative_path(*index) == b"docs/archive/data.bin")
        .expect("the large file is recorded");
    let NodeKind::File { chunks } = &entry.kind else {
        panic!("not a file");
    };
    assert!(chunks.len() > 5, "{} chunks", chunks.len());
    assert_eq!(
        chunks.iter().map(|c| u64::from(c.len)).sum::<u64>(),
        120_000
    );
    assert!(chunks.iter().all(|c| (1..=16_384).contains(&c.len)));
    assert_eq!(
        content(&repo, entry),
        pseudo_random(120_000, 1),
        "entry {index}"
    );
}

#[tokio::test]
async fn historical_snapshots_stay_intact_and_share_chunks() {
    let repo = repository().await;
    let source = source_tree();
    let first = take_snapshot(&repo, source.path(), ScanOptions::default(), &["monday"]);
    repo.snapshots.commit(&first).expect("commit first");
    let chunk_objects_after_first = count_files(&repo.path.join("data"));

    // Change the tree a little and take a second snapshot.
    write(source.path(), "readme.txt", b"hello again, snapshot");
    write(source.path(), "added/new.txt", b"fresh file");
    fs::remove_file(source.path().join("docs/archive/notes")).expect("remove");
    let second = take_snapshot(&repo, source.path(), ScanOptions::default(), &["tuesday"]);
    repo.snapshots.commit(&second).expect("commit second");
    let new_objects = count_files(&repo.path.join("data")) - chunk_objects_after_first;
    assert!(
        new_objects <= 3,
        "{new_objects} new chunk objects for a tiny change"
    );

    let ids = repo.snapshots.ids().expect("ids");
    assert_eq!(ids.len(), 2);
    assert_ne!(first.id, second.id);
    assert_eq!(repo.snapshots.load(&first.id).expect("load first"), first);
    assert_eq!(
        repo.snapshots.load(&second.id).expect("load second"),
        second
    );

    // The first snapshot still describes the tree as it was.
    let old = &first.roots[0];
    let readme = old
        .entries
        .iter()
        .enumerate()
        .find(|(i, _)| old.relative_path(*i) == b"readme.txt")
        .map(|(_, e)| e)
        .expect("readme in the first snapshot");
    assert_eq!(content(&repo, readme), b"hello snapshot");
    assert!(first.roots[0].entries.iter().any(|e| e.name == b"notes"));
    assert!(!second.roots[0].entries.iter().any(|e| e.name == b"notes"));

    let listing = repo.snapshots.list().expect("list");
    let mut tags: Vec<_> = listing
        .iter()
        .map(|l| l.summary.as_ref().expect("healthy").tags.clone())
        .collect();
    tags.sort();
    assert_eq!(
        tags,
        [vec!["monday".to_owned()], vec!["tuesday".to_owned()]]
    );
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

#[tokio::test]
async fn an_interrupted_backup_never_leaves_a_snapshot() {
    let repo = repository().await;
    let source = source_tree();

    // The backup stores its chunks and builds the manifest, then "crashes" before the commit.
    let manifest = take_snapshot(&repo, source.path(), ScanOptions::default(), &[]);
    assert!(repo.snapshots.ids().expect("ids").is_empty());
    assert!(repo.snapshots.list().expect("list").is_empty());
    assert!(matches!(
        repo.snapshots.load(&manifest.id),
        Err(Error::SnapshotNotFound(_))
    ));

    // A chunk that went missing (for example removed by a concurrent prune) blocks the commit.
    let victim = manifest.roots[0]
        .entries
        .iter()
        .find_map(|entry| match &entry.kind {
            NodeKind::File { chunks } => chunks.first().copied(),
            _ => None,
        })
        .expect("a file with chunks");
    let hex = victim.id.to_hex();
    let object = repo.path.join("data").join(&hex[..2]).join(&hex);
    let saved = fs::read(&object).expect("read chunk object");
    fs::remove_file(&object).expect("remove chunk object");
    let err = repo
        .snapshots
        .commit(&manifest)
        .expect_err("must not commit");
    assert!(matches!(err, Error::IncompleteSnapshot { .. }), "{err}");
    assert!(repo.snapshots.ids().expect("ids").is_empty());

    // Once the chunk is back the very same manifest commits.
    fs::write(&object, saved).expect("restore chunk object");
    repo.snapshots.commit(&manifest).expect("commit");
    assert_eq!(
        repo.snapshots.ids().expect("ids"),
        std::slice::from_ref(&manifest.id)
    );
}

#[tokio::test]
async fn a_partial_backup_records_what_it_skipped() {
    let repo = repository().await;
    let source = source_tree();
    // A snapshot of a tree with something unreadable is still a snapshot, but says so.
    #[cfg(unix)]
    let _restore = {
        use std::os::unix::fs::PermissionsExt;

        struct Unlock(std::path::PathBuf);
        impl Drop for Unlock {
            fn drop(&mut self) {
                let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
            }
        }
        write(source.path(), "locked/inside.txt", b"hidden");
        let locked = source.path().join("locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod");
        let guard = Unlock(locked.clone());
        if fs::read_dir(&locked).is_ok() {
            eprintln!("note: running with privileges that bypass permissions; case skipped");
            return;
        }
        guard
    };

    let manifest = take_snapshot(&repo, source.path(), ScanOptions::default(), &[]);
    #[cfg(unix)]
    {
        assert_eq!(manifest.stats.skipped, 1);
        assert_eq!(manifest.skipped.len(), 1);
        assert_eq!(manifest.skipped[0].path, b"locked");
        assert!(matches!(
            manifest.skipped[0].reason,
            vaultsnap_core::snapshot::SkippedReason::Unreadable { .. }
        ));
    }
    repo.snapshots.commit(&manifest).expect("commit");
    let summary = repo.snapshots.load(&manifest.id).expect("load").summary();
    assert_eq!(summary.stats.skipped, manifest.stats.skipped);
    assert_eq!(summary.roots.len(), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn links_and_special_files_round_trip_through_the_manifest() {
    use std::os::unix::fs::symlink;

    let repo = repository().await;
    let dir = tempfile::tempdir().expect("create temporary directory");
    let root = dir.path();
    write(root, "a", b"shared data");
    fs::hard_link(root.join("a"), root.join("b")).expect("hard link");
    symlink("a", root.join("link")).expect("symlink");
    symlink("/definitely/not/here", root.join("dangling")).expect("symlink");

    let manifest = take_snapshot(&repo, root, ScanOptions::default(), &[]);
    repo.snapshots.commit(&manifest).expect("commit");
    let loaded = repo.snapshots.load(&manifest.id).expect("load");
    let entries = &loaded.roots[0].entries;
    let by_name = |name: &[u8]| entries.iter().find(|e| e.name == name).expect("entry");

    assert!(matches!(by_name(b"a").kind, NodeKind::File { .. }));
    assert_eq!(by_name(b"b").kind, NodeKind::HardLink { target: 1 });
    assert_eq!(
        by_name(b"link").kind,
        NodeKind::Symlink {
            target: b"a".to_vec()
        }
    );
    assert_eq!(
        by_name(b"dangling").kind,
        NodeKind::Symlink {
            target: b"/definitely/not/here".to_vec()
        }
    );
    assert_eq!(loaded.stats.hard_links, 1);
    assert_eq!(loaded.stats.symlinks, 2);
    assert_eq!(
        loaded.stats.files, 1,
        "the data of a hard-linked file is recorded once"
    );
}

#[tokio::test]
async fn nothing_that_identifies_the_backed_up_data_is_visible_in_the_repository() {
    let repo = repository().await;
    let source = tempfile::tempdir().expect("create temporary directory");
    write(
        source.path(),
        "confidential-merger-plan.txt",
        &b"acquire the competitor\n".repeat(500),
    );
    write(
        source.path(),
        "projects-secret-codename/budget.xlsx",
        &pseudo_random(30_000, 5),
    );
    let manifest = take_snapshot(
        &repo,
        source.path(),
        ScanOptions::default(),
        &["board-only"],
    );
    repo.snapshots.commit(&manifest).expect("commit");

    let source_path = source.path().to_string_lossy().into_owned();
    let secrets: Vec<&[u8]> = vec![
        b"confidential-merger-plan",
        b"projects-secret-codename",
        b"budget.xlsx",
        b"acquire the competitor",
        b"board-only",
        b"test-host",
        source_path.as_bytes(),
    ];
    let mut pending = vec![repo.path.clone()];
    let mut scanned_files = 0;
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).expect("read_dir") {
            let path = entry.expect("entry").path();
            let name = path
                .file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned();
            assert!(
                !name.contains("confidential") && !name.contains("secret"),
                "{name}"
            );
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            let bytes = fs::read(&path).expect("read repository file");
            for secret in &secrets {
                let found = bytes.windows(secret.len()).any(|window| window == *secret);
                assert!(
                    !found,
                    "{:?} found in {}",
                    String::from_utf8_lossy(secret),
                    path.display()
                );
            }
            scanned_files += 1;
        }
    }
    assert!(scanned_files >= 5, "{scanned_files} files scanned");
}

#[tokio::test]
async fn snapshots_survive_closing_and_reopening_the_repository() {
    let repo = repository().await;
    let source = source_tree();
    let manifest = take_snapshot(&repo, source.path(), ScanOptions::default(), &["persisted"]);
    repo.snapshots
        .commit_async(manifest.clone())
        .await
        .expect("commit");

    let reopened = Repository::at(&repo.path);
    let keys = reopened.unlock(&passphrase()).await.expect("unlock again");
    let snapshots = reopened
        .snapshot_store(&keys)
        .await
        .expect("snapshot store");
    assert_eq!(
        snapshots
            .load_async(manifest.id.clone())
            .await
            .expect("load"),
        manifest
    );

    let wrong = Passphrase::new(b"not the passphrase".to_vec()).expect("valid");
    assert!(matches!(
        reopened.unlock(&wrong).await,
        Err(Error::WrongPassphrase)
    ));
}

#[tokio::test]
async fn a_snapshot_from_another_repository_cannot_be_loaded_or_committed() {
    let first = repository().await;
    let second = repository().await;
    let source = source_tree();
    let manifest = take_snapshot(&first, source.path(), ScanOptions::default(), &[]);
    first.snapshots.commit(&manifest).expect("commit");

    // Copy the file into the other repository: different keys and a different repository id.
    let name = manifest.id.to_string();
    fs::copy(
        first.path.join("snapshots").join(&name),
        second.path.join("snapshots").join(&name),
    )
    .expect("copy");
    assert!(matches!(
        second.snapshots.load(&manifest.id),
        Err(Error::CorruptSnapshot { .. })
    ));
    assert!(matches!(
        second.snapshots.commit(&manifest),
        Err(Error::InvalidManifest(_))
    ));
}
