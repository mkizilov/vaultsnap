//! The report as one JSON document on one line. The field names, their order, and the spellings of
//! `mode`, `severity`, and `kind` are a contract documented in `docs/CHECK.md`: they are written
//! as explicit structs below and covered by tests, and are only extended by adding fields at the
//! end of an object.
//!
//! * Sizes are byte counts; there is no time in the document, so the same repository state gives
//!   the same bytes.
//! * `counts` lists every kind, in the listing order, including those that did not occur.
//! * Strings are exact: a stray file whose name is not valid UTF-8 appears with replacement
//!   characters, and control characters are ordinary JSON escapes.

use std::io::{self, Write};

use serde::Serialize;
use vaultsnap_core::{CheckReport, CheckStats, FindingKind};

#[derive(Serialize)]
struct StatsView {
    key_files: u64,
    snapshots: u64,
    snapshots_damaged: u64,
    objects: u64,
    object_bytes: u64,
    chunks_referenced: u64,
    chunks_missing: u64,
    objects_checked: u64,
    objects_damaged: u64,
    bytes_authenticated: u64,
    orphans: u64,
    orphan_bytes: u64,
    orphans_exact: bool,
}

impl From<&CheckStats> for StatsView {
    fn from(stats: &CheckStats) -> Self {
        Self {
            key_files: stats.key_files,
            snapshots: stats.snapshots,
            snapshots_damaged: stats.snapshots_damaged,
            objects: stats.objects,
            object_bytes: stats.object_bytes,
            chunks_referenced: stats.chunks_referenced,
            chunks_missing: stats.chunks_missing,
            objects_checked: stats.objects_checked,
            objects_damaged: stats.objects_damaged,
            bytes_authenticated: stats.bytes_authenticated,
            orphans: stats.orphans,
            orphan_bytes: stats.orphan_bytes,
            orphans_exact: stats.orphans_exact,
        }
    }
}

/// How many findings of each kind occurred: every kind is present, in the listing order.
#[derive(Serialize)]
struct CountsView {
    missing_directory: u64,
    snapshot_damaged: u64,
    unsupported_version: u64,
    chunk_missing: u64,
    chunk_corrupt: u64,
    chunk_length_mismatch: u64,
    io_error: u64,
    orphan_object: u64,
    unexpected_file: u64,
    leftover_temp_file: u64,
    lock_present: u64,
    key_file_invalid: u64,
}

impl CountsView {
    fn of(report: &CheckReport) -> Self {
        let count = |kind| report.count(kind);
        Self {
            missing_directory: count(FindingKind::MissingDirectory),
            snapshot_damaged: count(FindingKind::SnapshotDamaged),
            unsupported_version: count(FindingKind::UnsupportedVersion),
            chunk_missing: count(FindingKind::ChunkMissing),
            chunk_corrupt: count(FindingKind::ChunkCorrupt),
            chunk_length_mismatch: count(FindingKind::ChunkLengthMismatch),
            io_error: count(FindingKind::IoError),
            orphan_object: count(FindingKind::OrphanObject),
            unexpected_file: count(FindingKind::UnexpectedFile),
            leftover_temp_file: count(FindingKind::LeftoverTempFile),
            lock_present: count(FindingKind::LockPresent),
            key_file_invalid: count(FindingKind::KeyFileInvalid),
        }
    }
}

#[derive(Serialize)]
struct AffectedView<'a> {
    id: &'a str,
    short_id: &'a str,
    files: u64,
    damaged_chunks: u64,
}

#[derive(Serialize)]
struct FindingView<'a> {
    severity: &'static str,
    kind: &'static str,
    subject: &'a str,
    detail: &'a str,
}

#[derive(Serialize)]
struct ReportView<'a> {
    repository_id: &'a str,
    format_version: u32,
    mode: &'static str,
    ok: bool,
    errors: u64,
    warnings: u64,
    suppressed: u64,
    counts: CountsView,
    stats: StatsView,
    affected_snapshots: Vec<AffectedView<'a>>,
    findings: Vec<FindingView<'a>>,
}

/// `check --json`.
pub fn report(out: &mut dyn Write, report: &CheckReport) -> io::Result<()> {
    let view = ReportView {
        repository_id: &report.repository_id,
        format_version: report.format_version,
        mode: report.mode.name(),
        ok: report.is_ok(),
        errors: report.errors,
        warnings: report.warnings,
        suppressed: report.suppressed,
        counts: CountsView::of(report),
        stats: StatsView::from(&report.stats),
        affected_snapshots: report
            .affected
            .iter()
            .map(|affected| AffectedView {
                id: affected.snapshot.as_str(),
                short_id: affected.snapshot.short(),
                files: affected.files,
                damaged_chunks: affected.damaged_chunks,
            })
            .collect(),
        findings: report
            .findings
            .iter()
            .map(|finding| FindingView {
                severity: finding.severity().name(),
                kind: finding.kind.name(),
                subject: &finding.subject,
                detail: &finding.detail,
            })
            .collect(),
    };
    serde_json::to_writer(&mut *out, &view).map_err(io::Error::from)?;
    out.write_all(b"\n")
}
