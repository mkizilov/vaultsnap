//! The `backup` command: unlocks the repository, runs the backup with a progress line and
//! interrupt handling, and prints what was done.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use vaultsnap_core::{
    BackupEvent, BackupObserver, BackupOptions, BackupReport, CancelToken, ParentPolicy,
    Repository, ScanOptions,
};

use crate::cli::BackupArgs;
use crate::commands::CliError;
use crate::passphrase;

/// The progress line is redrawn at most this often.
const DRAW_INTERVAL_MS: u64 = 200;

pub async fn run(
    repo: &Repository,
    password_file: Option<&Path>,
    args: BackupArgs,
) -> Result<(), CliError> {
    let passphrase = passphrase::read_existing(password_file)?;
    let keys = repo.unlock(&passphrase).await?;
    drop(passphrase);

    let cancel = CancelToken::new();
    cancel_on_interrupt(cancel.clone());

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

/// Asks the running backup to stop on Ctrl-C or SIGTERM. Nothing is committed in that case, and
/// the repository stays valid; a second signal changes nothing, so the backup always ends cleanly
/// (use SIGKILL to force it, which can leave a stale lock file behind).
fn cancel_on_interrupt(cancel: CancelToken) {
    tokio::spawn(async move {
        wait_for_interrupt().await;
        eprintln!("\ninterrupted: stopping; no snapshot will be created");
        cancel.cancel();
    });
}

/// Completes when Ctrl-C is pressed, and never if the signal cannot be observed.
async fn ctrl_c() {
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
}

#[cfg(unix)]
async fn wait_for_interrupt() {
    use tokio::signal::unix::{signal, SignalKind};

    match signal(SignalKind::terminate()) {
        Ok(mut terminate) => {
            tokio::select! {
                () = ctrl_c() => {}
                _ = terminate.recv() => {}
            }
        }
        Err(_) => ctrl_c().await,
    }
}

#[cfg(not(unix))]
async fn wait_for_interrupt() {
    ctrl_c().await;
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
        human(totals.bytes),
        human(transfer.bytes_read),
        human(transfer.bytes_unchanged)
    );
    println!(
        "  chunks       {} new ({} stored), {} already stored",
        transfer.chunks_new,
        human(transfer.bytes_stored),
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

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// A one-line status on the terminal, redrawn while the backup runs. Events arrive from several
/// threads, so everything is atomic and drawing is rate limited.
struct Progress {
    started: Instant,
    last_draw_ms: AtomicU64,
    files: AtomicU64,
    unchanged: AtomicU64,
    bytes: AtomicU64,
}

impl Progress {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            last_draw_ms: AtomicU64::new(0),
            files: AtomicU64::new(0),
            unchanged: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        }
    }

    fn draw(&self) {
        let now = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let last = self.last_draw_ms.load(Ordering::Relaxed);
        if now.saturating_sub(last) < DRAW_INTERVAL_MS
            || self
                .last_draw_ms
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_err()
        {
            return;
        }
        let mut stderr = std::io::stderr().lock();
        // A terminal that went away must not fail the backup, so write errors are ignored.
        write!(
            stderr,
            "\r\x1b[K{} files, {} unchanged, {} read",
            self.files.load(Ordering::Relaxed),
            self.unchanged.load(Ordering::Relaxed),
            human(self.bytes.load(Ordering::Relaxed))
        )
        .and_then(|()| stderr.flush())
        .ok();
    }

    fn clear(&self) {
        let mut stderr = std::io::stderr().lock();
        write!(stderr, "\r\x1b[K").and_then(|()| stderr.flush()).ok();
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
    fn sizes_are_shown_in_binary_units() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(1023), "1023 B");
        assert_eq!(human(1024), "1.0 KiB");
        assert_eq!(human(1536), "1.5 KiB");
        assert_eq!(human(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(human(3 * 1024 * 1024 * 1024), "3.0 GiB");
        assert_eq!(human(u64::MAX), "16777216.0 TiB");
    }

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
