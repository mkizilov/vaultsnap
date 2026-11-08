//! The `prune` command: applies a retention policy and deletes what no kept snapshot needs.
//!
//! Deleting is irreversible, so this command never does it by surprise:
//!
//! * `--dry-run` shows the plan and changes nothing.
//! * Otherwise the plan is shown and a person must type `yes`, **or** `--yes` is given. When
//!   standard input is not a terminal there is nobody to ask, and the command refuses unless
//!   `--yes` was given: a script has to say so on purpose.
//! * What a person confirms is executed exactly: the confirmation carries the digest of the plan,
//!   and the prune fails, deleting nothing, if the snapshots changed in between. No lock is held
//!   while the question is open.

mod human;
#[cfg(test)]
mod tests;

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use vaultsnap_core::{
    CancelToken, PruneEvent, PruneMode, PruneObserver, PruneOptions, PrunePhase, PruneReport,
    Repository, RepositoryKeys, RetentionPolicy,
};

use crate::cli::PruneArgs;
use crate::commands::CliError;
use crate::inspect::output;
use crate::progress::{self, Ticker};
use crate::{interrupt, passphrase};

const INTERRUPTED: &str =
    "interrupted: stopping before the next deletion; nothing a kept snapshot needs was touched";

/// How the command gets permission to delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Permission {
    /// Delete nothing.
    DryRun,
    /// `--yes`: delete without asking.
    Granted,
    /// Show the plan and ask a person.
    Ask,
}

/// Decides how deleting is authorized. `--dry-run` wins over everything; without `--yes` a person
/// must be there to answer.
fn permission(dry_run: bool, yes: bool, interactive: bool) -> Result<Permission, CliError> {
    if dry_run {
        Ok(Permission::DryRun)
    } else if yes {
        Ok(Permission::Granted)
    } else if interactive {
        Ok(Permission::Ask)
    } else {
        Err(CliError::PruneNeedsConfirmation)
    }
}

/// Shows `question` and reads one line. Only the word `yes` is a yes: not `y`, not `YES`, not an
/// empty line, not the end of the input.
fn decide(input: &mut impl BufRead, out: &mut impl Write, question: &str) -> io::Result<bool> {
    write!(out, "{question}")?;
    out.flush()?;
    let mut line = String::new();
    let read = input.read_line(&mut line)?;
    Ok(read > 0 && line.trim() == "yes")
}

pub async fn run(
    repo: &Repository,
    password_file: Option<&Path>,
    args: PruneArgs,
) -> Result<(), CliError> {
    let policy = RetentionPolicy {
        keep_last: args.keep_last,
        keep_daily: args.keep_daily,
        keep_weekly: args.keep_weekly,
        keep_monthly: args.keep_monthly,
        keep_yearly: args.keep_yearly,
    };
    // Before anything else: a bad policy and a missing permission need neither a passphrase nor
    // the repository.
    policy.validate()?;
    let permission = permission(args.dry_run, args.yes, io::stdin().is_terminal())?;

    let keys = passphrase::unlock(repo, password_file).await?;
    let cancel = CancelToken::new();
    let mut listener = interrupt::cancel_on_interrupt(cancel.clone(), INTERRUPTED);
    let session = Session {
        repo,
        keys: &keys,
        policy: &policy,
        cancel: &cancel,
        progress: std::io::stderr()
            .is_terminal()
            .then(|| Arc::new(Progress::new())),
    };

    match permission {
        Permission::DryRun => {
            let plan = session.execute(PruneMode::DryRun).await?;
            output(|out| human::plan(out, &plan, &policy))?;
            println!("\nDry run: nothing was changed.");
            Ok(())
        }
        Permission::Granted => {
            let done = session.execute(PruneMode::Unattended).await?;
            output(|out| {
                human::plan(out, &done, &policy)?;
                human::outcome(out, &done)
            })?;
            finish(&done)
        }
        Permission::Ask => {
            let plan = session.execute(PruneMode::DryRun).await?;
            output(|out| human::plan(out, &plan, &policy))?;
            if plan.is_noop() {
                println!("\nNothing to remove.");
                return Ok(());
            }
            // Nothing is locked while a person decides, and Ctrl-C at the question is handled
            // by the question, not by the cancellation of a prune that is not running.
            listener.abort();
            if !ask(&plan).await? {
                return Err(CliError::PruneDeclined);
            }
            listener = interrupt::cancel_on_interrupt(cancel.clone(), INTERRUPTED);
            let done = session.execute(PruneMode::Confirmed(plan.digest)).await?;
            output(|out| human::outcome(out, &done))?;
            let result = finish(&done);
            listener.abort();
            result
        }
    }
}

/// What one run of the command works with.
struct Session<'a> {
    repo: &'a Repository,
    keys: &'a RepositoryKeys,
    policy: &'a RetentionPolicy,
    cancel: &'a CancelToken,
    progress: Option<Arc<Progress>>,
}

impl Session<'_> {
    /// Runs the prune in `mode` and erases the progress line whatever the result.
    async fn execute(&self, mode: PruneMode) -> Result<PruneReport, CliError> {
        let options = PruneOptions {
            policy: self.policy.clone(),
            mode,
            observer: self
                .progress
                .clone()
                .map(|progress| progress as Arc<dyn PruneObserver>),
            cancel: self.cancel.clone(),
        };
        let result = self.repo.prune(self.keys, options).await;
        if let Some(progress) = &self.progress {
            progress.clear();
        }
        Ok(result?)
    }
}

/// Lists what could not be deleted and turns it into the exit status.
fn finish(report: &PruneReport) -> Result<(), CliError> {
    if report.failure_count == 0 {
        return Ok(());
    }
    eprintln!("{} could not be deleted:", report.failure_count);
    for line in human::failures(report) {
        eprintln!("{line}");
    }
    Err(CliError::PruneIncomplete {
        failed: usize::try_from(report.failure_count).unwrap_or(usize::MAX),
    })
}

/// Asks a person on the terminal. Ctrl-C at the question ends the command at once: nothing is
/// locked and nothing has been changed.
async fn ask(plan: &PruneReport) -> Result<bool, CliError> {
    let question = human::question(plan);
    let reader = tokio::task::spawn_blocking(move || {
        let stdin = io::stdin();
        let mut input = stdin.lock();
        decide(&mut input, &mut io::stderr(), &question)
    });
    tokio::select! {
        joined = reader => match joined {
            Ok(answer) => answer.map_err(CliError::Output),
            Err(err) => Err(CliError::Output(io::Error::other(err.to_string()))),
        },
        () = interrupt::interrupted() => {
            eprintln!("\ninterrupted; nothing was changed");
            std::process::exit(130)
        }
    }
}

/// A one-line status on the terminal while objects are deleted.
struct Progress {
    ticker: Ticker,
    phase: AtomicUsize,
    total: AtomicU64,
    done: AtomicU64,
}

const PHASES: [(PrunePhase, &str); 3] = [
    (PrunePhase::Reading, "reading snapshots"),
    (PrunePhase::RemovingSnapshots, "removing snapshots"),
    (PrunePhase::RemovingChunks, "removing chunk objects"),
];

fn phase_index(phase: PrunePhase) -> usize {
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

impl PruneObserver for Progress {
    fn event(&self, event: &PruneEvent) {
        match event {
            PruneEvent::Started { phase, total } => {
                self.phase.store(phase_index(*phase), Ordering::Relaxed);
                self.total.store(*total, Ordering::Relaxed);
                self.done.store(0, Ordering::Relaxed);
            }
            PruneEvent::Step { .. } => {
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
        assert_eq!(progress.text(), "reading snapshots");
        progress.event(&PruneEvent::Started {
            phase: PrunePhase::RemovingChunks,
            total: 30,
        });
        assert_eq!(progress.text(), "removing chunk objects: 0 of 30");
        for _ in 0..4 {
            progress.event(&PruneEvent::Step { phase: PrunePhase::RemovingChunks });
        }
        assert_eq!(progress.text(), "removing chunk objects: 4 of 30");
        progress.event(&PruneEvent::Started {
            phase: PrunePhase::RemovingSnapshots,
            total: 0,
        });
        assert_eq!(progress.text(), "removing snapshots");
    }

    #[test]
    fn every_phase_has_a_label() {
        for phase in [
            PrunePhase::Reading,
            PrunePhase::RemovingSnapshots,
            PrunePhase::RemovingChunks,
        ] {
            assert_eq!(PHASES[phase_index(phase)].0, phase);
        }
    }
}
