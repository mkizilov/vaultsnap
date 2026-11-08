mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use common::{run_without_repo, stderr, stdout, TestEnv, PASSPHRASE};
use vaultsnap_core::{Manifest, Passphrase, Repository, SnapshotId};

/// An initialized repository plus the password file that opens it.
fn initialized() -> (TestEnv, PathBuf) {
    let env = TestEnv::default();
    let password_file = env.password_file();
    let output = env.run_with_password_file(&password_file, &["init"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    (env, password_file)
}

/// A small tree: two files, one in a subdirectory, and a log file to exclude.
fn sample_source(env: &TestEnv) -> PathBuf {
    let root = env.path("source");
    fs::create_dir_all(root.join("docs")).expect("create source");
    fs::write(root.join("a.txt"), b"alpha").expect("write a.txt");
    fs::write(root.join("docs/b.txt"), vec![7_u8; 100_000]).expect("write b.txt");
    fs::write(root.join("debug.log"), b"noise").expect("write debug.log");
    root
}

fn backup(env: &TestEnv, password_file: &Path, args: &[&str], source: &Path) -> Output {
    let source = source.to_str().expect("UTF-8 path");
    let mut full = vec!["backup"];
    full.extend_from_slice(args);
    full.push(source);
    env.run_with_password_file(password_file, &full)
}

fn created_snapshot(output: &Output) -> SnapshotId {
    let text = stdout(output);
    let id = text
        .lines()
        .find_map(|line| line.strip_prefix("created snapshot "))
        .unwrap_or_else(|| panic!("no `created snapshot` line in:\n{text}"));
    id.parse().expect("a valid snapshot id")
}

fn files_below(dir: &Path) -> usize {
    fs::read_dir(dir)
        .expect("read directory")
        .map(|entry| {
            let path = entry.expect("directory entry").path();
            if path.is_dir() {
                files_below(&path)
            } else {
                1
            }
        })
        .sum()
}

fn snapshot_files(env: &TestEnv) -> usize {
    files_below(&env.repo_path().join("snapshots"))
}

fn chunk_files(env: &TestEnv) -> usize {
    files_below(&env.repo_path().join("data"))
}

async fn load(env: &TestEnv, id: &SnapshotId) -> Manifest {
    let repo = Repository::at(env.repo_path());
    let passphrase = Passphrase::new(PASSPHRASE.as_bytes().to_vec()).expect("valid passphrase");
    let keys = repo.unlock(&passphrase).await.expect("unlock");
    let store = repo.snapshot_store(&keys).await.expect("snapshot store");
    store.load(id).expect("the snapshot loads and validates")
}

fn names(manifest: &Manifest) -> Vec<String> {
    manifest
        .roots
        .iter()
        .flat_map(|root| root.entries.iter())
        .map(|entry| String::from_utf8_lossy(&entry.name).into_owned())
        .collect()
}

#[test]
fn backup_help_lists_its_options() {
    let output = run_without_repo(&["backup", "--help"]);
    assert!(output.status.success());
    let text = stdout(&output);
    for option in ["--exclude", "--tag", "--host", "--jobs", "--rescan"] {
        assert!(text.contains(option), "missing {option}:\n{text}");
    }
}

#[tokio::test]
async fn backup_creates_a_snapshot_that_loads_and_reports_what_it_did() {
    let (env, password_file) = initialized();
    let source = sample_source(&env);

    let output = backup(&env, &password_file, &[], &source);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let id = created_snapshot(&output);
    let text = stdout(&output);
    assert!(text.contains("no earlier snapshot"), "{text}");
    assert!(text.contains("3 total, 3 read, 0 unchanged"), "{text}");
    assert!(text.contains("chunks"), "{text}");
    assert!(!stderr(&output).contains('\x1b'), "no terminal control codes when piped");
    assert_eq!(snapshot_files(&env), 1);

    let manifest = load(&env, &id).await;
    let names = names(&manifest);
    for expected in ["a.txt", "docs", "b.txt", "debug.log"] {
        assert!(names.iter().any(|name| name == expected), "missing {expected}: {names:?}");
    }
    assert_eq!(manifest.stats.files, 3);
    assert_eq!(manifest.roots.len(), 1);
}

#[tokio::test]
async fn exclusions_tags_and_host_are_recorded_in_the_snapshot() {
    let (env, password_file) = initialized();
    let source = sample_source(&env);

    let output = backup(
        &env,
        &password_file,
        &["--exclude", "*.log", "--tag", "nightly", "--tag", "weekly", "--host", "testbox"],
        &source,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let manifest = load(&env, &created_snapshot(&output)).await;

    assert_eq!(manifest.tags, ["nightly", "weekly"]);
    assert_eq!(manifest.host, "testbox");
    let names = names(&manifest);
    assert!(names.iter().any(|name| name == "a.txt"), "{names:?}");
    assert!(!names.iter().any(|name| name == "debug.log"), "{names:?}");
}

#[test]
fn repeating_a_backup_adds_a_snapshot_but_no_chunks_and_changes_add_only_new_ones() {
    let (env, password_file) = initialized();
    let source = sample_source(&env);

    let first = backup(&env, &password_file, &[], &source);
    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));
    let chunks_after_first = chunk_files(&env);
    assert!(chunks_after_first > 0);

    let second = backup(&env, &password_file, &[], &source);
    assert_eq!(second.status.code(), Some(0), "{}", stderr(&second));
    let text = stdout(&second);
    assert!(text.contains("compared with"), "the earlier snapshot is the parent:\n{text}");
    assert!(text.contains(" 0 new "), "nothing new to store:\n{text}");
    assert_ne!(created_snapshot(&first), created_snapshot(&second));
    assert_eq!(snapshot_files(&env), 2);
    assert_eq!(chunk_files(&env), chunks_after_first);

    fs::write(source.join("a.txt"), b"changed contents that were never stored").expect("modify");
    let third = backup(&env, &password_file, &[], &source);
    assert_eq!(third.status.code(), Some(0), "{}", stderr(&third));
    assert_eq!(snapshot_files(&env), 3);
    assert_eq!(chunk_files(&env), chunks_after_first + 1, "only the changed file's chunk is new");
}

#[test]
fn a_wrong_passphrase_stores_nothing() {
    let (env, _) = initialized();
    let source = sample_source(&env);
    let wrong = env.write_password_file("wrong", b"not the passphrase\n");

    let output = backup(&env, &wrong, &[], &source);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("incorrect passphrase"), "{}", stderr(&output));
    assert_eq!(snapshot_files(&env), 0);
    assert_eq!(chunk_files(&env), 0);
}

#[test]
fn backup_without_a_passphrase_source_fails_when_there_is_no_terminal() {
    let (env, _) = initialized();
    let source = sample_source(&env);

    let output = env.run(&["backup", source.to_str().expect("UTF-8 path")]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("no terminal available"), "{}", stderr(&output));
    assert_eq!(snapshot_files(&env), 0);
}

#[test]
fn backup_into_something_that_is_not_a_repository_fails_without_creating_it() {
    let env = TestEnv::default();
    let password_file = env.password_file();
    let source = sample_source(&env);

    let output = backup(&env, &password_file, &[], &source);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("is not a VaultSnap repository"), "{}", stderr(&output));
    assert!(!env.repo_path().exists());
}

#[test]
fn bad_sources_fail_before_anything_is_stored() {
    let (env, password_file) = initialized();
    let source = sample_source(&env);

    let missing = backup(&env, &password_file, &[], &env.path("does-not-exist"));
    assert_eq!(missing.status.code(), Some(1), "{}", stderr(&missing));

    let inside = backup(&env, &password_file, &[], &env.repo_path().join("data"));
    assert_eq!(inside.status.code(), Some(1));
    assert!(stderr(&inside).contains("inside the repository"), "{}", stderr(&inside));

    let bad_pattern = backup(&env, &password_file, &["--exclude", "a["], &source);
    assert_eq!(bad_pattern.status.code(), Some(1), "{}", stderr(&bad_pattern));

    assert_eq!(snapshot_files(&env), 0);
    assert_eq!(chunk_files(&env), 0);
}

#[tokio::test]
async fn a_directory_that_contains_the_repository_is_backed_up_without_it() {
    let (env, password_file) = initialized();
    sample_source(&env);

    let output = backup(&env, &password_file, &[], &env.path(""));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let manifest = load(&env, &created_snapshot(&output)).await;
    let names = names(&manifest);
    assert!(names.iter().any(|name| name == "a.txt"), "{names:?}");
    for repository_part in ["config", "keys", "locks", "data"] {
        assert!(!names.iter().any(|name| name == repository_part), "{names:?}");
    }
}

#[cfg(unix)]
#[test]
fn unreadable_files_leave_a_snapshot_but_exit_with_code_3_and_name_the_file() {
    use std::os::unix::fs::PermissionsExt;

    struct Restore(PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            fs::set_permissions(&self.0, fs::Permissions::from_mode(0o600)).ok();
        }
    }

    let (env, password_file) = initialized();
    let source = sample_source(&env);
    let secret = source.join("secret.txt");
    fs::write(&secret, b"hidden").expect("write secret");
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o000)).expect("restrict");
    let _restore = Restore(secret.clone());
    if fs::File::open(&secret).is_ok() {
        eprintln!("skipped: file permissions are not enforced here (running as root?)");
        return;
    }

    let output = backup(&env, &password_file, &[], &source);
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert!(stdout(&output).contains("created snapshot "), "{}", stdout(&output));
    let errors = stderr(&output);
    assert!(errors.contains("secret.txt"), "{errors}");
    assert!(errors.contains("incomplete"), "{errors}");
    assert_eq!(snapshot_files(&env), 1, "the partial snapshot exists and is marked as partial");
}
