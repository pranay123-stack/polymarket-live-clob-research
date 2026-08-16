//! Deterministic replay: recorded events in, execution results out.
//!
//! # Ordering within one event
//!
//! The sequence below is not arbitrary; each step depends on the last:
//!
//! 1. **Advance the clock** to the event's exchange timestamp.
//! 2. **Land arrivals and cancels** against the book as it stood *before*
//!    this event. An order arriving at time `t` cannot see an event stamped
//!    `t`; doing this after step 3 would let orders trade on information
//!    that had not yet reached the matching engine.
//! 3. **Apply the event to truth**, the exchange-time view.
//! 4. **Feed trades and level updates to the simulator**, which is how
//!    resting orders fill and how cancellations ahead of them are inferred.
//! 5. **Release the event to the observed view** once its receive time has
//!    passed, and let the strategy decide on that view alone.
//!
//! # Determinism
//!
//! No wall clock, no randomness, no iteration over a `HashMap` that affects
//! results. The same file and configuration always produce identical fills,
//! P&L and statistics; `tests/replay_tests.rs` asserts this against a real
//! recorded session.

use anyhow::Result;

use crate::analytics::metrics::LatencyHistogram;
use crate::analytics::slippage::SlippageReport;
use crate::execution::fills::Fill;
use crate::execution::matcher::{ExecConfig, ExecStats, ExecutionSimulator};
use crate::lineage::{Decision, DecisionLedger};
use crate::market::event::{EventPayload, MarketEvent};
use crate::market::state::{MarketState, StateStats};
use crate::polymarket::market_discovery::MarketDescriptor;
use crate::portfolio::pnl::{PnlSnapshot, Portfolio};
use crate::replay::clock::{DelayLine, VirtualClock};
use crate::strategy::{ImbalanceStrategy, SignalConfig, StrategyContext};
use crate::types::{Price, Usdc};

/// Everything one replay pass needs.
#[derive(Debug, Clone)]
pub struct RunConfig {
    /// Execution realism settings.
    pub exec: ExecConfig,
    /// Reference-signal parameters.
    pub signal: SignalConfig,
    /// Starting cash.
    pub starting_cash: Usdc,
    /// Fixed market-data delay override, in ms; `None` uses the measured one.
    pub md_latency_override_ms: Option<i64>,
    /// How long after a decision to sample the midpoint that judges it.
    pub decision_horizon_ms: i64,
}

/// The result of one replay pass.
#[derive(Debug, Clone)]
pub struct RunResult {
    /// Final P&L snapshot.
    pub pnl: PnlSnapshot,
    /// Execution counters.
    pub exec: ExecStats,
    /// Slippage breakdown.
    pub slippage: SlippageReport,
    /// Book decode counters.
    pub state: StateStats,
    /// Events processed.
    pub events: u64,
    /// Strategy decisions taken.
    pub decisions: u64,
    /// Discrepancy in the P&L identity; must be exactly zero.
    pub identity_error: Usdc,
    /// Observed feed delay across the session.
    pub feed_delay: LatencyHistogram,
    /// Event-time span covered, in milliseconds.
    pub span_ms: i64,
    /// Every simulated fill, in the order it occurred.
    ///
    /// Retained because a run produces hundreds of fills, not millions —
    /// the event stream is what is large, and that is never held in memory.
    pub fills: Vec<Fill>,
    /// Full decision-to-P&L lineage for the run.
    pub ledger: DecisionLedger,
    /// Checksum of every reconstructed book at the end of the run.
    pub book_checksum: u64,
}

impl RunResult {
    /// Total P&L for the run.
    pub fn total_pnl(&self) -> Usdc {
        self.pnl.total_pnl
    }
}

/// Runs one replay pass over `events`.
///
/// The events iterator is consumed exactly once, so a caller running several
/// realism configurations re-opens the session per pass. That keeps memory
/// flat regardless of session length, at the cost of re-reading the file.
pub fn run<I>(events: I, markets: &[MarketDescriptor], cfg: &RunConfig) -> Result<RunResult>
where
    I: Iterator<Item = MarketEvent>,
{
    let mut clock = VirtualClock::new();
    let mut truth = MarketState::new();
    let mut observed = MarketState::new();
    let mut delay = DelayLine::new(cfg.exec.realism.md_latency, cfg.md_latency_override_ms);
    let mut sim = ExecutionSimulator::new(cfg.exec.clone());
    let mut strategy = ImbalanceStrategy::new(cfg.signal.clone());
    let mut portfolio = Portfolio::new(cfg.starting_cash);
    let mut slippage = SlippageReport::new();
    let mut feed_delay = LatencyHistogram::new();
    let mut all_fills: Vec<Fill> = Vec::new();
    let mut ledger = DecisionLedger::new();
    // Decisions awaiting the midpoint that will judge them, oldest first.
    let mut awaiting: std::collections::VecDeque<(crate::lineage::DecisionId, String, i64)> =
        std::collections::VecDeque::new();

    let mut n_events = 0u64;
    let mut first_ms = i64::MAX;
    let mut last_ms = 0i64;
    // Markets discovered mid-session via lifecycle markers.
    let mut live_markets: Vec<MarketDescriptor> = markets.to_vec();

    for ev in events {
        n_events += 1;
        first_ms = first_ms.min(ev.exchange_ms);
        last_ms = last_ms.max(ev.exchange_ms);
        if !matches!(
            ev.payload,
            EventPayload::MarketOpen { .. } | EventPayload::MarketClose { .. }
        ) {
            feed_delay.record(ev.feed_delay_ms());
        }

        // 1. Advance event time.
        let now = clock.advance_to(ev.exchange_ms);

        // 2. Land arrivals and cancels against the pre-event book.
        sim.tick(now, &truth);
        drain_fills(
            &mut sim,
            &mut portfolio,
            &mut slippage,
            &mut all_fills,
            &mut ledger,
        );

        // 3. Apply to the exchange-time view.
        truth.apply(&ev);

        // 4. Drive resting orders from real trades and level changes.
        match &ev.payload {
            EventPayload::Trade {
                asset_id,
                price,
                qty,
                side,
                ..
            } => {
                sim.on_trade(asset_id, *price, *qty, *side, now);
                drain_fills(
                    &mut sim,
                    &mut portfolio,
                    &mut slippage,
                    &mut all_fills,
                    &mut ledger,
                );
            }
            EventPayload::LevelUpdate {
                asset_id,
                side,
                price,
                qty,
                ..
            } => {
                sim.on_level_update(asset_id, *side, *price, *qty);
            }
            EventPayload::MarketOpen {
                slug,
                condition_id,
                up_token,
                down_token,
                close_ms,
            } => {
                // A session may span markets that did not exist in the
                // header; adopt them so the strategy can trade them.
                if !live_markets.iter().any(|m| &m.slug == slug) {
                    live_markets.push(MarketDescriptor {
                        slug: slug.clone(),
                        title: String::new(),
                        question: String::new(),
                        condition_id: condition_id.clone(),
                        open_ts: close_ms / 1_000 - 300,
                        close_ts: close_ms / 1_000,
                        up_token: up_token.clone(),
                        down_token: down_token.clone(),
                        tick_size: Price::parse("0.01").unwrap_or(Price(100)),
                        min_size: crate::types::Qty::from_shares(5),
                        accepting_orders: true,
                    });
                }
            }
            _ => {}
        }

        // 5. Release to the strategy's view and let it decide.
        let newly_visible = match delay.push(ev) {
            Some(immediate) => vec![immediate],
            None => delay.release(now),
        };
        if newly_visible.is_empty() {
            continue;
        }
        for v in &newly_visible {
            observed.apply(v);
        }

        let position = |token: &str| portfolio.position(token).map(|p| p.qty).unwrap_or(0);
        let decisions: Vec<Decision> = strategy.on_tick(&StrategyContext {
            now_ms: now,
            observed: &observed,
            markets: &live_markets,
            position: &position,
        });
        for decision in decisions {
            // Submit from the decision's own intents, so the order can only
            // ever carry the decision that authorised it.
            for intent in decision.intents.iter().cloned() {
                sim.submit(intent, now);
            }
            awaiting.push_back((decision.id, decision.token.clone(), now));
            ledger.record_decision(decision);
        }

        // Judge any decision whose horizon has elapsed, against the truth
        // book: what the market actually did, not what the strategy saw.
        while let Some((id, token, decided_at)) = awaiting.front().cloned() {
            if decided_at + cfg.decision_horizon_ms > now {
                break;
            }
            awaiting.pop_front();
            if let Some(mid) = truth.book(&token).and_then(|b| b.mid()) {
                ledger.record_forward_mid(id, mid, now - decided_at);
            }
        }
    }

    // Settle: release anything still in flight and cancel resting orders.
    for v in delay.drain() {
        observed.apply(&v);
    }
    let now = clock.now_ms();
    sim.tick(now, &truth);
    drain_fills(
        &mut sim,
        &mut portfolio,
        &mut slippage,
        &mut all_fills,
        &mut ledger,
    );
    sim.flush();
    drain_fills(
        &mut sim,
        &mut portfolio,
        &mut slippage,
        &mut all_fills,
        &mut ledger,
    );

    // Mark to the final observed midpoint of each token's book.
    let marks = |token: &str| truth.book(token).and_then(|b| b.mid());
    let pnl = portfolio.snapshot(&marks);
    let identity_error = portfolio.check_identity(&marks);
    // A session can end before every decision's horizon elapses. Judge the
    // stragglers against the last book that existed, recording the shorter
    // window rather than discarding the observation or pretending it was full.
    for (id, token, decided_at) in awaiting {
        if let Some(mid) = truth.book(&token).and_then(|b| b.mid()) {
            ledger.record_forward_mid(id, mid, now - decided_at);
        }
    }
    ledger.finalize(&marks);

    Ok(RunResult {
        pnl,
        exec: sim.stats(),
        slippage,
        state: truth.stats(),
        events: n_events,
        decisions: strategy.decisions(),
        identity_error,
        feed_delay,
        span_ms: if first_ms <= last_ms {
            last_ms - first_ms
        } else {
            0
        },
        fills: all_fills,
        ledger,
        book_checksum: truth.checksum(),
    })
}

/// Moves fills from the simulator into the portfolio and slippage report.
fn drain_fills(
    sim: &mut ExecutionSimulator,
    portfolio: &mut Portfolio,
    slippage: &mut SlippageReport,
    collected: &mut Vec<Fill>,
    ledger: &mut DecisionLedger,
) {
    for f in sim.drain_fills() {
        portfolio.apply_fill(&f.token, f.side, f.qty, f.price, f.fee);
        slippage.record(&f);
        ledger.record_fill(&f);
        collected.push(f);
    }
    for o in sim.drain_outcomes() {
        ledger.record_outcome(o);
    }
}
