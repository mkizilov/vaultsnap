//! Diagnostics setup. Logs go to stderr so stdout stays machine-readable.

use tracing_subscriber::EnvFilter;

/// The filter `-v` stands for: VaultSnap's own messages at the requested level and everything
/// else (the HTTP stack and the AWS SDK, which can log request details at debug and trace
/// level) at warnings only.
fn default_directives(verbose: u8) -> String {
    let level = match verbose {
        0 => return "warn".to_owned(),
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    format!("warn,vaultsnap={level},vaultsnap_core={level}")
}

/// Installs the global tracing subscriber. `RUST_LOG` overrides the `-v` count, and with it the
/// protection of the line above: whoever sets `RUST_LOG=trace` asks for everything.
pub fn init(verbose: u8) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(default_directives(verbose)));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbosity_raises_only_vaultsnaps_own_messages() {
        assert_eq!(default_directives(0), "warn");
        assert_eq!(
            default_directives(1),
            "warn,vaultsnap=info,vaultsnap_core=info"
        );
        assert_eq!(
            default_directives(2),
            "warn,vaultsnap=debug,vaultsnap_core=debug"
        );
        assert_eq!(
            default_directives(3),
            "warn,vaultsnap=trace,vaultsnap_core=trace"
        );
        assert_eq!(default_directives(9), default_directives(3));
    }

    #[test]
    fn the_directives_are_valid_filters() {
        for verbose in 0..=4 {
            assert!(
                EnvFilter::try_new(default_directives(verbose)).is_ok(),
                "{verbose}"
            );
        }
    }
}
