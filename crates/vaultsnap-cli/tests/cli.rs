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
fn every_command_reports_not_implemented_without_touching_the_repository() {
    let env = TestEnv::default();
    let id = "a".repeat(64);
    let restore = ["restore", id.as_str(), "--target", "out"];
    let invocations: [&[&str]; 6] = [
        &["init"],
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
fn verbose_flag_enables_debug_diagnostics() {
    let env = TestEnv::default();
    let output = env.run(&["-vv", "snapshots"]);
    assert!(stderr(&output).contains("using repository"));
}
