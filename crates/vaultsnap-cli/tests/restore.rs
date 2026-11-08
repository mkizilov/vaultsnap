//! Integration tests of `vaultsnap restore`, run against the compiled binary on repositories made
//! by real `backup` runs: the restored files are compared byte for byte with the originals, and
//! the conflict modes, selected paths, link policy, damaged data, exit statuses, and the output
//! are checked the way a user meets them.
//!
//! The engine's handling of hostile manifests and targets is covered by the unit tests in
//! `vaultsnap-core`; a real backup cannot produce such manifests.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use common::{stderr, stdout, TestEnv};

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
        Self { env, password_file }
    }

    fn run(&self, args: &[&str]) -> Output {
        self.env.run_with_password_file(&self.password_file, args)
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert_eq!(output.status.code(), Some(0), "{args:?}: {}", stderr(&output));
        stdout(&output)
    }

    fn backup(&self, source: &Path) -> String {
        let text = self.ok(&["backup", source.to_str().expect("UTF-8 path")]);
        text.lines()
            .find_map(|line| line.strip_prefix("created snapshot "))
            .unwrap_or_else(|| panic!("no snapshot id in:\n{text}"))
            .to_owned()
    }

    /// Restores with extra arguments after `--target`.
    fn restore(&self, id: &str, target: &Path, extra: &[&str]) -> Output {
        let target = target.to_str().expect("UTF-8 path");
        let mut args = vec!["restore", id, "--target", target];
        args.extend_from_slice(extra);
        self.run(&args)
    }

    fn objects(&self) -> BTreeSet<PathBuf> {
        let mut found = BTreeSet::new();
        let mut pending = vec![self.env.repo_path().join("data")];
        while let Some(dir) = pending.pop() {
            for entry in fs::read_dir(&dir).expect("read_dir") {
                let path = entry.expect("entry").path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    found.insert(path);
                }
            }
        }
        found
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

/// Where a source is restored below `target`.
fn restored(target: &Path, source: &Path) -> PathBuf {
    let canonical = fs::canonicalize(source).expect("the source exists");
    let target = fs::canonicalize(target).expect("the target exists");
    target.join(canonical.strip_prefix("/").expect("absolute"))
}

/// Relative path to content, for files only.
fn files_below(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut found = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).expect("read_dir") {
            let path = entry.expect("entry").path();
            let meta = fs::symlink_metadata(&path).expect("stat");
            if meta.is_dir() {
                pending.push(path);
            } else if meta.is_file() {
                let relative = path.strip_prefix(root).expect("inside").to_string_lossy().into_owned();
                found.insert(relative, fs::read(&path).expect("read"));
            }
        }
    }
    found
}

fn sample(src: &Path) {
    write(src, "readme.txt", b"hello restore");
    write(src, "empty.bin", b"");
    write(src, "docs/report.txt", &b"quarterly numbers\n".repeat(2000));
    write(src, "docs/archive/data.bin", &pseudo_random(200_000, 1));
    write(src, "unicode/日本語 ファイル.txt", "こんにちは".as_bytes());
    write(src, "with space/new\nline", b"odd names");
}

// ---------------------------------------------------------------------------------------------
// Round trips
// ---------------------------------------------------------------------------------------------

#[test]
fn a_snapshot_is_restored_byte_for_byte() {
    let repo = Repo::new();
    let src = repo.env.path("src");
    sample(&src);
    let id = repo.backup(&src);
    let target = repo.env.path("out");

    let text = repo.ok(&["restore", &id, "--target", target.to_str().expect("UTF-8 path")]);
    assert!(text.starts_with(&format!("restored snapshot {id} into ")), "{text}");
    assert!(text.contains("  files         6 ("), "{text}");
    assert!(text.contains("  directories   "), "{text}");

    let root = restored(&target, &src);
    assert_eq!(files_below(&root), files_below(&src));
    assert_eq!(fs::read(root.join("docs/archive/data.bin")).expect("read"), pseudo_random(200_000, 1));
    let locks = fs::read_dir(repo.env.repo_path().join("locks")).expect("read locks").count();
    assert_eq!(locks, 0, "no lock is left behind");
}

#[test]
fn the_start_of_an_id_is_enough_and_older_snapshots_restore_as_they_were() {
    let repo = Repo::new();
    let src = repo.env.path("src");
    write(&src, "a.txt", b"first version");
    let first = repo.backup(&src);
    write(&src, "a.txt", b"second version");
    write(&src, "b.txt", b"added later");
    let second = repo.backup(&src);

    let target = repo.env.path("old");
    let output = repo.restore(&first[..12], &target, &[]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let files = files_below(&restored(&target, &src));
    assert_eq!(files.keys().collect::<Vec<_>>(), ["a.txt"]);
    assert_eq!(files["a.txt"], b"first version");

    let target = repo.env.path("new");
    assert_eq!(repo.restore(&second, &target, &[]).status.code(), Some(0));
    assert_eq!(files_below(&restored(&target, &src)), files_below(&src));
}

#[test]
fn selected_paths_are_restored_and_nothing_else() {
    let repo = Repo::new();
    let src = repo.env.path("src");
    sample(&src);
    let id = repo.backup(&src);
    let root = fs::canonicalize(&src).expect("exists");
    let archive = root.join("docs/archive");
    let readme = root.join("readme.txt");
    let target = repo.env.path("out");

    let output = repo.restore(
        &id,
        &target,
        &[
            "--include",
            archive.to_str().expect("UTF-8 path"),
            "--include",
            readme.to_str().expect("UTF-8 path"),
        ],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let files = files_below(&restored(&target, &src));
    assert_eq!(files.keys().map(String::as_str).collect::<Vec<_>>(), ["docs/archive/data.bin", "readme.txt"]);
    assert_eq!(files["readme.txt"], b"hello restore");
}

#[test]
fn an_unknown_selection_fails_and_creates_nothing() {
    let repo = Repo::new();
    let src = repo.env.path("src");
    sample(&src);
    let id = repo.backup(&src);
    let target = repo.env.path("out");

    let output = repo.restore(&id, &target, &["--include", "/no/such/path"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("has no entry at or below /no/such/path"), "{}", stderr(&output));
    assert!(!target.exists());
}

// ---------------------------------------------------------------------------------------------
// Conflicts
// ---------------------------------------------------------------------------------------------

fn target_with_a_different_file(repo: &Repo, id: &str, src: &Path) -> (PathBuf, PathBuf) {
    let target = repo.env.path("out");
    let first = repo.restore(id, &target, &[]);
    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));
    let readme = restored(&target, src).join("readme.txt");
    fs::write(&readme, b"my own version").expect("write");
    fs::remove_file(restored(&target, src).join("empty.bin")).expect("remove");
    (target, readme)
}

#[test]
fn the_default_is_to_fail_and_change_nothing() {
    let repo = Repo::new();
    let src = repo.env.path("src");
    sample(&src);
    let id = repo.backup(&src);
    let (target, readme) = target_with_a_different_file(&repo, &id, &src);
    let before = files_below(&target);

    let output = repo.restore(&id, &target, &[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).is_empty());
    assert!(stderr(&output).contains("already exist"), "{}", stderr(&output));
    assert!(stderr(&output).contains("nothing was restored"), "{}", stderr(&output));
    assert_eq!(files_below(&target), before, "the target is exactly as it was");
    assert_eq!(fs::read(readme).expect("read"), b"my own version");
}

#[test]
fn skip_keeps_what_is_there_and_fills_the_gaps() {
    let repo = Repo::new();
    let src = repo.env.path("src");
    sample(&src);
    let id = repo.backup(&src);
    let (target, readme) = target_with_a_different_file(&repo, &id, &src);

    let output = repo.restore(&id, &target, &["--on-conflict", "skip"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stdout(&output).contains("  skipped       5 "), "{}", stdout(&output));
    assert!(stderr(&output).contains("5 skipped:"), "{}", stderr(&output));
    assert_eq!(fs::read(readme).expect("read"), b"my own version");
    assert!(restored(&target, &src).join("empty.bin").exists(), "the missing file was restored");
}

#[test]
fn overwrite_replaces_what_is_there() {
    let repo = Repo::new();
    let src = repo.env.path("src");
    sample(&src);
    let id = repo.backup(&src);
    let (target, readme) = target_with_a_different_file(&repo, &id, &src);

    let output = repo.restore(&id, &target, &["--on-conflict", "overwrite"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(fs::read(readme).expect("read"), b"hello restore");
    assert_eq!(files_below(&restored(&target, &src)), files_below(&src));
}

// ---------------------------------------------------------------------------------------------
// Symbolic links
// ---------------------------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn links_are_restored_only_when_they_cannot_lead_out() {
    let repo = Repo::new();
    let src = repo.env.path("src");
    write(&src, "inside.txt", b"inside");
    std::os::unix::fs::symlink("inside.txt", src.join("good.link")).expect("symlink");
    std::os::unix::fs::symlink("/etc/passwd", src.join("absolute.link")).expect("symlink");
    let id = repo.backup(&src);

    let target = repo.env.path("default");
    let output = repo.restore(&id, &target, &[]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let root = restored(&target, &src);
    assert!(root.join("good.link").symlink_metadata().is_ok());
    assert!(root.join("absolute.link").symlink_metadata().is_err(), "refused by default");
    assert!(stderr(&output).contains("1 skipped:"), "{}", stderr(&output));
    assert!(stderr(&output).contains("absolute.link"), "{}", stderr(&output));
    assert!(stderr(&output).contains("--symlinks as-recorded"), "{}", stderr(&output));

    let target = repo.env.path("none");
    assert_eq!(repo.restore(&id, &target, &["--symlinks", "skip"]).status.code(), Some(0));
    assert!(restored(&target, &src).join("good.link").symlink_metadata().is_err());

    let target = repo.env.path("all");
    assert_eq!(repo.restore(&id, &target, &["--symlinks", "as-recorded"]).status.code(), Some(0));
    let link = fs::read_link(restored(&target, &src).join("absolute.link")).expect("link");
    assert_eq!(link, Path::new("/etc/passwd"));
}

// ---------------------------------------------------------------------------------------------
// Damaged data
// ---------------------------------------------------------------------------------------------

#[test]
fn a_damaged_chunk_fails_that_file_alone_and_exits_with_status_three() {
    let repo = Repo::new();
    let src = repo.env.path("src");
    write(&src, "good-1.txt", b"a good file");
    write(&src, "good-2.txt", b"another good file");
    repo.backup(&src);
    let known = repo.objects();
    write(&src, "victim.bin", &pseudo_random(5000, 9));
    let id = repo.backup(&src);
    let added: Vec<PathBuf> = repo.objects().difference(&known).cloned().collect();
    assert!(!added.is_empty());
    for object in &added {
        let mut bytes = fs::read(object).expect("read object");
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0x01;
        fs::write(object, bytes).expect("write object");
    }

    let target = repo.env.path("out");
    let output = repo.restore(&id, &target, &[]);
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert!(stderr(&output).contains("1 could not be restored:"), "{}", stderr(&output));
    assert!(stderr(&output).contains("victim.bin"), "{}", stderr(&output));
    assert!(stderr(&output).contains("corrupt"), "{}", stderr(&output));
    assert!(stderr(&output).contains("the restore is incomplete"), "{}", stderr(&output));
    assert!(stdout(&output).contains("  failed        1"), "{}", stdout(&output));

    let files = files_below(&restored(&target, &src));
    assert_eq!(files.keys().map(String::as_str).collect::<Vec<_>>(), ["good-1.txt", "good-2.txt"]);
    assert_eq!(files["good-1.txt"], b"a good file");
}

#[test]
fn a_tampered_snapshot_restores_nothing() {
    let repo = Repo::new();
    let src = repo.env.path("src");
    sample(&src);
    let id = repo.backup(&src);
    let path = repo.env.repo_path().join("snapshots").join(&id);
    let mut bytes = fs::read(&path).expect("read snapshot file");
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0x01;
    fs::write(&path, bytes).expect("write snapshot file");

    let target = repo.env.path("out");
    let output = repo.restore(&id, &target, &[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("corrupt"), "{}", stderr(&output));
    assert!(!target.exists());
}

#[test]
fn a_wrong_passphrase_restores_nothing() {
    let repo = Repo::new();
    let src = repo.env.path("src");
    sample(&src);
    let id = repo.backup(&src);
    let wrong = repo.env.write_password_file("wrong", b"not the passphrase\n");

    let target = repo.env.path("out");
    let args = ["restore", id.as_str(), "--target", target.to_str().expect("UTF-8 path")];
    let output = repo.env.run_with_password_file(&wrong, &args);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("incorrect passphrase"), "{}", stderr(&output));
    assert!(!target.exists());
}

// ---------------------------------------------------------------------------------------------
// Targets and arguments
// ---------------------------------------------------------------------------------------------

#[test]
fn a_target_inside_the_repository_is_refused() {
    let repo = Repo::new();
    let src = repo.env.path("src");
    sample(&src);
    let id = repo.backup(&src);
    let inside = repo.env.repo_path().join("restored-here");

    let output = repo.restore(&id, &inside, &[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("inside the repository"), "{}", stderr(&output));
    assert!(!inside.exists());
}

#[test]
fn a_target_that_is_a_file_is_refused_and_left_alone() {
    let repo = Repo::new();
    let src = repo.env.path("src");
    sample(&src);
    let id = repo.backup(&src);
    let file = repo.env.path("a-file");
    fs::write(&file, b"precious").expect("write");

    let output = repo.restore(&id, &file, &[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("not a directory"), "{}", stderr(&output));
    assert_eq!(fs::read(&file).expect("read"), b"precious");
}

#[test]
fn unknown_snapshots_and_bad_arguments_are_reported() {
    let repo = Repo::new();
    let src = repo.env.path("src");
    sample(&src);
    let id = repo.backup(&src);
    let target = repo.env.path("out");

    let other = if id.starts_with('0') { "1111" } else { "0000" };
    let output = repo.restore(other, &target, &[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("does not exist"), "{}", stderr(&output));

    for bad in [
        vec!["--on-conflict", "merge"],
        vec!["--symlinks", "follow"],
        vec!["--on-conflict"],
    ] {
        let output = repo.restore(&id, &target, &bad);
        assert_eq!(output.status.code(), Some(2), "{bad:?}");
        assert!(stdout(&output).is_empty());
    }
    for args in [vec!["restore", "abc", "--target", "out"], vec!["restore", id.as_str()]] {
        assert_eq!(repo.run(&args).status.code(), Some(2), "{args:?}");
    }
    assert!(!target.exists());
}

#[test]
fn a_missing_repository_is_reported_and_nothing_is_created() {
    let env = TestEnv::default();
    let target = env.path("out");
    let args = ["restore", "abcd", "--target", target.to_str().expect("UTF-8 path")];
    let output = env.run_with_password_file(&env.password_file(), &args);
    assert_eq!(output.status.code(), Some(1));
    assert!(!target.exists());
    assert!(!env.repo_path().exists());
}

// ---------------------------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn names_in_error_output_cannot_inject_terminal_control_sequences() {
    let repo = Repo::new();
    let src = repo.env.path("src");
    write(&src, "fine.txt", b"fine");
    repo.backup(&src);
    let known = repo.objects();
    write(&src, "evil\x1b[2J\x07name\nforged line", &pseudo_random(3000, 11));
    let id = repo.backup(&src);
    for object in repo.objects().difference(&known) {
        let mut bytes = fs::read(object).expect("read object");
        bytes[0] ^= 0xff;
        fs::write(object, bytes).expect("write object");
    }

    let output = repo.restore(&id, &repo.env.path("out"), &[]);
    assert_eq!(output.status.code(), Some(3));
    let err = stderr(&output);
    for forbidden in ['\x1b', '\x07'] {
        assert!(!err.contains(forbidden), "{forbidden:?} reached standard error:\n{err:?}");
    }
    assert!(err.contains(r"evil\x1b[2J\x07name\nforged line"), "{err}");
    assert!(!err.lines().any(|line| line.starts_with("forged line")), "a name started a new line");
}
