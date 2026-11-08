//! Retention policy used by `prune`: which snapshots to keep.
//!
//! A policy is a set of rules, and a snapshot is kept if **any** rule selects it. All snapshots of
//! a repository are one history; there are no groups (by host, by path, by tag).
//!
//! * `keep_last N` keeps the N newest snapshots.
//! * `keep_daily N` keeps, for each of the N most recent **days that have a snapshot**, the newest
//!   snapshot of that day.
//! * `keep_weekly`, `keep_monthly`, `keep_yearly` do the same with weeks, months, and years.
//!
//! Days, months, and years are those of the **UTC** calendar and weeks are ISO weeks (Monday to
//! Sunday), so the outcome does not depend on the time zone of the machine that prunes. "The last
//! N periods that have a snapshot" rather than "the last N calendar periods before today" means
//! the outcome does not depend on the clock either: pruning a repository twice, or on another
//! day, selects the same snapshots, and a long pause in backups does not make the history vanish.
//!
//! Times are the `created` times recorded inside the authenticated manifests, never file times.
//! Snapshots with the same time are ordered by id, so the choice is deterministic.
//!
//! The newest snapshot is always kept by every rule, so a valid policy can never delete it.

use crate::error::{Error, Result};
use crate::scan::Timestamp;
use crate::snapshot::SnapshotId;

const SECONDS_PER_DAY: i64 = 86_400;

/// Which snapshots `prune` keeps. A snapshot is kept if any rule selects it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Keep the most recent N snapshots.
    pub keep_last: Option<u32>,
    /// Keep the newest snapshot of each of the last N days that have a snapshot.
    pub keep_daily: Option<u32>,
    /// Keep the newest snapshot of each of the last N weeks that have a snapshot.
    pub keep_weekly: Option<u32>,
    /// Keep the newest snapshot of each of the last N months that have a snapshot.
    pub keep_monthly: Option<u32>,
    /// Keep the newest snapshot of each of the last N years that have a snapshot.
    pub keep_yearly: Option<u32>,
}

/// Why a snapshot is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum KeepReason {
    /// One of the newest N.
    Last,
    /// The newest of its day, among the last N days.
    Daily,
    /// The newest of its week, among the last N weeks.
    Weekly,
    /// The newest of its month, among the last N months.
    Monthly,
    /// The newest of its year, among the last N years.
    Yearly,
}

impl KeepReason {
    /// The name used in output; never changes.
    pub fn name(self) -> &'static str {
        match self {
            Self::Last => "last",
            Self::Daily => "daily",
            Self::Weekly => "weekly",
            Self::Monthly => "monthly",
            Self::Yearly => "yearly",
        }
    }
}

/// A snapshot as retention sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The snapshot.
    pub id: SnapshotId,
    /// When it was taken, from its authenticated manifest.
    pub created: Timestamp,
}

/// What the policy decides about one snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// The snapshot.
    pub id: SnapshotId,
    /// When it was taken.
    pub created: Timestamp,
    /// The rules that keep it, in the order [`KeepReason`] lists them; empty means it is removed.
    pub keep: Vec<KeepReason>,
}

impl Decision {
    /// True if the snapshot is kept.
    pub fn is_kept(&self) -> bool {
        !self.keep.is_empty()
    }
}

impl RetentionPolicy {
    /// Rejects policies that would delete every snapshot or contain a zero count.
    pub fn validate(&self) -> Result<()> {
        let rules = [
            self.keep_last,
            self.keep_daily,
            self.keep_weekly,
            self.keep_monthly,
            self.keep_yearly,
        ];
        if rules.iter().all(Option::is_none) {
            return Err(Error::InvalidRetention("at least one keep rule is required"));
        }
        if rules.contains(&Some(0)) {
            return Err(Error::InvalidRetention("keep counts must be greater than zero"));
        }
        Ok(())
    }

    /// Decides, for every snapshot, whether it is kept and why. The result is ordered oldest first
    /// and does not depend on the order of `snapshots`.
    ///
    /// Fails if the policy is not valid, so a policy that keeps nothing cannot be applied.
    pub fn select(&self, snapshots: &[Candidate]) -> Result<Vec<Decision>> {
        self.validate()?;
        let mut newest_first: Vec<&Candidate> = snapshots.iter().collect();
        newest_first.sort_by(|a, b| {
            b.created
                .cmp(&a.created)
                .then_with(|| b.id.cmp(&a.id))
        });
        let mut reasons: Vec<Vec<KeepReason>> = vec![Vec::new(); newest_first.len()];

        if let Some(count) = self.keep_last {
            let count = usize::try_from(count).unwrap_or(usize::MAX);
            for slot in reasons.iter_mut().take(count) {
                slot.push(KeepReason::Last);
            }
        }
        mark_periods(&newest_first, &mut reasons, self.keep_daily, KeepReason::Daily, day_of);
        mark_periods(&newest_first, &mut reasons, self.keep_weekly, KeepReason::Weekly, week_of);
        mark_periods(&newest_first, &mut reasons, self.keep_monthly, KeepReason::Monthly, month_of);
        mark_periods(&newest_first, &mut reasons, self.keep_yearly, KeepReason::Yearly, year_of);

        Ok(newest_first
            .into_iter()
            .zip(reasons)
            .rev()
            .map(|(candidate, keep)| Decision {
                id: candidate.id.clone(),
                created: candidate.created,
                keep,
            })
            .collect())
    }
}

/// Marks the newest snapshot of each of the first `count` distinct periods. `snapshots` is
/// ordered newest first, so the snapshots of one period are next to each other and the first of
/// them is the newest.
fn mark_periods(
    snapshots: &[&Candidate],
    reasons: &mut [Vec<KeepReason>],
    count: Option<u32>,
    reason: KeepReason,
    period_of: fn(Timestamp) -> i64,
) {
    let Some(count) = count else {
        return;
    };
    let mut remaining = count;
    let mut current: Option<i64> = None;
    for (candidate, slot) in snapshots.iter().zip(reasons.iter_mut()) {
        let period = period_of(candidate.created);
        if current == Some(period) {
            continue;
        }
        if remaining == 0 {
            break;
        }
        current = Some(period);
        remaining -= 1;
        slot.push(reason);
    }
}

/// Days since 1970-01-01 (UTC) of a point in time; negative before.
fn day_of(time: Timestamp) -> i64 {
    time.secs.div_euclid(SECONDS_PER_DAY)
}

/// A number that is the same for all days of one ISO week and different for different weeks. The
/// epoch was a Thursday, so Monday-based weeks are counted from three days before it.
fn week_of(time: Timestamp) -> i64 {
    (day_of(time) + 3).div_euclid(7)
}

/// A number that is the same for all days of one calendar month and different for different ones.
fn month_of(time: Timestamp) -> i64 {
    let (year, month) = civil_from_days(day_of(time));
    year * 12 + i64::from(month) - 1
}

fn year_of(time: Timestamp) -> i64 {
    civil_from_days(day_of(time)).0
}

/// The (year, month) of a day count since 1970-01-01 in the proleptic Gregorian calendar
/// (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, u32::try_from(month).unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Days since 1970-01-01 of a calendar date (Hinnant's `days_from_civil`), used to write the
    /// times below as the dates they stand for.
    fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
        let year = if month <= 2 { year - 1 } else { year };
        let era = year.div_euclid(400);
        let year_of_era = year.rem_euclid(400);
        let month_index = if month > 2 { month - 3 } else { month + 9 };
        let day_of_year = (153 * month_index + 2) / 5 + day - 1;
        let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
        era * 146_097 + day_of_era - 719_468
    }

    fn at(year: i64, month: i64, day: i64, hour: i64, minute: i64) -> Timestamp {
        Timestamp {
            secs: days_from_civil(year, month, day) * SECONDS_PER_DAY + hour * 3600 + minute * 60,
            nanos: 0,
        }
    }

    fn id(n: u8) -> SnapshotId {
        format!("{n:02x}").repeat(32).parse().expect("valid id")
    }

    fn candidate(n: u8, created: Timestamp) -> Candidate {
        Candidate { id: id(n), created }
    }

    fn policy(last: u32, daily: u32, weekly: u32, monthly: u32, yearly: u32) -> RetentionPolicy {
        let rule = |count: u32| (count > 0).then_some(count);
        RetentionPolicy {
            keep_last: rule(last),
            keep_daily: rule(daily),
            keep_weekly: rule(weekly),
            keep_monthly: rule(monthly),
            keep_yearly: rule(yearly),
        }
    }

    /// The ids kept, oldest first, as numbers.
    fn kept(policy: &RetentionPolicy, snapshots: &[Candidate]) -> Vec<u8> {
        let decisions = policy.select(snapshots).expect("valid policy");
        decisions
            .iter()
            .filter(|decision| decision.is_kept())
            .map(|decision| u8::from_str_radix(&decision.id.as_str()[..2], 16).expect("hex"))
            .collect()
    }

    // --- the calendar -----------------------------------------------------------------------

    #[test]
    fn dates_are_converted_correctly_across_the_calendar() {
        // Anchors worked out by hand: 1970 to 2000 is 30 years with 7 leap days (10 957 days).
        let known = [
            (0, (1970, 1)),
            (-1, (1969, 12)),
            (59, (1970, 3)),
            (10_956, (1999, 12)),
            (10_957, (2000, 1)),
            (11_016, (2000, 2)),
            (11_017, (2000, 3)),
            (19_782, (2024, 2)),
            (19_783, (2024, 3)),
            (-719_468, (0, 3)),
        ];
        for (days, expected) in known {
            assert_eq!(civil_from_days(days), expected, "day {days}");
        }
    }

    #[test]
    fn every_day_belongs_to_the_month_that_the_inverse_conversion_says() {
        for days in -60_000..80_000_i64 {
            let (year, month) = civil_from_days(days);
            let month = i64::from(month);
            let (next_year, next_month) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
            assert!(days_from_civil(year, month, 1) <= days, "day {days}");
            assert!(days < days_from_civil(next_year, next_month, 1), "day {days}");
        }
    }

    #[test]
    fn weeks_run_from_monday_to_sunday() {
        // 2024-06-10 was a Monday.
        let monday = at(2024, 6, 10, 0, 0);
        let sunday = at(2024, 6, 16, 23, 59);
        let next_monday = at(2024, 6, 17, 0, 0);
        let previous_sunday = at(2024, 6, 9, 23, 59);
        assert_eq!(week_of(monday), week_of(sunday));
        assert_ne!(week_of(sunday), week_of(next_monday));
        assert_ne!(week_of(previous_sunday), week_of(monday));
        assert_eq!(week_of(next_monday), week_of(monday) + 1);
        // The epoch is a Thursday: the week of 1969-12-29 (a Monday) to 1970-01-04 is one week.
        assert_eq!(week_of(at(1969, 12, 29, 0, 0)), week_of(at(1970, 1, 4, 23, 59)));
        assert_ne!(week_of(at(1969, 12, 28, 23, 59)), week_of(at(1969, 12, 29, 0, 0)));
    }

    #[test]
    fn iso_weeks_straddle_the_new_year_like_the_calendar_says() {
        // 2020-12-31 (Thursday) and 2021-01-03 (Sunday) are both in ISO week 53 of 2020.
        assert_eq!(week_of(at(2020, 12, 31, 12, 0)), week_of(at(2021, 1, 3, 12, 0)));
        // 2021-01-04 is the Monday of ISO week 1 of 2021.
        assert_ne!(week_of(at(2021, 1, 3, 23, 59)), week_of(at(2021, 1, 4, 0, 0)));
        // ... while months and years split at midnight.
        assert_ne!(month_of(at(2020, 12, 31, 23, 59)), month_of(at(2021, 1, 1, 0, 0)));
        assert_ne!(year_of(at(2020, 12, 31, 23, 59)), year_of(at(2021, 1, 1, 0, 0)));
        assert_eq!(year_of(at(2020, 1, 1, 0, 0)), year_of(at(2020, 12, 31, 23, 59)));
    }

    #[test]
    fn leap_days_and_month_ends_belong_to_their_months() {
        assert_eq!(month_of(at(2024, 2, 29, 23, 59)), month_of(at(2024, 2, 1, 0, 0)));
        assert_ne!(month_of(at(2024, 2, 29, 23, 59)), month_of(at(2024, 3, 1, 0, 0)));
        assert_ne!(month_of(at(2023, 2, 28, 23, 59)), month_of(at(2023, 3, 1, 0, 0)));
        assert_ne!(month_of(at(2024, 1, 31, 23, 59)), month_of(at(2024, 2, 1, 0, 0)));
        // The same month number in different years is a different month.
        assert_ne!(month_of(at(2023, 5, 10, 0, 0)), month_of(at(2024, 5, 10, 0, 0)));
    }

    #[test]
    fn times_before_the_epoch_fall_in_the_right_periods() {
        let late = Timestamp { secs: -1, nanos: 999_999_999 };
        let early = Timestamp { secs: 0, nanos: 0 };
        assert_ne!(day_of(late), day_of(early), "the last second of 1969 is not 1970-01-01");
        assert_eq!(day_of(late), -1);
        assert_ne!(year_of(late), year_of(early));
        assert_eq!(year_of(Timestamp { secs: -SECONDS_PER_DAY * 366, nanos: 0 }), 1968);
    }

    // --- keep_last ---------------------------------------------------------------------------

    fn daily_series(count: u8) -> Vec<Candidate> {
        (0..count)
            .map(|n| candidate(n + 1, at(2024, 3, 1 + i64::from(n), 12, 0)))
            .collect()
    }

    #[test]
    fn keep_last_keeps_the_newest_n() {
        let snapshots = daily_series(5);
        assert_eq!(kept(&policy(3, 0, 0, 0, 0), &snapshots), [3, 4, 5]);
        assert_eq!(kept(&policy(1, 0, 0, 0, 0), &snapshots), [5]);
    }

    #[test]
    fn keeping_more_than_there_are_keeps_everything() {
        let snapshots = daily_series(3);
        assert_eq!(kept(&policy(100, 0, 0, 0, 0), &snapshots), [1, 2, 3]);
        assert_eq!(kept(&policy(0, 100, 0, 0, 0), &snapshots), [1, 2, 3]);
        assert_eq!(kept(&policy(0, 0, 100, 100, 100), &snapshots), [3]);
    }

    #[test]
    fn an_empty_history_and_a_single_snapshot_are_handled() {
        assert!(policy(3, 0, 0, 0, 0).select(&[]).expect("valid").is_empty());
        let one = [candidate(7, at(2024, 1, 1, 0, 0))];
        for rule in [policy(1, 0, 0, 0, 0), policy(0, 1, 0, 0, 0), policy(0, 0, 1, 0, 0), policy(0, 0, 0, 1, 0), policy(0, 0, 0, 0, 1)] {
            assert_eq!(kept(&rule, &one), [7]);
        }
    }

    // --- periods -----------------------------------------------------------------------------

    #[test]
    fn daily_keeps_the_newest_of_each_of_the_last_n_days_that_have_a_snapshot() {
        // Three snapshots on the 10th, none on the 11th, one on the 12th and two on the 13th.
        let snapshots = [
            candidate(1, at(2024, 3, 10, 8, 0)),
            candidate(2, at(2024, 3, 10, 20, 0)),
            candidate(3, at(2024, 3, 10, 23, 59)),
            candidate(4, at(2024, 3, 12, 9, 0)),
            candidate(5, at(2024, 3, 13, 1, 0)),
            candidate(6, at(2024, 3, 13, 23, 0)),
        ];
        assert_eq!(kept(&policy(0, 1, 0, 0, 0), &snapshots), [6]);
        assert_eq!(kept(&policy(0, 2, 0, 0, 0), &snapshots), [4, 6]);
        assert_eq!(kept(&policy(0, 3, 0, 0, 0), &snapshots), [3, 4, 6], "the empty 11th does not count");
        assert_eq!(kept(&policy(0, 4, 0, 0, 0), &snapshots), [3, 4, 6]);
    }

    #[test]
    fn daily_splits_exactly_at_midnight_utc() {
        let snapshots = [
            candidate(1, at(2024, 3, 10, 23, 59)),
            candidate(2, at(2024, 3, 11, 0, 0)),
        ];
        assert_eq!(kept(&policy(0, 2, 0, 0, 0), &snapshots), [1, 2]);
        let same_day = [
            candidate(1, at(2024, 3, 10, 0, 0)),
            candidate(2, at(2024, 3, 10, 23, 59)),
        ];
        assert_eq!(kept(&policy(0, 2, 0, 0, 0), &same_day), [2]);
    }

    #[test]
    fn weekly_keeps_the_newest_of_each_week_and_splits_between_sunday_and_monday() {
        let snapshots = [
            candidate(1, at(2024, 6, 3, 10, 0)),
            candidate(2, at(2024, 6, 9, 23, 59)),
            candidate(3, at(2024, 6, 10, 0, 0)),
            candidate(4, at(2024, 6, 12, 12, 0)),
            candidate(5, at(2024, 6, 16, 23, 59)),
            candidate(6, at(2024, 6, 17, 0, 0)),
        ];
        assert_eq!(kept(&policy(0, 0, 1, 0, 0), &snapshots), [6]);
        assert_eq!(kept(&policy(0, 0, 2, 0, 0), &snapshots), [5, 6]);
        assert_eq!(kept(&policy(0, 0, 3, 0, 0), &snapshots), [2, 5, 6]);
        assert_eq!(kept(&policy(0, 0, 4, 0, 0), &snapshots), [2, 5, 6]);
    }

    #[test]
    fn monthly_and_yearly_split_at_the_end_of_the_month_and_year() {
        let snapshots = [
            candidate(1, at(2022, 12, 31, 23, 59)),
            candidate(2, at(2023, 1, 1, 0, 0)),
            candidate(3, at(2023, 1, 31, 23, 59)),
            candidate(4, at(2023, 2, 1, 0, 0)),
            candidate(5, at(2023, 2, 28, 12, 0)),
            candidate(6, at(2024, 2, 29, 12, 0)),
        ];
        assert_eq!(kept(&policy(0, 0, 0, 1, 0), &snapshots), [6]);
        assert_eq!(kept(&policy(0, 0, 0, 2, 0), &snapshots), [5, 6]);
        assert_eq!(kept(&policy(0, 0, 0, 3, 0), &snapshots), [3, 5, 6]);
        assert_eq!(kept(&policy(0, 0, 0, 5, 0), &snapshots), [1, 3, 5, 6]);
        assert_eq!(kept(&policy(0, 0, 0, 0, 1), &snapshots), [6]);
        assert_eq!(kept(&policy(0, 0, 0, 0, 2), &snapshots), [5, 6]);
        assert_eq!(kept(&policy(0, 0, 0, 0, 3), &snapshots), [1, 5, 6]);
    }

    #[test]
    fn a_gap_in_the_history_does_not_push_old_snapshots_out() {
        // A year without backups: the previous history is still "the last" months.
        let snapshots = [
            candidate(1, at(2022, 1, 15, 0, 0)),
            candidate(2, at(2022, 2, 15, 0, 0)),
            candidate(3, at(2023, 3, 15, 0, 0)),
        ];
        assert_eq!(kept(&policy(0, 0, 0, 2, 0), &snapshots), [2, 3]);
    }

    // --- combining rules -----------------------------------------------------------------------

    #[test]
    fn a_snapshot_is_kept_if_any_rule_selects_it_and_says_which() {
        let snapshots = [
            candidate(1, at(2024, 1, 20, 12, 0)),
            candidate(2, at(2024, 2, 20, 12, 0)),
            candidate(3, at(2024, 3, 20, 12, 0)),
            candidate(4, at(2024, 3, 21, 12, 0)),
        ];
        let rule = policy(1, 1, 0, 3, 0);
        let decisions = rule.select(&snapshots).expect("valid");
        let described: Vec<(u8, Vec<KeepReason>)> = decisions
            .iter()
            .map(|d| (u8::from_str_radix(&d.id.as_str()[..2], 16).expect("hex"), d.keep.clone()))
            .collect();
        assert_eq!(
            described,
            [
                (1, vec![KeepReason::Monthly]),
                (2, vec![KeepReason::Monthly]),
                // Not the newest of March, and no other rule selects it: removed.
                (3, vec![]),
                (4, vec![KeepReason::Last, KeepReason::Daily, KeepReason::Monthly]),
            ]
        );
    }

    #[test]
    fn the_reasons_of_one_snapshot_are_listed_in_the_order_of_the_rules() {
        let snapshots = [candidate(1, at(2024, 3, 21, 12, 0))];
        let decisions = policy(1, 1, 1, 1, 1).select(&snapshots).expect("valid");
        assert_eq!(
            decisions[0].keep,
            [KeepReason::Last, KeepReason::Daily, KeepReason::Weekly, KeepReason::Monthly, KeepReason::Yearly]
        );
        let names: Vec<&str> = decisions[0].keep.iter().map(|reason| reason.name()).collect();
        assert_eq!(names, ["last", "daily", "weekly", "monthly", "yearly"]);
    }

    #[test]
    fn different_rules_may_keep_different_snapshots_of_the_same_period() {
        // keep-last keeps the two newest; weekly keeps the newest of the week, which is one of them.
        let snapshots = [
            candidate(1, at(2024, 3, 11, 9, 0)),
            candidate(2, at(2024, 3, 12, 9, 0)),
            candidate(3, at(2024, 3, 13, 9, 0)),
        ];
        assert_eq!(kept(&policy(2, 0, 1, 0, 0), &snapshots), [2, 3]);
        assert_eq!(kept(&policy(0, 0, 1, 0, 0), &snapshots), [3]);
    }

    // --- determinism and safety ----------------------------------------------------------------

    #[test]
    fn snapshots_with_the_same_time_are_ordered_by_id() {
        let time = at(2024, 3, 10, 12, 0);
        let snapshots = [candidate(5, time), candidate(9, time), candidate(2, time)];
        assert_eq!(kept(&policy(1, 0, 0, 0, 0), &snapshots), [9], "the greatest id counts as newest");
        assert_eq!(kept(&policy(0, 1, 0, 0, 0), &snapshots), [9]);
        let all = policy(3, 0, 0, 0, 0).select(&snapshots).expect("valid");
        let order: Vec<&str> = all.iter().map(|d| &d.id.as_str()[..2]).collect();
        assert_eq!(order, ["02", "05", "09"], "oldest first, ties by id");
    }

    #[test]
    fn the_outcome_does_not_depend_on_the_order_of_the_input() {
        let mut snapshots = vec![
            candidate(1, at(2024, 1, 5, 0, 0)),
            candidate(2, at(2024, 1, 6, 0, 0)),
            candidate(3, at(2024, 2, 6, 0, 0)),
            candidate(4, at(2024, 2, 7, 0, 0)),
            candidate(5, at(2024, 6, 7, 0, 0)),
            candidate(6, at(2025, 6, 7, 0, 0)),
        ];
        let rule = policy(2, 1, 2, 3, 2);
        let expected = rule.select(&snapshots).expect("valid");
        for rotation in 1..snapshots.len() {
            snapshots.rotate_left(rotation);
            assert_eq!(rule.select(&snapshots).expect("valid"), expected, "rotation {rotation}");
        }
        snapshots.reverse();
        assert_eq!(rule.select(&snapshots).expect("valid"), expected, "reversed");
    }

    #[test]
    fn the_newest_snapshot_is_kept_by_every_single_rule() {
        let snapshots = [
            candidate(1, at(2020, 5, 1, 0, 0)),
            candidate(2, at(2023, 8, 17, 11, 0)),
            candidate(3, at(2024, 2, 29, 23, 59)),
        ];
        for rule in [
            policy(1, 0, 0, 0, 0),
            policy(0, 1, 0, 0, 0),
            policy(0, 0, 1, 0, 0),
            policy(0, 0, 0, 1, 0),
            policy(0, 0, 0, 0, 1),
        ] {
            let decisions = rule.select(&snapshots).expect("valid");
            assert!(decisions.last().expect("not empty").is_kept(), "{rule:?}");
            assert_eq!(decisions.last().map(|d| d.id.clone()), Some(id(3)));
        }
    }

    #[test]
    fn a_rule_never_keeps_more_than_its_count() {
        let snapshots: Vec<Candidate> = (0..40_u8)
            .map(|n| candidate(n + 1, at(2023, 1, 1, 0, 0)).with_days(i64::from(n) * 9))
            .collect();
        for count in 1..=6_u32 {
            let by_rule = |rule: RetentionPolicy| {
                rule.select(&snapshots).expect("valid").iter().filter(|d| d.is_kept()).count()
            };
            assert_eq!(by_rule(policy(count, 0, 0, 0, 0)), count as usize);
            assert!(by_rule(policy(0, count, 0, 0, 0)) <= count as usize);
            assert!(by_rule(policy(0, 0, count, 0, 0)) <= count as usize);
            assert!(by_rule(policy(0, 0, 0, count, 0)) <= count as usize);
            assert!(by_rule(policy(0, 0, 0, 0, count)) <= count as usize);
        }
    }

    impl Candidate {
        fn with_days(mut self, days: i64) -> Self {
            self.created.secs += days * SECONDS_PER_DAY;
            self
        }
    }

    // --- validation ----------------------------------------------------------------------------

    #[test]
    fn empty_policy_is_rejected() {
        let err = RetentionPolicy::default().validate().expect_err("must fail");
        assert!(matches!(err, Error::InvalidRetention(_)));
    }

    #[test]
    fn zero_count_is_rejected() {
        let policy = RetentionPolicy {
            keep_last: Some(3),
            keep_daily: Some(0),
            ..Default::default()
        };
        assert!(policy.validate().is_err());
    }

    #[test]
    fn single_rule_is_accepted() {
        let policy = RetentionPolicy {
            keep_yearly: Some(1),
            ..Default::default()
        };
        assert!(policy.validate().is_ok());
    }

    #[test]
    fn selecting_with_an_invalid_policy_fails_instead_of_keeping_nothing() {
        let snapshots = daily_series(3);
        assert!(matches!(
            RetentionPolicy::default().select(&snapshots),
            Err(Error::InvalidRetention(_))
        ));
        let zero = RetentionPolicy {
            keep_last: Some(0),
            ..Default::default()
        };
        assert!(zero.select(&snapshots).is_err());
        assert!(RetentionPolicy::default().select(&[]).is_err(), "even an empty history is not selected from");
    }
}
