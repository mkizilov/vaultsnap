//! Integration tests of `snapshots`, `show`, `ls`, and `diff`, run against the compiled binary on
//! repositories made by real `backup` runs.
//!
//! Exact text and JSON for fixed data is covered by the unit tests next to the renderers; these
//! tests check the commands as a user meets them: ordering, references, filtering, changes between
//! real backups, unusual file names, failures, and that nothing is ever modified.

mod common;

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

    fn run(&self, args: &[&str]) -> Output {
        self.env.run_with_password_file(&self.password_file, args)
    }

    /// Runs a command that must succeed and returns its standard output.
    fn text(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert_eq!(output.status.code(), Some(0), "{args:?}: {}", stderr(&output));
        stdout(&output)
    }

    /// Runs a command with `--json` that must succeed and parses its one document.
    fn json(&self, args: &[&str]) -> Value {
        let mut full = args.to_vec();
        full.push("--json");
        let text = self.text(&full);
        assert!(text.ends_with('\n') && text.matches('\n').count() == 1, "one line: {text:?}");
        serde_json::from_str(&text).unwrap_or_else(|err| panic!("{err}: {text}"))
    }

    /// Backs up `source` and returns the new snapshot's id.
    fn backup(&self, source: &Path) -> String {
        let text = self.text(&["backup", source.to_str().expect("UTF-8 path")]);
        text.lines()
            .find_map(|line| line.strip_prefix("created snapshot "))
            .unwrap_or_else(|| panic!("no snapshot id in:\n{text}"))
            .to_owned()
    }

    fn snapshot_file(&self, id: &str) -> PathBuf {
        self.env.repo_path().join("snapshots").join(id)
    }

    fn corrupt(&self, id: &str) {
        let path = self.snapshot_file(id);
        let mut bytes = fs::read(&path).expect("read snapshot file");
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0x01;
        fs::write(&path, bytes).expect("write snapshot file");
    }
}

fn write(root: &Path, rel: &str, contents: &[u8]) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parents");
    }
    fs::write(path, contents).expect("write file");
}

/// The source as the snapshot records it (symbolic links in the temporary path resolved).
fn recorded(path: &Path) -> String {
    fs::canonicalize(path).expect("exists").to_str().expect("UTF-8 path").to_owned()
}

fn array<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value[key].as_array().unwrap_or_else(|| panic!("{key} is not an array in {value}"))
}

// ---------------------------------------------------------------------------------------------
// snapshots
// ---------------------------------------------------------------------------------------------

#[test]
fn an_empty_repository_has_no_snapshots_in_either_format() {
    let repo = Repo::new();
    assert_eq!(repo.text(&["snapshots"]), "No snapshots.\n");
    assert_eq!(
        repo.text(&["snapshots", "--json"]),
        concat!(
            r#"{"snapshots":[],"damaged":[],"referenced_chunks":0,"#,
            r#""referenced_stored_bytes":0,"unique_sizes_exact":true}"#,
            "\n"
        )
    );
}

#[test]
fn snapshots_are_listed_oldest_first_with_their_sizes() {
    let repo = Repo::new();
    let src = repo.env.path("src");
    write(&src, "a.txt", b"hello");
    write(&src, "b.bin", &[7_u8; 10_000]);
    let first = repo.backup(&src);
    write(&src, "c.txt", b"a third file");
    let second = repo.backup(&src);

    let list = repo.json(&["snapshots"]);
    let snapshots = array(&list, "snapshots");
    let ids: Vec<&str> = snapshots.iter().map(|s| s["id"].as_str().expect("id")).collect();
    assert_eq!(ids, [first.as_str(), second.as_str()]);
    assert!(snapshots[0]["created"].as_str() < snapshots[1]["created"].as_str());
    assert_eq!(snapshots[0]["short_id"], &first[..8]);
    assert_eq!(snapshots[0]["sources"][0]["path"], recorded(&src).as_str());
    assert_eq!(snapshots[0]["complete"], true);

    let (older, newer) = (&snapshots[0]["sizes"], &snapshots[1]["sizes"]);
    assert_eq!(older["logical_bytes"], 10_005);
    assert_eq!(newer["logical_bytes"], 10_017);
    assert_eq!(snapshots[1]["files"], 3);
    let stored = older["stored_bytes"].as_u64().expect("number");
    assert!(stored > 0 && stored < 10_005, "repeated bytes compress: {stored}");
    assert_eq!(older["unique_stored_bytes"], 0, "everything in the first is also in the second");
    assert!(newer["unique_stored_bytes"].as_u64().expect("number") > 0);
    assert_eq!(list["unique_sizes_exact"], true);
    assert_eq!(list["damaged"].as_array().map(Vec::len), Some(0));

    let text = repo.text(&["snapshots"]);
    let (at_first, at_second) = (text.find(&first[..8]), text.find(&second[..8]));
    assert!(at_first.is_some() && at_first < at_second, "oldest first:\n{text}");
    assert!(text.starts_with("ID "), "{text}");
    assert!(text.contains("2 snapshots; they refer to "), "{text}");
}

// ---------------------------------------------------------------------------------------------
// show
// ---------------------------------------------------------------------------------------------

#[test]
fn show_describes_a_snapshot_chosen_by_the_start_of_its_id() {
    let repo = Repo::new();
    let src = repo.env.path("src");
    write(&src, "a.txt", b"hello");
    write(&src, "dir/b.txt", b"world!");
    let id = repo.backup(&src);

    for length in [8, 12, 64] {
        let shown = repo.json(&["show", &id[..length]]);
        assert_eq!(shown["id"], id.as_str(), "{length} characters");
    }
    let shown = repo.json(&["show", &id[..8]]);
    assert_eq!(shown["complete"], true);
    assert_eq!(shown["totals"]["files"], 2);
    assert_eq!(shown["totals"]["directories"], 2);
    assert_eq!(shown["totals"]["logical_bytes"], 11);
    assert_eq!(shown["sizes"]["logical_bytes"], 11);
    assert_eq!(shown["sources"][0]["path"], recorded(&src).as_str());
    assert_eq!(shown["config"]["symlinks"], "preserve");
    assert_eq!(shown["skipped"].as_array().map(Vec::len), Some(0));

    let text = repo.text(&["show", &id[..8]]);
    assert!(text.contains(&format!("Snapshot        {id}\n")), "{text}");
    assert!(text.contains("Status          complete\n"), "{text}");
    assert!(text.contains("  logical         11 B"), "{text}");
    assert!(text.contains(&recorded(&src)), "{text}");
}

// ---------------------------------------------------------------------------------------------
// ls
// ---------------------------------------------------------------------------------------------

#[test]
fn ls_lists_the_files_and_can_be_limited_to_a_path() {
    let repo = Repo::new();
    let src = repo.env.path("src");
    write(&src, "a.txt", b"hello");
    write(&src, "dir/b.txt", b"world!");
    write(&src, "dir/deeper/c.txt", b"!");
    let id = repo.backup(&src);
    let root = recorded(&src);

    let text = repo.text(&["ls", &id[..8]]);
    let listed: Vec<&str> = text.lines().map(|line| line.split("  ").last().expect("a path")).collect();
    assert_eq!(
        listed,
        [
            format!("{root}/"),
            format!("{root}/a.txt"),
            format!("{root}/dir/"),
            format!("{root}/dir/b.txt"),
            format!("{root}/dir/deeper/"),
            format!("{root}/dir/deeper/c.txt"),
        ]
    );
    assert!(text.lines().next().expect("a line").starts_with('d'), "directories start with d");
    assert!(text.contains("  6  "), "the size of b.txt is shown:\n{text}");

    let below = repo.text(&["ls", &id[..8], &format!("{root}/dir")]);
    assert_eq!(below.lines().count(), 4);
    let one = repo.text(&["ls", &id[..8], &format!("{root}/a.txt/")]);
    assert_eq!(one.lines().count(), 1, "a trailing slash does not matter");

    let entries = repo.json(&["ls", &id[..8], &format!("{root}/dir/deeper")]);
    assert_eq!(entries["snapshot"], id.as_str());
    let entries = array(&entries, "entries");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[1]["path"], format!("{root}/dir/deeper/c.txt").as_str());
    assert_eq!(entries[1]["type"], "file");
    assert_eq!(entries[1]["size"], 1);
    assert_eq!(entries[1]["chunks"], 1);
}

#[test]
fn ls_of_a_path_that_is_not_in_the_snapshot_fails() {
    let repo = Repo::new();
    let src = repo.env.path("src");
    write(&src, "a.txt", b"hello");
    let id = repo.backup(&src);
    let root = recorded(&src);

    for path in [format!("{root}/missing"), format!("{root}/a"), "/nowhere".to_owned()] {
        let output = repo.run(&["ls", &id[..8], &path]);
        assert_eq!(output.status.code(), Some(1), "{path}");
        assert!(stdout(&output).is_empty());
        assert!(stderr(&output).contains("has no entry at or below"), "{}", stderr(&output));
    }
}

// ---------------------------------------------------------------------------------------------
// diff
// ---------------------------------------------------------------------------------------------

#[test]
fn diff_reports_what_changed_between_two_real_backups() {
    let repo = Repo::new();
    let src = repo.env.path("src");
    write(&src, "keep.txt", b"unchanged");
    write(&src, "edit.txt", b"before");
    write(&src, "remove.txt", b"going away");
    write(&src, "dir/inner.txt", b"stays");
    let first = repo.backup(&src);

    write(&src, "edit.txt", b"after, and longer");
    fs::remove_file(src.join("remove.txt")).expect("remove");
    write(&src, "added.txt", b"new!");
    let second = repo.backup(&src);
    let root = recorded(&src);

    let document = repo.json(&["diff", &first[..8], &second[..8]]);
    assert_eq!(document["from"]["id"], first.as_str());
    assert_eq!(document["to"]["id"], second.as_str());
    let summary = &document["summary"];
    assert_eq!(summary["added"], 1);
    assert_eq!(summary["deleted"], 1);
    assert_eq!(summary["modified"], 1);
    assert_eq!(summary["unchanged"], 4, "the root, keep.txt, dir, and dir/inner.txt");
    assert_eq!(summary["added_bytes"], 4);
    assert_eq!(summary["deleted_bytes"], 10);
    assert_eq!(summary["logical_bytes_from"], 9 + 6 + 10 + 5);
    assert_eq!(summary["logical_bytes_to"], 9 + 17 + 5 + 4);
    assert!(summary["stored_bytes_only_in_to"].as_u64().expect("number") > 0);
    assert_eq!(summary["chunker_differs"], false);

    let changes: Vec<(String, String)> = array(&document, "changes")
        .iter()
        .map(|c| {
            let path = c["path"].as_str().expect("path").strip_prefix(&root).expect("below the root");
            (c["status"].as_str().expect("status").to_owned(), path.to_owned())
        })
        .collect();
    assert_eq!(
        changes,
        [
            ("added".to_owned(), "/added.txt".to_owned()),
            ("modified".to_owned(), "/edit.txt".to_owned()),
            ("deleted".to_owned(), "/remove.txt".to_owned()),
        ]
    );
    let edited = array(&document, "changes")[1]["differences"].as_array().expect("array");
    assert!(edited.contains(&Value::from("content")), "{edited:?}");

    let text = repo.text(&["diff", &first[..8], &second[..8]]);
    assert!(text.contains(&format!("+ {root}/added.txt\n")), "{text}");
    assert!(text.contains(&format!("- {root}/remove.txt\n")), "{text}");
    assert!(text.contains(&format!("M {root}/edit.txt  (content")), "{text}");
    assert!(text.contains("1 added, 1 deleted, 1 modified, 4 unchanged\n"), "{text}");
    assert!(!text.contains("keep.txt"), "unchanged files are not listed");
}

#[test]
fn diff_of_a_snapshot_with_itself_finds_nothing_and_order_decides_direction() {
    let repo = Repo::new();
    let src = repo.env.path("src");
    write(&src, "a.txt", b"one");
    let first = repo.backup(&src);
    write(&src, "b.txt", b"two");
    let second = repo.backup(&src);

    let same = repo.text(&["diff", &first[..8], &first[..8]]);
    assert!(same.contains("No differences.\n"), "{same}");

    let forward = repo.json(&["diff", &first[..8], &second[..8]]);
    let backward = repo.json(&["diff", &second[..8], &first[..8]]);
    assert_eq!(forward["summary"]["added"], 1);
    assert_eq!(forward["summary"]["deleted"], 0);
    assert_eq!(backward["summary"]["added"], 0);
    assert_eq!(backward["summary"]["deleted"], 1);
}

// ---------------------------------------------------------------------------------------------
// Unusual names
// ---------------------------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn hostile_file_names_are_escaped_in_text_and_exact_in_json() {
    let repo = Repo::new();
    let src = repo.env.path("src");
    let names = ["new\nline.txt", "esc\x1b[2Jclear", "tab\there", "back\\slash", "cr\rname"];
    for name in names {
        write(&src, name, b"x");
    }
    let first = repo.backup(&src);
    for name in names {
        write(&src, name, b"changed");
    }
    let second = repo.backup(&src);
    let root = recorded(&src);

    for command in [vec!["ls", &first[..8]], vec!["diff", &first[..8], &second[..8]]] {
        let text = repo.text(&command);
        for forbidden in ['\x1b', '\r'] {
            assert!(!text.contains(forbidden), "{command:?} printed {forbidden:?}:\n{text:?}");
        }
    }
    let ls = repo.text(&["ls", &first[..8]]);
    assert_eq!(ls.lines().count(), names.len() + 1, "one line per entry, whatever the names hold");
    assert!(ls.contains(r"new\nline.txt") && ls.contains(r"esc\x1b[2Jclear"), "{ls}");
    assert!(ls.contains(r"tab\there") && ls.contains(r"back\\slash") && ls.contains(r"cr\rname"));

    let entries = repo.json(&["ls", &first[..8]]);
    let paths: Vec<&str> =
        array(&entries, "entries").iter().map(|e| e["path"].as_str().expect("path")).collect();
    for name in names {
        assert!(paths.contains(&format!("{root}/{name}").as_str()), "{name:?} in {paths:?}");
    }

    let diff = repo.json(&["diff", &first[..8], &second[..8]]);
    assert_eq!(array(&diff, "changes").len(), names.len());
    assert!(array(&diff, "changes").iter().all(|c| c["status"] == "modified"));
}

// ---------------------------------------------------------------------------------------------
// Failures and authentication
// ---------------------------------------------------------------------------------------------

#[test]
fn a_wrong_passphrase_shows_nothing() {
    let repo = Repo::new();
    let src = repo.env.path("src");
    write(&src, "a.txt", b"secret plans");
    let id = repo.backup(&src);
    let wrong = repo.env.write_password_file("wrong", b"not the passphrase\n");

    for args in [vec!["snapshots"], vec!["show", &id[..8]], vec!["ls", &id[..8]]] {
        let output = repo.env.run_with_password_file(&wrong, &args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert!(stdout(&output).is_empty(), "{args:?}");
        assert!(stderr(&output).contains("incorrect passphrase"), "{args:?}");
    }
}

#[test]
fn without_a_passphrase_source_the_commands_fail_before_reading_anything() {
    let repo = Repo::new();
    let output = repo.env.run(&["snapshots"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("no terminal available"), "{}", stderr(&output));
    assert!(stdout(&output).is_empty());
}

#[test]
fn bad_references_are_usage_errors_and_unknown_ones_are_reported() {
    let repo = Repo::new();
    let src = repo.env.path("src");
    write(&src, "a.txt", b"one");
    let id = repo.backup(&src);

    for args in [vec!["show", "abc"], vec!["show", "latest"], vec!["diff", &id[..8]], vec!["ls", "XYZ1"]] {
        let output = repo.run(&args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(stdout(&output).is_empty());
    }
    let other = if id.starts_with('0') { "1111" } else { "0000" };
    for args in [vec!["show", other], vec!["ls", other], vec!["diff", &id[..8], other]] {
        let output = repo.run(&args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert!(stderr(&output).contains("does not exist"), "{}", stderr(&output));
    }
}

#[test]
fn a_tampered_snapshot_is_reported_and_never_shown() {
    let repo = Repo::new();
    let src = repo.env.path("src");
    write(&src, "a.txt", b"one");
    let good = repo.backup(&src);
    write(&src, "a.txt", b"two");
    let bad = repo.backup(&src);
    repo.corrupt(&bad);

    let output = repo.run(&["snapshots"]);
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert!(stdout(&output).contains(&good[..8]));
    assert!(!stdout(&output).contains(&bad[..8]), "a damaged snapshot is not listed as if it were fine");
    assert!(stderr(&output).contains(&bad), "{}", stderr(&output));
    assert!(stderr(&output).contains("damaged and not shown"));

    let output = repo.run(&["snapshots", "--json"]);
    assert_eq!(output.status.code(), Some(3));
    let document: Value = serde_json::from_str(&stdout(&output)).expect("valid JSON");
    assert_eq!(array(&document, "snapshots").len(), 1);
    assert_eq!(array(&document, "damaged")[0]["id"], bad.as_str());
    assert_eq!(document["unique_sizes_exact"], false);

    for args in [vec!["show", &bad[..8]], vec!["ls", &bad[..8]], vec!["diff", &good[..8], &bad[..8]]] {
        let output = repo.run(&args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert!(stdout(&output).is_empty(), "{args:?}");
        assert!(stderr(&output).contains("corrupt"), "{}", stderr(&output));
    }
    let output = repo.run(&["show", &good[..8]]);
    assert_eq!(output.status.code(), Some(0), "the good snapshot still works");
    assert!(stdout(&output).contains("may be too large"), "and says its unique size is uncertain");
}

// ---------------------------------------------------------------------------------------------
// Stability and safety
// ---------------------------------------------------------------------------------------------

#[test]
fn output_is_identical_every_time_and_the_repository_is_never_changed() {
    let repo = Repo::new();
    let src = repo.env.path("src");
    write(&src, "a.txt", b"one");
    write(&src, "dir/b.txt", b"two");
    let first = repo.backup(&src);
    write(&src, "a.txt", b"three");
    let second = repo.backup(&src);

    let commands: Vec<Vec<&str>> = vec![
        vec!["snapshots"],
        vec!["snapshots", "--json"],
        vec!["show", &first[..8]],
        vec!["show", &second[..8], "--json"],
        vec!["ls", &second[..8]],
        vec!["ls", &second[..8], "--json"],
        vec!["diff", &first[..8], &second[..8]],
        vec!["diff", &first[..8], &second[..8], "--json"],
    ];
    let snapshot_bytes = |id: &str| fs::read(repo.snapshot_file(id)).expect("read");
    let before = (snapshot_bytes(&first), snapshot_bytes(&second));
    for command in &commands {
        let once = repo.text(command);
        let twice = repo.text(command);
        assert_eq!(once, twice, "{command:?} is not stable");
        assert!(!once.is_empty());
    }
    assert_eq!(before, (snapshot_bytes(&first), snapshot_bytes(&second)));
    let locks = fs::read_dir(repo.env.repo_path().join("locks")).expect("read locks").count();
    assert_eq!(locks, 0, "no lock is left behind");
}
