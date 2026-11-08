//! The report as text for people. The repository-relative names in findings can include names of
//! stray files, which anyone with write access to the repository chose, so everything printed goes
//! through [`escape`]: no terminal control sequences, one record per line.

use std::io::{self, Write};

use vaultsnap_core::{CheckMode, CheckReport, FindingKind, Severity};

use crate::format::{escape, human_size};

fn plural(count: u64, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

fn field(out: &mut dyn Write, label: &str, value: &str) -> io::Result<()> {
    writeln!(out, "{label:<13}{value}")
}

fn findings(
    out: &mut dyn Write,
    report: &CheckReport,
    heading: &str,
    severity: Severity,
    total: u64,
) -> io::Result<()> {
    if total == 0 {
        return Ok(());
    }
    writeln!(out)?;
    writeln!(out, "{heading} ({total})")?;
    for finding in report.findings.iter().filter(|f| f.severity() == severity) {
        writeln!(out, "  {}  {}", finding.kind, escape(finding.subject.as_bytes()))?;
        writeln!(out, "      {}", escape(finding.detail.as_bytes()))?;
    }
    Ok(())
}

/// `check`: what was examined, what was found, which snapshots are affected, what to do, and
/// whether the repository passed.
pub fn report(out: &mut dyn Write, report: &CheckReport) -> io::Result<()> {
    let stats = &report.stats;
    writeln!(
        out,
        "Repository {} (format version {})",
        report.repository_id, report.format_version
    )?;
    let mode = match report.mode {
        CheckMode::Quick => {
            "quick: snapshots authenticated, chunk objects examined by structure only"
        }
        CheckMode::Full => {
            "full: snapshots authenticated, every chunk object read, authenticated, and decrypted"
        }
    };
    field(out, "Mode", mode)?;

    writeln!(out)?;
    field(
        out,
        "Snapshots",
        &format!("{} readable, {} damaged", stats.snapshots, stats.snapshots_damaged),
    )?;
    field(
        out,
        "Chunks",
        &format!(
            "{} referenced by snapshots, {} missing",
            stats.chunks_referenced, stats.chunks_missing
        ),
    )?;
    field(
        out,
        "Objects",
        &format!(
            "{} in data/ ({}), {} damaged",
            stats.objects,
            human_size(stats.object_bytes),
            stats.objects_damaged
        ),
    )?;
    let caveat = if stats.orphans_exact {
        ""
    } else {
        "; may include objects of snapshots that could not be read"
    };
    field(
        out,
        "Unreferenced",
        &format!(
            "{} ({}){caveat}",
            plural(stats.orphans, "object", "objects"),
            human_size(stats.orphan_bytes)
        ),
    )?;
    field(out, "Key files", &stats.key_files.to_string())?;
    if report.mode == CheckMode::Full {
        field(
            out,
            "Verified",
            &format!("{} read and authenticated", human_size(stats.bytes_authenticated)),
        )?;
    }
    field(out, "Took", &format!("{:.1}s", report.duration.as_secs_f64()))?;

    findings(out, report, "ERRORS", Severity::Error, report.errors)?;
    findings(out, report, "WARNINGS", Severity::Warning, report.warnings)?;
    if report.suppressed > 0 {
        writeln!(
            out,
            "  and {} more not listed (--max-findings sets the limit, 0 lists all)",
            report.suppressed
        )?;
    }

    if !report.affected.is_empty() {
        writeln!(out)?;
        writeln!(out, "AFFECTED SNAPSHOTS (cannot be restored completely)")?;
        for affected in &report.affected {
            let chunks = plural(
                affected.damaged_chunks,
                "damaged or missing chunk",
                "damaged or missing chunks",
            );
            writeln!(
                out,
                "  {}  {} affected ({chunks})",
                affected.snapshot.short(),
                plural(affected.files, "file", "files"),
            )?;
        }
    }

    let present: Vec<FindingKind> = FindingKind::ALL
        .into_iter()
        .filter(|kind| report.count(*kind) > 0)
        .collect();
    if !present.is_empty() {
        writeln!(out)?;
        writeln!(out, "WHAT TO DO (the check changed nothing; see docs/CHECK.md)")?;
        for kind in present {
            writeln!(out, "  {kind}: {}", kind.advice())?;
        }
    }

    writeln!(out)?;
    if report.errors == 0 {
        if report.warnings == 0 {
            writeln!(out, "Result: no problems found.")?;
        } else {
            writeln!(
                out,
                "Result: no errors found, {}.",
                plural(report.warnings, "warning", "warnings")
            )?;
        }
    } else {
        writeln!(
            out,
            "Result: {} found, {}. The repository was not changed.",
            plural(report.errors, "error", "errors"),
            plural(report.warnings, "warning", "warnings")
        )?;
    }
    if report.mode == CheckMode::Quick {
        writeln!(
            out,
            "Quick mode does not read chunk contents: damage inside the encrypted data is found \
             only with --read-data."
        )?;
    }
    Ok(())
}
