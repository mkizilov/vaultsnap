//! Retention policy used by `prune`.

use crate::error::{Error, Result};

/// Which snapshots `prune` keeps. A snapshot is kept if any rule selects it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Keep the most recent N snapshots.
    pub keep_last: Option<u32>,
    /// Keep the newest snapshot of each of the last N days.
    pub keep_daily: Option<u32>,
    /// Keep the newest snapshot of each of the last N weeks.
    pub keep_weekly: Option<u32>,
    /// Keep the newest snapshot of each of the last N months.
    pub keep_monthly: Option<u32>,
    /// Keep the newest snapshot of each of the last N years.
    pub keep_yearly: Option<u32>,
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
