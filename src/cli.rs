//! Command-line surface.
//!
//! Five verbs, matching the data pipeline end to end:
//!
//! ```text
//! record   live Polymarket data  ->  session file
//! inspect  session file          ->  contents and liquidity summary
//! replay   session file          ->  deterministic book + execution run
//! shadow   live data             ->  hypothetical orders, never sent
//! analyze  session file          ->  ideal vs realistic edge-loss report
//! ```
//!
//! None of them can trade. `shadow` is the closest thing to live trading in
//! this crate and it only ever writes to a report.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

use crate::polymarket::market_discovery::Underlying;
use crate::strategy::ExecStyle;

/// Real-data Polymarket CLOB execution research platform.
#[derive(Debug, Parser)]
#[command(name = "polymarket-clob-research", version, about, long_about = None)]
pub struct Cli {
    /// Log filter, e.g. `info`, `debug`, `polymarket_live_clob_research=debug`.
    #[arg(long, global = true, default_value = "info")]
    pub log: String,

    /// What to do.
    #[command(subcommand)]
    pub command: Command,
}

/// Top-level verbs.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Capture real Polymarket market data to a session file.
    Record(RecordArgs),
    /// Summarise a recorded session.
    Inspect(InspectArgs),
    /// Replay a session deterministically and simulate execution.
    Replay(ReplayArgs),
    /// Observe the live market and simulate orders without sending any.
    Shadow(ShadowArgs),
    /// Produce the ideal-vs-realistic execution report for a session.
    Analyze(AnalyzeArgs),
    /// Replay a session twice and prove the two runs are identical.
    VerifyReplay(VerifyReplayArgs),
}

/// `verify-replay` arguments.
#[derive(Debug, Args)]
pub struct VerifyReplayArgs {
    /// Session file to replay twice.
    #[arg(long)]
    pub file: PathBuf,
    /// Execution realism settings applied to both runs.
    #[command(flatten)]
    pub exec: ExecArgs,
    /// Number of replays to compare. More than two catches rarer drift.
    #[arg(long, default_value_t = 2)]
    pub runs: usize,
}

/// `record` arguments.
#[derive(Debug, Args)]
pub struct RecordArgs {
    /// Underlying to follow.
    #[arg(long, value_enum, default_value = "btc")]
    pub underlying: Underlying,
    /// Recording length in seconds.
    #[arg(long, default_value_t = 600)]
    pub seconds: u64,
    /// Directory to write the session file into.
    #[arg(long, default_value = "data")]
    pub out: PathBuf,
    /// Rounds to subscribe to ahead of the current one.
    #[arg(long, default_value_t = 3)]
    pub lookahead: usize,
    /// Seconds between market re-discovery passes.
    #[arg(long, default_value_t = 60)]
    pub refresh: u64,
}

/// `inspect` arguments.
#[derive(Debug, Args)]
pub struct InspectArgs {
    /// Session file to summarise.
    #[arg(long)]
    pub file: PathBuf,
    /// Show per-market book detail.
    #[arg(long, default_value_t = false)]
    pub detail: bool,
}

/// `replay` arguments.
#[derive(Debug, Args)]
pub struct ReplayArgs {
    /// Session file to replay.
    #[arg(long)]
    pub file: PathBuf,
    /// Execution realism preset.
    #[command(flatten)]
    pub exec: ExecArgs,
    /// Print a progress line every N events.
    #[arg(long, default_value_t = 0)]
    pub progress: u64,
}

/// `shadow` arguments.
#[derive(Debug, Args)]
pub struct ShadowArgs {
    /// Underlying to follow.
    #[arg(long, value_enum, default_value = "btc")]
    pub underlying: Underlying,
    /// How long to observe, in seconds.
    #[arg(long, default_value_t = 300)]
    pub seconds: u64,
    /// Also write the observed frames to a session file in this directory.
    #[arg(long)]
    pub record_to: Option<PathBuf>,
    /// Execution realism settings applied to hypothetical orders.
    #[command(flatten)]
    pub exec: ExecArgs,
}

/// `analyze` arguments.
#[derive(Debug, Args)]
pub struct AnalyzeArgs {
    /// Session file to analyse.
    #[arg(long)]
    pub file: PathBuf,
    /// Execution realism settings for the realistic leg.
    #[command(flatten)]
    pub exec: ExecArgs,
    /// Write the per-factor edge-loss attribution table to this CSV.
    #[arg(long)]
    pub csv: Option<PathBuf>,
    /// Write every simulated fill from the realistic run to this CSV.
    ///
    /// One row per fill: timing, price, size, maker/taker, fee, and the
    /// slippage against the price the strategy was looking at.
    #[arg(long)]
    pub fills_csv: Option<PathBuf>,
    /// Write the full audit set into this directory.
    ///
    /// Produces `decisions.csv`, `orders.csv`, `fills.csv` and `pnl.csv`,
    /// joined on `decision_id`, so the whole run can be re-derived
    /// independently of this binary.
    #[arg(long)]
    pub csv_dir: Option<PathBuf>,
    /// Print the full lineage of the N worst decisions by execution gap.
    #[arg(long, default_value_t = 3)]
    pub explain: usize,
    /// Attribute edge loss by exact Shapley value rather than a waterfall.
    ///
    /// Shapley is order-independent and costs `2^k` simulation passes for
    /// `k` factors; the waterfall costs `k + 1` but its split depends on the
    /// order the factors are switched on.
    #[arg(long, default_value_t = true)]
    pub shapley: bool,
}

/// Execution realism knobs shared by the simulating verbs.
#[derive(Debug, Clone, Args)]
pub struct ExecArgs {
    /// Market-data latency in milliseconds: how stale the strategy's view is.
    ///
    /// Defaults to `-1`, meaning "measure it from the session itself" using
    /// the recorded `recv - exchange` median.
    #[arg(long, default_value_t = -1)]
    pub md_latency_ms: i64,
    /// Order submission latency in milliseconds.
    #[arg(long, default_value_t = 120)]
    pub submit_latency_ms: i64,
    /// Cancellation latency in milliseconds.
    #[arg(long, default_value_t = 120)]
    pub cancel_latency_ms: i64,
    /// Taker fee in basis points applied to fill notional.
    #[arg(long, default_value_t = 0)]
    pub taker_fee_bps: u32,
    /// Maker fee in basis points applied to fill notional.
    #[arg(long, default_value_t = 0)]
    pub maker_fee_bps: u32,
    /// Queue model for resting limit orders.
    #[arg(long, value_enum, default_value = "pessimistic")]
    pub queue_model: QueueModelArg,
    /// Order size in whole shares.
    #[arg(long, default_value_t = 100)]
    pub order_shares: u64,
    /// Order-book imbalance threshold the reference signal fires on.
    #[arg(long, default_value_t = 0.35)]
    pub signal_threshold: f64,
    /// Minimum milliseconds between decisions.
    #[arg(long, default_value_t = 2_000)]
    pub decision_cooldown_ms: i64,
    /// Milliseconds a resting order waits before being cancelled.
    #[arg(long, default_value_t = 5_000)]
    pub order_ttl_ms: i64,
    /// Starting cash in whole dollars.
    #[arg(long, default_value_t = 10_000)]
    pub cash: i64,
    /// How the reference signal places its orders.
    #[arg(long, value_enum, default_value = "split")]
    pub style: ExecStyle,
    /// Cap on absolute position per token, in whole shares.
    #[arg(long, default_value_t = 500)]
    pub max_position: u64,
    /// Book levels used in the imbalance calculation.
    #[arg(long, default_value_t = 5)]
    pub signal_depth: usize,
    /// Milliseconds after a decision at which the midpoint judges it.
    ///
    /// This is what separates a wrong decision from a badly executed one.
    #[arg(long, default_value_t = 30_000)]
    pub decision_horizon_ms: i64,
}

/// How cancellations ahead of a resting order are treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum QueueModelArg {
    /// Assume every cancellation came from behind: queue never improves.
    Pessimistic,
    /// Split cancellations in proportion to the queue ahead and behind.
    Proportional,
    /// Assume every cancellation came from ahead: queue improves fully.
    Optimistic,
}

impl RecordArgs {
    /// Duration to record for.
    pub fn duration(&self) -> Duration {
        Duration::from_secs(self.seconds)
    }
}

/// Initialises tracing with the requested filter.
pub fn init_tracing(filter: &str) -> Result<()> {
    use tracing_subscriber::EnvFilter;
    let env = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(filter))
        .context("building log filter")?;
    tracing_subscriber::fmt()
        .with_env_filter(env)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init()
        .ok();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn record_defaults_to_btc() {
        let cli = Cli::parse_from(["prog", "record"]);
        match cli.command {
            Command::Record(a) => {
                assert_eq!(a.underlying, Underlying::Btc);
                assert_eq!(a.seconds, 600);
                assert_eq!(a.out, PathBuf::from("data"));
            }
            other => panic!("expected record, got {other:?}"),
        }
    }

    #[test]
    fn md_latency_defaults_to_measure_from_session() {
        let cli = Cli::parse_from(["prog", "replay", "--file", "s.jsonl"]);
        match cli.command {
            Command::Replay(a) => assert_eq!(
                a.exec.md_latency_ms, -1,
                "the default must mean `measure it`, not `assume zero`"
            ),
            other => panic!("expected replay, got {other:?}"),
        }
    }

    #[test]
    fn queue_model_defaults_to_the_conservative_choice() {
        let cli = Cli::parse_from(["prog", "analyze", "--file", "s.jsonl"]);
        match cli.command {
            Command::Analyze(a) => assert_eq!(a.exec.queue_model, QueueModelArg::Pessimistic),
            other => panic!("expected analyze, got {other:?}"),
        }
    }
}
