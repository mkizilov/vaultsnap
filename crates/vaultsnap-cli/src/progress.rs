//! The one-line status shown on a terminal while a long command runs.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// The status line is redrawn at most this often.
const DRAW_INTERVAL_MS: u64 = 200;

/// Decides when the status line may be redrawn. Events arrive from several threads; exactly one of
/// the callers that find a redraw due gets to draw it.
pub struct Ticker {
    started: Instant,
    last_draw_ms: AtomicU64,
}

impl Ticker {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            last_draw_ms: AtomicU64::new(0),
        }
    }

    /// True if a redraw is due now, for one caller only.
    pub fn due(&self) -> bool {
        let now = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.claim(now)
    }

    fn claim(&self, now_ms: u64) -> bool {
        let last = self.last_draw_ms.load(Ordering::Relaxed);
        now_ms.saturating_sub(last) >= DRAW_INTERVAL_MS
            && self
                .last_draw_ms
                .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
    }
}

/// Replaces the status line on standard error with `text`.
pub fn draw(text: &str) {
    let mut stderr = std::io::stderr().lock();
    // A terminal that went away must not fail the command, so write errors are ignored.
    write!(stderr, "\r\x1b[K{text}").and_then(|()| stderr.flush()).ok();
}

/// Erases the status line.
pub fn clear() {
    let mut stderr = std::io::stderr().lock();
    write!(stderr, "\r\x1b[K").and_then(|()| stderr.flush()).ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_redraw_is_due_once_per_interval_and_only_for_one_caller() {
        let ticker = Ticker::new();
        assert!(!ticker.claim(0), "nothing has been drawn for no time at all");
        assert!(!ticker.claim(DRAW_INTERVAL_MS - 1));
        assert!(ticker.claim(DRAW_INTERVAL_MS), "due after the interval");
        assert!(!ticker.claim(DRAW_INTERVAL_MS), "taken: the second caller does not draw");
        assert!(!ticker.claim(DRAW_INTERVAL_MS + 10));
        assert!(ticker.claim(2 * DRAW_INTERVAL_MS));
    }
}
