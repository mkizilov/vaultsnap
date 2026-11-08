//! End-to-end throughput and deduplication on real directories, through the real engine.
//!
//! ```text
//! cargo bench -p vaultsnap-core --bench throughput               # 64 MiB datasets
//! cargo bench -p vaultsnap-core --bench throughput -- --quick    # 8 MiB
//! cargo bench -p vaultsnap-core --bench throughput -- --large    # 512 MiB
//! ```
//!
//! What it measures, for incompressible data, text, and many small files:
//!
//! * **Chunk store**: storing new chunks, storing chunks that already exist (deduplication hits),
//!   and reading them back, on the in-memory backend, so the cost is hashing, compression, and
//!   encryption, not a disk.
//! * **Backup**: a first backup with one and with two workers, a second backup of unchanged data
//!   (files reused from the parent snapshot, so almost nothing is read), and a third after about
//!   1 % of the files grew.
//! * **Restore**, and **check** (quick, which reads no payload, and full, which reads and decrypts
//!   everything).
//! * **Deduplication**: a dataset, a copy of it with small insertions in some files (content
//!   defined chunking should keep every chunk the insertions did not touch), and an exact copy at
//!   another path: how many chunks are new, and how much the repository holds against how much was
//!   backed up.
//!
//! The repository uses cheap key-derivation parameters (it measures the engine, not Argon2), the
//! default chunk sizes, and no safety margin for unchanged-file detection (the benchmark creates
//! files an instant before it backs them up). Nothing is compared with anything: the numbers
//! depend on the machine and its disk, so compare rows, not machines.
//!
//! Memory is bounded by the chunk store rows (at most 128 MiB of generated chunks) and by the
//! engine's own bound (about `jobs` x twice the maximum chunk size, plus the manifest). Generated
//! data lives in a temporary directory under `TMPDIR`, removed when the run ends, even on failure.
//! At most two workers are used.

use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use vaultsnap_core::{
    BackupOptions, BackupReport, CheckOptions, ChunkStore, KdfParams, MemoryBackend, Passphrase,
    Repository, RepositoryKeys, RestoreOptions, SnapshotRef,
};

const MIB: u64 = 1024 * 1024;

// ---------------------------------------------------------------------------------------------
// Data
// ---------------------------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: usize) -> usize {
        usize::try_from(self.next() % (bound as u64)).expect("smaller than bound")
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len + 8);
        while out.len() < len {
            out.extend_from_slice(&self.next().to_le_bytes());
        }
        out.truncate(len);
        out
    }

    /// Prose-like text from a small vocabulary: compressible, with some repetition.
    fn text(&mut self, len: usize) -> Vec<u8> {
        const WORDS: [&str; 16] = [
            "backup",
            "chunk",
            "snapshot",
            "the",
            "of",
            "repository",
            "encrypted",
            "and",
            "incremental",
            "restore",
            "data",
            "a",
            "file",
            "boundary",
            "hash",
            "stream",
        ];
        let mut out = Vec::with_capacity(len + 16);
        while out.len() < len {
            out.extend_from_slice(WORDS[self.below(WORDS.len())].as_bytes());
            out.push(if self.below(12) == 0 { b'\n' } else { b' ' });
        }
        out.truncate(len);
        out
    }
}

#[derive(Clone, Copy)]
enum Kind {
    /// 2 MiB files of random bytes: already-compressed or encrypted data.
    Random,
    /// 1 MiB files of text.
    Text,
    /// Files of 4 to 16 KiB in many directories.
    Small,
}

impl Kind {
    const ALL: [Self; 3] = [Self::Random, Self::Text, Self::Small];

    fn name(self) -> &'static str {
        match self {
            Self::Random => "random",
            Self::Text => "text",
            Self::Small => "small files",
        }
    }
}

/// Writes about `total` bytes of files of `kind` below `root` and returns the files in order.
fn build(root: &Path, kind: Kind, total: u64, seed: u64) -> Vec<PathBuf> {
    let mut rng = Rng(seed);
    let mut files = Vec::new();
    let mut written = 0_u64;
    let mut index = 0_usize;
    while written < total {
        let len = match kind {
            Kind::Random => 2 * MIB,
            Kind::Text => MIB,
            Kind::Small => 4096 + rng.below(12 * 1024) as u64,
        }
        .min(total - written);
        let len_usize = usize::try_from(len).expect("fits in memory");
        let data = match kind {
            Kind::Text => rng.text(len_usize),
            Kind::Random | Kind::Small => rng.bytes(len_usize),
        };
        let path = root
            .join(format!("d{:03}", index / 50))
            .join(format!("f{index:05}.dat"));
        fs::create_dir_all(path.parent().expect("has a parent")).expect("create directories");
        fs::write(&path, data).expect("write file");
        files.push(path);
        written += len;
        index += 1;
    }
    files
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("create directory");
    for entry in fs::read_dir(from).expect("read_dir") {
        let entry = entry.expect("entry");
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).expect("copy file");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Running and reporting
// ---------------------------------------------------------------------------------------------

struct Runner {
    runtime: tokio::runtime::Runtime,
}

impl Runner {
    fn new() -> Self {
        Self {
            runtime: tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("build a runtime"),
        }
    }

    fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(future)
    }

    fn timed<T>(&self, future: impl Future<Output = T>) -> (T, Duration) {
        let started = Instant::now();
        let value = self.block_on(future);
        (value, started.elapsed())
    }

    /// A new repository below `dir` (on disk), initialized and unlocked.
    fn repository(&self, dir: &Path) -> (Repository, RepositoryKeys) {
        let repo = Repository::at(dir);
        let passphrase = Passphrase::new(b"benchmark passphrase".to_vec()).expect("valid");
        let kdf = KdfParams {
            m_cost_kib: 64,
            t_cost: 1,
            p_cost: 1,
        };
        self.block_on(async {
            repo.init_with_kdf(&passphrase, kdf).await.expect("init");
            let keys = repo.unlock(&passphrase).await.expect("unlock");
            (repo, keys)
        })
    }

    fn backup(
        &self,
        repo: &Repository,
        keys: &RepositoryKeys,
        sources: &[&Path],
        jobs: usize,
    ) -> (BackupReport, Duration) {
        let options = BackupOptions {
            sources: sources.iter().map(|source| source.to_path_buf()).collect(),
            jobs,
            racy_margin: Duration::ZERO,
            host: "benchmark".to_owned(),
            ..BackupOptions::default()
        };
        let (report, elapsed) = self.timed(repo.backup(keys, options));
        (report.expect("backup succeeds"), elapsed)
    }
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / MIB as f64
}

fn row(name: &str, bytes: u64, elapsed: Duration, note: &str) {
    let seconds = elapsed.as_secs_f64().max(1e-9);
    println!(
        "{name:<46} {:>9.1} MiB {:>9.3} s {:>10.1} MiB/s  {note}",
        mib(bytes),
        seconds,
        mib(bytes) / seconds
    );
}

fn heading(title: &str) {
    println!("\n== {title}");
    println!(
        "{:<46} {:>13} {:>11} {:>14}  notes",
        "step", "data", "time", "throughput"
    );
}

fn stored_bytes(store: &ChunkStore) -> u64 {
    store
        .objects()
        .expect("list the objects")
        .iter()
        .map(|(_, len)| *len)
        .sum()
}

// ---------------------------------------------------------------------------------------------
// The measurements
// ---------------------------------------------------------------------------------------------

/// Storing, re-storing, and reading chunks on the in-memory backend.
fn chunk_store(runner: &Runner, total: u64) {
    heading("chunk store (in memory, 1 MiB chunks)");
    let total = total.min(128 * MIB);
    let repo = Repository::with_backend(Arc::new(MemoryBackend::new()));
    let passphrase = Passphrase::new(b"benchmark passphrase".to_vec()).expect("valid");
    let kdf = KdfParams {
        m_cost_kib: 64,
        t_cost: 1,
        p_cost: 1,
    };
    let keys = runner.block_on(async {
        repo.init_with_kdf(&passphrase, kdf).await.expect("init");
        repo.unlock(&passphrase).await.expect("unlock")
    });
    let store = repo.chunk_store(&keys).expect("chunk store");

    for (name, text) in [("random", false), ("text", true)] {
        let mut rng = Rng(if text { 11 } else { 7 });
        let chunks: Vec<Vec<u8>> = (0..total / MIB)
            .map(|_| {
                if text {
                    rng.text(MIB as usize)
                } else {
                    rng.bytes(MIB as usize)
                }
            })
            .collect();
        let bytes: u64 = chunks.iter().map(|chunk| chunk.len() as u64).sum();

        let before = stored_bytes(&store);
        let started = Instant::now();
        let ids: Vec<_> = chunks
            .iter()
            .map(|chunk| store.put(chunk).expect("put"))
            .collect();
        let elapsed = started.elapsed();
        let stored = stored_bytes(&store) - before;
        let note = format!(
            "stored {:.1} MiB ({:.2}x)",
            mib(stored),
            stored as f64 / bytes as f64
        );
        row(&format!("put new, {name}"), bytes, elapsed, &note);

        let started = Instant::now();
        let hits = chunks
            .iter()
            .filter(|chunk| !store.put(chunk).expect("put").is_new())
            .count();
        let elapsed = started.elapsed();
        row(
            &format!("put existing, {name}"),
            bytes,
            elapsed,
            &format!("{hits} hits"),
        );

        let started = Instant::now();
        for put in &ids {
            store.get(&put.id).expect("get");
        }
        row(
            &format!("get and verify, {name}"),
            bytes,
            started.elapsed(),
            "",
        );
    }
}

/// Backup, unchanged backup, small change, restore, and check, for one kind of data.
fn pipeline(runner: &Runner, work: &Path, kind: Kind, total: u64) {
    heading(&format!("{} ({:.0} MiB)", kind.name(), mib(total)));
    let src = work.join(format!("src-{}", kind.name().replace(' ', "-")));
    let files = build(&src, kind, total, 42);
    let bytes: u64 = files
        .iter()
        .map(|file| fs::metadata(file).expect("metadata").len())
        .sum();
    let note = format!("{} files", files.len());

    for jobs in [1, 2] {
        let (repo, keys) = runner.repository(&work.join(format!("repo-{}-{jobs}", kind.name())));
        let (report, elapsed) = runner.backup(&repo, &keys, &[&src], jobs);
        let stored = report.transfer.bytes_stored;
        let ratio = stored as f64 / bytes as f64;
        let text = format!("{note}; stored {:.1} MiB ({ratio:.2}x)", mib(stored));
        row(
            &format!("backup, first, {jobs} worker(s)"),
            bytes,
            elapsed,
            &text,
        );
        if jobs == 2 {
            run_follow_ups(runner, work, &repo, &keys, &src, &files, bytes, kind);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_follow_ups(
    runner: &Runner,
    work: &Path,
    repo: &Repository,
    keys: &RepositoryKeys,
    src: &Path,
    files: &[PathBuf],
    bytes: u64,
    kind: Kind,
) {
    let (report, elapsed) = runner.backup(repo, keys, &[src], 2);
    let note = format!(
        "{} files reused, {} read",
        report.transfer.files_unchanged, report.transfer.files_read
    );
    row("backup, nothing changed", bytes, elapsed, &note);

    // About 1 % of the files grow a little.
    let changed = (files.len() / 100).max(1);
    for file in &files[..changed] {
        let mut data = fs::read(file).expect("read");
        data.extend_from_slice(&Rng(99).bytes(64));
        fs::write(file, data).expect("write");
    }
    let (report, elapsed) = runner.backup(repo, keys, &[src], 2);
    let note = format!(
        "{changed} files changed: read {:.1} MiB, stored {:.2} MiB",
        mib(report.transfer.bytes_read),
        mib(report.transfer.bytes_stored)
    );
    row("backup, 1 % of files changed", bytes, elapsed, &note);

    let target = work.join(format!("out-{}", kind.name()));
    let options = RestoreOptions::new(SnapshotRef::from(&report.snapshot), &target);
    let (restored, elapsed) = runner.timed(repo.restore(keys, options));
    let restored = restored.expect("restore succeeds");
    row("restore", restored.bytes, elapsed, "");
    fs::remove_dir_all(&target).expect("remove the restored files");

    for (name, read_data) in [
        ("check, quick", false),
        ("check, full (reads and decrypts)", true),
    ] {
        let options = CheckOptions {
            read_data,
            jobs: 2,
            ..CheckOptions::default()
        };
        let (checked, elapsed) = runner.timed(repo.check(keys, options));
        let checked = checked.expect("the check runs");
        let note = if checked.is_ok() {
            "no errors"
        } else {
            "ERRORS FOUND"
        };
        row(name, bytes, elapsed, note);
    }
}

/// How much a repository holds against how much was backed up, as data changes and repeats.
fn deduplication(runner: &Runner, work: &Path, total: u64) {
    heading("deduplication (random data, default chunk sizes)");
    let (repo, keys) = runner.repository(&work.join("repo-dedup"));
    let store = repo.chunk_store(&keys).expect("chunk store");

    let original = work.join("dedup-original");
    let files = build(&original, Kind::Random, total, 1234);
    let bytes: u64 = files
        .iter()
        .map(|file| fs::metadata(file).expect("metadata").len())
        .sum();
    let mut logical = 0_u64;

    let mut step = |label: &str, source: &Path, expected_new: &str| {
        let (report, elapsed) = runner.backup(&repo, &keys, &[source], 2);
        logical += bytes;
        let stored = stored_bytes(&store);
        let ratio = logical as f64 / stored.max(1) as f64;
        let note = format!(
            "new {} / existing {} chunks; repository {:.1} MiB for {:.1} MiB ({ratio:.2}x); \
             {expected_new}",
            report.transfer.chunks_new,
            report.transfer.chunks_existing,
            mib(stored),
            mib(logical),
        );
        row(label, bytes, elapsed, &note);
        report
    };

    step("backup, original", &original, "everything is new");

    // Every 4th file gets 8 bytes inserted at the start, so every byte after it moves.
    let edited = work.join("dedup-edited");
    copy_tree(&original, &edited);
    let mut touched = 0;
    for file in files.iter().step_by(4) {
        let relative = file.strip_prefix(&original).expect("inside");
        let path = edited.join(relative);
        let mut data = b"inserted".to_vec();
        data.extend_from_slice(&fs::read(&path).expect("read"));
        fs::write(&path, data).expect("write");
        touched += 1;
    }
    step(
        "backup, copy with insertions",
        &edited,
        &format!("{touched} files edited: only chunks near an insertion should be new"),
    );

    let copy = work.join("dedup-copy");
    copy_tree(&original, &copy);
    let report = step(
        "backup, exact copy at another path",
        &copy,
        "nothing should be new",
    );
    assert_eq!(
        report.transfer.chunks_new, 0,
        "an exact copy of data already in the repository stored new chunks"
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let quick = args.iter().any(|arg| arg == "--quick");
    let large = args.iter().any(|arg| arg == "--large");
    let megabytes: u64 = if quick {
        8
    } else if large {
        512
    } else {
        64
    };
    let total = megabytes * MIB;

    println!("VaultSnap throughput: {megabytes} MiB per dataset, at most 2 workers");
    println!("(cheap key derivation; default chunk sizes; the numbers depend on this machine)");

    let work = tempfile::tempdir().expect("create a temporary directory");
    let runner = Runner::new();
    chunk_store(&runner, total);
    for kind in Kind::ALL {
        pipeline(&runner, work.path(), kind, total);
    }
    deduplication(&runner, work.path(), total.min(128 * MIB));
    println!(
        "\n(generated data is removed from {})",
        work.path().display()
    );
}
