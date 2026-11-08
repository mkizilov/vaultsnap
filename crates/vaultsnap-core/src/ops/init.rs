use crate::error::{Error, Result};
use crate::repository::Repository;

impl Repository {
    /// Creates a new, empty repository at the repository root.
    pub async fn init(&self) -> Result<()> {
        Err(Error::not_implemented("init"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn init_is_not_implemented_and_creates_nothing() {
        let repo = Repository::at("/definitely/not/a/real/path");
        let err = repo.init().await.expect_err("must fail");
        assert!(matches!(err, Error::NotImplemented { operation: "init" }));
        assert!(!repo.layout().root().exists());
    }
}
