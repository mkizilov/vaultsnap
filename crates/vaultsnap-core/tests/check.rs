//! Integration tests of the repository check on repositories made by real backups, which are then
//! damaged on purpose: chunks deleted, truncated, extended, flipped, swapped, replaced by links;
//! snapshots tampered with, cut, emptied, or foreign; directories removed; strays, leftovers, and
//! stale locks added.
//!
//! Each test says which mode finds which damage, including what the quick mode cannot see, and the
//! tests that damage a repository also prove that the check changed nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tempfile::TempDir;
use vaultsnap_core::snapshot::NodeKind;
use vaultsnap_core::{
    BackupOptions, BackupReport, CheckOptions, CheckReport, ChunkId, ChunkerConfig, Compression,
    Error, FindingKind, KdfParams, Passphrase, Repository, RepositoryKeys, SnapshotId, SnapshotRef,
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

    fn repo_path(&self, relative: &str) -> PathBuf {
        self.path("repo").join(relative)
    }

    async fn backup(&self) -> BackupReport {
        let options = BackupOptions {
            sources: vec![self.path("src")],
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

    async fn check(&self, read_data: bool) -> CheckReport {
        let options = CheckOptions {
            read_data,
            ..CheckOptions::default()
        };
        self.check_with(options).await
    }

    async fn check_with(&self, options: CheckOptions) -> CheckReport {
        self.repo
            .check(&self.keys, options)
            .await
            .expect("the check runs")
    }

    /// The object file of a chunk, from the documented layout.
    fn chunk_file(&self, id: &ChunkId) -> PathBuf {
        let hex = id.to_hex();
        self.repo_path("data").join(&hex[..2]).join(hex)
    }

    fn snapshot_file(&self, id: &SnapshotId) -> PathBuf {
        self.repo_path("snapshots").join(id.as_str())
    }

    /// The `(id, recorded length)` of every chunk of the file `rel` in a snapshot, in order.
    async fn chunks_of(&self, id: &SnapshotId, rel: &str) -> Vec<(ChunkId, u32)> {
        let manifest = self
            .repo
            .snapshot(&self.keys, &SnapshotRef::from(id))
            .await
            .expect("load");
        let root = fs::canonicalize(self.path("src")).expect("exists");
        let want = format!("{}/{rel}", root.display());
        for listed in vaultsnap_core::history::entries(&manifest, None) {
            if listed.path == want.as_bytes() {
                if let NodeKind::File { chunks } = &listed.entry.kind {
                    return chunks.iter().map(|chunk| (chunk.id, chunk.len)).collect();
                }
            }
        }
        panic!("{want} is not a file in the snapshot");
    }

    async fn compression_of(&self, id: &ChunkId) -> Compression {
        let chunks = self.repo.chunk_store(&self.keys).expect("chunk store");
        chunks.peek(id).expect("peek").compression
    }
}

/// What a finding's subject is for the object of a chunk.
fn object_subject(id: &ChunkId) -> String {
    let hex = id.to_hex();
    format!("data/{}/{hex}", &hex[..2])
}

/// Every entry below `root`: relative path to (content, modification time). Directories are
/// recorded by name only: their times change whenever the check creates and removes its own lock
/// file, which is the one thing it is allowed to do.
fn dump(root: &Path) -> BTreeMap<String, (Vec<u8>, SystemTime)> {
    let mut found = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).expect("read_dir") {
            let path = entry.expect("entry").path();
            let meta = fs::symlink_metadata(&path).expect("stat");
            let relative = path
                .strip_prefix(root)
                .expect("inside")
                .to_string_lossy()
                .into_owned();
            let modified = meta.modified().expect("mtime");
            if meta.is_dir() {
                found.insert(format!("{relative}/"), (Vec::new(), SystemTime::UNIX_EPOCH));
                pending.push(path);
            } else if meta.is_file() {
                found.insert(relative, (fs::read(&path).expect("read"), modified));
            } else {
                found.insert(relative, (Vec::new(), modified));
            }
        }
    }
    found
}

fn flip_a_byte(path: &Path, position: usize) {
    let mut bytes = fs::read(path).expect("read");
    let position = position.min(bytes.len() - 1);
    bytes[position] ^= 0x01;
    fs::write(path, bytes).expect("write");
}

fn truncate_by(path: &Path, cut: usize) {
    let bytes = fs::read(path).expect("read");
    fs::write(path, &bytes[..bytes.len() - cut]).expect("write");
}

fn kinds(report: &CheckReport) -> Vec<FindingKind> {
    report.findings.iter().map(|finding| finding.kind).collect()
}

/// A source tree with data of both kinds: random bytes (stored as they are) and repetitive text
/// (compressed), several chunks each.
fn first_tree(root: &Path) {
    write(root, "small.txt", b"hello check");
    write(root, "random.bin", &pseudo_random(120_000, 1));
    write(
        root,
        "notes.txt",
        &b"all work and no play makes a dull backup\n".repeat(12_000),
    );
}

/// A repository with two snapshots: the second adds `extra.bin`, whose chunks only it needs.
async fn two_snapshots() -> (Env, BackupReport, BackupReport) {
    let env = Env::new().await;
    first_tree(&env.path("src"));
    let first = env.backup().await;
    pause();
    write(&env.path("src"), "extra.bin", &pseudo_random(40_000, 2));
    let second = env.backup().await;
    (env, first, second)
}

// ---------------------------------------------------------------------------------------------
// A healthy repository
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_healthy_repository_is_clean_in_both_modes_and_full_reads_all_of_it() {
    let (env, _first, _second) = two_snapshots().await;

    let quick = env.check(false).await;
    assert!(quick.is_clean(), "{:?}", quick.findings);
    assert_eq!(quick.mode.name(), "quick");
    assert_eq!(
        (quick.stats.snapshots, quick.stats.snapshots_damaged),
        (2, 0)
    );
    assert_eq!(quick.stats.key_files, 1);
    assert_eq!(
        quick.stats.chunks_referenced, quick.stats.objects,
        "every object is used"
    );
    assert_eq!(
        (
            quick.stats.chunks_missing,
            quick.stats.objects_damaged,
            quick.stats.orphans
        ),
        (0, 0, 0)
    );
    assert_eq!(quick.stats.objects_checked, quick.stats.objects);
    assert!(quick.stats.orphans_exact);
    assert_eq!(
        quick.stats.bytes_authenticated, 0,
        "the quick mode reads no payload"
    );
    assert!(quick.stats.object_bytes > 0 && quick.affected.is_empty());
    assert_eq!(quick.repository_id.len(), 32);

    let full = env.check(true).await;
    assert!(full.is_clean(), "{:?}", full.findings);
    assert_eq!(full.mode.name(), "full");
    assert_eq!(full.stats.bytes_authenticated, full.stats.object_bytes);
    assert_eq!(full.stats.objects, quick.stats.objects);
    assert_eq!(
        fs::read_dir(env.repo_path("locks"))
            .expect("read_dir")
            .count(),
        0
    );
}

#[tokio::test]
async fn an_empty_repository_is_clean() {
    let env = Env::new().await;
    for read_data in [false, true] {
        let report = env.check(read_data).await;
        assert!(report.is_clean(), "{:?}", report.findings);
        assert_eq!((report.stats.snapshots, report.stats.objects), (0, 0));
    }
}

// ---------------------------------------------------------------------------------------------
// Never repairs, never deletes
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_check_changes_nothing_not_a_byte_and_not_a_time_even_in_a_badly_damaged_repository() {
    let (env, first, second) = two_snapshots().await;
    let random = env.chunks_of(&first.snapshot, "random.bin").await;
    flip_a_byte(&env.chunk_file(&random[3].0), 40);
    truncate_by(&env.chunk_file(&random[5].0), 7);
    fs::remove_file(env.chunk_file(&random[7].0)).expect("remove");
    flip_a_byte(&env.snapshot_file(&second.snapshot), 100);
    write(&env.repo_path(""), "notes.txt", b"a stray file");
    write(
        &env.repo_path("snapshots"),
        ".tmp-0011223344556677",
        b"leftover",
    );
    write(
        &env.repo_path("locks"),
        "0123456789abcdef0123456789abcdef.lock",
        b"kind=shared\npid=1\n",
    );

    let before = dump(&env.path("repo"));
    for read_data in [false, true] {
        let report = env.check(read_data).await;
        assert!(
            !report.is_ok() && report.warnings >= 3,
            "{:?}",
            report.findings
        );
    }
    assert_eq!(
        dump(&env.path("repo")),
        before,
        "the repository was not touched"
    );
}

// ---------------------------------------------------------------------------------------------
// Missing chunks
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_missing_chunk_is_an_error_in_both_modes_and_names_the_snapshot_it_breaks() {
    let (env, first, second) = two_snapshots().await;
    let extra = env.chunks_of(&second.snapshot, "extra.bin").await;
    let (gone, _) = extra[2];
    fs::remove_file(env.chunk_file(&gone)).expect("remove");

    for read_data in [false, true] {
        let report = env.check(read_data).await;
        assert!(!report.is_ok());
        assert_eq!(
            (report.errors, report.warnings),
            (1, 0),
            "{:?}",
            report.findings
        );
        assert_eq!(kinds(&report), [FindingKind::ChunkMissing]);
        assert_eq!(report.findings[0].subject, object_subject(&gone));
        assert_eq!(report.stats.chunks_missing, 1);
        assert_eq!(report.stats.objects, report.stats.chunks_referenced - 1);
        assert_eq!(
            report.affected.len(),
            1,
            "only the second snapshot needs it"
        );
        let affected = &report.affected[0];
        assert_eq!(affected.snapshot, second.snapshot);
        assert_eq!((affected.files, affected.damaged_chunks), (1, 1));
        assert_ne!(affected.snapshot, first.snapshot);
    }
}

#[tokio::test]
async fn a_chunk_shared_by_several_snapshots_breaks_all_of_them() {
    let (env, first, second) = two_snapshots().await;
    let random = env.chunks_of(&first.snapshot, "random.bin").await;
    fs::remove_file(env.chunk_file(&random[0].0)).expect("remove");
    fs::remove_file(env.chunk_file(&random[1].0)).expect("remove");

    let report = env.check(false).await;
    assert_eq!(report.count(FindingKind::ChunkMissing), 2);
    let affected: BTreeSet<&SnapshotId> = report.affected.iter().map(|a| &a.snapshot).collect();
    assert_eq!(
        affected,
        BTreeSet::from([&first.snapshot, &second.snapshot])
    );
    for entry in &report.affected {
        assert_eq!((entry.files, entry.damaged_chunks), (1, 2));
    }
}

#[tokio::test]
async fn removing_the_whole_data_directory_reports_it_and_every_chunk() {
    let (env, _first, _second) = two_snapshots().await;
    let referenced = env.check(false).await.stats.chunks_referenced;
    fs::remove_dir_all(env.repo_path("data")).expect("remove");

    let report = env.check(false).await;
    assert_eq!(report.count(FindingKind::MissingDirectory), 1);
    assert_eq!(report.count(FindingKind::ChunkMissing), referenced);
    assert_eq!(report.affected.len(), 2);
    assert_eq!(report.stats.objects, 0);
}

// ---------------------------------------------------------------------------------------------
// Damaged chunks: what each mode can see
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_truncated_chunk_is_found_by_both_modes() {
    let (env, first, _second) = two_snapshots().await;
    let random = env.chunks_of(&first.snapshot, "random.bin").await;
    let (victim, _) = random[4];
    assert_eq!(env.compression_of(&victim).await, Compression::Uncompressed);
    truncate_by(&env.chunk_file(&victim), 10);

    for read_data in [false, true] {
        let report = env.check(read_data).await;
        assert_eq!(
            kinds(&report),
            [FindingKind::ChunkCorrupt],
            "read_data={read_data}"
        );
        assert_eq!(report.findings[0].subject, object_subject(&victim));
        assert!(
            report.findings[0].detail.contains("truncated or extended"),
            "{}",
            report.findings[0].detail
        );
        assert_eq!(
            (report.stats.objects_damaged, report.affected.len()),
            (1, 2)
        );
    }
}

#[tokio::test]
async fn an_extended_chunk_and_a_damaged_header_are_found_by_both_modes() {
    let (env, first, _second) = two_snapshots().await;
    let random = env.chunks_of(&first.snapshot, "random.bin").await;

    let mut appended = fs::read(env.chunk_file(&random[1].0)).expect("read");
    appended.extend_from_slice(b"trailing garbage");
    fs::write(env.chunk_file(&random[1].0), appended).expect("write");
    let mut bad_magic = fs::read(env.chunk_file(&random[2].0)).expect("read");
    bad_magic[0] = b'X';
    fs::write(env.chunk_file(&random[2].0), bad_magic).expect("write");
    let mut bad_flags = fs::read(env.chunk_file(&random[3].0)).expect("read");
    bad_flags[7] = 1;
    fs::write(env.chunk_file(&random[3].0), bad_flags).expect("write");

    for read_data in [false, true] {
        let report = env.check(read_data).await;
        assert_eq!(
            report.count(FindingKind::ChunkCorrupt),
            3,
            "read_data={read_data}"
        );
        let details: Vec<&str> = report.findings.iter().map(|f| f.detail.as_str()).collect();
        assert!(
            details.iter().any(|d| d.contains("truncated or extended")),
            "{details:?}"
        );
        assert!(
            details.iter().any(|d| d.contains("bad magic")),
            "{details:?}"
        );
        assert!(
            details.iter().any(|d| d.contains("unknown flags")),
            "{details:?}"
        );
    }
}

#[tokio::test]
async fn a_chunk_from_a_newer_version_is_reported_as_unsupported_not_corrupt() {
    let (env, first, _second) = two_snapshots().await;
    let random = env.chunks_of(&first.snapshot, "random.bin").await;
    let mut bytes = fs::read(env.chunk_file(&random[0].0)).expect("read");
    bytes[4] = 9;
    fs::write(env.chunk_file(&random[0].0), bytes).expect("write");

    for read_data in [false, true] {
        let report = env.check(read_data).await;
        assert_eq!(
            kinds(&report),
            [FindingKind::UnsupportedVersion],
            "read_data={read_data}"
        );
        assert!(
            report.findings[0].detail.contains("version 9"),
            "{}",
            report.findings[0].detail
        );
    }
}

#[tokio::test]
async fn a_flipped_bit_inside_a_chunk_is_found_only_by_the_full_check() {
    let (env, first, _second) = two_snapshots().await;
    let random = env.chunks_of(&first.snapshot, "random.bin").await;
    let (victim, _) = random[6];
    let path = env.chunk_file(&victim);
    let middle = fs::read(&path).expect("read").len() / 2;
    flip_a_byte(&path, middle);

    // The documented limit of the quick mode: it never reads the payload.
    let quick = env.check(false).await;
    assert!(
        quick.is_clean(),
        "the quick mode cannot see this: {:?}",
        quick.findings
    );

    let full = env.check(true).await;
    assert_eq!(kinds(&full), [FindingKind::ChunkCorrupt]);
    assert_eq!(full.findings[0].subject, object_subject(&victim));
    assert!(
        full.findings[0].detail.contains("authentication failed"),
        "{}",
        full.findings[0].detail
    );
    assert_eq!(full.stats.objects_damaged, 1);
    assert_eq!(
        full.stats.bytes_authenticated,
        full.stats.object_bytes - fs::metadata(&path).expect("metadata").len(),
        "everything but the damaged object was read and authenticated"
    );
    assert_eq!(full.affected.len(), 2);
}

#[tokio::test]
async fn a_truncated_compressed_chunk_is_also_only_found_by_the_full_check() {
    let (env, first, _second) = two_snapshots().await;
    let notes = env.chunks_of(&first.snapshot, "notes.txt").await;
    let (victim, _) = notes[0];
    assert_eq!(
        env.compression_of(&victim).await,
        Compression::Zstd,
        "the test needs a compressed chunk"
    );
    truncate_by(&env.chunk_file(&victim), 1);

    let quick = env.check(false).await;
    assert!(
        quick.is_clean(),
        "a shorter compressed object is still possible: {:?}",
        quick.findings
    );
    let full = env.check(true).await;
    assert_eq!(kinds(&full), [FindingKind::ChunkCorrupt]);
}

#[tokio::test]
async fn a_valid_object_stored_under_the_wrong_id_is_caught_by_both_modes_in_their_own_way() {
    let (env, first, _second) = two_snapshots().await;
    let random = env.chunks_of(&first.snapshot, "random.bin").await;
    let ((target, target_len), (other, other_len)) = (random[8], random[9]);
    assert_ne!(
        target_len, other_len,
        "the test needs two chunks of different sizes"
    );
    // A perfectly authentic object, in every byte, under another chunk's name.
    fs::copy(env.chunk_file(&other), env.chunk_file(&target)).expect("copy");

    let quick = env.check(false).await;
    assert_eq!(
        kinds(&quick),
        [FindingKind::ChunkLengthMismatch],
        "{:?}",
        quick.findings
    );
    assert_eq!(quick.findings[0].subject, object_subject(&target));
    assert!(
        quick.findings[0].detail.contains(&format!(
            "{other_len} bytes but snapshots record {target_len}"
        )),
        "{}",
        quick.findings[0].detail
    );

    let full = env.check(true).await;
    assert_eq!(
        kinds(&full),
        [FindingKind::ChunkCorrupt],
        "{:?}",
        full.findings
    );
    assert!(full.findings[0].detail.contains("authentication failed"));
}

#[cfg(unix)]
#[tokio::test]
async fn a_chunk_replaced_by_a_link_is_reported_and_the_link_is_never_followed() {
    let (env, first, _second) = two_snapshots().await;
    let random = env.chunks_of(&first.snapshot, "random.bin").await;
    let (victim, _) = random[2];
    let outside = tempfile::tempdir().expect("create temporary directory");
    let target = outside.path().join("elsewhere");
    fs::write(&target, b"precious").expect("write");
    let path = env.chunk_file(&victim);
    fs::remove_file(&path).expect("remove");
    std::os::unix::fs::symlink(&target, &path).expect("symlink");

    for read_data in [false, true] {
        let report = env.check(read_data).await;
        assert_eq!(
            kinds(&report),
            [FindingKind::ChunkMissing, FindingKind::UnexpectedFile],
            "read_data={read_data}: {:?}",
            report.findings
        );
        assert!(report.findings[1].detail.starts_with("a symbolic link"));
    }
    assert_eq!(fs::read(&target).expect("read"), b"precious");
    assert!(
        fs::symlink_metadata(&path)
            .expect("stat")
            .file_type()
            .is_symlink(),
        "left as it was"
    );
}

// ---------------------------------------------------------------------------------------------
// Damaged snapshots
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn every_way_of_damaging_a_snapshot_file_is_detected_in_both_modes() {
    type Damage = fn(&Path);
    let damages: [(&str, Damage); 5] = [
        ("a flipped bit", |path| flip_a_byte(path, 200)),
        ("a truncated file", |path| truncate_by(path, 30)),
        ("an emptied file", |path| {
            fs::write(path, b"").expect("write")
        }),
        ("garbage", |path| {
            fs::write(path, b"this is not a snapshot").expect("write")
        }),
        ("an extended file", |path| {
            let mut bytes = fs::read(path).expect("read");
            bytes.extend_from_slice(b"more");
            fs::write(path, bytes).expect("write");
        }),
    ];
    for (what, damage) in damages {
        let (env, first, second) = two_snapshots().await;
        damage(&env.snapshot_file(&second.snapshot));

        for read_data in [false, true] {
            let report = env.check(read_data).await;
            assert_eq!(
                report.count(FindingKind::SnapshotDamaged),
                1,
                "{what}: {:?}",
                report.findings
            );
            assert_eq!(report.errors, 1, "{what}");
            assert_eq!(
                (report.stats.snapshots, report.stats.snapshots_damaged),
                (1, 1),
                "{what}"
            );
            let finding = &report.findings[0];
            assert_eq!(
                finding.subject,
                format!("snapshots/{}", second.snapshot),
                "{what}"
            );
            assert!(!finding.detail.is_empty(), "{what}");
            assert_ne!(first.snapshot, second.snapshot);
        }
    }
}

#[tokio::test]
async fn the_chunks_of_an_unreadable_snapshot_look_like_orphans_and_the_report_says_so() {
    let (env, _first, second) = two_snapshots().await;
    let extra = env.chunks_of(&second.snapshot, "extra.bin").await;
    flip_a_byte(&env.snapshot_file(&second.snapshot), 100);

    let report = env.check(true).await;
    assert_eq!(report.errors, 1);
    assert!(!report.stats.orphans_exact);
    assert_eq!(
        report.stats.orphans,
        extra.len() as u64,
        "only the damaged snapshot needed these"
    );
    assert_eq!(report.count(FindingKind::OrphanObject), extra.len() as u64);
    let orphan = report
        .findings
        .iter()
        .find(|f| f.kind == FindingKind::OrphanObject)
        .expect("an orphan");
    assert!(
        orphan
            .detail
            .contains("may belong to a snapshot that could not be read"),
        "{}",
        orphan.detail
    );
    assert_eq!(
        report.stats.chunks_missing, 0,
        "nothing is missing, only unreferenced"
    );
    assert_eq!(report.stats.objects_damaged, 0);
}

#[tokio::test]
async fn a_snapshot_from_another_repository_or_swapped_under_another_name_is_damaged() {
    let (env, first, second) = two_snapshots().await;
    // The second snapshot's file, authentic in itself, under the first one's name.
    fs::copy(
        env.snapshot_file(&second.snapshot),
        env.snapshot_file(&first.snapshot),
    )
    .expect("copy");
    let report = env.check(false).await;
    assert_eq!(
        report.count(FindingKind::SnapshotDamaged),
        1,
        "{:?}",
        report.findings
    );
    assert_eq!(
        report.findings[0].subject,
        format!("snapshots/{}", first.snapshot)
    );

    // A snapshot of a different repository, with a different key.
    let (env, _first, second) = two_snapshots().await;
    let stranger = Env::new().await;
    first_tree(&stranger.path("src"));
    let foreign = stranger.backup().await;
    fs::copy(
        stranger.snapshot_file(&foreign.snapshot),
        env.snapshot_file(&second.snapshot),
    )
    .expect("copy");
    let report = env.check(false).await;
    assert_eq!(
        report.count(FindingKind::SnapshotDamaged),
        1,
        "{:?}",
        report.findings
    );
}

#[tokio::test]
async fn a_deleted_snapshot_cannot_be_noticed_and_leaves_orphans() {
    // The limit of what any check of a repository without an external record can do: a file that
    // is gone leaves nothing to compare with. docs/SECURITY.md says so.
    let (env, _first, second) = two_snapshots().await;
    fs::remove_file(env.snapshot_file(&second.snapshot)).expect("remove");

    let report = env.check(true).await;
    assert!(report.is_ok(), "{:?}", report.findings);
    assert_eq!(report.stats.snapshots, 1);
    assert!(report.stats.orphans > 0 && report.stats.orphans_exact);
    assert_eq!(report.warnings, report.stats.orphans);
}

// ---------------------------------------------------------------------------------------------
// Structure, strays, leftovers, locks, keys
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn missing_directories_are_errors_and_the_check_goes_on() {
    for name in ["snapshots", "locks", "keys", "data"] {
        let (env, _first, _second) = two_snapshots().await;
        fs::remove_dir_all(env.repo_path(name)).expect("remove");
        let report = env.check(false).await;
        assert_eq!(
            report.count(FindingKind::MissingDirectory),
            1,
            "{name}: {:?}",
            report.findings
        );
        assert_eq!(report.findings[0].subject, name);
        assert_eq!(report.findings[0].detail, "the directory does not exist");
        assert!(!report.is_ok(), "{name}");
    }
    let (env, _first, _second) = two_snapshots().await;
    fs::remove_dir_all(env.repo_path("snapshots")).expect("remove");
    fs::write(env.repo_path("snapshots"), b"not a directory").expect("write");
    let report = env.check(false).await;
    assert_eq!(
        report.findings[0].detail,
        "a file where a directory is required"
    );
    assert_eq!(report.stats.snapshots, 0);
}

#[tokio::test]
async fn strays_leftovers_stale_locks_and_bad_key_files_are_warnings_and_are_left_alone() {
    let (env, _first, _second) = two_snapshots().await;
    let stray = env.repo_path("notes.txt");
    let temp_a = env.repo_path("snapshots/.tmp-0011223344556677");
    fs::create_dir_all(env.repo_path("data/ab")).expect("mkdir");
    let temp_b = env.repo_path("data/ab/.tmp-0011223344556677");
    let lock = env.repo_path("locks/0123456789abcdef0123456789abcdef.lock");
    let key = env.repo_path("keys/00112233445566778899aabbccddeeff");
    fs::write(&stray, b"not ours").expect("write");
    fs::write(&temp_a, b"half a snapshot").expect("write");
    fs::write(&temp_b, b"half a chunk").expect("write");
    fs::write(
        &lock,
        b"kind=shared\npid=424242\ncreated_at_unix=1700000000\n",
    )
    .expect("write");
    fs::write(&key, b"garbage").expect("write");

    for read_data in [false, true] {
        let report = env.check(read_data).await;
        assert!(
            report.is_ok(),
            "warnings are not errors: {:?}",
            report.findings
        );
        assert_eq!(report.errors, 0);
        assert_eq!(report.warnings, 5, "{:?}", report.findings);
        assert_eq!(report.count(FindingKind::UnexpectedFile), 1);
        assert_eq!(report.count(FindingKind::LeftoverTempFile), 2);
        assert_eq!(report.count(FindingKind::LockPresent), 1);
        assert_eq!(report.count(FindingKind::KeyFileInvalid), 1);
        let held = report
            .findings
            .iter()
            .find(|f| f.kind == FindingKind::LockPresent)
            .expect("lock");
        assert_eq!(
            held.detail,
            "a shared lock held by process 424242, created at unix time 1700000000"
        );
    }
    for path in [&stray, &temp_a, &temp_b, &lock, &key] {
        assert!(path.exists(), "{path:?} was removed");
    }
}

#[tokio::test]
async fn a_stale_exclusive_lock_stops_the_check_and_is_not_removed() {
    let (env, _first, _second) = two_snapshots().await;
    let lock = env.repo_path("locks/0123456789abcdef0123456789abcdef.lock");
    fs::write(&lock, b"kind=exclusive\npid=1\n").expect("write");

    let err = env
        .repo
        .check(&env.keys, CheckOptions::default())
        .await
        .expect_err("must refuse");
    assert!(matches!(err, Error::Locked { .. }), "{err}");
    assert!(
        err.to_string()
            .contains("locks/0123456789abcdef0123456789abcdef.lock"),
        "{err}"
    );
    assert!(lock.exists());
}

#[tokio::test]
async fn unreferenced_objects_are_orphans_and_only_warnings() {
    let (env, _first, _second) = two_snapshots().await;
    let chunks = env.repo.chunk_store(&env.keys).expect("chunk store");
    let mut stored = 0;
    let mut ids = Vec::new();
    for seed in 0..3_u64 {
        let put = chunks
            .put(&pseudo_random(2000 + seed as usize, 50 + seed))
            .expect("put");
        stored += chunks.stored_len(&put.id).expect("length");
        ids.push(put.id);
    }

    for read_data in [false, true] {
        let report = env.check(read_data).await;
        assert!(report.is_ok());
        assert_eq!(
            (report.stats.orphans, report.stats.orphan_bytes),
            (3, stored)
        );
        assert_eq!(report.count(FindingKind::OrphanObject), 3);
        assert!(report.stats.orphans_exact);
        let subjects: BTreeSet<&str> = report.findings.iter().map(|f| f.subject.as_str()).collect();
        let expected: Vec<String> = ids.iter().map(object_subject).collect();
        assert_eq!(
            subjects,
            expected.iter().map(String::as_str).collect::<BTreeSet<_>>()
        );
        assert!(report
            .findings
            .iter()
            .all(|f| f.detail == "no snapshot refers to this object"));
    }
}

#[tokio::test]
async fn a_damaged_orphan_is_an_error_because_a_later_backup_would_reuse_it() {
    let (env, _first, _second) = two_snapshots().await;
    let chunks = env.repo.chunk_store(&env.keys).expect("chunk store");
    let put = chunks.put(&pseudo_random(3000, 77)).expect("put");
    flip_a_byte(&env.chunk_file(&put.id), 30);

    let report = env.check(true).await;
    assert_eq!(kinds(&report), [FindingKind::ChunkCorrupt]);
    assert!(
        report.findings[0]
            .detail
            .contains("a later backup would reuse it"),
        "{}",
        report.findings[0].detail
    );
    assert_eq!(report.stats.orphans, 1);
    assert!(report.affected.is_empty(), "no snapshot is affected");
}

// ---------------------------------------------------------------------------------------------
// Limits, determinism, cancellation
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_number_of_listed_findings_is_limited_but_the_counts_are_exact_and_errors_come_first() {
    let (env, first, _second) = two_snapshots().await;
    let chunks = env.repo.chunk_store(&env.keys).expect("chunk store");
    for seed in 0..20_u64 {
        chunks
            .put(&pseudo_random(1500 + seed as usize, 200 + seed))
            .expect("put");
    }
    let random = env.chunks_of(&first.snapshot, "random.bin").await;
    fs::remove_file(env.chunk_file(&random[0].0)).expect("remove");

    let options = CheckOptions {
        max_findings: 4,
        ..CheckOptions::default()
    };
    let report = env.check_with(options).await;
    assert_eq!(report.findings.len(), 4);
    assert_eq!(
        kinds(&report)[0],
        FindingKind::ChunkMissing,
        "the error comes first"
    );
    assert_eq!(
        (report.errors, report.warnings, report.suppressed),
        (1, 20, 17)
    );
    assert_eq!(report.count(FindingKind::OrphanObject), 20);
    assert_eq!(report.stats.orphans, 20);

    let unlimited = CheckOptions {
        max_findings: 0,
        ..CheckOptions::default()
    };
    let report = env.check_with(unlimited).await;
    assert_eq!((report.findings.len(), report.suppressed), (21, 0));
}

#[tokio::test]
async fn the_report_does_not_depend_on_the_number_of_threads() {
    let (env, first, second) = two_snapshots().await;
    let random = env.chunks_of(&first.snapshot, "random.bin").await;
    let extra = env.chunks_of(&second.snapshot, "extra.bin").await;
    for (index, (id, _)) in random.iter().enumerate().take(12) {
        match index % 3 {
            0 => flip_a_byte(&env.chunk_file(id), 50),
            1 => truncate_by(&env.chunk_file(id), 2),
            _ => fs::remove_file(env.chunk_file(id)).expect("remove"),
        }
    }
    flip_a_byte(&env.chunk_file(&extra[1].0), 60);

    let run = |jobs: usize, read_data: bool| {
        let env = &env;
        async move {
            let options = CheckOptions {
                read_data,
                jobs,
                ..CheckOptions::default()
            };
            env.check_with(options).await
        }
    };
    for read_data in [false, true] {
        let single = run(1, read_data).await;
        assert!(single.errors >= 8, "{} errors", single.errors);
        for jobs in [2, 4, 7] {
            let other = run(jobs, read_data).await;
            assert_eq!(
                other.findings, single.findings,
                "{jobs} threads, read_data={read_data}"
            );
            assert_eq!(other.stats, single.stats, "{jobs} threads");
            assert_eq!(other.affected, single.affected, "{jobs} threads");
            assert_eq!(other.counts, single.counts, "{jobs} threads");
        }
    }
}

#[tokio::test]
async fn a_cancelled_check_produces_no_report_and_releases_its_lock() {
    let (env, _first, _second) = two_snapshots().await;
    let options = CheckOptions {
        read_data: true,
        ..CheckOptions::default()
    };
    options.cancel.cancel();
    let err = env
        .repo
        .check(&env.keys, options)
        .await
        .expect_err("cancelled");
    assert!(matches!(err, Error::CheckCancelled), "{err}");
    assert_eq!(
        fs::read_dir(env.repo_path("locks"))
            .expect("read_dir")
            .count(),
        0
    );
}

#[tokio::test]
async fn the_wrong_keys_make_every_snapshot_unreadable_rather_than_the_check_fail() {
    let (env, _first, _second) = two_snapshots().await;
    let stranger = Env::new().await;

    let report = env
        .repo
        .check(&stranger.keys, CheckOptions::default())
        .await
        .expect("check");
    assert_eq!(report.count(FindingKind::SnapshotDamaged), 2);
    assert_eq!(report.stats.snapshots, 0);
    assert!(!report.stats.orphans_exact);
    assert_eq!(
        report.stats.orphans, report.stats.objects,
        "with no readable snapshot, all look unused"
    );
}
