//! `vaultsnap`: command-line interface for the VaultSnap encrypted backup utility.

mod backup;
mod check;
mod cli;
mod commands;
mod format;
mod inspect;
mod interrupt;
mod logging;
mod passphrase;
mod progress;
mod prune;
mod restore;

use std::process::ExitCode;

use clap::Parser;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    logging::init(cli.verbose);
    match commands::run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // Messages can carry paths from a snapshot, which may hold anything.
            eprintln!("error: {}", format::escape(err.to_string().as_bytes()));
            ExitCode::from(err.exit_code())
        }
    }
}
