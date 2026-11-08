//! End-to-end regression tests: the compiled binary, used the way the documentation says to use
//! it, over a realistic history of a changing directory.
//!
//! * A repository lives through three backups with edits in between; every snapshot restores to
//!   exactly the files it was made from; `diff` reports what changed; a full `check` is clean;
//!   `prune` keeps the newest and the survivor still restores.
//! * The scheduled invocation (`docs/GUIDE.md`): configuration from the environment, no terminal,
//!   no arguments that carry secrets, and exit statuses a scheduler can act on.
//! * Version information.
//!
//! Single features have their own test files (`backup.rs`, `restore.rs`, `check.rs`, `prune.rs`,
//! `inspect.rs`); this file is the one that breaks if the pieces stop fitting together.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use common::{run_without_repo, stderr, stdout, TestEnv, PASSPHRASE};

type Files = BTreeMap<String, Vec<u8>>;

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

/// Relative path to content, for regular files.
fn files_below(root: &Path) -> Files {
    let mut found = Files::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).expect("read_dir") {
            let path = entry.expect("entry").path();
            let meta = fs::symlink_metadata(&path).expect("stat");
            if meta.is_dir() {
                pending.push(path);
            } else if meta.is_file() {
                let relative = path.strip_prefix(root).expect("inside");
                found.insert(
                    relative.to_string_lossy().into_owned(),
                    fs::read(&path).expect("read"),
                );
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

fn json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or_else(|err| panic!("not JSON ({err}):\n{text}"))
}

struct Repo {
    env: TestEnv,
    password_file: PathBuf,
}

impl Repo {
    fn new() -> Self {
        let env = TestEnv::default();
        let password_file = env.password_file();
        let output = env.run_with_password_file(&password_file, &["init"]);
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        let text = stdout(&output);
        assert!(text.starts_with("created VaultSnap repository "), "{text}");
        Self { env, password_file }
    }

    fn run(&self, args: &[&str]) -> Output {
        self.env.run_with_password_file(&self.password_file, args)
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{args:?}: {}",
            stderr(&output)
        );
        stdout(&output)
    }

    fn backup(&self, source: &Path) -> String {
        let text = self.ok(&["backup", source.to_str().expect("UTF-8 path")]);
        text.lines()
            .find_map(|line| line.strip_prefix("created snapshot "))
            .unwrap_or_else(|| panic!("no snapshot id in:\n{text}"))
            .to_owned()
    }

    fn restore(&self, id: &str, name: &str, source: &Path) -> Files {
        let target = self.env.path(name);
        let target_text = target.to_str().expect("UTF-8 path");
        self.ok(&["restore", id, "--target", target_text]);
        files_below(&restored(&target, source))
    }

    fn snapshot_ids(&self) -> Vec<String> {
        let document = json(&self.ok(&["snapshots", "--json"]));
        document["snapshots"]
            .as_array()
            .expect("an array")
            .iter()
            .map(|snapshot| snapshot["id"].as_str().expect("an id").to_owned())
            .collect()
    }
}

// ---------------------------------------------------------------------------------------------
// A repository over time
// ---------------------------------------------------------------------------------------------

#[test]
fn a_repository_lives_through_backups_edits_restores_checks_and_a_prune() {
    let repo = Repo::new();
    let src = repo.env.path("data");
    write(
        &src,
        "docs/report.txt",
        &b"quarterly numbers\n".repeat(2000),
    );
    write(&src, "docs/notes.txt", b"remember the milk");
    write(&src, "photos/one.bin", &pseudo_random(150_000, 1));
    write(&src, "photos/two.bin", &pseudo_random(90_000, 2));
    write(&src, "old/obsolete.txt", b"to be deleted");
    write(&src, "unicode/日本語 ファイル.txt", "こんにちは".as_bytes());
    let first_files = files_below(&src);
    let first = repo.backup(&src);

    std::thread::sleep(std::time::Duration::from_millis(20));
    write(
        &src,
        "docs/report.txt",
        &b"quarterly numbers, revised\n".repeat(2100),
    );
    fs::remove_file(src.join("old/obsolete.txt")).expect("delete");
    write(&src, "photos/new.bin", &pseudo_random(60_000, 3));
    let second_files = files_below(&src);
    let second = repo.backup(&src);

    // Nothing changed: a third snapshot with the same files.
    let third = repo.backup(&src);

    assert_eq!(
        repo.snapshot_ids(),
        [first.clone(), second.clone(), third.clone()]
    );
    assert_eq!(repo.restore(&first, "out1", &src), first_files);
    assert_eq!(repo.restore(&second, "out2", &src), second_files);
    assert_eq!(repo.restore(&third, "out3", &src), second_files);

    let diff = json(&repo.ok(&["diff", &first, &second, "--json"]));
    assert_eq!(diff["summary"]["added"], 1);
    assert_eq!(diff["summary"]["deleted"], 1);
    assert_eq!(diff["summary"]["modified"], 1);
    let nothing = json(&repo.ok(&["diff", &second, &third, "--json"]));
    assert_eq!(nothing["summary"]["added"], 0);
    assert_eq!(nothing["summary"]["deleted"], 0);
    assert_eq!(nothing["summary"]["modified"], 0);

    let check = repo.run(&["check", "--read-data"]);
    assert_eq!(
        check.status.code(),
        Some(0),
        "{}{}",
        stdout(&check),
        stderr(&check)
    );

    let dry = repo.ok(&["prune", "--keep-last", "1", "--dry-run"]);
    assert!(!dry.is_empty());
    assert_eq!(repo.snapshot_ids().len(), 3, "a dry run removes nothing");

    repo.ok(&["prune", "--keep-last", "1", "--yes"]);
    assert_eq!(
        repo.snapshot_ids(),
        std::slice::from_ref(&third),
        "only the newest is kept"
    );
    let check = repo.run(&["check", "--read-data"]);
    assert_eq!(
        check.status.code(),
        Some(0),
        "{}{}",
        stdout(&check),
        stderr(&check)
    );
    assert_eq!(
        repo.restore(&third, "out4", &src),
        second_files,
        "the survivor restores"
    );

    let shown = repo.ok(&["show", &third[..8]]);
    assert!(shown.contains(&third), "{shown}");
}

#[test]
fn a_wrong_passphrase_is_refused_everywhere_and_changes_nothing() {
    let repo = Repo::new();
    let src = repo.env.path("data");
    write(&src, "a.txt", b"secret plans");
    repo.backup(&src);

    let wrong = repo
        .env
        .write_password_file("wrong", b"not the passphrase\n");
    let before = files_below(&repo.env.repo_path());
    for args in [
        vec!["snapshots"],
        vec!["check"],
        vec!["backup", src.to_str().expect("UTF-8 path")],
        vec!["prune", "--keep-last", "1", "--yes"],
    ] {
        let output = repo.env.run_with_password_file(&wrong, &args);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{args:?}: {}",
            stderr(&output)
        );
        assert!(stdout(&output).is_empty(), "{args:?}: {}", stdout(&output));
    }
    let after = files_below(&repo.env.repo_path());
    assert_eq!(after, before, "a refused command changed the repository");
}

// ---------------------------------------------------------------------------------------------
// The scheduled invocation
// ---------------------------------------------------------------------------------------------

/// `vaultsnap` the way cron, systemd, and launchd run it: configuration from the environment, no
/// terminal on standard input, nothing secret in the arguments.
fn scheduled(env: &TestEnv, password_file: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_vaultsnap"))
        .env_remove("RUST_LOG")
        .env("VAULTSNAP_REPO", env.repo_path())
        .env("VAULTSNAP_PASSWORD_FILE", password_file)
        .stdin(Stdio::null())
        .args(args)
        .output()
        .expect("run vaultsnap")
}

#[test]
fn the_scheduled_invocation_needs_no_terminal_and_no_secret_arguments() {
    let env = TestEnv::default();
    let password_file = env.password_file();
    let src = env.path("data");
    write(&src, "daily/file.txt", b"version 1");
    let source = src.to_str().expect("UTF-8 path");

    let output = scheduled(&env, &password_file, &["init"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    for round in 0..4 {
        write(
            &src,
            "daily/file.txt",
            format!("version {round}").as_bytes(),
        );
        let output = scheduled(
            &env,
            &password_file,
            &["backup", source, "--tag", "scheduled"],
        );
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        assert!(!stderr(&output).contains(PASSPHRASE) && !stdout(&output).contains(PASSPHRASE));
    }
    let output = scheduled(
        &env,
        &password_file,
        &["prune", "--keep-last", "2", "--yes"],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let output = scheduled(&env, &password_file, &["check"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let output = scheduled(&env, &password_file, &["snapshots", "--json"]);
    assert_eq!(
        json(&stdout(&output))["snapshots"]
            .as_array()
            .expect("an array")
            .len(),
        2
    );

    // Without --yes and without a terminal, prune refuses instead of waiting for an answer.
    let output = scheduled(&env, &password_file, &["prune", "--keep-last", "1"]);
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(stderr(&output).contains("--yes"), "{}", stderr(&output));
    let output = scheduled(&env, &password_file, &["snapshots", "--json"]);
    assert_eq!(
        json(&stdout(&output))["snapshots"]
            .as_array()
            .expect("an array")
            .len(),
        2
    );
}

#[test]
fn a_scheduler_can_tell_failure_from_a_problem_in_the_result() {
    let env = TestEnv::default();
    let password_file = env.password_file();

    // 1: the command could not run (no repository there).
    let output = scheduled(&env, &password_file, &["snapshots"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));

    // 2: a usage error, before anything runs.
    let output = scheduled(&env, &password_file, &["backup"]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));

    // 3: damage found by a check.
    let output = scheduled(&env, &password_file, &["init"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let src = env.path("data");
    write(&src, "file.bin", &pseudo_random(40_000, 9));
    let source = src.to_str().expect("UTF-8 path");
    let output = scheduled(&env, &password_file, &["backup", source]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let chunk = first_chunk_object(&env.repo_path().join("data"));
    fs::remove_file(&chunk).expect("remove a chunk object");
    let output = scheduled(&env, &password_file, &["check"]);
    assert_eq!(
        output.status.code(),
        Some(3),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
}

fn first_chunk_object(data: &Path) -> PathBuf {
    let mut pending = vec![data.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).expect("read_dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                pending.push(path);
            } else {
                return path;
            }
        }
    }
    panic!("no chunk object in {}", data.display());
}

// ---------------------------------------------------------------------------------------------
// Version information
// ---------------------------------------------------------------------------------------------

#[test]
fn version_information_names_the_build_and_the_formats() {
    let expected = format!("vaultsnap {}", env!("CARGO_PKG_VERSION"));
    for flag in ["--version", "-V"] {
        let output = run_without_repo(&[flag]);
        assert_eq!(output.status.code(), Some(0), "{flag}");
        let text = stdout(&output);
        assert!(text.starts_with(&expected), "{flag}: {text}");
        assert!(
            text.trim_end().ends_with(')'),
            "the commit is shown: {text}"
        );
    }

    let output = run_without_repo(&["version"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    let value = |name: &str| -> String {
        text.lines()
            .find_map(|line| line.strip_prefix(name))
            .map(|rest| rest.trim().to_owned())
            .unwrap_or_else(|| panic!("no `{name}` line in:\n{text}"))
    };
    assert_eq!(value("version"), env!("CARGO_PKG_VERSION"));
    assert_eq!(value("repository format"), "1");
    assert_eq!(value("snapshot schema"), "1");
    assert_eq!(value("chunk object format"), "1");
    assert_eq!(value("key file version"), "1");
    assert!(value("encryption").contains("XChaCha20-Poly1305"), "{text}");
    assert!(
        !value("target").is_empty() && !value("commit").is_empty(),
        "{text}"
    );
}
