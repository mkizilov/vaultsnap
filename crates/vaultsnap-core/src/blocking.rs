//! Bridge from the synchronous store to the async operation API.

use crate::error::{Error, Result};

/// Runs blocking filesystem work on Tokio's blocking pool so it never stalls the executor.
pub(crate) async fn blocking<T, F>(task: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(task)
        .await
        .map_err(|err| Error::Task(err.to_string()))?
}
