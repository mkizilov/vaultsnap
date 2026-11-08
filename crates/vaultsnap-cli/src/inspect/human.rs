//! Text for people. Everything that comes from the backed-up machine (names, paths, host names,
//! tags, messages) goes through [`escape`], so it cannot inject terminal escape sequences or break
//! the one-record-per-line layout. Times are UTC and sizes use binary units, so the output does not
//! depend on the machine or its locale.

use std::io::{self, Write};
use std::path::Path;

use vaultsnap_core::history::{
    self, skipped_path, unreadable_paths, Aspect, Diff, EntryType, ListedEntry, SnapshotDetails,
    SnapshotList, Status,
};
use vaultsnap_core::scan::{
    ChangePolicy, ErrorPolicy, HardLinkPolicy, SpecialFilePolicy, SymlinkPolicy,
};
use vaultsnap_core::snapshot::{Manifest, NodeKind, SkippedReason};

use crate::format::{escape, human_size, mode_string, size_with_bytes, time_seconds};

fn or_dash(text: String) -> String {
    if text.is_empty() {
        "-".to_owned()
    } else {
        text
    }
}

fn plural(count: u64, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// Writes `rows` as columns: every column as wide as its widest cell, two spaces between columns,
/// numbers (the `right` columns) aligned on the right, and no trailing spaces.
fn table(
    out: &mut dyn Write,
    header: &[&str],
    right: &[bool],
    rows: &[Vec<String>],
) -> io::Result<()> {
    let widths: Vec<usize> = (0..header.len())
        .map(|column| {
            rows.iter()
                .map(|row| row[column].chars().count())
                .chain([header[column].chars().count()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let write_row = |out: &mut dyn Write, cells: Vec<&str>| -> io::Result<()> {
        let mut line = String::new();
        for (column, cell) in cells.iter().enumerate() {
            if column > 0 {
                line.push_str("  ");
            }
            let padding = " ".repeat(widths[column].saturating_sub(cell.chars().count()));
            if right[column] {
                line.push_str(&padding);
                line.push_str(cell);
            } else {
                line.push_str(cell);
                line.push_str(&padding);
            }
        }
        writeln!(out, "{}", line.trim_end())
    };
    write_row(out, header.to_vec())?;
    for row in rows {
        write_row(out, row.iter().map(String::as_str).collect())?;
    }
    Ok(())
}

/// `snapshots`: a table of the snapshots, oldest first.
pub fn list(out: &mut dyn Write, list: &SnapshotList) -> io::Result<()> {
    if list.snapshots.is_empty() {
        return writeln!(out, "No snapshots.");
    }
    let rows: Vec<Vec<String>> = list
        .snapshots
        .iter()
        .map(|info| {
            let summary = &info.summary;
            let marker = if info.unreadable > 0 { "!" } else { "" };
            let tags: Vec<String> = summary.tags.iter().map(|tag| escape(tag.as_bytes())).collect();
            let sources: Vec<String> = summary.roots.iter().map(|root| escape(root)).collect();
            vec![
                format!("{}{marker}", summary.id.short()),
                time_seconds(summary.created),
                or_dash(escape(summary.host.as_bytes())),
                or_dash(tags.join(",")),
                summary.stats.files.to_string(),
                human_size(info.sizes.logical_bytes),
                human_size(info.sizes.stored_bytes),
                human_size(info.sizes.unique_stored_bytes),
                sources.join(", "),
            ]
        })
        .collect();
    let header = ["ID", "CREATED", "HOST", "TAGS", "FILES", "LOGICAL", "STORED", "UNIQUE", "SOURCES"];
    let right = [false, false, false, false, true, true, true, true, false];
    table(out, &header, &right, &rows)?;

    writeln!(out)?;
    let count = list.snapshots.len() as u64;
    writeln!(
        out,
        "{}; they refer to {} of stored data in {}",
        plural(count, "snapshot", "snapshots"),
        human_size(list.referenced_stored_bytes),
        plural(list.referenced_chunks, "chunk", "chunks"),
    )?;
    if list.snapshots.iter().any(|info| info.unreadable > 0) {
        writeln!(out, "! incomplete: some paths could not be backed up (see `vaultsnap show`)")?;
    }
    if !list.unique_sizes_exact() {
        let damaged = list.damaged.len() as u64;
        writeln!(
            out,
            "UNIQUE may be too large: {} could not be read",
            plural(damaged, "snapshot", "snapshots")
        )?;
    }
    Ok(())
}

fn field(out: &mut dyn Write, label: &str, value: &str) -> io::Result<()> {
    writeln!(out, "{label:<16}{value}")
}

fn symlinks(policy: SymlinkPolicy) -> &'static str {
    match policy {
        SymlinkPolicy::Preserve => "recorded as links, never followed",
        SymlinkPolicy::FollowInsideRoot => "followed only when the target is inside the source",
        SymlinkPolicy::FollowAll => "followed everywhere",
    }
}

fn hard_links(policy: HardLinkPolicy) -> &'static str {
    match policy {
        HardLinkPolicy::Preserve => "stored once, other names linked to it",
        HardLinkPolicy::Duplicate => "every name stored as its own file",
    }
}

fn on_error(policy: ErrorPolicy) -> &'static str {
    match policy {
        ErrorPolicy::Skip => "skipped and reported",
        ErrorPolicy::Fail => "stop at the first one",
    }
}

fn special_files(policy: SpecialFilePolicy) -> &'static str {
    match policy {
        SpecialFilePolicy::Record => "recorded, never opened",
        SpecialFilePolicy::Skip => "skipped",
    }
}

fn changed_files(policy: ChangePolicy) -> &'static str {
    match policy {
        ChangePolicy::Fail => "the read fails (retried, then reported)",
        ChangePolicy::Accept => "accepted as read",
    }
}

fn patterns(list: &[String]) -> String {
    if list.is_empty() {
        "-".to_owned()
    } else {
        list.iter().map(|pattern| escape(pattern.as_bytes())).collect::<Vec<_>>().join(", ")
    }
}

fn skip_reason(reason: &SkippedReason) -> String {
    match reason {
        SkippedReason::Vanished => "disappeared during the backup".to_owned(),
        SkippedReason::Unreadable { detail } => format!("unreadable: {}", escape(detail.as_bytes())),
        SkippedReason::OtherFilesystem => "on another filesystem, not entered".to_owned(),
        SkippedReason::SpecialFile { kind } => {
            format!("special file ({}) skipped by policy", escape(kind.as_bytes()))
        }
    }
}

/// `show`: everything about one snapshot.
pub fn show(out: &mut dyn Write, details: &SnapshotDetails) -> io::Result<()> {
    let manifest = &details.manifest;
    let unreadable = unreadable_paths(manifest);
    let tags: Vec<String> = manifest.tags.iter().map(|tag| escape(tag.as_bytes())).collect();
    let status = if unreadable == 0 {
        "complete".to_owned()
    } else {
        format!("INCOMPLETE: {} could not be backed up", plural(unreadable, "path", "paths"))
    };
    field(out, "Snapshot", manifest.id.as_str())?;
    field(out, "Created", &time_seconds(manifest.created))?;
    field(out, "Host", &or_dash(escape(manifest.host.as_bytes())))?;
    field(out, "Tags", &or_dash(tags.join(", ")))?;
    field(out, "Program", &or_dash(escape(manifest.program.as_bytes())))?;
    field(out, "Status", &status)?;
    field(out, "Manifest", &format!("version {}", manifest.format_version))?;

    writeln!(out)?;
    writeln!(out, "Sources")?;
    for (root, stats) in manifest.roots.iter().zip(&details.roots) {
        let skipped = if stats.skipped > 0 {
            format!(", {} left out", stats.skipped)
        } else {
            String::new()
        };
        writeln!(
            out,
            "  {}  ({}, {}, {}{skipped})",
            escape(&root.source),
            plural(stats.files, "file", "files"),
            plural(stats.directories, "directory", "directories"),
            human_size(stats.bytes),
        )?;
    }

    let totals = &manifest.stats;
    writeln!(out)?;
    writeln!(out, "Contents")?;
    for (label, count) in [
        ("files", totals.files),
        ("directories", totals.directories),
        ("symbolic links", totals.symlinks),
        ("hard links", totals.hard_links),
        ("special files", totals.special_files),
    ] {
        writeln!(out, "  {label:<16}{count}")?;
    }

    let sizes = &details.sizes;
    let rows = [
        (
            "logical",
            size_with_bytes(sizes.logical_bytes),
            "the files as a restore would write them",
        ),
        (
            "deduplicated",
            size_with_bytes(sizes.deduplicated_bytes),
            "the distinct chunks of this snapshot, uncompressed",
        ),
        (
            "stored",
            size_with_bytes(sizes.stored_bytes),
            "the same chunks as stored (compressed, encrypted)",
        ),
        (
            "unique stored",
            size_with_bytes(sizes.unique_stored_bytes),
            "stored data no other snapshot refers to",
        ),
    ];
    let width = rows.iter().map(|(_, value, _)| value.chars().count()).max().unwrap_or(0);
    writeln!(out)?;
    writeln!(out, "Sizes")?;
    for (label, value, note) in rows {
        writeln!(out, "  {label:<16}{value:<width$}  {note}")?;
    }
    writeln!(
        out,
        "  {:<16}{} ({} unique to this snapshot)",
        "chunks", sizes.chunks, sizes.unique_chunks
    )?;
    if sizes.missing_chunks > 0 {
        writeln!(
            out,
            "  WARNING: {} this snapshot refers to are missing from the repository",
            plural(sizes.missing_chunks, "chunk", "chunks")
        )?;
    }
    if !details.unique_sizes_exact {
        writeln!(out, "  note: another snapshot could not be read, so the unique size may be too large")?;
    }

    let config = &manifest.config;
    let chunker = &config.chunker;
    writeln!(out)?;
    writeln!(out, "Configuration")?;
    let sizes_text = format!(
        "{} / {} / {} (minimum / average / maximum)",
        human_size(chunker.min_size() as u64),
        human_size(chunker.avg_size() as u64),
        human_size(chunker.max_size() as u64),
    );
    for (label, value) in [
        ("chunk sizes", sizes_text),
        ("symbolic links", symlinks(config.symlinks).to_owned()),
        ("hard links", hard_links(config.hard_links).to_owned()),
        ("unreadable paths", on_error(config.on_error).to_owned()),
        ("special files", special_files(config.special_files).to_owned()),
        ("changing files", changed_files(config.changed_files).to_owned()),
        (
            "other filesystems",
            if config.cross_filesystems { "entered" } else { "not entered" }.to_owned(),
        ),
        ("include patterns", patterns(&config.includes)),
        ("exclude patterns", patterns(&config.excludes)),
    ] {
        writeln!(out, "  {label:<18}{value}")?;
    }

    if !manifest.skipped.is_empty() {
        writeln!(out)?;
        writeln!(out, "Left out ({})", manifest.skipped.len())?;
        for record in &manifest.skipped {
            writeln!(
                out,
                "  {}  {}",
                escape(&skipped_path(manifest, record)),
                skip_reason(&record.reason)
            )?;
        }
    }
    Ok(())
}

fn owner(entry: &ListedEntry<'_>) -> String {
    let attributes = &entry.entry.attributes;
    match (attributes.uid, attributes.gid) {
        (None, None) => "-".to_owned(),
        (uid, gid) => {
            let part = |id: Option<u32>| id.map_or_else(|| "?".to_owned(), |id| id.to_string());
            format!("{}:{}", part(uid), part(gid))
        }
    }
}

/// `ls`: one line per entry in the order they are stored, like `ls -l`: permissions, owner, size,
/// modification time, path. Directories end with `/`; a symbolic link shows `-> target` and a hard
/// link `=> the path that carries its data`.
pub fn entries(out: &mut dyn Write, manifest: &Manifest, under: Option<&Path>) -> io::Result<()> {
    let (mut size_width, mut owner_width) = (1, 1);
    for entry in history::entries(manifest, under) {
        size_width = size_width.max(entry.size().map_or(1, |size| size.to_string().len()));
        owner_width = owner_width.max(owner(&entry).chars().count());
    }
    for entry in history::entries(manifest, under) {
        let kind = entry.entry_type();
        let attributes = &entry.entry.attributes;
        let mode = mode_string(kind, attributes.mode);
        let size = entry.size().map_or_else(|| "-".to_owned(), |size| size.to_string());
        let modified = attributes.modified.map_or_else(|| "-".to_owned(), time_seconds);
        let mut path = escape(&entry.path);
        if kind == EntryType::Directory && !path.ends_with('/') {
            path.push('/');
        }
        let suffix = match &entry.entry.kind {
            NodeKind::Symlink { target } => format!(" -> {}", escape(target)),
            NodeKind::HardLink { .. } => {
                format!(" => {}", escape(entry.link_to.as_deref().unwrap_or_default()))
            }
            _ => String::new(),
        };
        writeln!(
            out,
            "{mode}  {:<owner_width$}  {size:>size_width$}  {modified}  {path}{suffix}",
            owner(&entry)
        )?;
    }
    Ok(())
}

fn signed_size(from: u64, to: u64) -> String {
    match to.cmp(&from) {
        std::cmp::Ordering::Equal => "no change".to_owned(),
        std::cmp::Ordering::Greater => format!("+{}", human_size(to - from)),
        std::cmp::Ordering::Less => format!("-{}", human_size(from - to)),
    }
}

fn symbol(status: Status) -> char {
    match status {
        Status::Added => '+',
        Status::Deleted => '-',
        Status::Modified => 'M',
    }
}

/// `diff`: one line per added (`+`), deleted (`-`), and modified (`M`) path in path order, then
/// a summary with the sizes.
pub fn diff(out: &mut dyn Write, diff: &Diff) -> io::Result<()> {
    writeln!(
        out,
        "Comparing {} ({}) -> {} ({})",
        diff.from.id.short(),
        time_seconds(diff.from.created),
        diff.to.id.short(),
        time_seconds(diff.to.created),
    )?;
    writeln!(out)?;
    for change in &diff.changes {
        let mut path = escape(&change.path);
        if change.kind == EntryType::Directory && !path.ends_with('/') {
            path.push('/');
        }
        let detail = if change.aspects.is_empty() {
            String::new()
        } else {
            let names: Vec<String> = change
                .aspects
                .iter()
                .map(|aspect| match (aspect, change.previous_kind) {
                    (Aspect::Type, Some(previous)) => {
                        format!("type: {} -> {}", previous.name(), change.kind.name())
                    }
                    (aspect, _) => aspect.name().to_owned(),
                })
                .collect();
            format!("  ({})", names.join(", "))
        };
        writeln!(out, "{} {path}{detail}", symbol(change.status))?;
    }

    if diff.is_empty() {
        writeln!(out, "No differences.")?;
    }
    if !diff.changes.is_empty() {
        writeln!(out)?;
    }
    writeln!(
        out,
        "{} added, {} deleted, {} modified, {} unchanged",
        diff.count(Status::Added),
        diff.count(Status::Deleted),
        diff.count(Status::Modified),
        diff.unchanged,
    )?;
    if diff.is_empty() {
        return Ok(());
    }
    writeln!(
        out,
        "logical size:  {} -> {} ({})",
        human_size(diff.from.stats.bytes),
        human_size(diff.to.stats.bytes),
        signed_size(diff.from.stats.bytes, diff.to.stats.bytes),
    )?;
    writeln!(
        out,
        "files:         +{} added, -{} deleted (regular files only)",
        human_size(diff.added_bytes),
        human_size(diff.deleted_bytes),
    )?;
    writeln!(
        out,
        "stored data:   {} only in the older snapshot, {} only in the newer",
        human_size(diff.stored_only_in_from),
        human_size(diff.stored_only_in_to),
    )?;
    if diff.chunker_differs {
        writeln!(
            out,
            "note: the snapshots used different chunk sizes, so a file of unchanged size can be \
             reported as modified"
        )?;
    }
    if diff.missing_chunks > 0 {
        writeln!(
            out,
            "WARNING: {} are missing from the repository",
            plural(diff.missing_chunks, "chunk", "chunks")
        )?;
    }
    Ok(())
}
