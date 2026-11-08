//! End-to-end tests of the chunk store through the public API: a real repository, real unlocking,
//! the real chunker, and a scan of everything written to disk.

use std::collections::HashSet;
use std::fs;
use std::io::Cursor;
use std::path::Path;

use vaultsnap_core::{
    ChunkId, ChunkStore, Chunker, ChunkerConfig, Error, KdfParams, Passphrase, Repository,
};

fn passphrase() -> Passphrase {
    Passphrase::new(b"correct horse battery staple".to_vec()).expect("valid passphrase")
}

fn cheap_kdf() -> KdfParams {
    KdfParams {
        m_cost_kib: 64,
        t_cost: 1,
        p_cost: 1,
    }
}

fn chunk_config() -> ChunkerConfig {
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

async fn new_repository(root: &Path) -> (Repository, ChunkStore) {
    let repo = Repository::at(root);
    repo.init_with_kdf(&passphrase(), cheap_kdf())
        .await
        .expect("init");
    let keys = repo.unlock(&passphrase()).await.expect("unlock");
    let chunks = repo.chunk_store(&keys).expect("chunk store");
    (repo, chunks)
}

/// Chunks `data` and stores every chunk; returns the ids in order and how many were new.
fn store_file(chunks: &ChunkStore, data: &[u8]) -> (Vec<ChunkId>, usize) {
    let mut ids = Vec::new();
    let mut new = 0;
    for chunk in Chunker::new(Cursor::new(data), chunk_config()) {
        let chunk = chunk.expect("read chunk");
        let put = chunks.put(&chunk.data).expect("put");
        new += usize::from(put.is_new());
        ids.push(put.id);
    }
    (ids, new)
}

fn restore_file(chunks: &ChunkStore, ids: &[ChunkId]) -> Vec<u8> {
    let mut data = Vec::new();
    for id in ids {
        data.extend_from_slice(&chunks.get(id).expect("get chunk"));
    }
    data
}

fn all_files(root: &Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).expect("read_dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files
}

#[tokio::test]
async fn a_chunked_file_round_trips_through_the_encrypted_store() {
    let dir = tempfile::tempdir().expect("create temporary directory");
    let (_repo, chunks) = new_repository(dir.path()).await;

    let mut data = pseudo_random(600_000, 1);
    data.extend(b"compressible text ".iter().copied().cycle().take(300_000));
    data.extend(pseudo_random(100_000, 2));
    let (ids, new) = store_file(&chunks, &data);

    // The repeating text yields identical chunks, which are stored once: deduplication inside a
    // single file.
    let distinct: HashSet<&ChunkId> = ids.iter().collect();
    assert_eq!(new, distinct.len());
    assert!(
        distinct.len() < ids.len(),
        "repeated text should repeat chunks"
    );
    assert!(ids.len() > 100, "{} chunks", ids.len());
    assert_eq!(restore_file(&chunks, &ids), data);
}

#[tokio::test]
async fn editing_a_file_stores_only_the_changed_chunks() {
    let dir = tempfile::tempdir().expect("create temporary directory");
    let (_repo, chunks) = new_repository(dir.path()).await;

    let original = pseudo_random(1 << 20, 3);
    let (first_ids, first_new) = store_file(&chunks, &original);
    assert_eq!(first_new, first_ids.len());

    let mut edited = original[..500_000].to_vec();
    edited.extend_from_slice(b"a small insertion in the middle of the file");
    edited.extend_from_slice(&original[500_000..]);
    let (second_ids, second_new) = store_file(&chunks, &edited);

    assert!(
        second_new <= 6,
        "{second_new} of {} chunks were new",
        second_ids.len()
    );
    assert!(second_new >= 1);
    assert_eq!(restore_file(&chunks, &first_ids), original);
    assert_eq!(restore_file(&chunks, &second_ids), edited);

    // Storing an identical copy again writes nothing at all.
    let (third_ids, third_new) = store_file(&chunks, &edited);
    assert_eq!(third_new, 0);
    assert_eq!(third_ids, second_ids);
}

#[tokio::test]
async fn data_survives_reopening_the_repository_with_the_passphrase() {
    let dir = tempfile::tempdir().expect("create temporary directory");
    let data = pseudo_random(200_000, 4);
    let ids = {
        let (_repo, chunks) = new_repository(dir.path()).await;
        store_file(&chunks, &data).0
    };

    let repo = Repository::at(dir.path());
    let keys = repo.unlock(&passphrase()).await.expect("unlock again");
    let chunks = repo.chunk_store(&keys).expect("chunk store");
    assert_eq!(restore_file(&chunks, &ids), data);
    let (ids_again, new) = store_file(&chunks, &data);
    assert_eq!(
        new, 0,
        "ids are stable across sessions, so everything deduplicates"
    );
    assert_eq!(ids_again, ids);
}

#[tokio::test]
async fn a_different_repository_cannot_read_or_match_the_chunks() {
    let dir_a = tempfile::tempdir().expect("create temporary directory");
    let dir_b = tempfile::tempdir().expect("create temporary directory");
    let (_a, chunks_a) = new_repository(dir_a.path()).await;
    let (_b, chunks_b) = new_repository(dir_b.path()).await;

    let data = pseudo_random(5000, 5);
    let put_a = chunks_a.put(&data).expect("put");
    let put_b = chunks_b.put(&data).expect("put");
    assert_ne!(put_a.id, put_b.id, "ids are repository-specific");

    // Move A's object into B's directory under A's name: B cannot make sense of it.
    let name = put_a.id.to_hex();
    let source = dir_a.path().join("data").join(&name[..2]).join(&name);
    let target_dir = dir_b.path().join("data").join(&name[..2]);
    fs::create_dir_all(&target_dir).expect("mkdir");
    fs::copy(&source, target_dir.join(&name)).expect("copy");
    assert!(matches!(
        chunks_b.get(&put_a.id),
        Err(Error::CorruptObject { .. })
    ));
}

#[tokio::test]
async fn nothing_written_to_disk_contains_the_file_contents() {
    let dir = tempfile::tempdir().expect("create temporary directory");
    let (_repo, chunks) = new_repository(dir.path()).await;

    let marker = b"PAYROLL-2024-CONFIDENTIAL-RECORD-";
    let mut data = pseudo_random(50_000, 6);
    for index in 0..400 {
        data.extend_from_slice(marker);
        data.extend_from_slice(format!("{index:05}\n").as_bytes());
    }
    data.extend(pseudo_random(50_000, 7));
    let (ids, _) = store_file(&chunks, &data);
    assert_eq!(restore_file(&chunks, &ids), data);

    let needles: Vec<&[u8]> = vec![&marker[..24], &data[10_000..10_024], &data[80_000..80_024]];
    for path in all_files(dir.path()) {
        let bytes = fs::read(&path).expect("read file");
        for needle in &needles {
            let found = bytes.windows(needle.len()).any(|window| window == *needle);
            assert!(!found, "plaintext found in {}", path.display());
        }
    }
}

#[tokio::test]
async fn concurrent_backups_of_the_same_data_do_not_conflict() {
    let dir = tempfile::tempdir().expect("create temporary directory");
    let (_repo, chunks) = new_repository(dir.path()).await;
    let data = pseudo_random(400_000, 8);

    let tasks: Vec<_> = (0..6)
        .map(|_| {
            let chunks = chunks.clone();
            let data = data.clone();
            tokio::task::spawn_blocking(move || store_file(&chunks, &data))
        })
        .collect();
    let mut results = Vec::new();
    for task in tasks {
        results.push(task.await.expect("task panicked"));
    }

    let ids = &results[0].0;
    assert!(results.iter().all(|(other, _)| other == ids));
    let new_total: usize = results.iter().map(|(_, new)| new).sum();
    assert_eq!(
        new_total,
        ids.len(),
        "every chunk was written by exactly one worker"
    );
    assert_eq!(restore_file(&chunks, ids), data);

    let object_files = all_files(&dir.path().join("data"));
    assert_eq!(object_files.len(), ids.len());
    assert!(object_files.iter().all(|path| {
        !path
            .file_name()
            .expect("name")
            .to_string_lossy()
            .starts_with(".tmp-")
    }));
}
