mod common;

use common::{run_without_repo, stderr, stdout, TestEnv};

const COMMANDS: [&str; 6] = ["init", "backup", "snapshots", "restore", "check", "prune"];

#[test]
fn help_lists_every_command() {
    let output = run_without_repo(&["--help"]);
    assert!(output.status.success());
    let text = stdout(&output);
    for command in COMMANDS {
        assert!(
            text.lines().any(|line| line.trim_start().starts_with(command)),
            "help is missing `{command}`:\n{text}"
        );
    }
}

#[test]
fn version_is_reported() {
    let output = run_without_repo(&["--version"]);
    assert!(output.status.success());
    assert!(stdout(&output).starts_with("vaultsnap "));
}

#[test]
fn missing_repository_is_a_clear_error() {
    let output = run_without_repo(&["snapshots"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("no repository specified"));
}

#[test]
fn malformed_snapshot_id_is_a_usage_error() {
    let env = TestEnv::default();
    let output = env.run(&["restore", "latest", "--target", "out"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("invalid snapshot id"));
}

#[test]
fn prune_without_keep_rules_is_rejected() {
    let env = TestEnv::default();
    let output = env.run(&["prune"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("retention"));
}

#[test]
fn unfinished_commands_report_not_implemented_without_touching_the_repository() {
    let env = TestEnv::default();
    let id = "a".repeat(64);
    let restore = ["restore", id.as_str(), "--target", "out"];
    let invocations: [&[&str]; 5] = [
        &["backup", "."],
        &["snapshots"],
        &restore,
        &["check"],
        &["prune", "--keep-last", "1"],
    ];
    for args in invocations {
        let output = env.run(args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert!(stderr(&output).contains("not implemented"), "{args:?}");
    }
    assert!(!env.repo_path().exists());
}

#[test]
fn init_creates_a_repository() {
    let env = TestEnv::default();
    let output = env.run(&["init"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stdout(&output).contains("created VaultSnap repository"));
    let repo = env.repo_path();
    for entry in ["config", "data", "snapshots", "locks"] {
        assert!(repo.join(entry).exists(), "missing {entry}");
    }
    let config = std::fs::read_to_string(repo.join("config")).expect("read config");
    assert!(config.starts_with("vaultsnap-repository\n"));
    assert!(config.contains("format_version=1\n"));
}

#[test]
fn repeated_init_fails_and_keeps_the_repository() {
    let env = TestEnv::default();
    assert!(env.run(&["init"]).status.success());
    let config_path = env.repo_path().join("config");
    let before = std::fs::read(&config_path).expect("read config");

    let output = env.run(&["init"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("already exists"), "{}", stderr(&output));
    assert_eq!(std::fs::read(&config_path).expect("read config"), before);
}

#[test]
fn init_refuses_a_file_and_an_unrelated_directory() {
    let env = TestEnv::default();
    let repo = env.repo_path();
    std::fs::write(&repo, b"a file").expect("write file");
    let output = env.run(&["init"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("not a directory"), "{}", stderr(&output));
    assert_eq!(std::fs::read(&repo).expect("read file"), b"a file");

    std::fs::remove_file(&repo).expect("remove file");
    std::fs::create_dir(&repo).expect("mkdir");
    std::fs::write(repo.join("notes.txt"), b"mine").expect("write file");
    let output = env.run(&["init"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("notes.txt"), "{}", stderr(&output));
    assert!(!repo.join("config").exists());
}

#[test]
fn init_reports_incompatible_versions_without_changing_them() {
    let env = TestEnv::default();
    assert!(env.run(&["init"]).status.success());
    let config_path = env.repo_path().join("config");
    let text = std::fs::read_to_string(&config_path).expect("read config");
    let newer = text.replace("format_version=1", "format_version=2");
    std::fs::write(&config_path, &newer).expect("write config");

    let output = env.run(&["init"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("version 2 is not supported"), "{}", stderr(&output));
    assert_eq!(std::fs::read_to_string(&config_path).expect("read config"), newer);
}

#[test]
fn verbose_flag_enables_debug_diagnostics() {
    let env = TestEnv::default();
    let output = env.run(&["-vv", "snapshots"]);
    assert!(stderr(&output).contains("using repository"));
}
