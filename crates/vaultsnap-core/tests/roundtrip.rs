//! Randomized filesystem round trips: trees nobody wrote by hand are backed up, changed, backed
//! up again, and restored, and every restore must equal the tree as it was at backup time in
//! content, permissions, modification times, and links.
//!
//! A seeded generator (no external randomness) builds two source trees per seed: nested
//! directories with unusual names (spaces, a newline, a leading dash, non-ASCII, long names),
//! empty, tiny, small, medium, and large files of random, textual, zero, repeated, and
//! edited-copy content, symbolic links (relative, dangling, to files), hard links, and varied
//! permissions. Then random edits (change, delete, add, rename, chmod) make the second snapshot.
//!
//! * A failure message names the seed. `VAULTSNAP_ROUNDTRIP_SEED=<n>` replays exactly that seed;
//!   `VAULTSNAP_ROUNDTRIP_SEEDS=<count>` runs more of them (default 10).
//! * The repository is in memory, so the many small chunk objects cost nothing; the sources and
//!   the restore targets are real directories.
//! * Links, hard links, and permissions are generated on Unix only.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;
use vaultsnap_core::{
    BackupOptions, CheckOptions, ChunkerConfig, KdfParams, MemoryBackend, Passphrase, Repository,
    RepositoryKeys, RestoreOptions, SnapshotId, SnapshotRef,
};

fn passphrase() -> Passphrase {
    Passphrase::new(b"correct horse battery staple".to_vec()).expect("valid passphrase")
}

// ---------------------------------------------------------------------------------------------
// A small deterministic random number generator (SplitMix64)
// ---------------------------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A number from `0` to `bound - 1`; `bound` must not be zero.
    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    /// An index into a collection of `len` items; `len` must not be zero.
    fn index(&mut self, len: usize) -> usize {
        usize::try_from(self.next() % (len as u64)).expect("smaller than len")
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len + 8);
        while out.len() < len {
            out.extend_from_slice(&self.next().to_le_bytes());
        }
        out.truncate(len);
        out
    }
}

// ---------------------------------------------------------------------------------------------
// Generating a tree
// ---------------------------------------------------------------------------------------------

fn name(rng: &mut Rng, serial: u64) -> String {
    match rng.below(8) {
        0 => format!("日本語-{serial}"),
        1 => format!("with space {serial}"),
        2 => format!("-dash{serial}"),
        3 => format!("new\nline{serial}"),
        4 => format!("{}{serial}", "long".repeat(25)),
        5 => format!("ünïcödé-{serial}.txt"),
        6 => format!(".hidden{serial}"),
        _ => format!("file{serial}.dat"),
    }
}

/// Changes `data` a little: the kind of edit that moves chunk boundaries or leaves them alone.
fn edit(rng: &mut Rng, data: &mut Vec<u8>) {
    match rng.below(5) {
        0 if !data.is_empty() => {
            let at = rng.index(data.len());
            data[at] ^= 0x5a;
        }
        1 => {
            let at = rng.index(data.len() + 1);
            let len = 1 + rng.index(300);
            let inserted = rng.bytes(len);
            let tail = data.split_off(at);
            data.extend_from_slice(&inserted);
            data.extend_from_slice(&tail);
        }
        2 if !data.is_empty() => {
            let at = rng.index(data.len());
            let end = (at + 1 + rng.index(200)).min(data.len());
            let tail = data.split_off(end);
            data.truncate(at);
            data.extend_from_slice(&tail);
        }
        3 => {
            let len = 1 + rng.index(2_000);
            let extra = rng.bytes(len);
            data.extend_from_slice(&extra);
        }
        _ => data.truncate(data.len() / 2),
    }
}

/// File content of a random kind. `earlier` is what the tree already holds, so some files are
/// edited copies of others (the case content-defined chunking exists for).
fn content(rng: &mut Rng, earlier: &[Vec<u8>]) -> Vec<u8> {
    let roll = rng.below(100);
    if roll < 10 {
        Vec::new()
    } else if roll < 30 {
        let len = 1 + rng.index(100);
        rng.bytes(len)
    } else if roll < 55 {
        let len = 1_000 + rng.index(19_000);
        rng.bytes(len)
    } else if roll < 65 {
        let lines = 1 + rng.index(2_000);
        format!("line of text {}\n", rng.below(1000))
            .repeat(lines)
            .into_bytes()
    } else if roll < 72 {
        vec![0; 1 + rng.index(60_000)]
    } else if roll < 88 && !earlier.is_empty() {
        let mut data = earlier[rng.index(earlier.len())].clone();
        if data.is_empty() {
            data = rng.bytes(500);
        }
        edit(rng, &mut data);
        data
    } else if roll < 94 {
        let len = 200_000 + rng.index(300_000);
        rng.bytes(len)
    } else {
        let pattern_len = 1 + rng.index(64);
        let pattern = rng.bytes(pattern_len);
        pattern.repeat(1 + rng.index(3_000))
    }
}

/// The path of `target` as seen from the directory `from_dir`, both relative to the same root:
/// up to the root, then down. Such a link can never lead out of the tree.
#[cfg(unix)]
fn relative_target(from_dir: &Path, target: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for _ in from_dir.components() {
        out.push("..");
    }
    out.join(target)
}

#[cfg(unix)]
fn add_links(root: &Path, rng: &mut Rng, dirs: &[PathBuf], files: &[PathBuf], serial: &mut u64) {
    for _ in 0..rng.below(5) {
        *serial += 1;
        let dir = dirs[rng.index(dirs.len())].clone();
        let link = dir.join(name(rng, *serial));
        let target = if !files.is_empty() && rng.chance(70) {
            relative_target(&dir, &files[rng.index(files.len())])
        } else if dirs.len() > 1 && rng.chance(50) {
            // Never the root itself: from the root that would be an empty link target.
            relative_target(&dir, &dirs[1 + rng.index(dirs.len() - 1)])
        } else {
            PathBuf::from(format!("missing-{serial}"))
        };
        std::os::unix::fs::symlink(&target, root.join(&link)).expect("symlink");
    }
    for _ in 0..rng.below(3) {
        if files.is_empty() {
            break;
        }
        *serial += 1;
        let source = files[rng.index(files.len())].clone();
        let dir = dirs[rng.index(dirs.len())].clone();
        let link = dir.join(name(rng, *serial));
        fs::hard_link(root.join(&source), root.join(&link)).expect("hard link");
    }
}

/// Varied permissions. Directories keep the owner's read, write, and search bits so the test can
/// still create and remove things; files range from read-only to executable.
#[cfg(unix)]
fn randomize_modes(root: &Path, rng: &mut Rng, dirs: &[PathBuf], files: &[PathBuf]) {
    use std::os::unix::fs::PermissionsExt;

    const FILE_MODES: [u32; 6] = [0o600, 0o644, 0o755, 0o640, 0o444, 0o400];
    const DIR_MODES: [u32; 3] = [0o755, 0o700, 0o750];
    for file in files {
        let mode = FILE_MODES[rng.index(FILE_MODES.len())];
        fs::set_permissions(root.join(file), fs::Permissions::from_mode(mode)).expect("chmod");
    }
    let mut deepest_first: Vec<&PathBuf> = dirs.iter().collect();
    deepest_first.sort_by_key(|dir| std::cmp::Reverse(dir.components().count()));
    for dir in deepest_first {
        let mode = DIR_MODES[rng.index(DIR_MODES.len())];
        fs::set_permissions(root.join(dir), fs::Permissions::from_mode(mode)).expect("chmod");
    }
}

fn generate(root: &Path, rng: &mut Rng) {
    fs::create_dir_all(root).expect("create the root");
    let mut serial = 0_u64;
    let mut dirs = vec![PathBuf::new()];
    let mut files: Vec<PathBuf> = Vec::new();
    let mut contents: Vec<Vec<u8>> = Vec::new();

    for _ in 0..rng.below(10) {
        serial += 1;
        let parent = dirs[rng.index(dirs.len())].clone();
        if parent.components().count() >= 4 {
            continue;
        }
        let dir = parent.join(name(rng, serial));
        fs::create_dir(root.join(&dir)).expect("mkdir");
        dirs.push(dir);
    }
    for _ in 0..rng.below(40) {
        serial += 1;
        let parent = dirs[rng.index(dirs.len())].clone();
        let path = parent.join(name(rng, serial));
        let data = content(rng, &contents);
        fs::write(root.join(&path), &data).expect("write file");
        if data.len() <= 100_000 {
            contents.push(data);
        }
        files.push(path);
    }
    #[cfg(unix)]
    {
        add_links(root, rng, &dirs, &files, &mut serial);
        randomize_modes(root, rng, &dirs, &files);
    }
}

/// Edits a tree the way a user does between two backups.
fn mutate(root: &Path, rng: &mut Rng, serial: &mut u64) {
    let tree = capture(root);
    let files: Vec<String> = tree
        .iter()
        .filter(|(_, item)| matches!(item.kind, Kind::File(_)))
        .map(|(path, _)| path.clone())
        .collect();
    let dirs: Vec<String> = tree
        .iter()
        .filter(|(_, item)| matches!(item.kind, Kind::Dir))
        .map(|(path, _)| path.clone())
        .collect();

    for _ in 0..(1 + rng.below(6)) {
        *serial += 1;
        match rng.below(5) {
            0 if !files.is_empty() => {
                let path = root.join(&files[rng.index(files.len())]);
                if fs::symlink_metadata(&path).is_err() {
                    continue;
                }
                let mut data = fs::read(&path).expect("read");
                edit(rng, &mut data);
                make_writable(&path);
                fs::write(&path, &data).expect("rewrite");
            }
            1 if files.len() > 1 => {
                let path = root.join(&files[rng.index(files.len())]);
                if fs::symlink_metadata(&path).is_ok() {
                    fs::remove_file(&path).expect("delete");
                }
            }
            2 => {
                let dir = root.join(&dirs[rng.index(dirs.len())]);
                let data = content(rng, &[]);
                fs::write(dir.join(name(rng, *serial)), &data).expect("add");
            }
            3 if !files.is_empty() => {
                let path = root.join(&files[rng.index(files.len())]);
                if fs::symlink_metadata(&path).is_ok() {
                    set_mode(&path, rng);
                }
            }
            _ if !files.is_empty() => {
                let path = root.join(&files[rng.index(files.len())]);
                if fs::symlink_metadata(&path).is_ok() {
                    let renamed = path.with_file_name(name(rng, *serial));
                    fs::rename(&path, &renamed).expect("rename");
                }
            }
            _ => {}
        }
    }
}

#[cfg(unix)]
fn make_writable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o644)).expect("chmod");
}

#[cfg(not(unix))]
fn make_writable(_path: &Path) {}

#[cfg(unix)]
fn set_mode(path: &Path, rng: &mut Rng) {
    use std::os::unix::fs::PermissionsExt;

    const MODES: [u32; 4] = [0o600, 0o644, 0o755, 0o444];
    let mode = MODES[rng.index(MODES.len())];
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _rng: &mut Rng) {}

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

fn mtime(meta: &fs::Metadata) -> (u64, u32) {
    let elapsed = meta
        .modified()
        .expect("modification time")
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after the epoch");
    (elapsed.as_secs(), elapsed.subsec_nanos())
}

/// Every entry below `root`, and `root` itself as the empty path, with content, permissions, and
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

fn fingerprint(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn describe(item: &Captured) -> String {
    let what = match &item.kind {
        Kind::Dir => "directory".to_owned(),
        Kind::File(data) => {
            format!(
                "file of {} bytes (fingerprint {:x})",
                data.len(),
                fingerprint(data)
            )
        }
        Kind::Link(target) => format!("link to {target:?}"),
    };
    format!("{what}, mode {:o}, modified {:?}", item.mode, item.modified)
}

fn assert_same(seed: u64, what: &str, expected: &Tree, actual: &Tree) {
    let missing: Vec<&String> = expected
        .keys()
        .filter(|path| !actual.contains_key(*path))
        .collect();
    let unexpected: Vec<&String> = actual
        .keys()
        .filter(|path| !expected.contains_key(*path))
        .collect();
    assert!(
        missing.is_empty() && unexpected.is_empty(),
        "seed {seed}, {what}: missing {missing:?}, unexpected {unexpected:?}"
    );
    for (path, want) in expected {
        let got = &actual[path];
        assert!(
            got == want,
            "seed {seed}, {what}: {path:?} differs: expected {}, got {}",
            describe(want),
            describe(got)
        );
    }
}

// ---------------------------------------------------------------------------------------------
// The scenario
// ---------------------------------------------------------------------------------------------

struct Env {
    work: TempDir,
    repo: Repository,
    keys: RepositoryKeys,
}

impl Env {
    async fn new() -> Self {
        let work = tempfile::tempdir().expect("create temporary directory");
        let repo = Repository::with_backend(Arc::new(MemoryBackend::new()));
        let kdf = KdfParams {
            m_cost_kib: 64,
            t_cost: 1,
            p_cost: 1,
        };
        repo.init_with_kdf(&passphrase(), kdf).await.expect("init");
        let keys = repo.unlock(&passphrase()).await.expect("unlock");
        Self { work, repo, keys }
    }

    async fn backup(&self, sources: &[PathBuf]) -> SnapshotId {
        let options = BackupOptions {
            sources: sources.to_vec(),
            chunker: ChunkerConfig::new(1024, 4096, 16_384).expect("valid config"),
            host: "roundtrip".to_owned(),
            jobs: 2,
            racy_margin: Duration::ZERO,
            ..BackupOptions::default()
        };
        let report = self
            .repo
            .backup(&self.keys, options)
            .await
            .expect("backup succeeds");
        assert!(!report.is_partial(), "{:?}", report.skipped);
        report.snapshot
    }

    /// Restores `id` into `name` below the work directory and returns the restored trees of
    /// `sources`, in order.
    async fn restore(
        &self,
        seed: u64,
        id: &SnapshotId,
        name: &str,
        sources: &[PathBuf],
    ) -> Vec<Tree> {
        let options = RestoreOptions::new(SnapshotRef::from(id), self.work.path().join(name));
        let report = self
            .repo
            .restore(&self.keys, options)
            .await
            .expect("restore succeeds");
        assert!(report.is_complete(), "seed {seed}: {report:?}");
        assert!(
            report.skipped.is_empty(),
            "seed {seed}: {:?}",
            report.skipped
        );
        sources
            .iter()
            .map(|source| {
                let canonical = fs::canonicalize(source).expect("the source exists");
                capture(
                    &report
                        .target
                        .join(canonical.strip_prefix("/").expect("absolute")),
                )
            })
            .collect()
    }
}

fn seeds() -> Vec<u64> {
    let one = std::env::var("VAULTSNAP_ROUNDTRIP_SEED")
        .ok()
        .and_then(|value| value.parse().ok());
    if let Some(seed) = one {
        return vec![seed];
    }
    let count = std::env::var("VAULTSNAP_ROUNDTRIP_SEEDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(10);
    (1..=count).collect()
}

async fn run_seed(seed: u64) {
    let mut rng = Rng::new(seed);
    let env = Env::new().await;
    let sources = vec![env.work.path().join("src-a"), env.work.path().join("src-b")];
    for source in &sources {
        generate(source, &mut rng);
    }

    let before: Vec<Tree> = sources.iter().map(|source| capture(source)).collect();
    let first = env.backup(&sources).await;
    let restored = env.restore(seed, &first, "out1", &sources).await;
    for (source, (want, got)) in sources.iter().zip(before.iter().zip(restored.iter())) {
        assert_same(
            seed,
            &format!("first restore of {}", source.display()),
            want,
            got,
        );
    }

    std::thread::sleep(Duration::from_millis(20));
    let mut serial = 1_000_000;
    for source in &sources {
        mutate(source, &mut rng, &mut serial);
    }
    let after: Vec<Tree> = sources.iter().map(|source| capture(source)).collect();
    let second = env.backup(&sources).await;
    assert_ne!(first, second);

    let restored = env.restore(seed, &second, "out2", &sources).await;
    for (source, (want, got)) in sources.iter().zip(after.iter().zip(restored.iter())) {
        assert_same(
            seed,
            &format!("second restore of {}", source.display()),
            want,
            got,
        );
    }
    // History is immutable: the first snapshot still restores to the first tree.
    let again = env.restore(seed, &first, "out3", &sources).await;
    for (source, (want, got)) in sources.iter().zip(before.iter().zip(again.iter())) {
        let what = format!("first snapshot, restored again, of {}", source.display());
        assert_same(seed, &what, want, got);
    }

    let check = CheckOptions {
        read_data: true,
        ..CheckOptions::default()
    };
    let report = env
        .repo
        .check(&env.keys, check)
        .await
        .expect("the check runs");
    assert!(report.is_clean(), "seed {seed}: {:?}", report.findings);
}

#[tokio::test]
async fn random_trees_survive_backup_change_backup_and_restore() {
    for seed in seeds() {
        run_seed(seed).await;
    }
}

#[test]
fn the_generator_is_deterministic_and_produces_varied_trees() {
    let first = tempfile::tempdir().expect("temporary directory");
    let second = tempfile::tempdir().expect("temporary directory");
    generate(&first.path().join("t"), &mut Rng::new(7));
    generate(&second.path().join("t"), &mut Rng::new(7));
    let one = capture(&first.path().join("t"));
    let two = capture(&second.path().join("t"));
    let names = |tree: &Tree| tree.keys().cloned().collect::<Vec<_>>();
    assert_eq!(
        names(&one),
        names(&two),
        "the same seed builds the same names"
    );
    let contents = |tree: &Tree| {
        tree.values()
            .filter_map(|item| match &item.kind {
                Kind::File(data) => Some(data.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(contents(&one), contents(&two), "and the same contents");

    let mut kinds = (0, 0, 0);
    for seed in 1..=20 {
        let dir = tempfile::tempdir().expect("temporary directory");
        generate(&dir.path().join("t"), &mut Rng::new(seed));
        for item in capture(&dir.path().join("t")).values() {
            match item.kind {
                Kind::Dir => kinds.0 += 1,
                Kind::File(_) => kinds.1 += 1,
                Kind::Link(_) => kinds.2 += 1,
            }
        }
    }
    assert!(
        kinds.0 > 20 && kinds.1 > 100,
        "directories and files: {kinds:?}"
    );
    #[cfg(unix)]
    assert!(kinds.2 > 10, "links: {kinds:?}");
}
