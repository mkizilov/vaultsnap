//! JSON for programs. Each command prints exactly one JSON document on one line, followed by a
//! newline. The field names, their order, and the spellings of the fixed values (`type`, `status`,
//! `differences`, policy names) are a contract documented in `docs/CLI.md`: they are written as
//! explicit structs below and covered by tests, and are only ever extended by adding fields at the
//! end of an object.
//!
//! * Times are UTC, RFC 3339, with nine fractional digits (`2025-06-15T15:06:40.000000005Z`).
//! * Sizes are plain byte counts. (JSON numbers above 2^53 lose precision in some parsers; no
//!   realistic size is that large.)
//! * Paths are JSON strings. A path that is not valid UTF-8 is written with replacement
//!   characters, and a sibling `…_hex` field then holds its exact bytes; the field is absent
//!   otherwise.
//! * Unknown values are `null`, never omitted (except the `…_hex` fields).

use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::Path;

use serde::{Serialize, Serializer};
use vaultsnap_core::history::{
    self, skipped_path, unreadable_paths, Change, Diff, ListedEntry, SnapshotDetails, SnapshotList,
    SnapshotSizes,
};
use vaultsnap_core::scan::{
    ChangePolicy, ErrorPolicy, HardLinkPolicy, SpecialFilePolicy, SymlinkPolicy,
};
use vaultsnap_core::snapshot::{Manifest, ManifestStats, NodeKind, SkippedReason};

use crate::format::time_nanos;

fn write_document<T: Serialize>(out: &mut dyn Write, document: &T) -> io::Result<()> {
    serde_json::to_writer(&mut *out, document).map_err(io::Error::from)?;
    out.write_all(b"\n")
}

/// A path as JSON text, and its exact bytes in hex if the text had to be altered.
fn json_text(bytes: &[u8]) -> (String, Option<String>) {
    match std::str::from_utf8(bytes) {
        Ok(valid) => (valid.to_owned(), None),
        Err(_) => {
            let mut hex = String::with_capacity(bytes.len() * 2);
            for byte in bytes {
                let _ = write!(hex, "{byte:02x}");
            }
            (String::from_utf8_lossy(bytes).into_owned(), Some(hex))
        }
    }
}

#[derive(Serialize)]
struct SizesView {
    logical_bytes: u64,
    chunks: u64,
    deduplicated_bytes: u64,
    stored_bytes: u64,
    unique_chunks: u64,
    unique_stored_bytes: u64,
    missing_chunks: u64,
}

impl From<&SnapshotSizes> for SizesView {
    fn from(sizes: &SnapshotSizes) -> Self {
        Self {
            logical_bytes: sizes.logical_bytes,
            chunks: sizes.chunks,
            deduplicated_bytes: sizes.deduplicated_bytes,
            stored_bytes: sizes.stored_bytes,
            unique_chunks: sizes.unique_chunks,
            unique_stored_bytes: sizes.unique_stored_bytes,
            missing_chunks: sizes.missing_chunks,
        }
    }
}

#[derive(Serialize)]
struct SourceView {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    path_hex: Option<String>,
}

impl SourceView {
    fn new(bytes: &[u8]) -> Self {
        let (path, path_hex) = json_text(bytes);
        Self { path, path_hex }
    }
}

// ---------------------------------------------------------------------------------------------
// snapshots
// ---------------------------------------------------------------------------------------------

#[derive(Serialize)]
struct SnapshotView<'a> {
    id: &'a str,
    short_id: &'a str,
    created: String,
    host: &'a str,
    tags: &'a [String],
    sources: Vec<SourceView>,
    complete: bool,
    unreadable_paths: u64,
    skipped_paths: u64,
    files: u64,
    directories: u64,
    symlinks: u64,
    hard_links: u64,
    special_files: u64,
    sizes: SizesView,
}

#[derive(Serialize)]
struct DamagedView<'a> {
    id: &'a str,
    error: &'a str,
}

#[derive(Serialize)]
struct ListView<'a> {
    snapshots: Vec<SnapshotView<'a>>,
    damaged: Vec<DamagedView<'a>>,
    referenced_chunks: u64,
    referenced_stored_bytes: u64,
    unique_sizes_exact: bool,
}

/// `snapshots --json`.
pub fn list(out: &mut dyn Write, list: &SnapshotList) -> io::Result<()> {
    let snapshots = list
        .snapshots
        .iter()
        .map(|info| {
            let summary = &info.summary;
            SnapshotView {
                id: summary.id.as_str(),
                short_id: summary.id.short(),
                created: time_nanos(summary.created),
                host: &summary.host,
                tags: &summary.tags,
                sources: summary
                    .roots
                    .iter()
                    .map(|root| SourceView::new(root))
                    .collect(),
                complete: info.unreadable == 0,
                unreadable_paths: info.unreadable,
                skipped_paths: summary.stats.skipped,
                files: summary.stats.files,
                directories: summary.stats.directories,
                symlinks: summary.stats.symlinks,
                hard_links: summary.stats.hard_links,
                special_files: summary.stats.special_files,
                sizes: SizesView::from(&info.sizes),
            }
        })
        .collect();
    let damaged = list
        .damaged
        .iter()
        .map(|snapshot| DamagedView {
            id: snapshot.id.as_str(),
            error: &snapshot.error,
        })
        .collect();
    write_document(
        out,
        &ListView {
            snapshots,
            damaged,
            referenced_chunks: list.referenced_chunks,
            referenced_stored_bytes: list.referenced_stored_bytes,
            unique_sizes_exact: list.unique_sizes_exact(),
        },
    )
}

// ---------------------------------------------------------------------------------------------
// show
// ---------------------------------------------------------------------------------------------

#[derive(Serialize)]
struct SourceStatsView {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    path_hex: Option<String>,
    files: u64,
    directories: u64,
    symlinks: u64,
    hard_links: u64,
    special_files: u64,
    bytes: u64,
    skipped: u64,
}

#[derive(Serialize)]
struct TotalsView {
    files: u64,
    directories: u64,
    symlinks: u64,
    hard_links: u64,
    special_files: u64,
    logical_bytes: u64,
}

impl From<&ManifestStats> for TotalsView {
    fn from(stats: &ManifestStats) -> Self {
        Self {
            files: stats.files,
            directories: stats.directories,
            symlinks: stats.symlinks,
            hard_links: stats.hard_links,
            special_files: stats.special_files,
            logical_bytes: stats.bytes,
        }
    }
}

#[derive(Serialize)]
struct ConfigView<'a> {
    chunk_min_bytes: usize,
    chunk_avg_bytes: usize,
    chunk_max_bytes: usize,
    symlinks: &'static str,
    hard_links: &'static str,
    on_error: &'static str,
    special_files: &'static str,
    changed_files: &'static str,
    cross_filesystems: bool,
    includes: &'a [String],
    excludes: &'a [String],
}

#[derive(Serialize)]
struct SkippedView {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    path_hex: Option<String>,
    source: u32,
    reason: &'static str,
    detail: Option<String>,
}

#[derive(Serialize)]
struct ShowView<'a> {
    id: &'a str,
    short_id: &'a str,
    created: String,
    host: &'a str,
    tags: &'a [String],
    program: &'a str,
    manifest_version: u32,
    complete: bool,
    unreadable_paths: u64,
    sources: Vec<SourceStatsView>,
    totals: TotalsView,
    sizes: SizesView,
    unique_sizes_exact: bool,
    config: ConfigView<'a>,
    skipped: Vec<SkippedView>,
}

fn symlink_policy(policy: SymlinkPolicy) -> &'static str {
    match policy {
        SymlinkPolicy::Preserve => "preserve",
        SymlinkPolicy::FollowInsideRoot => "follow_inside_root",
        SymlinkPolicy::FollowAll => "follow_all",
    }
}

fn hard_link_policy(policy: HardLinkPolicy) -> &'static str {
    match policy {
        HardLinkPolicy::Preserve => "preserve",
        HardLinkPolicy::Duplicate => "duplicate",
    }
}

fn error_policy(policy: ErrorPolicy) -> &'static str {
    match policy {
        ErrorPolicy::Skip => "skip",
        ErrorPolicy::Fail => "fail",
    }
}

fn special_file_policy(policy: SpecialFilePolicy) -> &'static str {
    match policy {
        SpecialFilePolicy::Record => "record",
        SpecialFilePolicy::Skip => "skip",
    }
}

fn change_policy(policy: ChangePolicy) -> &'static str {
    match policy {
        ChangePolicy::Fail => "fail",
        ChangePolicy::Accept => "accept",
    }
}

fn skipped_view(manifest: &Manifest) -> Vec<SkippedView> {
    manifest
        .skipped
        .iter()
        .map(|record| {
            let (path, path_hex) = json_text(&skipped_path(manifest, record));
            let (reason, detail) = match &record.reason {
                SkippedReason::Vanished => ("vanished", None),
                SkippedReason::Unreadable { detail } => ("unreadable", Some(detail.clone())),
                SkippedReason::OtherFilesystem => ("other_filesystem", None),
                SkippedReason::SpecialFile { kind } => ("special_file", Some(kind.clone())),
            };
            SkippedView {
                path,
                path_hex,
                source: record.root,
                reason,
                detail,
            }
        })
        .collect()
}

/// `show --json`.
pub fn show(out: &mut dyn Write, details: &SnapshotDetails) -> io::Result<()> {
    let manifest = &details.manifest;
    let config = &manifest.config;
    let unreadable = unreadable_paths(manifest);
    let sources = manifest
        .roots
        .iter()
        .zip(&details.roots)
        .map(|(root, stats)| {
            let (path, path_hex) = json_text(&root.source);
            SourceStatsView {
                path,
                path_hex,
                files: stats.files,
                directories: stats.directories,
                symlinks: stats.symlinks,
                hard_links: stats.hard_links,
                special_files: stats.special_files,
                bytes: stats.bytes,
                skipped: stats.skipped,
            }
        })
        .collect();
    write_document(
        out,
        &ShowView {
            id: manifest.id.as_str(),
            short_id: manifest.id.short(),
            created: time_nanos(manifest.created),
            host: &manifest.host,
            tags: &manifest.tags,
            program: &manifest.program,
            manifest_version: manifest.format_version,
            complete: unreadable == 0,
            unreadable_paths: unreadable,
            sources,
            totals: TotalsView::from(&manifest.stats),
            sizes: SizesView::from(&details.sizes),
            unique_sizes_exact: details.unique_sizes_exact,
            config: ConfigView {
                chunk_min_bytes: config.chunker.min_size(),
                chunk_avg_bytes: config.chunker.avg_size(),
                chunk_max_bytes: config.chunker.max_size(),
                symlinks: symlink_policy(config.symlinks),
                hard_links: hard_link_policy(config.hard_links),
                on_error: error_policy(config.on_error),
                special_files: special_file_policy(config.special_files),
                changed_files: change_policy(config.changed_files),
                cross_filesystems: config.cross_filesystems,
                includes: &config.includes,
                excludes: &config.excludes,
            },
            skipped: skipped_view(manifest),
        },
    )
}

// ---------------------------------------------------------------------------------------------
// ls
// ---------------------------------------------------------------------------------------------

#[derive(Serialize)]
struct EntryView {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    path_hex: Option<String>,
    #[serde(rename = "type")]
    kind: &'static str,
    size: Option<u64>,
    mode: Option<String>,
    uid: Option<u32>,
    gid: Option<u32>,
    modified: Option<String>,
    target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_hex: Option<String>,
    link_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    link_to_hex: Option<String>,
    chunks: Option<usize>,
}

impl EntryView {
    fn new(listed: &ListedEntry<'_>) -> Self {
        let attributes = &listed.entry.attributes;
        let (path, path_hex) = json_text(&listed.path);
        let (target, target_hex) = match &listed.entry.kind {
            NodeKind::Symlink { target } => {
                let (value, hex) = json_text(target);
                (Some(value), hex)
            }
            _ => (None, None),
        };
        let (link_to, link_to_hex) = match &listed.link_to {
            Some(linked) => {
                let (value, hex) = json_text(linked);
                (Some(value), hex)
            }
            None => (None, None),
        };
        Self {
            path,
            path_hex,
            kind: listed.entry_type().name(),
            size: listed.size(),
            mode: attributes.mode.map(|mode| format!("{mode:04o}")),
            uid: attributes.uid,
            gid: attributes.gid,
            modified: attributes.modified.map(time_nanos),
            target,
            target_hex,
            link_to,
            link_to_hex,
            chunks: match &listed.entry.kind {
                NodeKind::File { chunks } => Some(chunks.len()),
                _ => None,
            },
        }
    }
}

/// The entries of a snapshot as a JSON array, produced one element at a time.
struct EntryList<'a> {
    manifest: &'a Manifest,
    under: Option<&'a Path>,
}

impl Serialize for EntryList<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(
            history::entries(self.manifest, self.under).map(|listed| EntryView::new(&listed)),
        )
    }
}

#[derive(Serialize)]
struct EntriesView<'a> {
    snapshot: &'a str,
    entries: EntryList<'a>,
}

/// `ls --json`.
pub fn entries(out: &mut dyn Write, manifest: &Manifest, under: Option<&Path>) -> io::Result<()> {
    write_document(
        out,
        &EntriesView {
            snapshot: manifest.id.as_str(),
            entries: EntryList { manifest, under },
        },
    )
}

// ---------------------------------------------------------------------------------------------
// diff
// ---------------------------------------------------------------------------------------------

#[derive(Serialize)]
struct EndView<'a> {
    id: &'a str,
    short_id: &'a str,
    created: String,
}

#[derive(Serialize)]
struct DiffSummaryView {
    added: usize,
    deleted: usize,
    modified: usize,
    unchanged: u64,
    added_bytes: u64,
    deleted_bytes: u64,
    logical_bytes_from: u64,
    logical_bytes_to: u64,
    chunks_only_in_from: u64,
    chunks_only_in_to: u64,
    stored_bytes_only_in_from: u64,
    stored_bytes_only_in_to: u64,
    missing_chunks: u64,
    chunker_differs: bool,
}

#[derive(Serialize)]
struct ChangeView {
    status: &'static str,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    path_hex: Option<String>,
    #[serde(rename = "type")]
    kind: &'static str,
    previous_type: Option<&'static str>,
    size: Option<u64>,
    previous_size: Option<u64>,
    differences: Vec<&'static str>,
}

impl ChangeView {
    fn new(change: &Change) -> Self {
        let (path, path_hex) = json_text(&change.path);
        Self {
            status: change.status.name(),
            path,
            path_hex,
            kind: change.kind.name(),
            previous_type: change.previous_kind.map(|kind| kind.name()),
            size: change.size,
            previous_size: change.previous_size,
            differences: change.aspects.iter().map(|aspect| aspect.name()).collect(),
        }
    }
}

#[derive(Serialize)]
struct DiffView<'a> {
    from: EndView<'a>,
    to: EndView<'a>,
    summary: DiffSummaryView,
    changes: Vec<ChangeView>,
}

/// `diff --json`.
pub fn diff(out: &mut dyn Write, diff: &Diff) -> io::Result<()> {
    use history::Status;

    write_document(
        out,
        &DiffView {
            from: EndView {
                id: diff.from.id.as_str(),
                short_id: diff.from.id.short(),
                created: time_nanos(diff.from.created),
            },
            to: EndView {
                id: diff.to.id.as_str(),
                short_id: diff.to.id.short(),
                created: time_nanos(diff.to.created),
            },
            summary: DiffSummaryView {
                added: diff.count(Status::Added),
                deleted: diff.count(Status::Deleted),
                modified: diff.count(Status::Modified),
                unchanged: diff.unchanged,
                added_bytes: diff.added_bytes,
                deleted_bytes: diff.deleted_bytes,
                logical_bytes_from: diff.from.stats.bytes,
                logical_bytes_to: diff.to.stats.bytes,
                chunks_only_in_from: diff.chunks_only_in_from,
                chunks_only_in_to: diff.chunks_only_in_to,
                stored_bytes_only_in_from: diff.stored_only_in_from,
                stored_bytes_only_in_to: diff.stored_only_in_to,
                missing_chunks: diff.missing_chunks,
                chunker_differs: diff.chunker_differs,
            },
            changes: diff.changes.iter().map(ChangeView::new).collect(),
        },
    )
}
