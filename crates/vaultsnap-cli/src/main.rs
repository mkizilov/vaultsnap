//! `vaultsnap`: command-line interface for the VaultSnap encrypted backup utility.

mod cli;
mod commands;
mod logging;
mod passphrase;

use std::process::ExitCode;

use clap::Parser;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    logging::init(cli.verbose);
    match commands::run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}
