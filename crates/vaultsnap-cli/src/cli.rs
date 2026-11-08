//! Command-line definition.

use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};
use vaultsnap_core::SnapshotRef;

/// Reliable encrypted incremental backups.
#[derive(Debug, Parser)]
#[command(name = "vaultsnap", version, propagate_version = true, arg_required_else_help = true)]
pub struct Cli {
    /// Repository location: a directory, or s3://BUCKET[/FOLDER] for an S3-compatible bucket
    #[arg(short, long, global = true, env = "VAULTSNAP_REPO", value_name = "PATH|s3://URL")]
    pub repo: Option<PathBuf>,

    #[command(flatten)]
    pub s3: S3Args,

    /// File containing the repository passphrase (instead of asking on the terminal)
    #[arg(long, global = true, env = "VAULTSNAP_PASSWORD_FILE", value_name = "FILE")]
    pub password_file: Option<PathBuf>,

    /// Increase log detail (-v info, -vv debug, -vvv trace); RUST_LOG overrides
    #[arg(short, long, global = true, action = ArgAction::Count)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Command,
}

/// How to reach a repository in an S3-compatible bucket (`--repo s3://BUCKET[/FOLDER]`).
///
/// There is no option for credentials, on purpose: they come from the standard AWS credential
/// chain (environment variables, `~/.aws` files and profiles, SSO, roles).
#[derive(Debug, Args)]
pub struct S3Args {
    /// S3 endpoint URL for an S3-compatible service such as MinIO (default: AWS)
    #[arg(long = "s3-endpoint", global = true, env = "VAULTSNAP_S3_ENDPOINT", value_name = "URL")]
    pub endpoint: Option<String>,

    /// Region to sign requests for (default: the AWS configuration, or us-east-1)
    #[arg(long = "s3-region", global = true, env = "VAULTSNAP_S3_REGION", value_name = "REGION")]
    pub region: Option<String>,

    /// Named profile of the AWS config and credentials files (default: AWS_PROFILE or default)
    #[arg(long = "s3-profile", global = true, env = "VAULTSNAP_S3_PROFILE", value_name = "NAME")]
    pub profile: Option<String>,

    /// Address buckets in the path rather than the host name (default: true with --s3-endpoint)
    #[arg(
        long = "s3-path-style",
        global = true,
        env = "VAULTSNAP_S3_PATH_STYLE",
        value_name = "BOOL"
    )]
    pub path_style: Option<bool>,

    /// PEM file of the certificate authorities to trust instead of the system's
    #[arg(long = "s3-ca-bundle", global = true, env = "AWS_CA_BUNDLE", value_name = "FILE")]
    pub ca_bundle: Option<PathBuf>,

    /// Accept an http:// endpoint that is not on this machine (traffic is not protected)
    #[arg(long = "s3-allow-insecure-http", global = true)]
    pub allow_insecure_http: bool,

    /// Most S3 requests in flight at once (default 8, at most 256)
    #[arg(
        long = "s3-max-requests",
        global = true,
        env = "VAULTSNAP_S3_MAX_REQUESTS",
        value_name = "N"
    )]
    pub max_requests: Option<usize>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a new, empty repository
    Init,
    /// Back up files and directories into a new snapshot
    Backup(BackupArgs),
    /// List snapshots, oldest first, with their sizes
    Snapshots(SnapshotsArgs),
    /// Show everything about one snapshot
    Show(ShowArgs),
    /// List the files in a snapshot
    Ls(LsArgs),
    /// Compare two snapshots: what was added, deleted, and modified
    Diff(DiffArgs),
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
pub struct SnapshotsArgs {
    /// Print JSON instead of a table
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct ShowArgs {
    /// Snapshot id, or the start of one (at least 4 characters, as `snapshots` shows them)
    pub snapshot: SnapshotRef,

    /// Print JSON instead of text
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct LsArgs {
    /// Snapshot id, or the start of one (at least 4 characters)
    pub snapshot: SnapshotRef,

    /// Only this path and what is below it, as an absolute path on the backed-up machine
    #[arg(value_name = "PATH")]
    pub path: Option<PathBuf>,

    /// Print JSON instead of text
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct DiffArgs {
    /// The older snapshot: id, or the start of one (at least 4 characters)
    pub from: SnapshotRef,

    /// The newer snapshot: id, or the start of one (at least 4 characters)
    pub to: SnapshotRef,

    /// Print JSON instead of text
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct RestoreArgs {
    /// Snapshot id, or the start of one (at least 4 characters, as `snapshots` shows them)
    pub snapshot: SnapshotRef,

    /// Directory to restore into (created if it does not exist, never inside the repository)
    #[arg(long, value_name = "DIR")]
    pub target: PathBuf,

    /// Path to restore, as `ls` shows it, with everything below it (repeatable; default is
    /// everything)
    #[arg(long = "include", value_name = "PATH")]
    pub includes: Vec<PathBuf>,

    /// What to do when something already exists in the target
    #[arg(long, value_enum, default_value_t = ConflictArg::Fail, value_name = "MODE")]
    pub on_conflict: ConflictArg,

    /// What to do with symbolic links in the snapshot
    #[arg(long, value_enum, default_value_t = SymlinkArg::Contained, value_name = "POLICY")]
    pub symlinks: SymlinkArg,
}

/// What `restore` does about things that already exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ConflictArg {
    /// Restore nothing and report every conflict (the default)
    Fail,
    /// Keep what exists, restore the rest, and report what was skipped
    Skip,
    /// Replace files and links that exist; directories are merged and never replaced
    Overwrite,
}

/// What `restore` does with symbolic links.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SymlinkArg {
    /// Restore only links that cannot lead out of the target (the default)
    Contained,
    /// Restore no links
    Skip,
    /// Restore every link exactly as recorded, including absolute and escaping ones
    AsRecorded,
}

#[derive(Debug, Args)]
pub struct CheckArgs {
    /// Also read, authenticate, and decrypt every chunk object (reads the whole repository)
    #[arg(long)]
    pub read_data: bool,

    /// Print JSON instead of text
    #[arg(long)]
    pub json: bool,

    /// Threads for reading chunk objects (default: the number of CPUs, at most 4)
    #[arg(long, value_name = "N")]
    pub jobs: Option<usize>,

    /// List at most N findings, errors first; the counts stay exact (0 lists all)
    #[arg(
        long,
        value_name = "N",
        default_value_t = vaultsnap_core::check::DEFAULT_MAX_FINDINGS
    )]
    pub max_findings: usize,
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

    /// Show what would be removed and change nothing
    #[arg(long)]
    pub dry_run: bool,

    /// Delete without asking for confirmation (required when standard input is not a terminal)
    #[arg(long)]
    pub yes: bool,
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
        let snapshots = parse(&["snapshots"]).map(|c| c.command);
        assert!(matches!(snapshots, Ok(Command::Snapshots(_))));
        assert!(matches!(parse(&["show", "abcd"]).map(|c| c.command), Ok(Command::Show(_))));
        assert!(matches!(parse(&["ls", "abcd"]).map(|c| c.command), Ok(Command::Ls(_))));
        assert!(matches!(parse(&["diff", "abcd", "ef01"]).map(|c| c.command), Ok(Command::Diff(_))));
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
    fn s3_options_are_global_and_have_no_credentials() {
        let cli = parse(&[
            "--repo",
            "s3://bucket/folder",
            "snapshots",
            "--s3-endpoint",
            "http://127.0.0.1:21100",
            "--s3-region",
            "eu-west-1",
            "--s3-path-style",
            "false",
            "--s3-max-requests",
            "4",
            "--s3-allow-insecure-http",
        ])
        .expect("valid arguments");
        assert_eq!(cli.repo, Some(PathBuf::from("s3://bucket/folder")));
        assert_eq!(cli.s3.endpoint.as_deref(), Some("http://127.0.0.1:21100"));
        assert_eq!(cli.s3.region.as_deref(), Some("eu-west-1"));
        assert_eq!(cli.s3.path_style, Some(false));
        assert_eq!(cli.s3.max_requests, Some(4));
        assert!(cli.s3.allow_insecure_http);

        let cli = parse(&["init"]).expect("valid arguments");
        assert!(cli.s3.endpoint.is_none() && cli.s3.profile.is_none() && !cli.s3.allow_insecure_http);

        let command = Cli::command();
        let names: Vec<&str> = command.get_arguments().filter_map(|arg| arg.get_long()).collect();
        for forbidden in ["access-key", "secret-key", "s3-access-key", "s3-secret-key", "s3-token"] {
            assert!(!names.contains(&forbidden), "{forbidden} must not exist");
        }
        assert!(parse(&["snapshots", "--s3-path-style", "maybe"]).is_err());
    }

    #[test]
    fn inspection_commands_take_snapshot_references_and_a_json_flag() {
        let Command::Snapshots(args) = parse(&["snapshots", "--json"]).expect("valid").command else {
            panic!("expected snapshots");
        };
        assert!(args.json);

        let id = "a".repeat(64);
        let Command::Show(args) = parse(&["show", id.as_str()]).expect("valid").command else {
            panic!("expected show");
        };
        assert!(args.snapshot.is_full() && !args.json);

        let cli = parse(&["ls", "abcd", "/home/me", "--json"]).expect("valid");
        let Command::Ls(args) = cli.command else {
            panic!("expected ls");
        };
        assert_eq!(args.snapshot.as_str(), "abcd");
        assert_eq!(args.path, Some(PathBuf::from("/home/me")));
        assert!(args.json);

        let cli = parse(&["diff", "abcd", "ef012345"]).expect("valid");
        let Command::Diff(args) = cli.command else {
            panic!("expected diff");
        };
        assert_eq!((args.from.as_str(), args.to.as_str()), ("abcd", "ef012345"));
    }

    #[test]
    fn inspection_commands_reject_malformed_references_and_missing_arguments() {
        for args in [
            &["show", "abc"][..],
            &["show", "latest"],
            &["show", "ABCD"],
            &["ls", "abcd/"],
            &["diff", "abcd"],
            &["diff", "abcd", "xyz"],
            &["show"],
        ] {
            assert!(parse(args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn restore_defaults_to_the_safest_choices() {
        let id = "a".repeat(64);
        let id = id.as_str();
        let cli = parse(&["restore", id, "--target", "out"]).expect("valid arguments");
        let Command::Restore(args) = cli.command else {
            panic!("expected restore");
        };
        assert_eq!(args.on_conflict, ConflictArg::Fail);
        assert_eq!(args.symlinks, SymlinkArg::Contained);
        assert!(args.includes.is_empty());
    }

    #[test]
    fn restore_takes_its_policies_and_paths() {
        let id = "a".repeat(64);
        let id = id.as_str();
        let cli = parse(&[
            "restore",
            id,
            "--target",
            "out",
            "--include",
            "/home/me/a",
            "--include",
            "/home/me/b",
            "--on-conflict",
            "overwrite",
            "--symlinks",
            "as-recorded",
        ])
        .expect("valid arguments");
        let Command::Restore(args) = cli.command else {
            panic!("expected restore");
        };
        assert_eq!(args.on_conflict, ConflictArg::Overwrite);
        assert_eq!(args.symlinks, SymlinkArg::AsRecorded);
        assert_eq!(args.includes, [PathBuf::from("/home/me/a"), PathBuf::from("/home/me/b")]);
        let skipping = ["restore", id, "--target", "out", "--on-conflict", "skip"];
        let mut skipping = skipping.to_vec();
        skipping.extend(["--symlinks", "skip"]);
        let cli = parse(&skipping).expect("valid arguments");
        let Command::Restore(args) = cli.command else {
            panic!("expected restore");
        };
        assert_eq!((args.on_conflict, args.symlinks), (ConflictArg::Skip, SymlinkArg::Skip));
    }

    #[test]
    fn restore_rejects_unknown_policies_and_a_missing_target() {
        let id = "a".repeat(64);
        let id = id.as_str();
        for extra in [
            ["--on-conflict", "merge"],
            ["--on-conflict", ""],
            ["--symlinks", "follow"],
            ["--symlinks", "Contained"],
        ] {
            let mut args = vec!["restore", id, "--target", "out"];
            args.extend(extra);
            assert!(parse(&args).is_err(), "{extra:?}");
        }
        assert!(parse(&["restore", id]).is_err(), "--target is required");
    }

    #[test]
    fn check_is_quick_and_bounded_by_default() {
        let Command::Check(args) = parse(&["check"]).expect("valid arguments").command else {
            panic!("expected check");
        };
        assert!(!args.read_data && !args.json);
        assert_eq!((args.jobs, args.max_findings), (None, 1000));
    }

    #[test]
    fn check_takes_its_options() {
        let cli = parse(&[
            "check",
            "--read-data",
            "--json",
            "--jobs",
            "3",
            "--max-findings",
            "0",
        ])
        .expect("valid arguments");
        let Command::Check(args) = cli.command else {
            panic!("expected check");
        };
        assert!(args.read_data && args.json);
        assert_eq!((args.jobs, args.max_findings), (Some(3), 0));
        for bad in [["--jobs", "-1"], ["--jobs", "many"], ["--max-findings", "-5"]] {
            let mut args = vec!["check"];
            args.extend(bad);
            assert!(parse(&args).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn prune_asks_for_confirmation_unless_told_not_to() {
        let Command::Prune(args) = parse(&["prune", "--keep-last", "2"]).expect("valid").command
        else {
            panic!("expected prune");
        };
        assert!(!args.yes && !args.dry_run);
        let cli = parse(&["prune", "--keep-daily", "7", "--keep-weekly", "4", "--yes"])
            .expect("valid");
        let Command::Prune(args) = cli.command else {
            panic!("expected prune");
        };
        assert!(args.yes && !args.dry_run);
        assert_eq!((args.keep_daily, args.keep_weekly, args.keep_last), (Some(7), Some(4), None));
        assert!(parse(&["prune", "--keep-last", "-1"]).is_err());
        assert!(parse(&["prune", "--keep-last", "many"]).is_err());
        assert!(parse(&["prune", "--keep-last", "1", "--yes=maybe"]).is_err());
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
