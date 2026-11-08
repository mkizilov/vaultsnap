//! Shared helpers for CLI integration tests. Temporary directories honor `TMPDIR`, which the
//! project environment points inside the repository.

use std::path::PathBuf;
use std::process::{Command, Output};

use tempfile::TempDir;

/// A scratch directory plus a way to run the `vaultsnap` binary against a repository inside it.
pub struct TestEnv {
    dir: TempDir,
}

impl Default for TestEnv {
    fn default() -> Self {
        Self {
            dir: tempfile::tempdir().expect("create temporary directory"),
        }
    }
}

impl TestEnv {
    /// Repository path inside the scratch directory; the directory is never created implicitly.
    pub fn repo_path(&self) -> PathBuf {
        self.dir.path().join("repo")
    }

    /// Runs `vaultsnap --repo <repo_path> <args>`.
    pub fn run(&self, args: &[&str]) -> Output {
        command()
            .arg("--repo")
            .arg(self.repo_path())
            .args(args)
            .output()
            .expect("run vaultsnap")
    }
}

/// Runs `vaultsnap <args>` with no repository configured.
pub fn run_without_repo(args: &[&str]) -> Output {
    command().args(args).output().expect("run vaultsnap")
}

pub fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_vaultsnap"));
    command.env_remove("VAULTSNAP_REPO").env_remove("RUST_LOG");
    command
}
