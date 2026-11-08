//! The commands that look at snapshots that already exist: `snapshots`, `show`, `ls`, and `diff`.
//!
//! Each one unlocks the repository, asks the core for verified data (every snapshot is
//! authenticated and decrypted before anything is shown), and prints it either as text for people
//! ([`human`]) or as one JSON document ([`json`]). They only read the repository.

mod human;
mod json;
#[cfg(test)]
mod tests;

use std::io::{self, BufWriter, Write};
use std::path::Path;

use vaultsnap_core::history::entries;
use vaultsnap_core::Repository;

use crate::cli::{DiffArgs, LsArgs, ShowArgs, SnapshotsArgs};
use crate::commands::CliError;
use crate::passphrase;

/// Turns the result of writing output into the command's result. If the reader went away
/// (`vaultsnap ls | head`) the output just ends; that is not a failure of the command.
fn finish(result: io::Result<()>) -> Result<(), CliError> {
    match result {
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        other => other.map_err(CliError::Output),
    }
}

/// Renders to standard output through a buffer.
fn output(render: impl FnOnce(&mut dyn Write) -> io::Result<()>) -> Result<(), CliError> {
    let mut out = BufWriter::new(io::stdout().lock());
    finish(render(&mut out).and_then(|()| out.flush()))
}

pub async fn snapshots(
    repo: &Repository,
    password_file: Option<&Path>,
    args: SnapshotsArgs,
) -> Result<(), CliError> {
    let keys = passphrase::unlock(repo, password_file).await?;
    let list = repo.snapshots(&keys).await?;
    output(|out| {
        if args.json {
            json::list(out, &list)
        } else {
            human::list(out, &list)
        }
    })?;
    for damaged in &list.damaged {
        eprintln!("warning: snapshot {} is damaged and not shown: {}", damaged.id, damaged.error);
    }
    match list.damaged.len() {
        0 => Ok(()),
        count => Err(CliError::DamagedSnapshots { count }),
    }
}

pub async fn show(
    repo: &Repository,
    password_file: Option<&Path>,
    args: ShowArgs,
) -> Result<(), CliError> {
    let keys = passphrase::unlock(repo, password_file).await?;
    let details = repo.snapshot_details(&keys, &args.snapshot).await?;
    output(|out| {
        if args.json {
            json::show(out, &details)
        } else {
            human::show(out, &details)
        }
    })
}

pub async fn ls(
    repo: &Repository,
    password_file: Option<&Path>,
    args: LsArgs,
) -> Result<(), CliError> {
    let keys = passphrase::unlock(repo, password_file).await?;
    let manifest = repo.snapshot(&keys, &args.snapshot).await?;
    let under = args.path.as_deref();
    if let Some(path) = under {
        if entries(&manifest, Some(path)).next().is_none() {
            return Err(CliError::PathNotInSnapshot {
                snapshot: manifest.id.to_string(),
                path: path.to_path_buf(),
            });
        }
    }
    output(|out| {
        if args.json {
            json::entries(out, &manifest, under)
        } else {
            human::entries(out, &manifest, under)
        }
    })
}

pub async fn diff(
    repo: &Repository,
    password_file: Option<&Path>,
    args: DiffArgs,
) -> Result<(), CliError> {
    let keys = passphrase::unlock(repo, password_file).await?;
    let diff = repo.compare_snapshots(&keys, &args.from, &args.to).await?;
    output(|out| {
        if args.json {
            json::diff(out, &diff)
        } else {
            human::diff(out, &diff)
        }
    })
}
