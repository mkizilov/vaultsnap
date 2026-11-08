//! Integration tests of `vaultsnap prune`, run against the compiled binary on repositories made by
//! real `backup` runs. They check what a user meets: that nothing is deleted without `--yes` or
//! `--dry-run`, the plan that is printed, what a confirmed prune leaves behind, the exit statuses,
//! and that a repository is never changed by a refused or failed prune.
//!
//! The question put to a person at a terminal (`Type "yes"`) needs a terminal, which a test
//! cannot provide without `unsafe`; its logic is covered by unit tests in `src/prune/tests.rs`
//! and the flow around it is unverified end to end.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use common::{stderr, stdout, TestEnv};

type Tree = BTreeMap<String, Vec<u8>>;

/// What a prune of `Repo::with_history` under `--keep-last 1` says after verifying the kept one.
const VERIFIED: &str = "\nVerified     all 2 chunks used by the kept snapshots are present\n";

struct Repo {
    env: TestEnv,
    password_file: PathBuf,
    ids: Vec<String>,
}

impl Repo {
    fn new() -> Self {
        let env = TestEnv::default();
        let password_file = env.password_file();
        let output = env.run_with_password_file(&password_file, &["init"]);
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        Self {
            env,
            password_file,
            ids: Vec::new(),
        }
    }

    /// A repository with three snapshots of a source that changes between them. Every file is a
    /// single chunk, so the snapshots share exactly one chunk and each of the first two has one
    /// chunk of its own.
    fn with_history() -> Self {
        let mut repo = Self::new();
        let src = repo.env.path("src");
        write(&src, "shared.bin", &pseudo_random(20_000, 1));
        write(&src, "first.bin", &pseudo_random(6000, 2));
        repo.take(&src);
        fs::remove_file(src.join("first.bin")).expect("remove");
        write(&src, "second.bin", &pseudo_random(6000, 3));
        repo.take(&src);
        fs::remove_file(src.join("second.bin")).expect("remove");
        write(&src, "third.bin", &pseudo_random(6000, 4));
        repo.take(&src);
        repo
    }

    fn run(&self, args: &[&str]) -> Output {
        self.env.run_with_password_file(&self.password_file, args)
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert_eq!(output.status.code(), Some(0), "{args:?}: {}", stderr(&output));
        stdout(&output)
    }

    fn take(&mut self, source: &Path) {
        let text = self.ok(&["backup", source.to_str().expect("UTF-8 path")]);
        let id = text
            .lines()
            .find_map(|line| line.strip_prefix("created snapshot "))
            .unwrap_or_else(|| panic!("no snapshot id in:\n{text}"));
        self.ids.push(id.to_owned());
    }

    fn state(&self) -> Tree {
        files_below(&self.env.repo_path())
    }

    fn snapshot_files(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.env.repo_path().join("snapshots"))
            .expect("read snapshots")
            .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn assert_unlocked(&self) {
        let locks = fs::read_dir(self.env.repo_path().join("locks")).expect("read locks").count();
        assert_eq!(locks, 0, "no lock is left behind");
    }
}

fn write(root: &Path, rel: &str, contents: &[u8]) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parents");
    }
    fs::write(path, contents).expect("write file");
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

/// Relative path to content, for files only.
fn files_below(root: &Path) -> Tree {
    let mut found = Tree::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).expect("read_dir") {
            let path = entry.expect("entry").path();
            let meta = fs::symlink_metadata(&path).expect("stat");
            if meta.is_dir() {
                pending.push(path);
            } else if meta.is_file() {
                let relative = path.strip_prefix(root).expect("inside").to_string_lossy();
                found.insert(relative.into_owned(), fs::read(&path).expect("read"));
            }
        }
    }
    found
}

/// Where a source is restored below `target`.
fn restored(target: &Path, source: &Path) -> PathBuf {
    let canonical = fs::canonicalize(source).expect("the source exists");
    let target = fs::canonicalize(target).expect("the target exists");
    target.join(canonical.strip_prefix("/").expect("absolute"))
}

/// The line of the plan that names the snapshot (by the first 8 characters of its id).
fn plan_line<'a>(text: &'a str, id: &str) -> &'a str {
    text.lines()
        .find(|line| line.contains(&id[..8]))
        .unwrap_or_else(|| panic!("no line for {id} in:\n{text}"))
}

/// The plan of `with_history` under `--keep-last 1`: the first two snapshots go, the third stays.
fn assert_the_plan(text: &str, ids: &[String]) {
    assert!(text.starts_with("Policy       keep last 1\n"), "{text}");
    assert!(text.contains("Snapshots    3 (1 kept, 2 to remove)\n"), "{text}");
    for id in &ids[..2] {
        let line = plan_line(text, id);
        assert!(line.starts_with("  REMOVE  "), "{line:?}");
        assert!(line.ends_with('Z'), "{line:?}: the time is in UTC");
    }
    let kept = plan_line(text, &ids[2]);
    assert!(kept.starts_with("  KEEP    "), "{kept:?}");
    assert!(kept.ends_with("Z  last"), "{kept:?}: it says why it is kept");
    assert!(text.contains("Chunks       2 in use by the kept snapshots\n"), "{text}");
    assert!(text.contains("Unreferenced 2 objects ("), "{text}");
    assert!(text.contains(") that no kept snapshot needs\n"), "{text}");
}

#[test]
fn help_documents_every_option() {
    let repo = Repo::new();
    let text = repo.ok(&["prune", "--help"]);
    for option in [
        "--keep-last",
        "--keep-daily",
        "--keep-weekly",
        "--keep-monthly",
        "--keep-yearly",
        "--dry-run",
        "--yes",
    ] {
        assert!(text.contains(option), "`{option}` is not in:\n{text}");
    }
}

#[test]
fn a_dry_run_shows_the_plan_and_changes_nothing() {
    let repo = Repo::with_history();
    let before = repo.state();

    let text = repo.ok(&["prune", "--keep-last", "1", "--dry-run"]);
    assert_the_plan(&text, &repo.ids);
    assert!(text.ends_with("\nDry run: nothing was changed.\n"), "{text}");
    assert!(!text.contains("Removed"), "{text}");
    assert_eq!(repo.state(), before);
    repo.assert_unlocked();

    // --dry-run wins over --yes.
    let both = repo.ok(&["prune", "--keep-last", "1", "--yes", "--dry-run"]);
    assert!(both.ends_with("\nDry run: nothing was changed.\n"), "{both}");
    assert_eq!(repo.state(), before);
}

#[test]
fn yes_deletes_what_the_plan_showed_and_every_kept_snapshot_stays_whole() {
    let repo = Repo::with_history();
    let src = repo.env.path("src");
    let kept = repo.ids[2].clone();

    let text = repo.ok(&["prune", "--keep-last", "1", "--yes"]);
    assert_the_plan(&text, &repo.ids);
    assert!(text.contains("\nRemoved      2 snapshots and 2 chunk objects ("), "{text}");
    assert!(text.contains(VERIFIED), "{text}");
    assert!(!text.contains("Dry run"), "{text}");
    assert_eq!(repo.snapshot_files(), [kept.clone()]);
    repo.assert_unlocked();

    let listing = repo.ok(&["snapshots"]);
    assert!(listing.contains(&kept[..8]), "{listing}");
    for gone in &repo.ids[..2] {
        assert!(!listing.contains(&gone[..8]), "{gone} should be gone:\n{listing}");
    }

    let target = repo.env.path("out");
    repo.ok(&["restore", &kept, "--target", target.to_str().expect("UTF-8 path")]);
    assert_eq!(files_below(&restored(&target, &src)), files_below(&src));
    repo.ok(&["check", "--read-data"]);
}

#[test]
fn pruning_again_has_nothing_to_do() {
    let repo = Repo::with_history();
    repo.ok(&["prune", "--keep-last", "1", "--yes"]);
    let after = repo.state();

    let text = repo.ok(&["prune", "--keep-last", "1", "--yes"]);
    assert!(text.contains("Snapshots    1 (1 kept, 0 to remove)\n"), "{text}");
    assert!(text.contains("\nRemoved      0 snapshots and 0 chunk objects (0 B)\n"), "{text}");
    assert_eq!(repo.state(), after);
}

#[test]
fn a_policy_that_keeps_everything_removes_nothing() {
    let repo = Repo::with_history();
    let before = repo.state();
    let text = repo.ok(&["prune", "--keep-last", "10", "--yes"]);
    assert!(text.contains("Snapshots    3 (3 kept, 0 to remove)\n"), "{text}");
    assert!(text.contains("\nRemoved      0 snapshots and 0 chunk objects (0 B)\n"), "{text}");
    assert_eq!(repo.state(), before);
}

#[test]
fn without_a_person_to_ask_and_without_yes_nothing_is_deleted() {
    let repo = Repo::with_history();
    let before = repo.state();

    // The test harness gives the child no terminal on standard input.
    let output = repo.run(&["prune", "--keep-last", "1"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let text = stderr(&output);
    assert!(text.contains("standard input is not a terminal"), "{text}");
    assert!(text.contains("--dry-run") && text.contains("--yes"), "{text}");
    assert_eq!(stdout(&output), "");
    assert_eq!(repo.state(), before);
    repo.assert_unlocked();
}

#[test]
fn policies_that_could_delete_everything_are_refused_and_nothing_is_touched() {
    let repo = Repo::with_history();
    let before = repo.state();
    let cases: [(&[&str], &str); 3] = [
        (&["prune", "--yes"], "at least one keep rule is required"),
        (&["prune", "--keep-last", "0", "--yes"], "keep counts must be greater than zero"),
        (&["prune", "--keep-last", "2", "--keep-daily", "0", "--dry-run"], "greater than zero"),
    ];
    for (args, message) in cases {
        let output = repo.run(args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert!(stderr(&output).contains(message), "{args:?}: {}", stderr(&output));
        assert_eq!(stdout(&output), "", "{args:?}");
    }
    assert_eq!(repo.state(), before);

    let output = repo.run(&["prune", "--keep-last", "many", "--yes"]);
    assert_eq!(output.status.code(), Some(2), "a usage error: {}", stderr(&output));
    assert_eq!(repo.state(), before);
}

#[test]
fn a_wrong_passphrase_prunes_nothing() {
    let repo = Repo::with_history();
    let before = repo.state();
    let wrong = repo.env.write_password_file("wrong", b"not the passphrase\n");

    let output = repo.env.run_with_password_file(&wrong, &["prune", "--keep-last", "1", "--yes"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("incorrect passphrase"), "{}", stderr(&output));
    assert_eq!(repo.state(), before);
    repo.assert_unlocked();
}

#[test]
fn a_snapshot_that_cannot_be_read_stops_the_prune_before_anything_is_deleted() {
    let repo = Repo::with_history();
    let victim = repo.env.repo_path().join("snapshots").join(&repo.ids[1]);
    let mut bytes = fs::read(&victim).expect("read");
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0x01;
    fs::write(&victim, bytes).expect("write");
    let before = repo.state();

    for args in [
        ["prune", "--keep-last", "1", "--yes"].as_slice(),
        ["prune", "--keep-last", "1", "--dry-run"].as_slice(),
    ] {
        let output = repo.run(args);
        assert_eq!(output.status.code(), Some(1), "{args:?}: {}", stderr(&output));
        let text = stderr(&output);
        assert!(text.contains("1 snapshot(s) cannot be read"), "{text}");
        assert!(text.contains(&repo.ids[1]), "the damaged snapshot is named: {text}");
        assert_eq!(stdout(&output), "", "no plan is printed from data that cannot be trusted");
        assert_eq!(repo.state(), before);
    }
    repo.assert_unlocked();
}

#[cfg(unix)]
struct Writable(Vec<PathBuf>);

#[cfg(unix)]
impl Drop for Writable {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;

        for dir in &self.0 {
            let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o755));
        }
    }
}

#[cfg(unix)]
#[test]
fn objects_that_cannot_be_deleted_make_the_prune_incomplete_and_harm_no_kept_snapshot() {
    use std::os::unix::fs::PermissionsExt;

    let repo = Repo::with_history();
    let shards: Vec<PathBuf> = fs::read_dir(repo.env.repo_path().join("data"))
        .expect("read data")
        .map(|entry| entry.expect("entry").path())
        .filter(|path| path.is_dir())
        .collect();
    let _restore_permissions = Writable(shards.clone());
    for shard in &shards {
        fs::set_permissions(shard, fs::Permissions::from_mode(0o555)).expect("chmod");
    }
    let probe = shards[0].join(".probe");
    if fs::write(&probe, b"x").is_ok() {
        let _ = fs::remove_file(&probe);
        eprintln!("skipped: directory permissions do not bind this user (root?); unverified");
        return;
    }

    let output = repo.run(&["prune", "--keep-last", "1", "--yes"]);
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    let errors = stderr(&output);
    assert!(errors.contains("2 could not be deleted:"), "{errors}");
    assert!(errors.contains("2 unreferenced object(s) could not be deleted"), "{errors}");
    let text = stdout(&output);
    assert!(text.contains("\nRemoved      2 snapshots and 0 chunk objects (0 B)\n"), "{text}");
    assert!(text.contains("\nNot removed  2 objects (listed on standard error"), "{text}");
    assert!(text.contains(VERIFIED), "{text}");
    assert_eq!(repo.snapshot_files(), [repo.ids[2].clone()]);
    repo.assert_unlocked();

    // The leftovers are orphans, which a check reports as warnings, and the kept snapshot is whole.
    repo.ok(&["check", "--read-data"]);

    for shard in &shards {
        fs::set_permissions(shard, fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let text = repo.ok(&["prune", "--keep-last", "1", "--yes"]);
    assert!(text.contains("\nRemoved      0 snapshots and 2 chunk objects ("), "{text}");
    repo.ok(&["check", "--read-data"]);
}
