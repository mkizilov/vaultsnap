//! The `check` command: unlocks the repository, runs the check with a progress line and interrupt
//! handling, and prints the report as text for people ([`human`]) or as one JSON document
//! ([`json`]). The check only reads; nothing it finds is repaired or removed.

mod human;
mod json;
#[cfg(test)]
mod tests;

use std::io::IsTerminal;
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use vaultsnap_core::{
    CancelToken, CheckEvent, CheckObserver, CheckOptions, CheckPhase, Repository,
};

use crate::cli::CheckArgs;
use crate::commands::CliError;
use crate::inspect::output;
use crate::progress::{self, Ticker};
use crate::{interrupt, passphrase};

pub async fn run(
    repo: &Repository,
    password_file: Option<&Path>,
    args: CheckArgs,
) -> Result<(), CliError> {
    let keys = passphrase::unlock_for_check(repo, password_file).await?;

    let cancel = CancelToken::new();
    interrupt::cancel_on_interrupt(
        cancel.clone(),
        "interrupted: stopping; there will be no report, and nothing was changed",
    );

    let progress = std::io::stderr()
        .is_terminal()
        .then(|| Arc::new(Progress::new()));
    let options = CheckOptions {
        read_data: args.read_data,
        jobs: args.jobs.unwrap_or(0),
        max_findings: args.max_findings,
        observer: progress
            .clone()
            .map(|progress| progress as Arc<dyn CheckObserver>),
        cancel,
    };

    let result = repo.check(&keys, options).await;
    if let Some(progress) = progress {
        progress.clear();
    }
    let report = result?;

    output(|out| {
        if args.json {
            json::report(out, &report)
        } else {
            human::report(out, &report)
        }
    })?;
    match report.errors {
        0 => Ok(()),
        errors => Err(CliError::CheckFailed(usize::try_from(errors).unwrap_or(usize::MAX))),
    }
}

/// A one-line status on the terminal: which step, and how far. Events arrive from several
/// threads, so everything is atomic and drawing is rate limited.
struct Progress {
    ticker: Ticker,
    phase: AtomicUsize,
    total: AtomicU64,
    done: AtomicU64,
}

const PHASES: [(CheckPhase, &str); 3] = [
    (CheckPhase::Structure, "walking the repository"),
    (CheckPhase::Snapshots, "authenticating snapshots"),
    (CheckPhase::Objects, "examining chunk objects"),
];

fn phase_index(phase: CheckPhase) -> usize {
    PHASES
        .iter()
        .position(|(candidate, _)| *candidate == phase)
        .unwrap_or(0)
}

impl Progress {
    fn new() -> Self {
        Self {
            ticker: Ticker::new(),
            phase: AtomicUsize::new(0),
            total: AtomicU64::new(0),
            done: AtomicU64::new(0),
        }
    }

    fn text(&self) -> String {
        let label = PHASES[self.phase.load(Ordering::Relaxed)].1;
        let done = self.done.load(Ordering::Relaxed);
        let total = self.total.load(Ordering::Relaxed);
        if total == 0 {
            label.to_owned()
        } else {
            format!("{label}: {done} of {total}")
        }
    }

    fn clear(&self) {
        progress::clear();
    }
}

impl CheckObserver for Progress {
    fn event(&self, event: &CheckEvent) {
        match event {
            CheckEvent::Started { phase, total } => {
                self.phase.store(phase_index(*phase), Ordering::Relaxed);
                self.total.store(*total, Ordering::Relaxed);
                self.done.store(0, Ordering::Relaxed);
            }
            CheckEvent::Step { .. } => {
                self.done.fetch_add(1, Ordering::Relaxed);
            }
        }
        if self.ticker.due() {
            progress::draw(&self.text());
        }
    }
}

#[cfg(test)]
mod progress_tests {
    use super::*;

    #[test]
    fn the_status_line_names_the_step_and_how_far_it_is() {
        let progress = Progress::new();
        assert_eq!(progress.text(), "walking the repository");
        progress.event(&CheckEvent::Started { phase: CheckPhase::Objects, total: 40 });
        assert_eq!(progress.text(), "examining chunk objects: 0 of 40");
        for _ in 0..3 {
            progress.event(&CheckEvent::Step { phase: CheckPhase::Objects });
        }
        assert_eq!(progress.text(), "examining chunk objects: 3 of 40");
        progress.event(&CheckEvent::Started { phase: CheckPhase::Snapshots, total: 2 });
        assert_eq!(progress.text(), "authenticating snapshots: 0 of 2");
        progress.event(&CheckEvent::Started { phase: CheckPhase::Structure, total: 0 });
        assert_eq!(progress.text(), "walking the repository");
    }

    #[test]
    fn every_phase_has_a_label() {
        for phase in [CheckPhase::Structure, CheckPhase::Snapshots, CheckPhase::Objects] {
            assert_eq!(PHASES[phase_index(phase)].0, phase);
        }
    }
}
