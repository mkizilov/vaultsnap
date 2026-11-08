//! Integration tests of `--repo s3://BUCKET[/FOLDER]` that need no network: whatever can be
//! refused from the command line alone is refused before any connection is attempted, with a
//! clear message and without creating anything on disk. Working with a real bucket is tested
//! against MinIO in `crates/vaultsnap-core/tests/s3_minio.rs` (see `docs/S3.md`); nothing in this
//! file talks to a server.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::{stderr, stdout, TestEnv};

/// Runs `vaultsnap --password-file <file> <args>` in an empty directory of its own and returns the
/// output together with that directory.
fn run(env: &TestEnv, args: &[&str]) -> (Output, PathBuf) {
    let cwd = env.path("cwd");
    fs::create_dir_all(&cwd).expect("create the working directory");
    let output = Command::new(env!("CARGO_BIN_EXE_vaultsnap"))
        .current_dir(&cwd)
        .env_remove("VAULTSNAP_REPO")
        .env_remove("VAULTSNAP_PASSWORD_FILE")
        .env_remove("VAULTSNAP_S3_ENDPOINT")
        .env_remove("RUST_LOG")
        .arg("--password-file")
        .arg(env.password_file())
        .args(args)
        .output()
        .expect("run vaultsnap");
    (output, cwd)
}

fn assert_nothing_created(cwd: &Path) {
    let entries: Vec<_> = fs::read_dir(cwd).expect("read the working directory").collect();
    assert!(entries.is_empty(), "the command created {entries:?}");
}

#[test]
fn a_bad_bucket_name_is_refused_before_any_connection() {
    let env = TestEnv::default();
    for repo in ["s3://UPPER/case", "s3://ab", "s3://my-bucket/../up", "s3://"] {
        let (output, cwd) = run(&env, &["--repo", repo, "snapshots"]);
        assert_eq!(output.status.code(), Some(1), "{repo}: {}", stderr(&output));
        assert!(stderr(&output).contains("invalid S3 repository"), "{repo}: {}", stderr(&output));
        assert!(stdout(&output).is_empty(), "{repo}");
        assert_nothing_created(&cwd);
    }
}

#[test]
fn plain_http_to_another_machine_is_refused_unless_allowed() {
    let env = TestEnv::default();
    let args = [
        "--repo",
        "s3://my-bucket",
        "--s3-endpoint",
        "http://s3.example.com",
        "snapshots",
    ];
    let (output, cwd) = run(&env, &args);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("plain HTTP"), "{}", stderr(&output));
    assert_nothing_created(&cwd);
}

#[test]
fn credentials_in_the_endpoint_are_refused_and_not_repeated() {
    let env = TestEnv::default();
    let args = [
        "--repo",
        "s3://my-bucket",
        "--s3-endpoint",
        "https://user:topsecret@s3.example.com",
        "--s3-allow-insecure-http",
        "snapshots",
    ];
    let (output, cwd) = run(&env, &args);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("user name or password"), "{}", stderr(&output));
    for text in [stderr(&output), stdout(&output)] {
        assert!(!text.contains("topsecret"), "the password was repeated: {text}");
    }
    assert_nothing_created(&cwd);
}

#[test]
fn there_is_no_way_to_pass_credentials_on_the_command_line() {
    let env = TestEnv::default();
    let options = [
        "--s3-access-key",
        "--s3-secret-key",
        "--s3-token",
        "--access-key",
        "--secret-key",
    ];
    for option in options {
        let (output, cwd) = run(&env, &["--repo", "s3://my-bucket", option, "value", "snapshots"]);
        assert_eq!(output.status.code(), Some(2), "{option}: {}", stderr(&output));
        assert_nothing_created(&cwd);
    }
}

#[test]
fn the_help_describes_the_bucket_location_and_options() {
    let env = TestEnv::default();
    let (output, _) = run(&env, &["--help"]);
    assert_eq!(output.status.code(), Some(0));
    let help = stdout(&output);
    let expected = [
        "s3://",
        "--s3-endpoint",
        "--s3-region",
        "--s3-ca-bundle",
        "--s3-max-requests",
    ];
    for expected in expected {
        assert!(help.contains(expected), "{expected} is missing from the help:\n{help}");
    }
}
