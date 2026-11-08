//! The `restore` command: unlocks the repository, restores with a progress line and interrupt
//! handling, and reports what was done, skipped, and could not be done.

use std::io::IsTerminal;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use vaultsnap_core::{
    CancelToken, ConflictPolicy, Repository, RestoreEvent, RestoreObserver, RestoreOptions,
    RestoreProblem, RestoreReport, RestoreSymlinks,
};

use crate::cli::{ConflictArg, RestoreArgs, SymlinkArg};
use crate::commands::CliError;
use crate::format::{escape, escape_path, human_size};
use crate::progress::{self, Ticker};
use crate::{interrupt, passphrase};

/// At most this many skipped paths and warnings are listed; the counts are always complete.
const LISTED: usize = 20;

pub async fn run(
    repo: &Repository,
    password_file: Option<&Path>,
    args: RestoreArgs,
) -> Result<(), CliError> {
    let keys = passphrase::unlock(repo, password_file).await?;

    let cancel = CancelToken::new();
    interrupt::cancel_on_interrupt(
        cancel.clone(),
        "interrupted: stopping; the files restored so far stay, and nothing half-written is left",
    );

    let progress = std::io::stderr()
        .is_terminal()
        .then(|| Arc::new(Progress::new()));
    let mut options = RestoreOptions::new(args.snapshot, args.target);
    options.includes = args.includes;
    options.on_conflict = conflict_policy(args.on_conflict);
    options.symlinks = symlink_policy(args.symlinks);
    options.observer = progress
        .clone()
        .map(|progress| progress as Arc<dyn RestoreObserver>);
    options.cancel = cancel;

    let result = repo.restore(&keys, options).await;
    if let Some(progress) = progress {
        progress.clear();
    }
    let report = result?;

    print_report(&report);
    list_problems(&report);
    match report.failed.len() {
        0 => Ok(()),
        failed => Err(CliError::RestoreIncomplete { failed }),
    }
}

fn conflict_policy(arg: ConflictArg) -> ConflictPolicy {
    match arg {
        ConflictArg::Fail => ConflictPolicy::Fail,
        ConflictArg::Skip => ConflictPolicy::Skip,
        ConflictArg::Overwrite => ConflictPolicy::Overwrite,
    }
}

fn symlink_policy(arg: SymlinkArg) -> RestoreSymlinks {
    match arg {
        SymlinkArg::Contained => RestoreSymlinks::Contained,
        SymlinkArg::Skip => RestoreSymlinks::Skip,
        SymlinkArg::AsRecorded => RestoreSymlinks::AsRecorded,
    }
}

fn print_report(report: &RestoreReport) {
    println!(
        "restored snapshot {} into {}",
        report.snapshot,
        escape_path(&report.target)
    );
    println!("  files         {} ({} written)", report.files, human_size(report.bytes));
    println!("  directories   {}", report.directories);
    println!(
        "  links         {} symbolic, {} hard",
        report.symlinks, report.hard_links
    );
    if !report.skipped.is_empty() {
        println!("  skipped       {} (left out on purpose)", report.skipped.len());
    }
    if !report.failed.is_empty() {
        println!("  failed        {}", report.failed.len());
    }
    if !report.warnings.is_empty() {
        println!("  attributes    {} could not be applied", report.warnings.len());
    }
    println!("  duration      {:.1}s", report.duration.as_secs_f64());
}

/// Everything that was not restored, on standard error. Failures are always listed in full; the
/// lists of skipped paths and warnings can be long (skipping over a big existing tree) and stop
/// after a few. Paths come from the snapshot, so they are escaped before being printed.
fn list_problems(report: &RestoreReport) {
    list("could not be restored", &report.failed, usize::MAX);
    list("skipped", &report.skipped, LISTED);
    list("attributes that could not be applied", &report.warnings, LISTED);
}

fn list(heading: &str, problems: &[RestoreProblem], limit: usize) {
    if problems.is_empty() {
        return;
    }
    eprintln!("{} {heading}:", problems.len());
    for problem in problems.iter().take(limit) {
        eprintln!(
            "  {}: {}",
            escape_path(&problem.path),
            escape(problem.reason.as_bytes())
        );
    }
    if problems.len() > limit {
        eprintln!("  and {} more", problems.len() - limit);
    }
}

/// A one-line status on the terminal while files are written.
struct Progress {
    ticker: Ticker,
    planned_bytes: AtomicU64,
    written: AtomicU64,
    entries: AtomicU64,
}

impl Progress {
    fn new() -> Self {
        Self {
            ticker: Ticker::new(),
            planned_bytes: AtomicU64::new(0),
            written: AtomicU64::new(0),
            entries: AtomicU64::new(0),
        }
    }

    fn clear(&self) {
        progress::clear();
    }
}

impl RestoreObserver for Progress {
    fn event(&self, event: &RestoreEvent<'_>) {
        match event {
            RestoreEvent::Planned { bytes, .. } => {
                self.planned_bytes.store(*bytes, Ordering::Relaxed);
            }
            RestoreEvent::BytesWritten { bytes } => {
                self.written.fetch_add(*bytes, Ordering::Relaxed);
            }
            RestoreEvent::EntryRestored { .. } => {
                self.entries.fetch_add(1, Ordering::Relaxed);
            }
        }
        if self.ticker.due() {
            progress::draw(&format!(
                "{} of {} written, {} entries",
                human_size(self.written.load(Ordering::Relaxed)),
                human_size(self.planned_bytes.load(Ordering::Relaxed)),
                self.entries.load(Ordering::Relaxed)
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn the_policies_map_one_to_one() {
        assert_eq!(conflict_policy(ConflictArg::Fail), ConflictPolicy::Fail);
        assert_eq!(conflict_policy(ConflictArg::Skip), ConflictPolicy::Skip);
        assert_eq!(conflict_policy(ConflictArg::Overwrite), ConflictPolicy::Overwrite);
        assert_eq!(symlink_policy(SymlinkArg::Contained), RestoreSymlinks::Contained);
        assert_eq!(symlink_policy(SymlinkArg::Skip), RestoreSymlinks::Skip);
        assert_eq!(symlink_policy(SymlinkArg::AsRecorded), RestoreSymlinks::AsRecorded);
    }

    #[test]
    fn progress_counts_what_it_is_told() {
        let progress = Progress::new();
        progress.event(&RestoreEvent::Planned { files: 2, bytes: 100 });
        progress.event(&RestoreEvent::BytesWritten { bytes: 30 });
        progress.event(&RestoreEvent::BytesWritten { bytes: 12 });
        progress.event(&RestoreEvent::EntryRestored {
            path: Path::new("/out/a"),
        });
        assert_eq!(progress.planned_bytes.load(Ordering::Relaxed), 100);
        assert_eq!(progress.written.load(Ordering::Relaxed), 42);
        assert_eq!(progress.entries.load(Ordering::Relaxed), 1);
    }
}
