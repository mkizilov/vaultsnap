mod common;

use common::{run_without_repo, stderr, stdout, TestEnv, PASSPHRASE};
use vaultsnap_core::{Error, Passphrase, Repository};

const COMMANDS: [&str; 9] =
    ["init", "backup", "snapshots", "show", "ls", "diff", "restore", "check", "prune"];

fn passphrase(text: &str) -> Passphrase {
    Passphrase::new(text.as_bytes().to_vec()).expect("valid passphrase")
}

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
    assert!(text.contains("--password-file"), "{text}");
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
    assert!(stderr(&output).contains("invalid snapshot reference"), "{}", stderr(&output));
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
    let invocations: [&[&str]; 1] = [&["prune", "--keep-last", "1"]];
    for args in invocations {
        let output = env.run(args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert!(stderr(&output).contains("not implemented"), "{args:?}");
    }
    assert!(!env.repo_path().exists());
}

#[test]
fn init_creates_a_repository_with_an_encrypted_master_key() {
    let env = TestEnv::default();
    let output = env.init();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stdout(&output).contains("created VaultSnap repository"));

    let repo = env.repo_path();
    for entry in ["config", "data", "snapshots", "locks", "keys"] {
        assert!(repo.join(entry).exists(), "missing {entry}");
    }
    let config = std::fs::read_to_string(repo.join("config")).expect("read config");
    assert!(config.starts_with("vaultsnap-repository\n"));
    assert!(config.contains("format_version=1\n"));

    let key_files: Vec<_> = std::fs::read_dir(repo.join("keys"))
        .expect("read keys")
        .map(|entry| entry.expect("entry").path())
        .collect();
    assert_eq!(key_files.len(), 1, "{key_files:?}");
    let key_file = std::fs::read_to_string(&key_files[0]).expect("read key file");
    for line in ["key_version=1", "kdf=argon2id", "cipher=xchacha20poly1305"] {
        assert!(key_file.lines().any(|l| l == line), "missing {line}:\n{key_file}");
    }
    assert!(!key_file.contains(PASSPHRASE));
}

#[tokio::test]
async fn the_initialized_repository_unlocks_only_with_the_passphrase() {
    let env = TestEnv::default();
    assert!(env.init().status.success());
    let repo = Repository::at(env.repo_path());

    repo.unlock(&passphrase(PASSPHRASE)).await.expect("file newline is not part of it");
    let with_newline = format!("{PASSPHRASE}\n");
    for wrong in [with_newline.as_str(), "wrong", "Correct horse battery staple"] {
        let err = repo.unlock(&passphrase(wrong)).await.expect_err("must fail");
        assert!(matches!(err, Error::WrongPassphrase), "{wrong:?}: {err}");
    }
}

#[test]
fn the_password_file_may_come_from_the_environment() {
    let env = TestEnv::default();
    let file = env.password_file();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_vaultsnap"))
        .env_remove("RUST_LOG")
        .env("VAULTSNAP_PASSWORD_FILE", &file)
        .env("VAULTSNAP_REPO", env.repo_path())
        .arg("init")
        .output()
        .expect("run vaultsnap");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(env.repo_path().join("config").exists());
}

#[test]
fn init_without_a_password_source_fails_when_there_is_no_terminal() {
    let env = TestEnv::default();
    let output = env.run(&["init"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("--password-file"), "{}", stderr(&output));
    assert!(!env.repo_path().exists());
}

#[test]
fn bad_password_files_are_rejected_before_anything_is_created() {
    let env = TestEnv::default();
    let missing = env.repo_path().with_file_name("does-not-exist");
    let empty = env.write_password_file("empty", b"");
    let only_newline = env.write_password_file("newline", b"\n");
    let huge = env.write_password_file("huge", &vec![b'x'; 5000]);

    for (file, expected) in [
        (&missing, "cannot read password file"),
        (&empty, "must not be empty"),
        (&only_newline, "must not be empty"),
        (&huge, "larger than"),
    ] {
        let output = env.run_with_password_file(file, &["init"]);
        assert_eq!(output.status.code(), Some(1), "{file:?}");
        assert!(stderr(&output).contains(expected), "{file:?}: {}", stderr(&output));
        assert!(!env.repo_path().exists(), "{file:?}");
    }
}

#[test]
fn secrets_never_appear_in_output_even_with_trace_logging() {
    let env = TestEnv::default();
    let output = env.run_with_password_file(&env.password_file(), &["-vvv", "init"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let all = format!("{}{}", stdout(&output), stderr(&output));
    assert!(!all.contains("correct horse"), "{all}");
    assert!(!all.contains("battery"), "{all}");

    let output = env.run_with_password_file(&env.password_file(), &["-vvv", "init"]);
    assert!(!output.status.success());
    let all = format!("{}{}", stdout(&output), stderr(&output));
    assert!(!all.contains("correct horse"), "{all}");
}

#[test]
fn repeated_init_fails_and_keeps_the_repository() {
    let env = TestEnv::default();
    assert!(env.init().status.success());
    let config_path = env.repo_path().join("config");
    let before = std::fs::read(&config_path).expect("read config");

    let output = env.init();
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("already exists"), "{}", stderr(&output));
    assert_eq!(std::fs::read(&config_path).expect("read config"), before);
    assert_eq!(std::fs::read_dir(env.repo_path().join("keys")).expect("read keys").count(), 1);
}

#[test]
fn init_refuses_a_file_and_an_unrelated_directory() {
    let env = TestEnv::default();
    let repo = env.repo_path();
    std::fs::write(&repo, b"a file").expect("write file");
    let output = env.init();
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("not a directory"), "{}", stderr(&output));
    assert_eq!(std::fs::read(&repo).expect("read file"), b"a file");

    std::fs::remove_file(&repo).expect("remove file");
    std::fs::create_dir(&repo).expect("mkdir");
    std::fs::write(repo.join("notes.txt"), b"mine").expect("write file");
    let output = env.init();
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("notes.txt"), "{}", stderr(&output));
    assert!(!repo.join("config").exists());
}

#[test]
fn init_reports_incompatible_versions_without_changing_them() {
    let env = TestEnv::default();
    assert!(env.init().status.success());
    let config_path = env.repo_path().join("config");
    let text = std::fs::read_to_string(&config_path).expect("read config");
    let newer = text.replace("format_version=1", "format_version=2");
    std::fs::write(&config_path, &newer).expect("write config");

    let output = env.init();
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
