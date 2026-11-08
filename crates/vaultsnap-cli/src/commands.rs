//! Maps parsed arguments onto `vaultsnap-core` operations and prints results.

use vaultsnap_core::{Error, PruneOptions, Repository, RetentionPolicy};

use crate::cli::{Cli, Command};
use crate::{backup, check, inspect, passphrase, restore};

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("no repository specified: pass --repo <PATH> or set VAULTSNAP_REPO")]
    MissingRepository,
    #[error("cannot read password file {}: {}", .path.display(), .source)]
    PasswordFile {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error("password file {} is larger than 4096 bytes", .path.display())]
    PasswordFileTooLarge { path: std::path::PathBuf },
    #[error(
        "no terminal available to ask for a passphrase: pass --password-file <FILE> \
         (or set VAULTSNAP_PASSWORD_FILE)"
    )]
    NoTerminal,
    #[error("the two passphrases do not match")]
    PassphraseMismatch,
    #[error("cannot read the passphrase from the terminal: {0}")]
    Terminal(std::io::Error),
    #[error(
        "the check found {0} error(s) (listed above); nothing in the repository was changed"
    )]
    CheckFailed(usize),
    #[error(
        "the snapshot was created but is incomplete: {failed} path(s) could not be read and are \
         not in it (listed above)"
    )]
    BackupIncomplete { failed: usize },
    #[error(
        "{count} snapshot(s) could not be verified and are not shown: they are damaged or were \
         modified (listed above)"
    )]
    DamagedSnapshots { count: usize },
    #[error(
        "the restore is incomplete: {failed} path(s) could not be restored (listed above); \
         nothing half-written was left behind"
    )]
    RestoreIncomplete { failed: usize },
    #[error("snapshot {snapshot} has no entry at or below {}", .path.display())]
    PathNotInSnapshot {
        snapshot: String,
        path: std::path::PathBuf,
    },
    #[error("cannot write the output: {0}")]
    Output(std::io::Error),
    #[error(transparent)]
    Core(#[from] vaultsnap_core::Error),
}

impl CliError {
    /// 1 for failures, 3 when the command finished but with a problem in its result (a snapshot
    /// that is missing some paths, snapshots that could not be verified, or paths that could not
    /// be restored), and 130 (the shell convention for SIGINT) when a backup or restore was
    /// cancelled. Usage errors exit with 2 before any command runs.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::BackupIncomplete { .. }
            | Self::DamagedSnapshots { .. }
            | Self::RestoreIncomplete { .. }
            | Self::CheckFailed(_) => 3,
            Self::Core(Error::Cancelled | Error::RestoreCancelled | Error::CheckCancelled) => 130,
            _ => 1,
        }
    }
}

pub async fn run(cli: Cli) -> Result<(), CliError> {
    let root = cli.repo.ok_or(CliError::MissingRepository)?;
    tracing::debug!(repo = %root.display(), "using repository");
    let repo = Repository::at(root);

    match cli.command {
        Command::Init => {
            let passphrase = passphrase::read_new(cli.password_file.as_deref())?;
            let config = repo.init(&passphrase).await?;
            println!(
                "created VaultSnap repository {} at {}",
                config.repository_id,
                repo.layout().root().display()
            );
        }
        Command::Backup(args) => {
            backup::run(&repo, cli.password_file.as_deref(), args).await?;
        }
        Command::Snapshots(args) => {
            inspect::snapshots(&repo, cli.password_file.as_deref(), args).await?;
        }
        Command::Show(args) => {
            inspect::show(&repo, cli.password_file.as_deref(), args).await?;
        }
        Command::Ls(args) => {
            inspect::ls(&repo, cli.password_file.as_deref(), args).await?;
        }
        Command::Diff(args) => {
            inspect::diff(&repo, cli.password_file.as_deref(), args).await?;
        }
        Command::Restore(args) => {
            restore::run(&repo, cli.password_file.as_deref(), args).await?;
        }
        Command::Check(args) => {
            check::run(&repo, cli.password_file.as_deref(), args).await?;
        }
        Command::Prune(args) => {
            let options = PruneOptions {
                policy: RetentionPolicy {
                    keep_last: args.keep_last,
                    keep_daily: args.keep_daily,
                    keep_weekly: args.keep_weekly,
                    keep_monthly: args.keep_monthly,
                    keep_yearly: args.keep_yearly,
                },
                dry_run: args.dry_run,
            };
            let report = repo.prune(&options).await?;
            println!("{} snapshot(s) selected for removal", report.removed.len());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_tell_failures_incomplete_backups_and_cancellation_apart() {
        assert_eq!(CliError::MissingRepository.exit_code(), 1);
        assert_eq!(CliError::Core(vaultsnap_core::Error::WrongPassphrase).exit_code(), 1);
        assert_eq!(CliError::BackupIncomplete { failed: 2 }.exit_code(), 3);
        assert_eq!(CliError::DamagedSnapshots { count: 1 }.exit_code(), 3);
        assert_eq!(CliError::RestoreIncomplete { failed: 4 }.exit_code(), 3);
        assert_eq!(CliError::CheckFailed(2).exit_code(), 3);
        assert_eq!(CliError::Core(vaultsnap_core::Error::CheckCancelled).exit_code(), 130);
        assert_eq!(CliError::Core(vaultsnap_core::Error::RestoreCancelled).exit_code(), 130);
        let conflict = vaultsnap_core::Error::RestoreConflict {
            count: 1,
            first: std::path::PathBuf::from("x"),
        };
        assert_eq!(CliError::Core(conflict).exit_code(), 1);
        assert_eq!(CliError::Output(std::io::Error::other("disk full")).exit_code(), 1);
        assert_eq!(CliError::Core(vaultsnap_core::Error::Cancelled).exit_code(), 130);
    }
}
