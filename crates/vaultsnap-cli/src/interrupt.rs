//! Turning Ctrl-C and SIGTERM into a request to stop a running command cleanly.

use tokio::task::JoinHandle;
use vaultsnap_core::CancelToken;

/// Asks the running command to stop when Ctrl-C or SIGTERM arrives, after printing `message`. The
/// command then ends at its next safe point, so the repository and the files it was writing stay
/// consistent. A second signal changes nothing, so the command always ends cleanly (SIGKILL forces
/// it, which can leave a stale lock file behind).
///
/// The returned handle can be aborted to stop listening, for a moment when a command waits for an
/// answer from a person instead (see [`interrupted`]).
pub fn cancel_on_interrupt(cancel: CancelToken, message: &'static str) -> JoinHandle<()> {
    tokio::spawn(async move {
        interrupted().await;
        eprintln!("\n{message}");
        cancel.cancel();
    })
}

/// Completes when Ctrl-C is pressed or SIGTERM arrives, and never if the signal cannot be
/// observed.
pub async fn interrupted() {
    wait_for_interrupt().await;
}

/// Completes when Ctrl-C is pressed, and never if the signal cannot be observed.
async fn ctrl_c() {
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
}

#[cfg(unix)]
async fn wait_for_interrupt() {
    use tokio::signal::unix::{signal, SignalKind};

    match signal(SignalKind::terminate()) {
        Ok(mut terminate) => {
            tokio::select! {
                () = ctrl_c() => {}
                _ = terminate.recv() => {}
            }
        }
        Err(_) => ctrl_c().await,
    }
}

#[cfg(not(unix))]
async fn wait_for_interrupt() {
    ctrl_c().await;
}
