//! Output stability: the exact text and JSON of the check report for fixed data, that the JSON is
//! always valid, and that nothing a stray file's name contains can reach the terminal as a control
//! sequence or add lines. The fixtures are built by hand, so the expected output below does not
//! depend on randomness, the clock, or the file system.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::time::Duration;

use vaultsnap_core::snapshot::SnapshotId;
use vaultsnap_core::{AffectedSnapshot, CheckMode, CheckReport, CheckStats, Finding, FindingKind};

use super::{human, json};

fn render(write: impl FnOnce(&mut dyn Write) -> io::Result<()>) -> String {
    let mut buffer = Vec::new();
    write(&mut buffer).expect("rendering to memory cannot fail");
    String::from_utf8(buffer).expect("output is UTF-8")
}

fn id(pair: &str) -> SnapshotId {
    pair.repeat(32).parse().expect("valid id")
}

const REPOSITORY: &str = "0123456789abcdef0123456789abcdef";

fn finding(kind: FindingKind, subject: &str, detail: &str) -> Finding {
    Finding {
        kind,
        subject: subject.to_owned(),
        detail: detail.to_owned(),
    }
}

fn counted(findings: &[Finding], extra: &[(FindingKind, u64)]) -> BTreeMap<FindingKind, u64> {
    let mut counts = BTreeMap::new();
    for finding in findings {
        *counts.entry(finding.kind).or_insert(0) += 1;
    }
    for (kind, more) in extra {
        *counts.entry(*kind).or_insert(0) += more;
    }
    counts
}

fn chunk(pair: &str) -> String {
    format!("data/{}/{}", &pair[..2], pair.repeat(32))
}

/// A quick check that found three errors and three warnings, listing one warning less than it
/// found, in a repository with one snapshot that cannot be read.
fn damaged() -> CheckReport {
    let findings = vec![
        finding(
            FindingKind::SnapshotDamaged,
            &format!("snapshots/{}", "bb".repeat(32)),
            "snapshot is corrupt: authentication failed",
        ),
        finding(
            FindingKind::ChunkMissing,
            &chunk("ab"),
            "no object exists for a chunk that snapshots refer to",
        ),
        finding(
            FindingKind::ChunkCorrupt,
            &chunk("cd"),
            "length does not match the header (truncated or extended)",
        ),
        finding(
            FindingKind::OrphanObject,
            &chunk("ef"),
            "no readable snapshot refers to this object",
        ),
        finding(
            FindingKind::UnexpectedFile,
            "notes\nforged line",
            "a file that is not part of a VaultSnap repository",
        ),
    ];
    CheckReport {
        repository_id: REPOSITORY.to_owned(),
        format_version: 1,
        mode: CheckMode::Quick,
        stats: CheckStats {
            key_files: 1,
            snapshots: 2,
            snapshots_damaged: 1,
            objects: 5,
            object_bytes: 12_345,
            chunks_referenced: 4,
            chunks_missing: 1,
            objects_checked: 5,
            objects_damaged: 1,
            bytes_authenticated: 0,
            orphans: 2,
            orphan_bytes: 2048,
            orphans_exact: false,
        },
        affected: vec![AffectedSnapshot {
            snapshot: id("aa"),
            damaged_chunks: 2,
            files: 3,
        }],
        counts: counted(&findings, &[(FindingKind::OrphanObject, 1)]),
        findings,
        errors: 3,
        warnings: 3,
        suppressed: 1,
        duration: Duration::from_millis(1500),
    }
}

fn clean_full() -> CheckReport {
    CheckReport {
        repository_id: REPOSITORY.to_owned(),
        format_version: 1,
        mode: CheckMode::Full,
        stats: CheckStats {
            key_files: 1,
            snapshots: 3,
            objects: 10,
            object_bytes: 1_048_576,
            chunks_referenced: 10,
            objects_checked: 10,
            bytes_authenticated: 1_048_576,
            orphans_exact: true,
            ..CheckStats::default()
        },
        affected: Vec::new(),
        findings: Vec::new(),
        errors: 0,
        warnings: 0,
        suppressed: 0,
        counts: BTreeMap::new(),
        duration: Duration::ZERO,
    }
}

fn field(label: &str, value: &str) -> String {
    format!("{label:<13}{value}")
}

#[test]
fn a_damaged_repository_as_text() {
    let text = render(|out| human::report(out, &damaged()));
    let advice = |kind: FindingKind| format!("  {kind}: {}", kind.advice());
    let expected = [
        format!("Repository {REPOSITORY} (format version 1)"),
        field(
            "Mode",
            "quick: snapshots authenticated, chunk objects examined by structure only",
        ),
        String::new(),
        field("Snapshots", "2 readable, 1 damaged"),
        field("Chunks", "4 referenced by snapshots, 1 missing"),
        field("Objects", "5 in data/ (12.1 KiB), 1 damaged"),
        field(
            "Unreferenced",
            "2 objects (2.0 KiB); may include objects of snapshots that could not be read",
        ),
        field("Key files", "1"),
        field("Took", "1.5s"),
        String::new(),
        "ERRORS (3)".to_owned(),
        format!("  snapshot_damaged  snapshots/{}", "bb".repeat(32)),
        "      snapshot is corrupt: authentication failed".to_owned(),
        format!("  chunk_missing  {}", chunk("ab")),
        "      no object exists for a chunk that snapshots refer to".to_owned(),
        format!("  chunk_corrupt  {}", chunk("cd")),
        "      length does not match the header (truncated or extended)".to_owned(),
        String::new(),
        "WARNINGS (3)".to_owned(),
        format!("  orphan_object  {}", chunk("ef")),
        "      no readable snapshot refers to this object".to_owned(),
        r"  unexpected_file  notes\nforged line".to_owned(),
        "      a file that is not part of a VaultSnap repository".to_owned(),
        "  and 1 more not listed (--max-findings sets the limit, 0 lists all)".to_owned(),
        String::new(),
        "AFFECTED SNAPSHOTS (cannot be restored completely)".to_owned(),
        "  aaaaaaaa  3 files affected (2 damaged or missing chunks)".to_owned(),
        String::new(),
        "WHAT TO DO (the check changed nothing; see docs/CHECK.md)".to_owned(),
        advice(FindingKind::SnapshotDamaged),
        advice(FindingKind::ChunkMissing),
        advice(FindingKind::ChunkCorrupt),
        advice(FindingKind::OrphanObject),
        advice(FindingKind::UnexpectedFile),
        String::new(),
        "Result: 3 errors found, 3 warnings. The repository was not changed.".to_owned(),
        "Quick mode does not read chunk contents: damage inside the encrypted data is found \
         only with --read-data."
            .to_owned(),
    ];
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines, expected);
    assert!(
        text.ends_with('\n') && !text.contains("  \n"),
        "no trailing spaces"
    );
}

#[test]
fn a_clean_full_check_as_text() {
    let text = render(|out| human::report(out, &clean_full()));
    let expected = [
        format!("Repository {REPOSITORY} (format version 1)"),
        field(
            "Mode",
            "full: snapshots authenticated, every chunk object read, authenticated, and decrypted",
        ),
        String::new(),
        field("Snapshots", "3 readable, 0 damaged"),
        field("Chunks", "10 referenced by snapshots, 0 missing"),
        field("Objects", "10 in data/ (1.0 MiB), 0 damaged"),
        field("Unreferenced", "0 objects (0 B)"),
        field("Key files", "1"),
        field("Verified", "1.0 MiB read and authenticated"),
        field("Took", "0.0s"),
        String::new(),
        "Result: no problems found.".to_owned(),
    ];
    assert_eq!(text.lines().collect::<Vec<_>>(), expected);
}

#[test]
fn warnings_alone_pass_and_a_quick_check_says_what_it_did_not_read() {
    let findings = vec![
        finding(
            FindingKind::OrphanObject,
            &chunk("ab"),
            "no snapshot refers to this object",
        ),
        finding(
            FindingKind::LockPresent,
            "locks/x.lock",
            "a shared lock held by process 7",
        ),
    ];
    let mut report = clean_full();
    report.mode = CheckMode::Quick;
    report.counts = counted(&findings, &[]);
    report.findings = findings;
    report.warnings = 2;
    let text = render(|out| human::report(out, &report));
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines.contains(&"Result: no errors found, 2 warnings."),
        "{text}"
    );
    assert!(lines
        .last()
        .is_some_and(|line| line.starts_with("Quick mode does not read")));
    assert!(
        !text.contains("ERRORS") && !text.contains("AFFECTED"),
        "{text}"
    );
    assert!(
        text.contains("  lock_present: A process may be using the repository"),
        "{text}"
    );
    assert!(
        !text.contains("Verified"),
        "nothing was authenticated in quick mode"
    );

    report.warnings = 1;
    report.counts.remove(&FindingKind::LockPresent);
    report.findings.pop();
    let text = render(|out| human::report(out, &report));
    assert!(
        text.contains("Result: no errors found, 1 warning.\n"),
        "{text}"
    );
}

#[test]
fn a_damaged_repository_as_json() {
    let text = render(|out| json::report(out, &damaged()));
    let expected = [
        r#"{"repository_id":"0123456789abcdef0123456789abcdef","format_version":1,"#,
        r#""mode":"quick","ok":false,"errors":3,"warnings":3,"suppressed":1,"#,
        r#""counts":{"missing_directory":0,"snapshot_damaged":1,"unsupported_version":0,"#,
        r#""chunk_missing":1,"chunk_corrupt":1,"chunk_length_mismatch":0,"io_error":0,"#,
        r#""orphan_object":2,"unexpected_file":1,"leftover_temp_file":0,"lock_present":0,"#,
        r#""key_file_invalid":0},"stats":{"key_files":1,"snapshots":2,"snapshots_damaged":1,"#,
        r#""objects":5,"object_bytes":12345,"chunks_referenced":4,"chunks_missing":1,"#,
        r#""objects_checked":5,"objects_damaged":1,"bytes_authenticated":0,"orphans":2,"#,
        r#""orphan_bytes":2048,"orphans_exact":false},"affected_snapshots":[{"id":"@A@","#,
        r#""short_id":"aaaaaaaa","files":3,"damaged_chunks":2}],"findings":["#,
        r#"{"severity":"error","kind":"snapshot_damaged","subject":"snapshots/@B@","#,
        r#""detail":"snapshot is corrupt: authentication failed"},"#,
        r#"{"severity":"error","kind":"chunk_missing","subject":"@AB@","#,
        r#""detail":"no object exists for a chunk that snapshots refer to"},"#,
        r#"{"severity":"error","kind":"chunk_corrupt","subject":"@CD@","#,
        r#""detail":"length does not match the header (truncated or extended)"},"#,
        r#"{"severity":"warning","kind":"orphan_object","subject":"@EF@","#,
        r#""detail":"no readable snapshot refers to this object"},"#,
        r#"{"severity":"warning","kind":"unexpected_file","subject":"notes\nforged line","#,
        r#""detail":"a file that is not part of a VaultSnap repository"}]}"#,
        "\n",
    ]
    .concat()
    .replace("@A@", &"aa".repeat(32))
    .replace("@B@", &"bb".repeat(32))
    .replace("@AB@", &chunk("ab"))
    .replace("@CD@", &chunk("cd"))
    .replace("@EF@", &chunk("ef"));
    assert_eq!(text, expected);
}

#[test]
fn a_clean_full_check_as_json() {
    let text = render(|out| json::report(out, &clean_full()));
    let expected = [
        r#"{"repository_id":"0123456789abcdef0123456789abcdef","format_version":1,"#,
        r#""mode":"full","ok":true,"errors":0,"warnings":0,"suppressed":0,"#,
        r#""counts":{"missing_directory":0,"snapshot_damaged":0,"unsupported_version":0,"#,
        r#""chunk_missing":0,"chunk_corrupt":0,"chunk_length_mismatch":0,"io_error":0,"#,
        r#""orphan_object":0,"unexpected_file":0,"leftover_temp_file":0,"lock_present":0,"#,
        r#""key_file_invalid":0},"stats":{"key_files":1,"snapshots":3,"snapshots_damaged":0,"#,
        r#""objects":10,"object_bytes":1048576,"chunks_referenced":10,"chunks_missing":0,"#,
        r#""objects_checked":10,"objects_damaged":0,"bytes_authenticated":1048576,"orphans":0,"#,
        r#""orphan_bytes":0,"orphans_exact":true},"affected_snapshots":[],"findings":[]}"#,
        "\n",
    ]
    .concat();
    assert_eq!(text, expected);
}

#[test]
fn the_counts_object_has_a_field_for_every_kind() {
    let text = render(|out| json::report(out, &damaged()));
    let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
    let counts = parsed["counts"].as_object().expect("an object");
    assert_eq!(counts.len(), FindingKind::ALL.len());
    for kind in FindingKind::ALL {
        let expected = damaged().count(kind);
        assert_eq!(
            counts[kind.name()],
            expected,
            "{kind}: a new kind must be added to the JSON too"
        );
    }
}

#[test]
fn json_is_one_valid_line_and_the_output_does_not_depend_on_the_clock() {
    for report in [damaged(), clean_full()] {
        let text = render(|out| json::report(out, &report));
        assert!(
            text.ends_with('\n') && !text[..text.len() - 1].contains('\n'),
            "{text}"
        );
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert!(parsed.is_object());

        let mut later = report.clone();
        later.duration = Duration::from_secs(99);
        assert_eq!(
            render(|out| json::report(out, &later)),
            text,
            "no time is in the document"
        );
    }
}

#[test]
fn hostile_names_cannot_reach_the_terminal_and_stay_exact_in_json() {
    let name = "x\x1b[2J\x07\r\nforged: ok\u{202e}";
    let findings = vec![finding(
        FindingKind::UnexpectedFile,
        name,
        &format!("a file {name} that is not part of a VaultSnap repository"),
    )];
    let mut report = clean_full();
    report.counts = counted(&findings, &[]);
    report.findings = findings;
    report.warnings = 1;

    let text = render(|out| human::report(out, &report));
    for forbidden in ['\x1b', '\x07', '\r', '\u{202e}'] {
        assert!(
            !text.contains(forbidden),
            "{forbidden:?} reached the output:\n{text:?}"
        );
    }
    assert!(
        !text.lines().any(|line| line.starts_with("forged")),
        "a name started a new line"
    );
    assert!(
        text.contains(r"x\x1b[2J\x07\r\nforged: ok\u{202e}"),
        "{text}"
    );

    let text = render(|out| json::report(out, &report));
    let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
    assert_eq!(
        parsed["findings"][0]["subject"], name,
        "JSON keeps the name exact"
    );
    assert!(!text.contains('\x1b') && !text[..text.len() - 1].contains('\n'));
}
