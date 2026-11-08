//! The prune command's rules and output: when deleting is allowed, what counts as a yes, and the
//! exact text of the plan, the outcome, the question and the failure list. The fixtures are built
//! by hand, so the expected text does not depend on randomness, the clock, or the file system.

use std::io::{self, Write};
use std::time::Duration;

use vaultsnap_core::scan::Timestamp;
use vaultsnap_core::snapshot::SnapshotId;
use vaultsnap_core::{Decision, KeepReason, PlanDigest, PruneFailure, PruneReport, RetentionPolicy};

use super::{decide, human, permission, Permission};
use crate::commands::CliError;

fn render(write: impl FnOnce(&mut dyn Write) -> io::Result<()>) -> String {
    let mut buffer = Vec::new();
    write(&mut buffer).expect("rendering to memory cannot fail");
    String::from_utf8(buffer).expect("output is UTF-8")
}

fn decision(pair: &str, secs: i64, keep: &[KeepReason]) -> Decision {
    let id: SnapshotId = pair.repeat(32).parse().expect("valid id");
    Decision {
        id,
        created: Timestamp { secs, nanos: 0 },
        keep: keep.to_vec(),
    }
}

fn policy() -> RetentionPolicy {
    RetentionPolicy {
        keep_last: Some(1),
        keep_daily: Some(7),
        ..RetentionPolicy::default()
    }
}

/// Three snapshots one day apart, the oldest to be removed, and three unreferenced objects of
/// 3.5 KiB in all. Nothing has been deleted.
fn planned() -> PruneReport {
    PruneReport {
        executed: false,
        decisions: vec![
            decision("aa", 1_700_000_000, &[]),
            decision("bb", 1_700_086_400, &[KeepReason::Last, KeepReason::Daily]),
            decision("cc", 1_700_172_800, &[KeepReason::Last]),
        ],
        digest: PlanDigest::from_hex(&"ab".repeat(32)).expect("valid digest"),
        chunks_in_use: 40,
        objects: 43,
        chunks_unreferenced: 3,
        bytes_unreferenced: 3 * 1024 + 512,
        snapshots_removed: 0,
        chunks_removed: 0,
        bytes_removed: 0,
        failures: Vec::new(),
        failure_count: 0,
        chunks_verified: 0,
        duration: Duration::ZERO,
    }
}

/// The same plan after it was executed in full.
fn executed() -> PruneReport {
    PruneReport {
        executed: true,
        snapshots_removed: 1,
        chunks_removed: 3,
        bytes_removed: 3 * 1024 + 512,
        chunks_verified: 40,
        duration: Duration::from_millis(1500),
        ..planned()
    }
}

fn failure(subject: &str, reason: &str) -> PruneFailure {
    PruneFailure {
        subject: subject.to_owned(),
        reason: reason.to_owned(),
    }
}

#[test]
fn deleting_needs_a_dry_run_an_explicit_yes_or_a_person_to_ask() {
    let cases = [
        // (dry_run, yes, interactive) -> permission
        ((true, false, false), Permission::DryRun),
        ((true, false, true), Permission::DryRun),
        ((true, true, false), Permission::DryRun),
        ((true, true, true), Permission::DryRun),
        ((false, true, false), Permission::Granted),
        ((false, true, true), Permission::Granted),
        ((false, false, true), Permission::Ask),
    ];
    for ((dry_run, yes, interactive), expected) in cases {
        let got = permission(dry_run, yes, interactive).expect("permitted");
        assert_eq!(got, expected, "dry_run={dry_run} yes={yes} interactive={interactive}");
    }
}

#[test]
fn without_a_person_and_without_yes_nothing_is_deleted() {
    let err = permission(false, false, false).expect_err("must refuse");
    assert!(matches!(err, CliError::PruneNeedsConfirmation), "{err}");
    let text = err.to_string();
    assert!(text.contains("--dry-run") && text.contains("--yes"), "{text}");
}

#[test]
fn only_the_word_yes_is_a_yes() {
    let accepted = ["yes\n", "yes", "yes\r\n", "  yes  \n"];
    for answer in accepted {
        let mut input = answer.as_bytes();
        let mut out = Vec::new();
        assert!(decide(&mut input, &mut out, "Proceed? ").expect("read"), "{answer:?}");
    }
    let refused = ["", "\n", "y\n", "Y\n", "YES\n", "Yes\n", "no\n", "yes please\n", "yess\n"];
    for answer in refused {
        let mut input = answer.as_bytes();
        let mut out = Vec::new();
        assert!(!decide(&mut input, &mut out, "Proceed? ").expect("read"), "{answer:?}");
    }
}

#[test]
fn the_question_is_shown_before_reading_and_only_the_first_line_counts() {
    let mut input = "no\nyes\n".as_bytes();
    let mut out = Vec::new();
    assert!(!decide(&mut input, &mut out, "Proceed? ").expect("read"));
    assert_eq!(out, b"Proceed? ");
    assert_eq!(input, b"yes\n", "the second line was not consumed");
}

#[test]
fn the_plan_lists_every_snapshot_with_its_fate_and_the_objects_that_go() {
    let text = render(|out| human::plan(out, &planned(), &policy()));
    let expected = "\
Policy       keep last 1, daily 7
Snapshots    3 (2 kept, 1 to remove)

  REMOVE  aaaaaaaa  2023-11-14T22:13:20Z
  KEEP    bbbbbbbb  2023-11-15T22:13:20Z  last, daily
  KEEP    cccccccc  2023-11-16T22:13:20Z  last

Chunks       40 in use by the kept snapshots
Unreferenced 3 objects (3.5 KiB) that no kept snapshot needs
";
    assert_eq!(text, expected);
}

#[test]
fn a_plan_without_snapshots_has_no_empty_table() {
    let report = PruneReport {
        decisions: Vec::new(),
        chunks_unreferenced: 1,
        ..planned()
    };
    let text = render(|out| human::plan(out, &report, &policy()));
    let expected = "\
Policy       keep last 1, daily 7
Snapshots    0 (0 kept, 0 to remove)

Chunks       40 in use by the kept snapshots
Unreferenced 1 object (3.5 KiB) that no kept snapshot needs
";
    assert_eq!(text, expected);
}

#[test]
fn the_policy_is_written_in_the_order_of_the_rules_and_only_with_the_rules_that_are_set() {
    assert_eq!(human::policy_text(&policy()), "keep last 1, daily 7");
    let all = RetentionPolicy {
        keep_last: Some(3),
        keep_daily: Some(7),
        keep_weekly: Some(4),
        keep_monthly: Some(12),
        keep_yearly: Some(2),
    };
    assert_eq!(human::policy_text(&all), "keep last 3, daily 7, weekly 4, monthly 12, yearly 2");
    let only_yearly = RetentionPolicy {
        keep_yearly: Some(5),
        ..RetentionPolicy::default()
    };
    assert_eq!(human::policy_text(&only_yearly), "keep yearly 5");
}

#[test]
fn the_outcome_reports_what_was_removed_and_that_the_kept_snapshots_are_whole() {
    let text = render(|out| human::outcome(out, &executed()));
    let expected = "
Removed      1 snapshot and 3 chunk objects (3.5 KiB)
Verified     all 40 chunks used by the kept snapshots are present
Took         1.5s
";
    assert_eq!(text, expected);
}

#[test]
fn objects_that_could_not_be_deleted_are_counted_in_the_outcome() {
    let report = PruneReport {
        chunks_removed: 1,
        failures: vec![failure("data/ab/x", "denied")],
        failure_count: 2,
        ..executed()
    };
    let text = render(|out| human::outcome(out, &report));
    let expected = "
Removed      1 snapshot and 1 chunk object (3.5 KiB)
Not removed  2 objects (listed on standard error; harmless, and the next prune tries again)
Verified     all 40 chunks used by the kept snapshots are present
Took         1.5s
";
    assert_eq!(text, expected);
}

#[test]
fn the_question_says_what_is_deleted_and_that_it_cannot_be_undone() {
    assert_eq!(
        human::question(&planned()),
        "This permanently deletes 1 snapshot and 3 unreferenced chunk objects (3.5 KiB) from \
         the repository.\nThis cannot be undone. Type \"yes\" to continue: "
    );
    let more = PruneReport {
        decisions: vec![
            decision("aa", 1, &[]),
            decision("bb", 2, &[]),
            decision("cc", 3, &[KeepReason::Last]),
        ],
        chunks_unreferenced: 1,
        bytes_unreferenced: 0,
        ..planned()
    };
    assert!(
        human::question(&more).starts_with(
            "This permanently deletes 2 snapshots and 1 unreferenced chunk object (0 B) from"
        ),
        "{}",
        human::question(&more)
    );
}

#[test]
fn failures_are_listed_safely_with_the_number_that_are_not_listed() {
    let report = PruneReport {
        failures: vec![
            failure("data/ab/ab01", "permission denied"),
            failure("data/cd/cd02", "denied\n\u{1b}[2Jgone"),
        ],
        failure_count: 5,
        ..executed()
    };
    assert_eq!(
        human::failures(&report),
        vec![
            "  data/ab/ab01: permission denied".to_owned(),
            "  data/cd/cd02: denied\\n\\x1b[2Jgone".to_owned(),
            "  and 3 more".to_owned(),
        ]
    );
    assert!(human::failures(&executed()).is_empty());
}
