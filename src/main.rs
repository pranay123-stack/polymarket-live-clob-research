//! Entry point for the Polymarket CLOB execution research platform.
//!
//! Read-only by design: no key material, no signing, no order placement.

use anyhow::{Context, Result};
use clap::Parser;

use polymarket_live_clob_research::cli::{self, Cli, Command};
use polymarket_live_clob_research::commands;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    cli::init_tracing(&cli.log)?;

    match cli.command {
        Command::Record(args) => commands::record(args).await.context("record"),
        Command::Inspect(args) => commands::inspect(args).context("inspect"),
        Command::Replay(args) => commands::replay(args).context("replay"),
        Command::Shadow(args) => commands::shadow(args).await.context("shadow"),
        Command::Analyze(args) => commands::analyze(args).context("analyze"),
        Command::VerifyReplay(args) => commands::verify_replay(args).context("verify-replay"),
    }
}
