//! Command-line definition.

use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand};
use vaultsnap_core::SnapshotId;

/// Reliable encrypted incremental backups.
#[derive(Debug, Parser)]
#[command(name = "vaultsnap", version, propagate_version = true, arg_required_else_help = true)]
pub struct Cli {
    /// Repository location
    #[arg(short, long, global = true, env = "VAULTSNAP_REPO", value_name = "PATH")]
    pub repo: Option<PathBuf>,

    /// File containing the repository passphrase (instead of asking on the terminal)
    #[arg(long, global = true, env = "VAULTSNAP_PASSWORD_FILE", value_name = "FILE")]
    pub password_file: Option<PathBuf>,

    /// Increase log detail (-v info, -vv debug, -vvv trace); RUST_LOG overrides
    #[arg(short, long, global = true, action = ArgAction::Count)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a new, empty repository
    Init,
    /// Back up files and directories into a new snapshot
    Backup(BackupArgs),
    /// List snapshots
    Snapshots,
    /// Restore a snapshot into a directory
    Restore(RestoreArgs),
    /// Verify repository integrity
    Check(CheckArgs),
    /// Remove snapshots outside the retention policy and reclaim space
    Prune(PruneArgs),
}

#[derive(Debug, Args)]
pub struct BackupArgs {
    /// Files and directories to back up
    #[arg(required = true, value_name = "PATH")]
    pub paths: Vec<PathBuf>,

    /// Glob pattern to skip (repeatable)
    #[arg(long = "exclude", value_name = "PATTERN")]
    pub excludes: Vec<String>,

    /// Label to store with the snapshot (repeatable)
    #[arg(long = "tag", value_name = "TAG")]
    pub tags: Vec<String>,

    /// Host name to store with the snapshot
    #[arg(long, value_name = "NAME")]
    pub host: Option<String>,

    /// Worker threads (default: one per CPU, at most 4; larger values are reduced to 64)
    #[arg(long, value_name = "N")]
    pub jobs: Option<usize>,

    /// Read every file instead of trusting unchanged metadata from the previous snapshot
    #[arg(long)]
    pub rescan: bool,
}

#[derive(Debug, Args)]
pub struct RestoreArgs {
    /// Snapshot to restore (64 lowercase hex characters)
    pub snapshot: SnapshotId,

    /// Directory to restore into
    #[arg(long, value_name = "DIR")]
    pub target: PathBuf,

    /// Path inside the snapshot to restore (repeatable; default is everything)
    #[arg(long = "include", value_name = "PATH")]
    pub includes: Vec<PathBuf>,
}

#[derive(Debug, Args)]
pub struct CheckArgs {
    /// Also read and authenticate all stored data
    #[arg(long)]
    pub read_data: bool,
}

#[derive(Debug, Args)]
pub struct PruneArgs {
    /// Keep the most recent N snapshots
    #[arg(long, value_name = "N")]
    pub keep_last: Option<u32>,

    /// Keep the newest snapshot of each of the last N days
    #[arg(long, value_name = "N")]
    pub keep_daily: Option<u32>,

    /// Keep the newest snapshot of each of the last N weeks
    #[arg(long, value_name = "N")]
    pub keep_weekly: Option<u32>,

    /// Keep the newest snapshot of each of the last N months
    #[arg(long, value_name = "N")]
    pub keep_monthly: Option<u32>,

    /// Keep the newest snapshot of each of the last N years
    #[arg(long, value_name = "N")]
    pub keep_yearly: Option<u32>,

    /// Report what would be removed without removing anything
    #[arg(long)]
    pub dry_run: bool,
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("vaultsnap").chain(args.iter().copied()))
    }

    #[test]
    fn command_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_every_subcommand() {
        assert!(matches!(parse(&["init"]).map(|c| c.command), Ok(Command::Init)));
        assert!(matches!(parse(&["backup", "a"]).map(|c| c.command), Ok(Command::Backup(_))));
        assert!(matches!(parse(&["snapshots"]).map(|c| c.command), Ok(Command::Snapshots)));
        assert!(matches!(parse(&["check"]).map(|c| c.command), Ok(Command::Check(_))));
        assert!(matches!(parse(&["prune"]).map(|c| c.command), Ok(Command::Prune(_))));
        let id = "a".repeat(64);
        let restore = parse(&["restore", &id, "--target", "out"]);
        assert!(matches!(restore.map(|c| c.command), Ok(Command::Restore(_))));
    }

    #[test]
    fn password_file_is_a_global_option() {
        let cli = parse(&["init", "--password-file", "/p"]).expect("valid arguments");
        assert_eq!(cli.password_file, Some(PathBuf::from("/p")));
        let cli = parse(&["--password-file", "/q", "snapshots"]).expect("valid arguments");
        assert_eq!(cli.password_file, Some(PathBuf::from("/q")));
    }

    #[test]
    fn backup_requires_a_path() {
        assert!(parse(&["backup"]).is_err());
    }

    #[test]
    fn backup_collects_its_options() {
        let cli = parse(&[
            "backup",
            "a",
            "b",
            "--exclude",
            "*.log",
            "--exclude",
            "tmp/",
            "--tag",
            "x",
            "--host",
            "box",
            "--jobs",
            "3",
            "--rescan",
        ])
        .expect("valid arguments");
        let Command::Backup(args) = cli.command else {
            panic!("expected backup");
        };
        assert_eq!(args.paths, [PathBuf::from("a"), PathBuf::from("b")]);
        assert_eq!(args.excludes, ["*.log", "tmp/"]);
        assert_eq!(args.tags, ["x"]);
        assert_eq!(args.host.as_deref(), Some("box"));
        assert_eq!(args.jobs, Some(3));
        assert!(args.rescan);
    }

    #[test]
    fn backup_defaults_are_conservative() {
        let cli = parse(&["backup", "a"]).expect("valid arguments");
        let Command::Backup(args) = cli.command else {
            panic!("expected backup");
        };
        assert!(args.excludes.is_empty() && args.tags.is_empty());
        assert_eq!((args.host, args.jobs, args.rescan), (None, None, false));
    }

    #[test]
    fn backup_rejects_a_non_numeric_job_count() {
        assert!(parse(&["backup", "a", "--jobs", "many"]).is_err());
    }

    #[test]
    fn restore_rejects_malformed_snapshot_ids() {
        assert!(parse(&["restore", "latest", "--target", "out"]).is_err());
    }

    #[test]
    fn global_flags_work_after_the_subcommand() {
        let cli = parse(&["snapshots", "--repo", "/r", "-vv"]).expect("valid arguments");
        assert_eq!(cli.repo, Some(PathBuf::from("/r")));
        assert_eq!(cli.verbose, 2);
    }

    #[test]
    fn prune_collects_retention_flags() {
        let cli = parse(&["prune", "--keep-last", "3", "--keep-yearly", "2", "--dry-run"])
            .expect("valid arguments");
        let Command::Prune(args) = cli.command else {
            panic!("expected prune");
        };
        assert_eq!(args.keep_last, Some(3));
        assert_eq!(args.keep_yearly, Some(2));
        assert_eq!(args.keep_daily, None);
        assert!(args.dry_run);
    }
}
