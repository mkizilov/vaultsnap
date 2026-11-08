//! Maps parsed arguments onto `vaultsnap-core` operations and prints results.

use vaultsnap_core::{
    BackupOptions, CheckOptions, PruneOptions, Repository, RestoreOptions, RetentionPolicy,
};

use crate::cli::{Cli, Command};
use crate::passphrase;

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
    #[error("repository check found {0} problem(s)")]
    CheckFailed(usize),
    #[error(transparent)]
    Core(#[from] vaultsnap_core::Error),
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
            let options = BackupOptions {
                sources: args.paths,
                excludes: args.excludes,
                tags: args.tags,
            };
            println!("{}", repo.backup(&options).await?);
        }
        Command::Snapshots => {
            for snapshot in repo.snapshots().await? {
                println!(
                    "{}\t{}\t{}",
                    snapshot.id.short(),
                    snapshot.created_at_unix,
                    snapshot.tags.join(",")
                );
            }
        }
        Command::Restore(args) => {
            let options = RestoreOptions {
                snapshot: args.snapshot,
                target: args.target,
                includes: args.includes,
            };
            repo.restore(&options).await?;
        }
        Command::Check(args) => {
            let options = CheckOptions {
                read_data: args.read_data,
            };
            let report = repo.check(&options).await?;
            for problem in &report.problems {
                eprintln!("problem: {problem}");
            }
            if !report.is_ok() {
                return Err(CliError::CheckFailed(report.problems.len()));
            }
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
