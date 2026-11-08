//! Shared helpers for CLI integration tests. Temporary directories honor `TMPDIR`, which the
//! project environment points inside the repository.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

/// The passphrase written (with a trailing newline) to the test password file.
pub const PASSPHRASE: &str = "correct horse battery staple";

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

    /// Writes `contents` to a private file in the scratch directory and returns its path.
    pub fn write_password_file(&self, name: &str, contents: &[u8]) -> PathBuf {
        let path = self.dir.path().join(name);
        std::fs::write(&path, contents).expect("write password file");
        restrict(&path);
        path
    }

    /// A password file holding [`PASSPHRASE`].
    pub fn password_file(&self) -> PathBuf {
        self.write_password_file("passphrase", format!("{PASSPHRASE}\n").as_bytes())
    }

    /// Runs `vaultsnap --repo <repo_path> <args>`.
    pub fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().expect("run vaultsnap")
    }

    /// Runs `vaultsnap --repo <repo_path> --password-file <file> <args>`.
    pub fn run_with_password_file(&self, file: &Path, args: &[&str]) -> Output {
        self.command()
            .arg("--password-file")
            .arg(file)
            .args(args)
            .output()
            .expect("run vaultsnap")
    }

    /// Initializes the repository with [`TestEnv::password_file`].
    pub fn init(&self) -> Output {
        self.run_with_password_file(&self.password_file(), &["init"])
    }

    fn command(&self) -> Command {
        let mut command = command();
        command.arg("--repo").arg(self.repo_path());
        command
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
    command
        .env_remove("VAULTSNAP_REPO")
        .env_remove("VAULTSNAP_PASSWORD_FILE")
        .env_remove("RUST_LOG");
    command
}

#[cfg(unix)]
fn restrict(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let permissions = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, permissions).expect("restrict password file");
}

#[cfg(not(unix))]
fn restrict(_path: &Path) {}
