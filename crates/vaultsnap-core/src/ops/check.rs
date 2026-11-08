use crate::error::{Error, Result};
use crate::repository::Repository;

/// Inputs of an integrity check.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CheckOptions {
    /// Also read and authenticate every stored pack, not just the metadata.
    pub read_data: bool,
}

/// Outcome of an integrity check.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckReport {
    /// Human-readable descriptions of every problem found.
    pub problems: Vec<String>,
}

impl CheckReport {
    /// True when no problems were found.
    pub fn is_ok(&self) -> bool {
        self.problems.is_empty()
    }
}

impl Repository {
    /// Verifies repository structure and, optionally, stored data.
    pub async fn check(&self, options: &CheckOptions) -> Result<CheckReport> {
        let _ = options;
        Err(Error::not_implemented("check"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_report_is_ok() {
        assert!(CheckReport::default().is_ok());
    }

    #[test]
    fn report_with_problems_is_not_ok() {
        let report = CheckReport {
            problems: vec!["missing pack".to_owned()],
        };
        assert!(!report.is_ok());
    }

    #[tokio::test]
    async fn check_is_not_implemented() {
        let err = Repository::at("/unused")
            .check(&CheckOptions::default())
            .await
            .expect_err("must fail");
        assert!(matches!(err, Error::NotImplemented { operation: "check" }));
    }
}
