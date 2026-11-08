//! Diagnostics setup. Logs go to stderr so stdout stays machine-readable.

use tracing_subscriber::EnvFilter;

/// Installs the global tracing subscriber. `RUST_LOG` overrides the `-v` count.
pub fn init(verbose: u8) {
    let default_level = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
