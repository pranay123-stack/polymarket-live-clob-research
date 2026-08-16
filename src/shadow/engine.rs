//! Shadow mode: run the strategy against the live market, send nothing.
//!
//! ```text
//! live market ─▶ strategy ─▶ hypothetical order ─▶ execution simulator
//!       │                                                  │
//!       └──────────── what the market did next ◀───────────┘
//! ```
//!
//! Two simulators consume the same live stream: one under naive assumptions
//! and one under full realism. The running difference between them is the
//! reality gap, visible as it accrues.
//!
//! # What shadow mode cannot do
//!
//! It cannot isolate market-data latency. Live, there is only one view of
//! the book — the delayed one that actually arrived — and no way to observe
//! the exchange-time truth alongside it. Separating those two requires the
//! recorded `recv_ms`/`exchange_ms` pair, so the market-data factor is
//! measurable only in `replay` and `analyze`. Shadow mode holds it on for
//! both legs and reports the remaining factors.
//!
//! Nothing here can trade. The simulator's output is a report.

use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::execution::matcher::{ExecConfig, ExecutionSimulator, Realism};
use crate::lineage::Decision;
use crate::market::event::{EventPayload, MarketEvent};
use crate::market::state::MarketState;
use crate::polymarket::api::PolymarketClient;
use crate::polymarket::market_discovery::{self, MarketDescriptor, Underlying};
use crate::polymarket::parser::{FrameError, Normalizer};
use crate::polymarket::websocket::{self, now_ms, FeedConfig, FeedMessage, MARKET_WS_URL};
use crate::portfolio::pnl::Portfolio;
use crate::recorder::storage::{session_path, SessionHeader, SessionWriter, FORMAT_VERSION};
use crate::replay::engine::RunConfig;
use crate::strategy::{ImbalanceStrategy, StrategyContext};
use crate::types::{Price, Usdc};

/// How a shadow session should run.
#[derive(Debug, Clone)]
pub struct ShadowConfig {
    /// Underlying to follow.
    pub underlying: Underlying,
    /// How long to observe.
    pub duration: Duration,
    /// Optional directory to also record the observed frames into.
    pub record_to: Option<std::path::PathBuf>,
    /// Simulation settings.
    pub run: RunConfig,
    /// How long after a decision to measure what the market did next.
    pub horizon_ms: i64,
}

/// One hypothetical decision and what the market did afterwards.
///
/// Holds the strategy's own [`Decision`] verbatim. Side, token, size and
/// timestamp are read from it and are never reconstructed from an execution
/// result — see the module docs on why that distinction matters.
#[derive(Debug, Clone)]
pub struct ShadowDecision {
    /// The decision exactly as the strategy emitted it.
    pub decision: Decision,
    /// Midpoint one horizon after the decision, once known.
    pub mid_after: Option<Price>,
}

impl ShadowDecision {
    /// Midpoint at the moment of the decision, as the strategy saw it.
    pub fn mid_at_decision(&self) -> Option<Price> {
        self.decision.mid
    }

    /// Midpoint move since the decision, in price ticks, signed by direction.
    ///
    /// Positive means the market moved the way the strategy wanted.
    pub fn forward_ticks(&self) -> Option<i64> {
        let before = self.decision.mid?;
        let after = self.mid_after?;
        Some((after.ticks() as i64 - before.ticks() as i64) * self.decision.side.sign())
    }
}

/// Outcome of a shadow session.
#[derive(Debug, Clone)]
pub struct ShadowSummary {
    /// Frames observed.
    pub frames: u64,
    /// Events normalized.
    pub events: u64,
    /// Decisions taken.
    pub decisions: usize,
    /// Decisions whose forward horizon completed.
    pub resolved: usize,
    /// Decisions where the market subsequently moved favourably.
    pub favourable: usize,
    /// P&L under naive assumptions.
    pub ideal_pnl: Usdc,
    /// P&L under full realism.
    pub real_pnl: Usdc,
    /// Fill ratio under naive assumptions.
    pub ideal_fill_ratio: f64,
    /// Fill ratio under full realism.
    pub real_fill_ratio: f64,
    /// Session file written, when recording was requested.
    pub recorded_to: Option<std::path::PathBuf>,
}

impl ShadowSummary {
    /// Edge destroyed by realistic execution.
    pub fn edge_loss(&self) -> Usdc {
        self.ideal_pnl - self.real_pnl
    }
}

/// One simulated leg of the comparison.
struct Leg {
    sim: ExecutionSimulator,
    portfolio: Portfolio,
    strategy: ImbalanceStrategy,
}

impl Leg {
    fn new(cfg: &RunConfig, realism: Realism) -> Leg {
        Leg {
            sim: ExecutionSimulator::new(ExecConfig {
                realism,
                ..cfg.exec.clone()
            }),
            portfolio: Portfolio::new(cfg.starting_cash),
            strategy: ImbalanceStrategy::new(cfg.signal.clone()),
        }
    }

    /// Applies one event, then lets the strategy act on the same view.
    ///
    /// Live, the observed and truth views are one and the same: the only
    /// book that exists is the one that arrived.
    ///
    /// Returns the decisions the strategy took, so the caller records what
    /// the strategy actually chose rather than inferring it.
    fn step(
        &mut self,
        ev: &MarketEvent,
        now: i64,
        state: &MarketState,
        markets: &[MarketDescriptor],
    ) -> Vec<Decision> {
        self.sim.tick(now, state);
        self.settle();
        match &ev.payload {
            EventPayload::Trade {
                asset_id,
                price,
                qty,
                side,
                ..
            } => {
                self.sim.on_trade(asset_id, *price, *qty, *side, now);
                self.settle();
            }
            EventPayload::LevelUpdate {
                asset_id,
                side,
                price,
                qty,
                ..
            } => {
                self.sim.on_level_update(asset_id, *side, *price, *qty);
            }
            _ => {}
        }
        let position = |t: &str| self.portfolio.position(t).map(|p| p.qty).unwrap_or(0);
        let decisions = self.strategy.on_tick(&StrategyContext {
            now_ms: now,
            observed: state,
            markets,
            position: &position,
        });
        for d in &decisions {
            for intent in d.intents.iter().cloned() {
                self.sim.submit(intent, now);
            }
        }
        decisions
    }

    fn settle(&mut self) {
        for f in self.sim.drain_fills() {
            self.portfolio
                .apply_fill(&f.token, f.side, f.qty, f.price, f.fee);
        }
    }

    fn pnl(&self, state: &MarketState) -> Usdc {
        let marks = |t: &str| state.book(t).and_then(|b| b.mid());
        self.portfolio.total_pnl(&marks)
    }
}

/// Observes the live market and reports the reality gap without trading.
pub async fn run(
    client: &PolymarketClient,
    cfg: ShadowConfig,
    mut shutdown: watch::Receiver<bool>,
) -> Result<ShadowSummary> {
    let started_ms = now_ms();
    let deadline = tokio::time::Instant::now() + cfg.duration;

    let now_s = client.server_time().await.unwrap_or(started_ms / 1000);
    let markets = market_discovery::discover(client, cfg.underlying, now_s, 4)
        .await
        .context("discovering markets for shadow session")?;
    if markets.is_empty() {
        anyhow::bail!("no live {} 5-minute markets found", cfg.underlying.prefix());
    }
    let tokens: Vec<String> = markets
        .iter()
        .flat_map(|m| [m.up_token.clone(), m.down_token.clone()])
        .collect();

    let mut writer = match &cfg.record_to {
        Some(dir) => {
            let path = session_path(dir, cfg.underlying, started_ms);
            let header = SessionHeader {
                v: FORMAT_VERSION,
                kind: "header".into(),
                tool: format!(
                    "{}/{} shadow",
                    env!("CARGO_PKG_NAME"),
                    env!("CARGO_PKG_VERSION")
                ),
                started_ms,
                clock: client.probe_clock(&markets[0].up_token, 3).await.ok(),
                underlying: cfg.underlying,
                source_url: MARKET_WS_URL.to_owned(),
                markets: markets.clone(),
            };
            Some(SessionWriter::create(&path, &header)?)
        }
        None => None,
    };

    println!("POLYMARKET SHADOW MODE\n");
    println!(
        "Observing {} market(s) for {} seconds.",
        markets.len(),
        cfg.duration.as_secs()
    );
    println!("No orders are sent. Nothing here can trade.\n");
    for m in &markets {
        println!("  {}  {}", m.slug, m.title);
    }
    println!();

    let (feed_tx, feed_rx) = watch::channel(false);
    let (mut rx, handle) = websocket::spawn(
        FeedConfig {
            url: MARKET_WS_URL.to_owned(),
            tokens,
        },
        feed_rx,
    );

    let mut norm = Normalizer::new();
    let mut state = MarketState::new();
    let mut ideal = Leg::new(&cfg.run, Realism::IDEAL);
    let mut real = Leg::new(&cfg.run, Realism::REAL);
    let mut decisions: Vec<ShadowDecision> = Vec::new();

    let mut frames = 0u64;
    let mut events = 0u64;
    let mut last_report = now_ms();

    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            _ = shutdown.changed() => {
                if *shutdown.borrow() { println!("\nstopping on request…"); break }
            }
            msg = rx.recv() => {
                let Some(msg) = msg else { break };
                let FeedMessage::Frame { recv_ms, raw } = msg else { continue };
                frames += 1;
                if let Some(w) = writer.as_mut() {
                    w.write_frame(recv_ms, &raw)?;
                }
                let parsed = match norm.normalize(&raw, recv_ms) {
                    Ok(evs) => evs,
                    Err(FrameError::Unhandled(t)) => { warn!(event_type = %t, "unhandled frame"); continue }
                    Err(e) => { warn!(error = %e, "undecodable frame"); continue }
                };
                for ev in parsed {
                    events += 1;
                    let now = ev.exchange_ms;
                    state.apply(&ev);
                    ideal.step(&ev, now, &state, &markets);

                    // The realistic leg's decisions are recorded verbatim.
                    // One decision may carry several orders; it is still one
                    // decision, because that is the unit the strategy chose in.
                    for decision in real.step(&ev, now, &state, &markets) {
                        decisions.push(ShadowDecision {
                            decision,
                            mid_after: None,
                        });
                    }

                    // Close out any decision whose horizon has elapsed.
                    for d in decisions.iter_mut() {
                        if d.mid_after.is_none() && now - d.decision.ts_ms >= cfg.horizon_ms {
                            d.mid_after = state.book(&d.decision.token).and_then(|b| b.mid());
                        }
                    }
                }

                if now_ms() - last_report >= 15_000 {
                    last_report = now_ms();
                    println!(
                        "  t+{:>4}s  frames {:>8}  decisions {:>4}  ideal {:>10}  realistic {:>10}",
                        (now_ms() - started_ms) / 1000,
                        frames,
                        real.strategy.decisions(),
                        ideal.pnl(&state).to_string(),
                        real.pnl(&state).to_string(),
                    );
                }
            }
        }
    }

    let _ = feed_tx.send(true);
    drop(rx);
    let _ = handle.await;
    ideal.sim.flush();
    real.sim.flush();
    ideal.settle();
    real.settle();
    if let Some(w) = writer.as_mut() {
        w.flush()?;
    }
    info!(frames, events, "shadow session complete");

    let resolved = decisions.iter().filter(|d| d.mid_after.is_some()).count();
    let favourable = decisions
        .iter()
        .filter_map(|d| d.forward_ticks())
        .filter(|&t| t > 0)
        .count();

    Ok(ShadowSummary {
        frames,
        events,
        decisions: decisions.len(),
        resolved,
        favourable,
        ideal_pnl: ideal.pnl(&state),
        real_pnl: real.pnl(&state),
        ideal_fill_ratio: ideal.sim.stats().fill_ratio(),
        real_fill_ratio: real.sim.stats().fill_ratio(),
        recorded_to: writer.map(|w| w.path().to_path_buf()),
    })
}
