//! Integration tests of `vaultsnap check`, run against the compiled binary on repositories made by
//! real `backup` runs and then damaged on purpose. They check what a user sees: the exit status,
//! what is printed where, that the two modes find what they promise to find, that the JSON can be
//! parsed, and that nothing in the repository is ever changed.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use common::{stderr, stdout, TestEnv};
use serde_json::Value;

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

    /// A repository holding one backup of a few files; `big.bin` is a single chunk object.
    fn with_backup() -> (Self, String) {
        let repo = Self::new();
        let src = repo.env.path("src");
        fs::create_dir_all(&src).expect("mkdir");
        fs::write(src.join("small.txt"), b"hello check").expect("write");
        fs::write(src.join("big.bin"), pseudo_random(5000, 3)).expect("write");
        let id = repo.backup(&src);
        (repo, id)
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

    fn repo(&self, relative: &str) -> PathBuf {
        self.env.repo_path().join(relative)
    }

    fn objects(&self) -> BTreeSet<PathBuf> {
        let mut found = BTreeSet::new();
        let mut pending = vec![self.repo("data")];
        while let Some(dir) = pending.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries {
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

    /// The only chunk object of `big.bin`: the largest object in the repository.
    fn big_object(&self) -> PathBuf {
        self.objects()
            .into_iter()
            .max_by_key(|path| fs::metadata(path).expect("metadata").len())
            .expect("there are objects")
    }

    /// The JSON report of a check with extra arguments.
    fn json(&self, extra: &[&str]) -> (Option<i32>, Value) {
        let mut args = vec!["check", "--json"];
        args.extend_from_slice(extra);
        let output = self.run(&args);
        let text = stdout(&output);
        assert!(text.ends_with('\n') && text.matches('\n').count() == 1, "one line: {text:?}");
        let value = serde_json::from_str(&text).unwrap_or_else(|err| panic!("{err}: {text}"));
        (output.status.code(), value)
    }
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

/// Every file below `root` with its content, to prove a repository did not change.
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

fn flip_middle(path: &Path) {
    let mut bytes = fs::read(path).expect("read");
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0x01;
    fs::write(path, bytes).expect("write");
}

/// A line of the summary block: the label padded to the width the command uses.
fn field(label: &str, value: &str) -> String {
    format!("{label:<13}{value}")
}

fn kinds(report: &Value) -> Vec<&str> {
    report["findings"]
        .as_array()
        .expect("an array")
        .iter()
        .map(|finding| finding["kind"].as_str().expect("a kind"))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// A healthy repository
// ---------------------------------------------------------------------------------------------

#[test]
fn a_healthy_repository_passes_both_modes_quietly() {
    let (repo, _id) = Repo::with_backup();

    let output = repo.run(&["check"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains(&field("Mode", "quick: snapshots authenticated")), "{text}");
    assert!(text.contains(&field("Snapshots", "1 readable, 0 damaged")), "{text}");
    assert!(text.contains("Result: no problems found.\n"), "{text}");
    assert!(text.contains("Quick mode does not read chunk contents"), "{text}");
    assert!(stderr(&output).is_empty(), "nothing on standard error: {}", stderr(&output));

    let output = repo.run(&["check", "--read-data"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    let mode = field("Mode", "full: snapshots authenticated, every chunk object read");
    assert!(text.contains(&mode), "{text}");
    assert!(text.contains(&field("Verified", "")), "{text}");
    assert!(text.contains("Result: no problems found.\n"), "{text}");
    assert!(!text.contains("Quick mode does not read"), "{text}");

    let locks = fs::read_dir(repo.repo("locks")).expect("read locks").count();
    assert_eq!(locks, 0, "no lock is left behind");
}

#[test]
fn an_empty_repository_passes() {
    let repo = Repo::new();
    let text = repo.ok(&["check"]);
    assert!(text.contains(&field("Snapshots", "0 readable, 0 damaged")), "{text}");
    assert!(repo.ok(&["check", "--read-data"]).contains("Result: no problems found."));
}

// ---------------------------------------------------------------------------------------------
// The two modes
// ---------------------------------------------------------------------------------------------

#[test]
fn a_flipped_bit_in_a_chunk_passes_the_quick_check_and_fails_the_full_one() {
    let (repo, id) = Repo::with_backup();
    flip_middle(&repo.big_object());

    // The documented limit of the quick mode; the output says so.
    let quick = repo.run(&["check"]);
    assert_eq!(quick.status.code(), Some(0), "{}", stderr(&quick));
    assert!(stdout(&quick).contains("Quick mode does not read chunk contents"));

    let full = repo.run(&["check", "--read-data"]);
    assert_eq!(full.status.code(), Some(3), "{}", stderr(&full));
    let text = stdout(&full);
    assert!(text.contains("ERRORS (1)"), "{text}");
    assert!(text.contains("chunk_corrupt  data/"), "{text}");
    assert!(text.contains("authentication failed"), "{text}");
    assert!(text.contains("AFFECTED SNAPSHOTS"), "{text}");
    assert!(text.contains(&format!("{}  1 file affected (1 damaged or missing chunk)", &id[..8])), "{text}");
    assert!(text.contains("WHAT TO DO (the check changed nothing; see docs/CHECK.md)"), "{text}");
    assert!(text.contains("Result: 1 error found, 0 warnings. The repository was not changed."), "{text}");
    assert!(stderr(&full).contains("the check found 1 error(s)"), "{}", stderr(&full));
    assert!(stderr(&full).contains("nothing in the repository was changed"), "{}", stderr(&full));
}

#[test]
fn a_truncated_chunk_fails_both_modes() {
    let (repo, _id) = Repo::with_backup();
    let object = repo.big_object();
    let bytes = fs::read(&object).expect("read");
    fs::write(&object, &bytes[..bytes.len() - 9]).expect("write");

    for args in [vec!["check"], vec!["check", "--read-data"]] {
        let output = repo.run(&args);
        assert_eq!(output.status.code(), Some(3), "{args:?}");
        assert!(stdout(&output).contains("truncated or extended"), "{args:?}: {}", stdout(&output));
    }
}

#[test]
fn a_missing_chunk_fails_both_modes_and_names_the_snapshot() {
    let (repo, id) = Repo::with_backup();
    let object = repo.big_object();
    fs::remove_file(&object).expect("remove");

    for args in [vec!["check"], vec!["check", "--read-data"]] {
        let output = repo.run(&args);
        assert_eq!(output.status.code(), Some(3), "{args:?}");
        let text = stdout(&output);
        assert!(text.contains("chunk_missing  data/"), "{text}");
        assert!(text.contains("no object exists for a chunk that snapshots refer to"), "{text}");
        assert!(text.contains(&id[..8]), "{text}");
    }
    assert!(!object.exists(), "a missing object is not created again by the check");
}

// ---------------------------------------------------------------------------------------------
// Snapshots and structure
// ---------------------------------------------------------------------------------------------

#[test]
fn a_tampered_snapshot_is_reported_as_damaged() {
    let (repo, id) = Repo::with_backup();
    flip_middle(&repo.repo(&format!("snapshots/{id}")));

    let output = repo.run(&["check"]);
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains(&format!("snapshot_damaged  snapshots/{id}")), "{text}");
    assert!(text.contains(&field("Snapshots", "0 readable, 1 damaged")), "{text}");
    assert!(text.contains("Unreferenced") && text.contains("may include objects of snapshots"), "{text}");
}

#[test]
fn a_missing_directory_is_an_error_with_a_clear_description() {
    for name in ["snapshots", "data", "locks"] {
        let (repo, _id) = Repo::with_backup();
        fs::remove_dir_all(repo.repo(name)).expect("remove");
        let output = repo.run(&["check"]);
        assert_eq!(output.status.code(), Some(3), "{name}: {}", stderr(&output));
        let text = stdout(&output);
        assert!(text.contains(&format!("missing_directory  {name}")), "{name}: {text}");
        assert!(text.contains("the directory does not exist"), "{name}: {text}");
    }
}

#[test]
fn without_the_key_directory_nothing_can_be_unlocked_so_the_check_cannot_run() {
    let (repo, _id) = Repo::with_backup();
    fs::remove_dir_all(repo.repo("keys")).expect("remove");
    let output = repo.run(&["check"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).is_empty());
    assert!(stderr(&output).contains("keys"), "{}", stderr(&output));
}

// ---------------------------------------------------------------------------------------------
// Warnings
// ---------------------------------------------------------------------------------------------

#[test]
fn strays_leftovers_and_stale_locks_are_warnings_that_leave_the_exit_status_alone() {
    let (repo, _id) = Repo::with_backup();
    fs::write(repo.repo("notes.txt"), b"not ours").expect("write");
    fs::write(repo.repo("snapshots/.tmp-0011223344556677"), b"half").expect("write");
    fs::write(
        repo.repo("locks/0123456789abcdef0123456789abcdef.lock"),
        b"kind=shared\npid=424242\ncreated_at_unix=1700000000\n",
    )
    .expect("write");

    let output = repo.run(&["check", "--read-data"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("WARNINGS (3)"), "{text}");
    assert!(text.contains("unexpected_file  notes.txt"), "{text}");
    assert!(text.contains("leftover_temp_file  snapshots/.tmp-0011223344556677"), "{text}");
    assert!(text.contains("lock_present  locks/0123456789abcdef0123456789abcdef.lock"), "{text}");
    assert!(text.contains("a shared lock held by process 424242"), "{text}");
    assert!(text.contains("Result: no errors found, 3 warnings."), "{text}");
    assert!(text.contains("WHAT TO DO"), "{text}");
    assert!(repo.repo("notes.txt").exists() && repo.repo("snapshots/.tmp-0011223344556677").exists());
}

#[test]
fn objects_no_snapshot_refers_to_are_reported_as_orphans() {
    let (repo, _id) = Repo::with_backup();
    // The objects of a backup whose snapshot was removed, the usual way of getting orphans.
    let src = repo.env.path("more");
    fs::create_dir_all(&src).expect("mkdir");
    fs::write(src.join("extra.bin"), pseudo_random(5000, 9)).expect("write");
    let second = repo.backup(&src);
    fs::remove_file(repo.repo(&format!("snapshots/{second}"))).expect("remove");

    let (code, report) = repo.json(&[]);
    assert_eq!(code, Some(0));
    assert_eq!(report["ok"], true);
    assert_eq!(report["counts"]["orphan_object"], 1);
    assert_eq!(report["stats"]["orphans"], 1);
    assert_eq!(report["stats"]["orphans_exact"], true);
    assert_eq!(report["findings"][0]["severity"], "warning");
    assert_eq!(report["findings"][0]["detail"], "no snapshot refers to this object");
}

#[test]
fn the_limit_on_listed_findings_is_stated_and_the_counts_stay_exact() {
    let (repo, _id) = Repo::with_backup();
    for name in ["a.txt", "b.txt", "c.txt", "d.txt"] {
        fs::write(repo.repo(name), b"stray").expect("write");
    }

    let output = repo.run(&["check", "--max-findings", "1"]);
    assert_eq!(output.status.code(), Some(0));
    let text = stdout(&output);
    assert!(text.contains("WARNINGS (4)"), "{text}");
    assert!(text.contains("and 3 more not listed (--max-findings sets the limit, 0 lists all)"), "{text}");
    assert_eq!(text.matches("unexpected_file  ").count(), 1, "{text}");

    let (_, report) = repo.json(&["--max-findings", "0"]);
    assert_eq!(report["findings"].as_array().map(Vec::len), Some(4));
    assert_eq!(report["suppressed"], 0);
}

#[cfg(unix)]
#[test]
fn a_stray_file_cannot_inject_terminal_control_sequences() {
    let (repo, _id) = Repo::with_backup();
    let name = "evil\x1b[2J\x07name\nforged line";
    fs::write(repo.repo(name), b"x").expect("write");

    let output = repo.run(&["check"]);
    assert_eq!(output.status.code(), Some(0));
    let text = stdout(&output);
    for forbidden in ['\x1b', '\x07'] {
        assert!(!text.contains(forbidden), "{forbidden:?} reached standard output:\n{text:?}");
    }
    assert!(text.contains(r"evil\x1b[2J\x07name\nforged line"), "{text}");
    assert!(!text.lines().any(|line| line.starts_with("forged line")), "a name started a new line");

    let (_, report) = repo.json(&[]);
    assert_eq!(report["findings"][0]["subject"], name, "JSON keeps the name exact");
}

// ---------------------------------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------------------------------

#[test]
fn the_json_report_of_a_damaged_repository_is_complete_and_stable() {
    let (repo, id) = Repo::with_backup();
    let object = repo.big_object();
    let relative = object.strip_prefix(repo.repo("")).expect("inside").to_string_lossy().into_owned();
    fs::remove_file(&object).expect("remove");

    let (code, report) = repo.json(&["--read-data"]);
    assert_eq!(code, Some(3));
    assert_eq!(report["mode"], "full");
    assert_eq!(report["ok"], false);
    assert_eq!((&report["errors"], &report["warnings"], &report["suppressed"]), (&Value::from(1), &Value::from(0), &Value::from(0)));
    assert_eq!(report["repository_id"].as_str().map(str::len), Some(32));
    assert_eq!(report["format_version"], 1);
    assert_eq!(report["counts"]["chunk_missing"], 1);
    assert_eq!(report["stats"]["chunks_missing"], 1);
    assert_eq!(kinds(&report), ["chunk_missing"]);
    assert_eq!(report["findings"][0]["severity"], "error");
    assert_eq!(report["findings"][0]["subject"], relative.as_str());
    assert_eq!(report["affected_snapshots"][0]["id"], id.as_str());
    assert_eq!(report["affected_snapshots"][0]["short_id"], &id[..8]);
    assert_eq!(report["affected_snapshots"][0]["files"], 1);

    let (_, again) = repo.json(&["--read-data"]);
    assert_eq!(again, report, "the same repository gives the same report");
    let first = stdout(&repo.run(&["check", "--read-data", "--json"]));
    let second = stdout(&repo.run(&["check", "--read-data", "--json"]));
    assert_eq!(first, second, "and the same bytes");
}

#[test]
fn the_json_report_of_a_healthy_repository_says_ok() {
    let (repo, _id) = Repo::with_backup();
    let (code, report) = repo.json(&[]);
    assert_eq!(code, Some(0));
    assert_eq!(report["ok"], true);
    assert_eq!(report["mode"], "quick");
    assert_eq!(report["findings"].as_array().map(Vec::len), Some(0));
    assert_eq!(report["affected_snapshots"].as_array().map(Vec::len), Some(0));
    assert_eq!(report["stats"]["snapshots"], 1);
    assert_eq!(report["stats"]["bytes_authenticated"], 0);
}

// ---------------------------------------------------------------------------------------------
// The repository is never changed
// ---------------------------------------------------------------------------------------------

#[test]
fn a_check_changes_nothing_in_a_damaged_repository_in_either_mode() {
    let (repo, id) = Repo::with_backup();
    flip_middle(&repo.big_object());
    fs::write(repo.repo("notes.txt"), b"stray").expect("write");
    fs::write(repo.repo("data/.tmp-0011223344556677"), b"leftover").expect("write");
    flip_middle(&repo.repo(&format!("snapshots/{id}")));

    let before = files_below(&repo.repo(""));
    for args in [vec!["check"], vec!["check", "--read-data"], vec!["check", "--read-data", "--json"]] {
        let output = repo.run(&args);
        assert_eq!(output.status.code(), Some(3), "{args:?}");
    }
    assert_eq!(files_below(&repo.repo("")), before, "not a byte of the repository changed");
}

// ---------------------------------------------------------------------------------------------
// When the check cannot run
// ---------------------------------------------------------------------------------------------

#[test]
fn a_wrong_passphrase_prints_nothing_and_fails() {
    let (repo, _id) = Repo::with_backup();
    let wrong = repo.env.write_password_file("wrong", b"not the passphrase\n");
    let output = repo.env.run_with_password_file(&wrong, &["check"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).is_empty());
    assert!(stderr(&output).contains("incorrect passphrase"), "{}", stderr(&output));
}

#[test]
fn a_location_that_is_not_a_repository_fails_clearly() {
    let env = TestEnv::default();
    let output = env.run_with_password_file(&env.password_file(), &["check"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("is not a VaultSnap repository"), "{}", stderr(&output));
    assert!(!env.repo_path().exists(), "nothing was created");
}

#[test]
fn a_damaged_or_missing_config_stops_the_check_with_a_message_and_changes_nothing() {
    let (repo, _id) = Repo::with_backup();
    let config = repo.repo("config");
    let original = fs::read(&config).expect("read");

    fs::write(&config, b"vaultsnap-repository\nformat_version=1\n").expect("write");
    let output = repo.run(&["check"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).is_empty());
    assert!(stderr(&output).contains("invalid repository config"), "{}", stderr(&output));

    fs::write(&config, String::from_utf8_lossy(&original).replace("format_version=1", "format_version=2")).expect("write");
    let output = repo.run(&["check"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("format version 2 is not supported"), "{}", stderr(&output));

    fs::remove_file(&config).expect("remove");
    let output = repo.run(&["check"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("is not a VaultSnap repository"), "{}", stderr(&output));
}

#[test]
fn an_exclusive_lock_stops_the_check_and_stays() {
    let (repo, _id) = Repo::with_backup();
    let lock = repo.repo("locks/0123456789abcdef0123456789abcdef.lock");
    fs::write(&lock, b"kind=exclusive\npid=1\n").expect("write");

    let output = repo.run(&["check"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).is_empty());
    assert!(stderr(&output).contains("repository is locked by locks/0123456789abcdef0123456789abcdef.lock"));
    assert!(lock.exists());
}

#[test]
fn bad_arguments_are_usage_errors() {
    let (repo, _id) = Repo::with_backup();
    for args in [
        vec!["check", "--jobs", "many"],
        vec!["check", "--jobs", "-2"],
        vec!["check", "--max-findings", "-1"],
        vec!["check", "--surprise"],
    ] {
        let output = repo.run(&args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(stdout(&output).is_empty());
    }
}

#[test]
fn the_number_of_threads_does_not_change_the_report() {
    let (repo, _id) = Repo::with_backup();
    flip_middle(&repo.big_object());
    let outputs: Vec<String> = ["1", "2", "5"]
        .iter()
        .map(|jobs| {
            let output = repo.run(&["check", "--read-data", "--json", "--jobs", jobs]);
            assert_eq!(output.status.code(), Some(3));
            stdout(&output)
        })
        .collect();
    assert!(outputs.windows(2).all(|pair| pair[0] == pair[1]), "{outputs:?}");
}
