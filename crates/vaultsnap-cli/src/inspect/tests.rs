//! Output stability: the exact text and JSON of every renderer for fixed data, that all JSON is
//! valid, and that nothing a hostile file name contains can reach the terminal as a control
//! sequence or add lines.
//!
//! The fixtures are built by hand from the public types of `vaultsnap-core`, so the expected
//! output below does not depend on randomness, the clock, or the file system.

use std::io::{self, Write};
use std::path::Path;

use vaultsnap_core::history::{
    Aspect, Change, DamagedSnapshot, Diff, EntryType, SnapshotDetails, SnapshotInfo, SnapshotList,
    SnapshotSizes, Status,
};
use vaultsnap_core::scan::{ScanOptions, Timestamp};
use vaultsnap_core::snapshot::{
    Attributes, BackupConfig, ChunkRef, Manifest, ManifestEntry, ManifestStats, NodeKind, Root,
    SkippedReason, SkippedRecord, SnapshotId, SnapshotSummary, MANIFEST_FORMAT_VERSION,
};
use vaultsnap_core::{ChunkId, ChunkerConfig};

use super::{finish, human, json};
use crate::commands::CliError;

fn render(write: impl FnOnce(&mut dyn Write) -> io::Result<()>) -> String {
    let mut buffer = Vec::new();
    write(&mut buffer).expect("rendering to memory cannot fail");
    String::from_utf8(buffer).expect("output is UTF-8")
}

fn id(pair: &str) -> SnapshotId {
    pair.repeat(32).parse().expect("valid id")
}

// ---------------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------------

const MODIFIED: Timestamp = Timestamp {
    secs: 1_700_000_000,
    nanos: 123_456_789,
};

fn attributes(size: u64, mode: u32) -> Attributes {
    Attributes {
        size,
        modified: Some(MODIFIED),
        accessed: None,
        changed: None,
        created: None,
        mode: Some(mode),
        uid: Some(1000),
        gid: Some(1000),
        readonly: false,
        device: None,
        inode: None,
        links: None,
    }
}

fn entry(parent: Option<u32>, name: &[u8], kind: NodeKind, attributes: Attributes) -> ManifestEntry {
    ManifestEntry {
        parent,
        name: name.to_vec(),
        kind,
        attributes,
    }
}

fn chunk(seed: u8, len: u32) -> ChunkRef {
    ChunkRef {
        id: ChunkId::from_bytes([seed; 32]),
        len,
    }
}

/// One source, `/data`, with a directory, files (one with a newline in its name, one whose name
/// is not UTF-8, one empty), a symbolic link and a hard link, and one path that was left out.
fn sample_manifest() -> Manifest {
    let entries = vec![
        entry(None, b"", NodeKind::Directory, attributes(0, 0o755)),
        entry(
            Some(0),
            b"a b.txt",
            NodeKind::File { chunks: vec![chunk(1, 5)] },
            attributes(5, 0o644),
        ),
        entry(
            Some(0),
            b"new\nline",
            NodeKind::File { chunks: vec![chunk(2, 3)] },
            attributes(3, 0o600),
        ),
        entry(
            Some(0),
            b"link",
            NodeKind::Symlink { target: b"a b.txt".to_vec() },
            attributes(7, 0o777),
        ),
        entry(Some(0), b"alias", NodeKind::HardLink { target: 1 }, attributes(5, 0o644)),
        entry(
            Some(0),
            b"bad-\xff",
            NodeKind::File { chunks: Vec::new() },
            attributes(0, 0o644),
        ),
    ];
    let scan = ScanOptions {
        excludes: vec!["*.tmp".to_owned()],
        ..ScanOptions::default()
    };
    let mut manifest = Manifest {
        format_version: MANIFEST_FORMAT_VERSION,
        id: id("ab"),
        repository_id: "0123456789abcdef0123456789abcdef".to_owned(),
        created: Timestamp {
            secs: 1_750_000_000,
            nanos: 5,
        },
        host: "workstation".to_owned(),
        tags: vec!["laptop".to_owned(), "weekly".to_owned()],
        program: "vaultsnap 0.1.0".to_owned(),
        config: BackupConfig::new(ChunkerConfig::default(), &scan),
        roots: vec![Root {
            source: b"/data".to_vec(),
            entries,
        }],
        skipped: vec![SkippedRecord {
            root: 0,
            path: b"locked".to_vec(),
            reason: SkippedReason::Unreadable {
                detail: "open: Permission denied".to_owned(),
            },
        }],
        stats: ManifestStats::default(),
    };
    manifest.stats = manifest.compute_stats();
    manifest
}

fn sample_details() -> SnapshotDetails {
    let manifest = sample_manifest();
    let mut root = manifest.roots[0].stats();
    root.skipped = 1;
    SnapshotDetails {
        roots: vec![root],
        sizes: SnapshotSizes {
            logical_bytes: 8,
            chunks: 2,
            deduplicated_bytes: 8,
            stored_bytes: 160,
            unique_chunks: 1,
            unique_stored_bytes: 80,
            missing_chunks: 0,
        },
        unique_sizes_exact: true,
        manifest,
    }
}

fn info(pair: &str, secs: i64, files: u64, unreadable: u64, sizes: SnapshotSizes) -> SnapshotInfo {
    SnapshotInfo {
        summary: SnapshotSummary {
            id: id(pair),
            format_version: MANIFEST_FORMAT_VERSION,
            created: Timestamp { secs, nanos: 0 },
            host: String::new(),
            tags: Vec::new(),
            program: "vaultsnap 0.1.0".to_owned(),
            roots: vec![b"/data".to_vec()],
            stats: ManifestStats {
                files,
                directories: 1,
                skipped: unreadable,
                ..ManifestStats::default()
            },
        },
        sizes,
        unreadable,
    }
}

fn sample_list() -> SnapshotList {
    let mut first = info(
        "aa",
        1_700_000_000,
        3,
        0,
        SnapshotSizes {
            logical_bytes: 8,
            chunks: 2,
            deduplicated_bytes: 8,
            stored_bytes: 160,
            unique_chunks: 1,
            unique_stored_bytes: 80,
            missing_chunks: 0,
        },
    );
    first.summary.host = "workstation".to_owned();
    first.summary.tags = vec!["laptop".to_owned()];
    let mut second = info(
        "bb",
        1_700_086_400,
        4,
        2,
        SnapshotSizes {
            logical_bytes: 1_572_864,
            chunks: 5,
            deduplicated_bytes: 1_500_000,
            stored_bytes: 1_048_576,
            unique_chunks: 3,
            unique_stored_bytes: 524_288,
            missing_chunks: 0,
        },
    );
    second.summary.roots = vec![b"/data".to_vec(), b"/etc/hosts".to_vec()];
    SnapshotList {
        snapshots: vec![first, second],
        damaged: Vec::new(),
        referenced_chunks: 7,
        referenced_stored_bytes: 1_100_000,
    }
}

fn change(
    status: Status,
    path: &[u8],
    kind: EntryType,
    sizes: (Option<u64>, Option<u64>),
    aspects: Vec<Aspect>,
    previous_kind: Option<EntryType>,
) -> Change {
    Change {
        status,
        path: path.to_vec(),
        kind,
        previous_kind,
        size: sizes.0,
        previous_size: sizes.1,
        aspects,
    }
}

fn sample_diff() -> Diff {
    let end = |pair: &str, secs: i64, bytes: u64| SnapshotSummary {
        id: id(pair),
        format_version: MANIFEST_FORMAT_VERSION,
        created: Timestamp { secs, nanos: 0 },
        host: String::new(),
        tags: Vec::new(),
        program: String::new(),
        roots: vec![b"/data".to_vec()],
        stats: ManifestStats {
            bytes,
            ..ManifestStats::default()
        },
    };
    let file = EntryType::File;
    Diff {
        from: end("aa", 1_700_000_000, 1_048_576),
        to: end("bb", 1_700_086_400, 1_572_864),
        changes: vec![
            change(Status::Added, b"/data/added.txt", file, (Some(9), None), vec![], None),
            change(
                Status::Modified,
                b"/data/edit.txt",
                file,
                (Some(29), Some(13)),
                vec![Aspect::Content, Aspect::ModifiedTime],
                Some(file),
            ),
            change(
                Status::Deleted,
                b"/data/gone dir",
                EntryType::Directory,
                (None, None),
                vec![],
                None,
            ),
            change(
                Status::Modified,
                b"/data/link",
                EntryType::Symlink,
                (None, Some(4)),
                vec![Aspect::Type],
                Some(file),
            ),
            change(Status::Added, b"/data/new\nline", file, (Some(3), None), vec![], None),
        ],
        unchanged: 10,
        added_bytes: 9,
        deleted_bytes: 0,
        chunks_only_in_from: 1,
        chunks_only_in_to: 2,
        stored_only_in_from: 2048,
        stored_only_in_to: 4096,
        missing_chunks: 0,
        chunker_differs: false,
    }
}

fn lines(text: &str) -> Vec<&str> {
    text.lines().collect()
}

// ---------------------------------------------------------------------------------------------
// snapshots
// ---------------------------------------------------------------------------------------------

#[test]
fn the_snapshot_table_has_aligned_columns_and_a_footer() {
    let text = render(|out| human::list(out, &sample_list()));
    let row = |cells: [&str; 9]| {
        format!(
            "{:<9}  {:<20}  {:<11}  {:<6}  {:>5}  {:>7}  {:>7}  {:>9}  {}",
            cells[0], cells[1], cells[2], cells[3], cells[4], cells[5], cells[6], cells[7], cells[8]
        )
    };
    let expected = vec![
        row([
            "ID", "CREATED", "HOST", "TAGS", "FILES", "LOGICAL", "STORED", "UNIQUE", "SOURCES",
        ]),
        row([
            "aaaaaaaa",
            "2023-11-14T22:13:20Z",
            "workstation",
            "laptop",
            "3",
            "8 B",
            "160 B",
            "80 B",
            "/data",
        ]),
        row([
            "bbbbbbbb!",
            "2023-11-15T22:13:20Z",
            "-",
            "-",
            "4",
            "1.5 MiB",
            "1.0 MiB",
            "512.0 KiB",
            "/data, /etc/hosts",
        ]),
        String::new(),
        "2 snapshots; they refer to 1.0 MiB of stored data in 7 chunks".to_owned(),
        "! incomplete: some paths could not be backed up (see `vaultsnap show`)".to_owned(),
    ];
    assert_eq!(lines(&text), expected);
    assert!(text.ends_with('\n') && !text.contains("  \n"), "no trailing spaces");
}

#[test]
fn an_empty_repository_and_damaged_neighbours_are_described() {
    let empty = SnapshotList {
        snapshots: Vec::new(),
        damaged: Vec::new(),
        referenced_chunks: 0,
        referenced_stored_bytes: 0,
    };
    assert_eq!(render(|out| human::list(out, &empty)), "No snapshots.\n");

    let mut list = sample_list();
    list.snapshots.truncate(1);
    list.damaged.push(DamagedSnapshot {
        id: id("cc"),
        error: "snapshot is corrupt".to_owned(),
    });
    let text = render(|out| human::list(out, &list));
    assert!(text.ends_with("UNIQUE may be too large: 1 snapshot could not be read\n"), "{text}");
    assert!(!text.contains('!'), "a complete snapshot is not marked");
}

#[test]
fn the_snapshot_list_as_json() {
    let mut list = sample_list();
    list.snapshots.truncate(1);
    list.damaged.push(DamagedSnapshot {
        id: id("cc"),
        error: "snapshot is corrupt".to_owned(),
    });
    let text = render(|out| json::list(out, &list));
    let expected = [
        r#"{"snapshots":[{"id":"@A@","short_id":"aaaaaaaa","created":"2023-11-14T22:13:20.000000000Z","#,
        r#""host":"workstation","tags":["laptop"],"sources":[{"path":"/data"}],"complete":true,"#,
        r#""unreadable_paths":0,"skipped_paths":0,"files":3,"directories":1,"symlinks":0,"#,
        r#""hard_links":0,"special_files":0,"sizes":{"logical_bytes":8,"chunks":2,"#,
        r#""deduplicated_bytes":8,"stored_bytes":160,"unique_chunks":1,"unique_stored_bytes":80,"#,
        r#""missing_chunks":0}}],"damaged":[{"id":"@C@","error":"snapshot is corrupt"}],"#,
        r#""referenced_chunks":7,"referenced_stored_bytes":1100000,"unique_sizes_exact":false}"#,
        "\n",
    ]
    .concat()
    .replace("@A@", &"aa".repeat(32))
    .replace("@C@", &"cc".repeat(32));
    assert_eq!(text, expected);
}

// ---------------------------------------------------------------------------------------------
// show
// ---------------------------------------------------------------------------------------------

#[test]
fn show_as_text() {
    let text = render(|out| human::show(out, &sample_details()));
    let snapshot_line = format!("Snapshot        {}", "ab".repeat(32));
    let expected = [
        snapshot_line.as_str(),
        "Created         2025-06-15T15:06:40Z",
        "Host            workstation",
        "Tags            laptop, weekly",
        "Program         vaultsnap 0.1.0",
        "Status          INCOMPLETE: 1 path could not be backed up",
        "Manifest        version 1",
        "",
        "Sources",
        "  /data  (3 files, 1 directory, 8 B, 1 left out)",
        "",
        "Contents",
        "  files           3",
        "  directories     1",
        "  symbolic links  1",
        "  hard links      1",
        "  special files   0",
        "",
        "Sizes",
        "  logical         8 B    the files as a restore would write them",
        "  deduplicated    8 B    the distinct chunks of this snapshot, uncompressed",
        "  stored          160 B  the same chunks as stored (compressed, encrypted)",
        "  unique stored   80 B   stored data no other snapshot refers to",
        "  chunks          2 (1 unique to this snapshot)",
        "",
        "Configuration",
        "  chunk sizes       256.0 KiB / 1.0 MiB / 4.0 MiB (minimum / average / maximum)",
        "  symbolic links    recorded as links, never followed",
        "  hard links        stored once, other names linked to it",
        "  unreadable paths  skipped and reported",
        "  special files     recorded, never opened",
        "  changing files    the read fails (retried, then reported)",
        "  other filesystems not entered",
        "  include patterns  -",
        "  exclude patterns  *.tmp",
        "",
        "Left out (1)",
        "  /data/locked  unreadable: open: Permission denied",
    ];
    assert_eq!(lines(&text), expected);
}

#[test]
fn show_flags_trouble_and_stays_quiet_otherwise() {
    let mut details = sample_details();
    details.manifest.skipped.clear();
    details.manifest.stats = details.manifest.compute_stats();
    details.roots[0].skipped = 0;
    let text = render(|out| human::show(out, &details));
    assert!(text.contains("Status          complete\n"), "{text}");
    assert!(!text.contains("Left out") && !text.contains("WARNING") && !text.contains("note:"));

    details.sizes.missing_chunks = 2;
    details.unique_sizes_exact = false;
    let text = render(|out| human::show(out, &details));
    assert!(text.contains("WARNING: 2 chunks this snapshot refers to are missing"), "{text}");
    assert!(text.contains("note: another snapshot could not be read"), "{text}");
}

#[test]
fn show_as_json() {
    let text = render(|out| json::show(out, &sample_details()));
    let expected = [
        r#"{"id":"@ID@","short_id":"abababab","created":"2025-06-15T15:06:40.000000005Z","#,
        r#""host":"workstation","tags":["laptop","weekly"],"program":"vaultsnap 0.1.0","#,
        r#""manifest_version":1,"complete":false,"unreadable_paths":1,"sources":[{"path":"/data","#,
        r#""files":3,"directories":1,"symlinks":1,"hard_links":1,"special_files":0,"bytes":8,"#,
        r#""skipped":1}],"totals":{"files":3,"directories":1,"symlinks":1,"hard_links":1,"#,
        r#""special_files":0,"logical_bytes":8},"sizes":{"logical_bytes":8,"chunks":2,"#,
        r#""deduplicated_bytes":8,"stored_bytes":160,"unique_chunks":1,"unique_stored_bytes":80,"#,
        r#""missing_chunks":0},"unique_sizes_exact":true,"config":{"chunk_min_bytes":262144,"#,
        r#""chunk_avg_bytes":1048576,"chunk_max_bytes":4194304,"symlinks":"preserve","#,
        r#""hard_links":"preserve","on_error":"skip","special_files":"record","#,
        r#""changed_files":"fail","cross_filesystems":false,"includes":[],"excludes":["*.tmp"]},"#,
        r#""skipped":[{"path":"/data/locked","source":0,"reason":"unreadable","#,
        r#""detail":"open: Permission denied"}]}"#,
        "\n",
    ]
    .concat()
    .replace("@ID@", &"ab".repeat(32));
    assert_eq!(text, expected);
}

// ---------------------------------------------------------------------------------------------
// ls
// ---------------------------------------------------------------------------------------------

#[test]
fn ls_as_text() {
    let manifest = sample_manifest();
    let text = render(|out| human::entries(out, &manifest, None));
    let expected = [
        "drwxr-xr-x  1000:1000  -  2023-11-14T22:13:20Z  /data/",
        "-rw-r--r--  1000:1000  5  2023-11-14T22:13:20Z  /data/a b.txt",
        r"-rw-------  1000:1000  3  2023-11-14T22:13:20Z  /data/new\nline",
        "lrwxrwxrwx  1000:1000  -  2023-11-14T22:13:20Z  /data/link -> a b.txt",
        "-rw-r--r--  1000:1000  5  2023-11-14T22:13:20Z  /data/alias => /data/a b.txt",
        r"-rw-r--r--  1000:1000  0  2023-11-14T22:13:20Z  /data/bad-\xff",
    ];
    assert_eq!(lines(&text), expected);
}

#[test]
fn ls_below_a_path_lists_only_what_is_there() {
    let manifest = sample_manifest();
    let text = render(|out| human::entries(out, &manifest, Some(Path::new("/data/link"))));
    assert_eq!(text, "lrwxrwxrwx  1000:1000  -  2023-11-14T22:13:20Z  /data/link -> a b.txt\n");
    let text = render(|out| human::entries(out, &manifest, Some(Path::new("/nowhere"))));
    assert_eq!(text, "");
}

#[test]
fn columns_widen_for_big_sizes_and_other_owners() {
    let mut manifest = sample_manifest();
    manifest.roots[0].entries[1].attributes.size = 1_234_567;
    manifest.roots[0].entries[2].attributes.uid = Some(0);
    manifest.roots[0].entries[2].attributes.gid = None;
    let text = render(|out| human::entries(out, &manifest, None));
    let time = "2023-11-14T22:13:20Z";
    let expected = [
        format!("drwxr-xr-x  {:<9}  {:>7}  {time}  /data/", "1000:1000", "-"),
        format!("-rw-r--r--  {:<9}  {:>7}  {time}  /data/a b.txt", "1000:1000", "1234567"),
        format!("-rw-------  {:<9}  {:>7}  {time}  /data/new\\nline", "0:?", "3"),
    ];
    assert_eq!(lines(&text)[..3], expected);
}

#[test]
fn ls_as_json() {
    let manifest = sample_manifest();
    let entries = [
        r#"{"path":"/data","type":"directory","size":null,"mode":"0755","uid":1000,"gid":1000,"modified":"@T@","target":null,"link_to":null,"chunks":null}"#,
        r#"{"path":"/data/a b.txt","type":"file","size":5,"mode":"0644","uid":1000,"gid":1000,"modified":"@T@","target":null,"link_to":null,"chunks":1}"#,
        r#"{"path":"/data/new\nline","type":"file","size":3,"mode":"0600","uid":1000,"gid":1000,"modified":"@T@","target":null,"link_to":null,"chunks":1}"#,
        r#"{"path":"/data/link","type":"symlink","size":null,"mode":"0777","uid":1000,"gid":1000,"modified":"@T@","target":"a b.txt","link_to":null,"chunks":null}"#,
        r#"{"path":"/data/alias","type":"hardlink","size":5,"mode":"0644","uid":1000,"gid":1000,"modified":"@T@","target":null,"link_to":"/data/a b.txt","chunks":null}"#,
        r#"{"path":"/data/bad-@R@","path_hex":"2f646174612f6261642dff","type":"file","size":0,"mode":"0644","uid":1000,"gid":1000,"modified":"@T@","target":null,"link_to":null,"chunks":0}"#,
    ];
    let document = |selected: &[&str]| {
        format!(r#"{{"snapshot":"{}","entries":[{}]}}"#, "ab".repeat(32), selected.join(","))
            .replace("@T@", "2023-11-14T22:13:20.123456789Z")
            .replace("@R@", "\u{fffd}")
            + "\n"
    };

    let everything = render(|out| json::entries(out, &manifest, None));
    assert_eq!(everything, document(&entries));
    let one = render(|out| json::entries(out, &manifest, Some(Path::new("/data/link"))));
    assert_eq!(one, document(&entries[3..4]));
    let none = render(|out| json::entries(out, &manifest, Some(Path::new("/elsewhere"))));
    assert_eq!(none, document(&[]));
}

// ---------------------------------------------------------------------------------------------
// diff
// ---------------------------------------------------------------------------------------------

#[test]
fn diff_as_text() {
    let text = render(|out| human::diff(out, &sample_diff()));
    let expected = [
        "Comparing aaaaaaaa (2023-11-14T22:13:20Z) -> bbbbbbbb (2023-11-15T22:13:20Z)",
        "",
        "+ /data/added.txt",
        "M /data/edit.txt  (content, mtime)",
        "- /data/gone dir/",
        "M /data/link  (type: file -> symlink)",
        r"+ /data/new\nline",
        "",
        "2 added, 1 deleted, 2 modified, 10 unchanged",
        "logical size:  1.0 MiB -> 1.5 MiB (+512.0 KiB)",
        "files:         +9 B added, -0 B deleted (regular files only)",
        "stored data:   2.0 KiB only in the older snapshot, 4.0 KiB only in the newer",
    ];
    assert_eq!(lines(&text), expected);
}

#[test]
fn identical_snapshots_have_no_differences() {
    let mut diff = sample_diff();
    diff.changes.clear();
    diff.to = diff.from.clone();
    diff.unchanged = 3;
    let text = render(|out| human::diff(out, &diff));
    let expected = [
        "Comparing aaaaaaaa (2023-11-14T22:13:20Z) -> aaaaaaaa (2023-11-14T22:13:20Z)",
        "",
        "No differences.",
        "0 added, 0 deleted, 0 modified, 3 unchanged",
    ];
    assert_eq!(lines(&text), expected);
}

#[test]
fn diff_notes_what_could_mislead() {
    let mut diff = sample_diff();
    diff.chunker_differs = true;
    diff.missing_chunks = 3;
    diff.to.stats.bytes = 0;
    let text = render(|out| human::diff(out, &diff));
    assert!(text.contains("logical size:  1.0 MiB -> 0 B (-1.0 MiB)"), "{text}");
    assert!(text.contains("note: the snapshots used different chunk sizes"), "{text}");
    assert!(text.contains("WARNING: 3 chunks are missing from the repository"), "{text}");
}

#[test]
fn diff_as_json() {
    let text = render(|out| json::diff(out, &sample_diff()));
    let expected = [
        r#"{"from":{"id":"@A@","short_id":"aaaaaaaa","created":"2023-11-14T22:13:20.000000000Z"},"#,
        r#""to":{"id":"@B@","short_id":"bbbbbbbb","created":"2023-11-15T22:13:20.000000000Z"},"#,
        r#""summary":{"added":2,"deleted":1,"modified":2,"unchanged":10,"added_bytes":9,"#,
        r#""deleted_bytes":0,"logical_bytes_from":1048576,"logical_bytes_to":1572864,"#,
        r#""chunks_only_in_from":1,"chunks_only_in_to":2,"stored_bytes_only_in_from":2048,"#,
        r#""stored_bytes_only_in_to":4096,"missing_chunks":0,"chunker_differs":false},"#,
        r#""changes":[{"status":"added","path":"/data/added.txt","type":"file","previous_type":null,"#,
        r#""size":9,"previous_size":null,"differences":[]},"#,
        r#"{"status":"modified","path":"/data/edit.txt","type":"file","previous_type":"file","#,
        r#""size":29,"previous_size":13,"differences":["content","mtime"]},"#,
        r#"{"status":"deleted","path":"/data/gone dir","type":"directory","previous_type":null,"#,
        r#""size":null,"previous_size":null,"differences":[]},"#,
        r#"{"status":"modified","path":"/data/link","type":"symlink","previous_type":"file","#,
        r#""size":null,"previous_size":4,"differences":["type"]},"#,
        r#"{"status":"added","path":"/data/new\nline","type":"file","previous_type":null,"#,
        r#""size":3,"previous_size":null,"differences":[]}]}"#,
        "\n",
    ]
    .concat()
    .replace("@A@", &"aa".repeat(32))
    .replace("@B@", &"bb".repeat(32));
    assert_eq!(text, expected);
}

// ---------------------------------------------------------------------------------------------
// Properties of all output
// ---------------------------------------------------------------------------------------------

fn all_json() -> Vec<String> {
    let manifest = sample_manifest();
    vec![
        render(|out| json::list(out, &sample_list())),
        render(|out| json::show(out, &sample_details())),
        render(|out| json::entries(out, &manifest, None)),
        render(|out| json::diff(out, &sample_diff())),
    ]
}

#[test]
fn every_json_document_is_one_valid_line() {
    for text in all_json() {
        assert!(text.ends_with('\n') && !text[..text.len() - 1].contains('\n'), "{text}");
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert!(parsed.is_object());
    }
}

#[test]
fn the_json_of_a_path_that_is_not_text_carries_its_exact_bytes() {
    let text = render(|out| json::entries(out, &sample_manifest(), None));
    let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
    let entries = parsed["entries"].as_array().expect("an array");
    let odd = &entries[5];
    assert_eq!(odd["path_hex"], "2f646174612f6261642dff");
    assert!(odd["path"].as_str().expect("a string").contains('\u{fffd}'));
    for exact in &entries[..5] {
        assert!(exact.get("path_hex").is_none(), "only altered paths have the hex field");
    }
    assert_eq!(entries[2]["path"], "/data/new\nline", "a newline survives JSON exactly");
}

/// Everything a hostile machine could control: names, link targets, host, tags, patterns, and
/// messages, stuffed with terminal control sequences and line breaks.
fn hostile_details() -> SnapshotDetails {
    let mut details = sample_details();
    let manifest = &mut details.manifest;
    manifest.host = "evil\x1b]0;owned\x07host\nsecond line".to_owned();
    manifest.tags = vec!["tag\x1b[2J".to_owned(), "bidi\u{202e}txt".to_owned()];
    manifest.config.excludes = vec!["pattern\x1b[31m\nnext".to_owned()];
    manifest.skipped[0].reason = SkippedReason::Unreadable {
        detail: "failed\x1b[0m\r\nforged line".to_owned(),
    };
    manifest.roots[0].entries.push(entry(
        Some(0),
        b"esc\x1b[2Jname\r\nforged",
        NodeKind::Symlink { target: b"to\x1b[31m\nred\xff".to_vec() },
        attributes(1, 0o777),
    ));
    details
}

fn assert_safe(text: &str, expected_lines: Option<usize>) {
    for forbidden in ['\x1b', '\x07', '\r', '\u{202e}'] {
        assert!(!text.contains(forbidden), "{forbidden:?} reached the output:\n{text:?}");
    }
    if let Some(expected) = expected_lines {
        assert_eq!(text.lines().count(), expected, "a name added lines:\n{text}");
    }
}

#[test]
fn hostile_names_cannot_reach_the_terminal_through_any_text_output() {
    let details = hostile_details();
    let manifest = &details.manifest;
    assert_safe(&render(|out| human::entries(out, manifest, None)), Some(7));
    assert_safe(&render(|out| human::show(out, &details)), None);

    let mut diff = sample_diff();
    diff.changes[0].path = b"/data/\x1b[2Jevil\nline".to_vec();
    assert_safe(&render(|out| human::diff(out, &diff)), Some(12));

    let mut list = sample_list();
    list.snapshots[0].summary.host = "h\x1b[2J\nost".to_owned();
    list.snapshots[0].summary.tags = vec!["t\rag".to_owned()];
    list.snapshots[0].summary.roots = vec![b"/da\x1bta\n".to_vec()];
    assert_safe(&render(|out| human::list(out, &list)), Some(6));
}

#[test]
fn show_prints_the_same_lines_whatever_the_hostile_content_is() {
    let calm = render(|out| human::show(out, &sample_details()));
    let hostile = render(|out| human::show(out, &hostile_details()));
    assert_eq!(calm.lines().count(), hostile.lines().count(), "no line was added or split");
}

#[test]
fn hostile_content_in_json_is_data_not_syntax() {
    let details = hostile_details();
    let text = render(|out| json::show(out, &details));
    let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
    assert_eq!(parsed["host"], "evil\x1b]0;owned\x07host\nsecond line");
    assert_eq!(parsed["config"]["excludes"][0], "pattern\x1b[31m\nnext");
    assert!(!text[..text.len() - 1].contains('\n'), "raw newlines are escaped");
    assert!(!text.contains('\x1b'), "control characters are escaped too");
}

#[test]
fn writing_output_ends_quietly_when_the_reader_goes_away() {
    assert!(finish(Ok(())).is_ok());
    assert!(finish(Err(io::ErrorKind::BrokenPipe.into())).is_ok());
    let disk_full = io::Error::other("disk full");
    assert!(matches!(finish(Err(disk_full)), Err(CliError::Output(_))));
}

#[test]
fn the_sample_manifest_is_a_valid_manifest() {
    sample_manifest().validate().expect("the fixture must satisfy the format's rules");
    let mut hostile = hostile_details().manifest;
    hostile.stats = hostile.compute_stats();
    hostile.validate().expect("so must the hostile one");
}
