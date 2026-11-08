//! The plan and the outcome of a prune as text for people. Everything printed here is a snapshot
//! id, a time, a count, a size, or a repository path: none of it comes from the backed-up data, and
//! the paths of objects that could not be deleted are ids, but they go through [`escape`] anyway.

use std::io::{self, Write};

use vaultsnap_core::{PruneReport, RetentionPolicy};

use crate::format::{escape, human_size, time_seconds};

fn plural(count: u64, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

fn field(out: &mut dyn Write, label: &str, value: &str) -> io::Result<()> {
    writeln!(out, "{label:<13}{value}")
}

/// The policy as `keep last 3, daily 7`, in the order of the rules.
pub fn policy_text(policy: &RetentionPolicy) -> String {
    let rules = [
        ("last", policy.keep_last),
        ("daily", policy.keep_daily),
        ("weekly", policy.keep_weekly),
        ("monthly", policy.keep_monthly),
        ("yearly", policy.keep_yearly),
    ];
    let parts: Vec<String> = rules
        .iter()
        .filter_map(|(name, count)| count.map(|count| format!("{name} {count}")))
        .collect();
    format!("keep {}", parts.join(", "))
}

/// What the plan decides: every snapshot, kept or removed, and what happens to the objects. The
/// same text is printed for a dry run, before asking, and before an unattended prune.
pub fn plan(out: &mut dyn Write, report: &PruneReport, policy: &RetentionPolicy) -> io::Result<()> {
    let total = report.decisions.len() as u64;
    let removed = report.snapshots_to_remove();
    field(out, "Policy", &policy_text(policy))?;
    field(
        out,
        "Snapshots",
        &format!("{total} ({} kept, {removed} to remove)", total - removed),
    )?;
    if total > 0 {
        writeln!(out)?;
    }
    for decision in &report.decisions {
        let (action, why) = if decision.is_kept() {
            let reasons: Vec<&str> = decision.keep.iter().map(|reason| reason.name()).collect();
            ("KEEP", reasons.join(", "))
        } else {
            ("REMOVE", String::new())
        };
        let line = format!(
            "  {action:<6}  {}  {}  {why}",
            decision.id.short(),
            time_seconds(decision.created)
        );
        writeln!(out, "{}", line.trim_end())?;
    }
    writeln!(out)?;
    field(
        out,
        "Chunks",
        &format!("{} in use by the kept snapshots", report.chunks_in_use),
    )?;
    field(
        out,
        "Unreferenced",
        &format!(
            "{} ({}) that no kept snapshot needs",
            plural(report.chunks_unreferenced, "object", "objects"),
            human_size(report.bytes_unreferenced)
        ),
    )?;
    Ok(())
}

/// What an executed prune did.
pub fn outcome(out: &mut dyn Write, report: &PruneReport) -> io::Result<()> {
    writeln!(out)?;
    field(
        out,
        "Removed",
        &format!(
            "{} and {} ({})",
            plural(report.snapshots_removed, "snapshot", "snapshots"),
            plural(report.chunks_removed, "chunk object", "chunk objects"),
            human_size(report.bytes_removed)
        ),
    )?;
    if report.failure_count > 0 {
        field(
            out,
            "Not removed",
            &format!(
                "{} (listed on standard error; harmless, and the next prune tries again)",
                plural(report.failure_count, "object", "objects")
            ),
        )?;
    }
    field(
        out,
        "Verified",
        &format!(
            "all {} used by the kept snapshots are present",
            plural(report.chunks_verified, "chunk", "chunks")
        ),
    )?;
    field(out, "Took", &format!("{:.1}s", report.duration.as_secs_f64()))
}

/// The question put to a person before anything is deleted.
pub fn question(report: &PruneReport) -> String {
    format!(
        "This permanently deletes {} and {} ({}) from the repository.\n\
         This cannot be undone. Type \"yes\" to continue: ",
        plural(report.snapshots_to_remove(), "snapshot", "snapshots"),
        plural(
            report.chunks_unreferenced,
            "unreferenced chunk object",
            "unreferenced chunk objects"
        ),
        human_size(report.bytes_unreferenced)
    )
}

/// The objects that could not be deleted, for standard error.
pub fn failures(report: &PruneReport) -> Vec<String> {
    let mut lines: Vec<String> = report
        .failures
        .iter()
        .map(|failure| {
            format!(
                "  {}: {}",
                escape(failure.subject.as_bytes()),
                escape(failure.reason.as_bytes())
            )
        })
        .collect();
    let unlisted = report.failure_count.saturating_sub(report.failures.len() as u64);
    if unlisted > 0 {
        lines.push(format!("  and {unlisted} more"));
    }
    lines
}
