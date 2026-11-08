//! The `backup` command: unlocks the repository, runs the backup with a progress line and
//! interrupt handling, and prints what was done.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use vaultsnap_core::{
    BackupEvent, BackupObserver, BackupOptions, BackupReport, CancelToken, ParentPolicy,
    Repository, ScanOptions,
};

use crate::cli::BackupArgs;
use crate::commands::CliError;
use crate::format::human_size;
use crate::progress::{self, Ticker};
use crate::{interrupt, passphrase};

pub async fn run(
    repo: &Repository,
    password_file: Option<&Path>,
    args: BackupArgs,
) -> Result<(), CliError> {
    let keys = passphrase::unlock(repo, password_file).await?;

    let cancel = CancelToken::new();
    interrupt::cancel_on_interrupt(
        cancel.clone(),
        "interrupted: stopping; no snapshot will be created",
    );

    let progress = std::io::stderr()
        .is_terminal()
        .then(|| Arc::new(Progress::new()));
    let options = BackupOptions {
        sources: args.paths,
        scan: ScanOptions {
            excludes: args.excludes,
            ..ScanOptions::default()
        },
        tags: args.tags,
        host: args.host.unwrap_or_default(),
        jobs: args.jobs.unwrap_or(0),
        parent: if args.rescan {
            ParentPolicy::Disabled
        } else {
            ParentPolicy::Latest
        },
        observer: progress
            .clone()
            .map(|progress| progress as Arc<dyn BackupObserver>),
        cancel,
        ..BackupOptions::default()
    };

    let result = repo.backup(&keys, options).await;
    if let Some(progress) = progress {
        progress.clear();
    }
    let report = result?;

    print_report(&report);
    match warn_about_skipped(&report) {
        0 => Ok(()),
        failed => Err(CliError::BackupIncomplete { failed }),
    }
}

fn print_report(report: &BackupReport) {
    let totals = &report.totals;
    let transfer = &report.transfer;
    println!("created snapshot {}", report.snapshot);
    for (source, parent) in report.sources.iter().zip(&report.parents) {
        match parent {
            Some(parent) => println!(
                "  source       {} (compared with {})",
                source.display(),
                parent.short()
            ),
            None => println!("  source       {} (no earlier snapshot)", source.display()),
        }
    }
    println!(
        "  entries      {} directories, {} symbolic links, {} hard links, {} special",
        totals.directories, totals.symlinks, totals.hard_links, totals.special_files
    );
    println!(
        "  files        {} total, {} read, {} unchanged",
        totals.files, transfer.files_read, transfer.files_unchanged
    );
    println!(
        "  size         {} total, {} read, {} unchanged",
        human_size(totals.bytes),
        human_size(transfer.bytes_read),
        human_size(transfer.bytes_unchanged)
    );
    println!(
        "  chunks       {} new ({} stored), {} already stored",
        transfer.chunks_new,
        human_size(transfer.bytes_stored),
        transfer.chunks_existing
    );
    let by_policy = report.skipped.iter().filter(|s| !s.failure).count();
    if by_policy > 0 {
        println!("  not included {by_policy} path(s) by policy or because they disappeared");
    }
    println!("  duration     {:.1}s", report.duration.as_secs_f64());
}

/// Lists the paths that should have been backed up but were not, and returns how many there
/// were. Paths left out by policy or because they vanished are only logged.
fn warn_about_skipped(report: &BackupReport) -> usize {
    let location = |source: usize, path: &str| -> PathBuf {
        report
            .sources
            .get(source)
            .map_or_else(|| PathBuf::from(path), |root| root.join(path))
    };
    let mut failed = 0;
    for skipped in &report.skipped {
        let path = location(skipped.source, &skipped.path);
        if skipped.failure {
            if failed == 0 {
                eprintln!("warning: these paths could not be backed up:");
            }
            failed += 1;
            eprintln!("  {}: {}", path.display(), skipped.reason);
        } else {
            tracing::info!(path = %path.display(), reason = %skipped.reason, "not included");
        }
    }
    failed
}

/// A one-line status on the terminal, redrawn while the backup runs. Events arrive from several
/// threads, so everything is atomic and drawing is rate limited.
struct Progress {
    ticker: Ticker,
    files: AtomicU64,
    unchanged: AtomicU64,
    bytes: AtomicU64,
}

impl Progress {
    fn new() -> Self {
        Self {
            ticker: Ticker::new(),
            files: AtomicU64::new(0),
            unchanged: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        }
    }

    fn draw(&self) {
        if self.ticker.due() {
            progress::draw(&format!(
                "{} files, {} unchanged, {} read",
                self.files.load(Ordering::Relaxed),
                self.unchanged.load(Ordering::Relaxed),
                human_size(self.bytes.load(Ordering::Relaxed))
            ));
        }
    }

    fn clear(&self) {
        progress::clear();
    }
}

impl BackupObserver for Progress {
    fn event(&self, event: &BackupEvent<'_>) {
        match event {
            BackupEvent::BytesRead { bytes } => {
                self.bytes.fetch_add(*bytes, Ordering::Relaxed);
            }
            BackupEvent::FileFinished { result, .. } => {
                self.files.fetch_add(1, Ordering::Relaxed);
                if result.unchanged {
                    self.unchanged.fetch_add(1, Ordering::Relaxed);
                }
            }
            _ => return,
        }
        self.draw();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_counts_events_and_ignores_the_rest() {
        let progress = Progress::new();
        progress.event(&BackupEvent::BytesRead { bytes: 10 });
        progress.event(&BackupEvent::BytesRead { bytes: 5 });
        progress.event(&BackupEvent::Committing);
        assert_eq!(progress.bytes.load(Ordering::Relaxed), 15);
        assert_eq!(progress.files.load(Ordering::Relaxed), 0);
    }
}
