//! The execution simulator: what a hypothetical order would really have done.
//!
//! # The counterfactual
//!
//! Every run is parameterised by a [`Realism`] set. With all factors off the
//! simulator reproduces the assumptions a naive backtest makes; with all on
//! it applies every constraint this crate can defend from real data. The
//! difference between the two runs is the edge that execution destroys, and
//! [`crate::analytics::attribution`] splits it by factor.
//!
//! | Factor | Off (ideal) | On (realistic) |
//! |--------|-------------|----------------|
//! | [`Factor::MdLatency`]    | strategy sees the live book | sees it delayed by the measured feed delay |
//! | [`Factor::OrderLatency`] | orders act at decision time | arrive later; cancels land later still |
//! | [`Factor::Queue`]        | passive orders fill in full, instantly | join the back of the real queue; may never fill |
//! | [`Factor::Depth`]        | full size at the touch price | walk real levels; partial fills; slippage |
//! | [`Factor::Fees`]         | free | charged per fill |
//!
//! # Queue position from public data
//!
//! Polymarket publishes aggregate size per level, never individual orders,
//! so a resting order's true queue position is not observable. It is
//! modelled: on arrival the order joins **behind** all size currently at its
//! price, and that queue is then drawn down by real events.
//!
//! What makes the model more than a guess is that trades and cancellations
//! can be told apart. A `last_trade_price` print says exactly how much
//! traded, at what price, and on which side — and trades always consume from
//! the *front* of the queue. Any level decrease beyond what trades explain is
//! a cancellation, whose position in the queue is genuinely unknowable, and
//! is handled by an explicit [`QueueModel`].
//!
//! The aggressor convention was verified empirically rather than assumed:
//! across a real session, `BUY` prints preceded ask-side decreases 545 times
//! against 154 bid-side, and `SELL` prints preceded bid-side decreases 79
//! times against 13. `side` is the taker.
//!
//! # Market impact
//!
//! A hypothetical order never removes real liquidity, so the replayed book
//! is left untouched. Fills are read off the book that actually existed.
//! This understates the cost of large orders and is stated as a limitation
//! in `docs/EXECUTION_MODEL.md`; it is sound for sizes small against
//! displayed depth.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::execution::fills::{Fill, Liquidity};
use crate::execution::latency::LatencyModel;
use crate::execution::orders::{Order, OrderIntent, OrderState, OrderStyle, Terminal};
use crate::lineage::OrderOutcome;
use crate::market::orderbook::OrderBook;
use crate::market::state::MarketState;
use crate::types::{Price, Qty, Side, Usdc};

/// One dimension of execution realism.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Factor {
    /// The strategy acts on a delayed view of the book.
    MdLatency,
    /// Orders and cancels take time to reach the matching engine.
    OrderLatency,
    /// Passive orders must queue behind real resting size.
    Queue,
    /// Orders consume real depth, at real prices.
    Depth,
    /// Fees are charged.
    Fees,
}

impl Factor {
    /// Every factor, in report order.
    pub const ALL: [Factor; 5] = [
        Factor::MdLatency,
        Factor::OrderLatency,
        Factor::Queue,
        Factor::Depth,
        Factor::Fees,
    ];

    /// Short label used in reports.
    pub fn label(self) -> &'static str {
        match self {
            Factor::MdLatency => "Stale market data",
            Factor::OrderLatency => "Order latency",
            Factor::Queue => "Queue position / missed fills",
            Factor::Depth => "Depth & slippage",
            Factor::Fees => "Fees",
        }
    }
}

/// Which realism factors are switched on for a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct Realism {
    /// Strategy sees a delayed book.
    pub md_latency: bool,
    /// Orders and cancels are delayed.
    pub order_latency: bool,
    /// Passive orders queue.
    pub queue: bool,
    /// Orders are depth-constrained.
    pub depth: bool,
    /// Fees are charged.
    pub fees: bool,
}

impl Realism {
    /// Every factor off: the naive backtest.
    pub const IDEAL: Realism = Realism {
        md_latency: false,
        order_latency: false,
        queue: false,
        depth: false,
        fees: false,
    };

    /// Every factor on.
    pub const REAL: Realism = Realism {
        md_latency: true,
        order_latency: true,
        queue: true,
        depth: true,
        fees: true,
    };

    /// Whether `f` is enabled.
    pub fn has(self, f: Factor) -> bool {
        match f {
            Factor::MdLatency => self.md_latency,
            Factor::OrderLatency => self.order_latency,
            Factor::Queue => self.queue,
            Factor::Depth => self.depth,
            Factor::Fees => self.fees,
        }
    }

    /// Returns a copy with `f` set to `on`.
    pub fn with(mut self, f: Factor, on: bool) -> Realism {
        match f {
            Factor::MdLatency => self.md_latency = on,
            Factor::OrderLatency => self.order_latency = on,
            Factor::Queue => self.queue = on,
            Factor::Depth => self.depth = on,
            Factor::Fees => self.fees = on,
        }
        self
    }

    /// Builds a realism set from a subset of factors.
    pub fn from_set(factors: &[Factor]) -> Realism {
        factors.iter().fold(Realism::IDEAL, |r, &f| r.with(f, true))
    }

    /// Enabled factors, in report order.
    pub fn enabled(self) -> Vec<Factor> {
        Factor::ALL.into_iter().filter(|&f| self.has(f)).collect()
    }
}

/// How cancellations ahead of a resting order are treated.
///
/// Cancellations are the one queue event public data cannot place. Each
/// option is a defensible bound rather than a measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum QueueModel {
    /// Every cancellation came from behind: the queue never improves.
    ///
    /// The conservative bound, and the default. Adverse selection makes it
    /// closer to the truth than it first looks: the orders most likely to
    /// cancel are informed ones at the front reacting to news.
    #[default]
    Pessimistic,
    /// Cancellations are split in proportion to the queue ahead and behind.
    Proportional,
    /// Every cancellation came from ahead: the queue improves in full.
    Optimistic,
}

/// Everything the simulator needs to evaluate a run.
#[derive(Debug, Clone)]
pub struct ExecConfig {
    /// Latency assumptions.
    pub latency: LatencyModel,
    /// Treatment of cancellations ahead of a resting order.
    pub queue_model: QueueModel,
    /// Fee charged when taking liquidity, in basis points of notional.
    pub taker_fee_bps: u32,
    /// Fee charged when providing liquidity, in basis points of notional.
    pub maker_fee_bps: u32,
    /// How long a resting order waits before being cancelled.
    pub order_ttl_ms: i64,
    /// Which realism factors are active.
    pub realism: Realism,
}

impl Default for ExecConfig {
    fn default() -> ExecConfig {
        ExecConfig {
            latency: LatencyModel::default(),
            queue_model: QueueModel::default(),
            // Zero matches every `fee_rate_bps` observed on live BTC
            // 5-minute prints; it is a measurement, not an assumption that
            // trading is free, and should be raised for fee-bearing markets.
            taker_fee_bps: 0,
            maker_fee_bps: 0,
            order_ttl_ms: 5_000,
            realism: Realism::REAL,
        }
    }
}

/// Counters describing what happened to the simulated orders.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecStats {
    /// Orders submitted.
    pub submitted: u64,
    /// Orders that filled completely.
    pub filled_full: u64,
    /// Orders that filled in part.
    pub filled_partial: u64,
    /// Orders that expired without a single fill.
    pub expired_unfilled: u64,
    /// Orders the book could not support at all on arrival.
    pub unfillable: u64,
    /// Fills that took liquidity.
    pub taker_fills: u64,
    /// Fills that provided liquidity.
    pub maker_fills: u64,
    /// Total size requested.
    pub requested: Qty,
    /// Total size executed.
    pub executed: Qty,
}

impl ExecStats {
    /// Fraction of requested size that executed, in `[0, 1]`.
    pub fn fill_ratio(&self) -> f64 {
        if self.requested.is_zero() {
            return 0.0;
        }
        self.executed.0 as f64 / self.requested.0 as f64
    }

    /// Orders that produced no fill at all.
    pub fn missed(&self) -> u64 {
        self.expired_unfilled + self.unfillable
    }
}

/// A simulated order with its live bookkeeping.
#[derive(Debug, Clone)]
struct LiveOrder {
    order: Order,
    state: OrderState,
    remaining: Qty,
    filled: Qty,
    /// Real size resting ahead of this order at its price.
    queue_ahead: Qty,
    /// When the cancel actually takes effect.
    cancel_effective_ms: i64,
}

/// Key identifying a book level the simulator is tracking.
type LevelKey = (String, Side, u32);

/// Simulates hypothetical orders against replayed real market data.
#[derive(Debug)]
pub struct ExecutionSimulator {
    cfg: ExecConfig,
    next_id: u64,
    next_fill_id: u64,
    outcomes: Vec<OrderOutcome>,
    in_flight: Vec<LiveOrder>,
    resting: Vec<LiveOrder>,
    fills: Vec<Fill>,
    stats: ExecStats,
    /// Last observed aggregate size at levels where we rest.
    level_seen: HashMap<LevelKey, Qty>,
    /// Size at tracked levels already explained by trade prints.
    trade_credit: HashMap<LevelKey, Qty>,
}

impl ExecutionSimulator {
    /// Creates a simulator for one run.
    pub fn new(cfg: ExecConfig) -> ExecutionSimulator {
        ExecutionSimulator {
            cfg,
            next_id: 1,
            next_fill_id: 1,
            outcomes: Vec::new(),
            in_flight: Vec::new(),
            resting: Vec::new(),
            fills: Vec::new(),
            stats: ExecStats::default(),
            level_seen: HashMap::new(),
            trade_credit: HashMap::new(),
        }
    }

    /// Configuration in force.
    pub fn config(&self) -> &ExecConfig {
        &self.cfg
    }

    /// Counters accumulated so far.
    pub fn stats(&self) -> ExecStats {
        self.stats
    }

    /// Orders currently resting on the book.
    pub fn resting_count(&self) -> usize {
        self.resting.len()
    }

    /// Removes and returns the fills produced since the last drain.
    pub fn drain_fills(&mut self) -> Vec<Fill> {
        std::mem::take(&mut self.fills)
    }

    /// Removes and returns the order outcomes recorded since the last drain.
    ///
    /// An order's terminal state is what makes a missed fill attributable:
    /// `Expired` means the queue never reached it, `Unfillable` means the
    /// book had moved past its limit by the time it arrived.
    pub fn drain_outcomes(&mut self) -> Vec<OrderOutcome> {
        std::mem::take(&mut self.outcomes)
    }

    /// Accepts a strategy intent and schedules its arrival.
    pub fn submit(&mut self, intent: OrderIntent, now_ms: i64) -> u64 {
        let id = self.next_id;
        self.next_id += 1;

        let submit_delay = if self.cfg.realism.order_latency {
            self.cfg.latency.submit_ms
        } else {
            0
        };
        let order = Order {
            id,
            decision_id: intent.decision_id,
            token: intent.token,
            side: intent.side,
            limit_price: intent.limit_price,
            qty: intent.qty,
            style: intent.style,
            decided_ms: now_ms,
            arrive_ms: now_ms + submit_delay,
            expire_ms: now_ms + submit_delay + self.cfg.order_ttl_ms,
            reference_price: intent.reference_price,
        };
        self.stats.submitted += 1;
        self.stats.requested = Qty(self.stats.requested.0 + order.qty.0);
        self.in_flight.push(LiveOrder {
            remaining: order.qty,
            filled: Qty::ZERO,
            queue_ahead: Qty::ZERO,
            cancel_effective_ms: i64::MAX,
            state: OrderState::InFlight,
            order,
        });
        id
    }

    /// Advances simulator time: lands arrivals, then expires stale orders.
    pub fn tick(&mut self, now_ms: i64, state: &MarketState) {
        // Arrivals.
        let mut arriving = Vec::new();
        self.in_flight.retain(|o| {
            if o.order.arrive_ms <= now_ms {
                arriving.push(o.clone());
                false
            } else {
                true
            }
        });
        for o in arriving {
            self.activate(o, now_ms, state);
        }

        // Expiries. A cancel requested at `expire_ms` only lands after the
        // cancellation latency, during which the order can still be filled —
        // which is precisely the risk of cancelling into a fast market.
        let cancel_delay = if self.cfg.realism.order_latency {
            self.cfg.latency.cancel_ms
        } else {
            0
        };
        for o in &mut self.resting {
            if o.cancel_effective_ms == i64::MAX && now_ms >= o.order.expire_ms {
                o.cancel_effective_ms = o.order.expire_ms + cancel_delay;
            }
        }
        let mut done = Vec::new();
        self.resting.retain(|o| {
            if now_ms >= o.cancel_effective_ms {
                done.push(o.clone());
                false
            } else {
                true
            }
        });
        for o in done {
            self.finish(&o);
        }
    }

    /// Lands an order on the book at its arrival time.
    fn activate(&mut self, mut o: LiveOrder, now_ms: i64, state: &MarketState) {
        let Some(book) = state.book(&o.order.token) else {
            o.state = OrderState::Done(Terminal::Unfillable);
            self.finish(&o);
            return;
        };

        let opposite = o.order.side.opposite();
        let touch = book.best(opposite);
        let marketable = match (o.order.side, touch) {
            (Side::Buy, Some(l)) => o.order.limit_price >= l.price,
            (Side::Sell, Some(l)) => o.order.limit_price <= l.price,
            (_, None) => false,
        };

        // A passive order can arrive to find the market has come to it; the
        // exchange would fill it immediately, with price improvement.
        if o.order.style == OrderStyle::Aggressive || marketable {
            if !marketable {
                // The book moved past our limit while the order was in
                // flight. This is the archetypal latency-driven missed fill.
                o.state = OrderState::Done(Terminal::Unfillable);
                self.finish(&o);
                return;
            }
            self.take_liquidity(&mut o, book, now_ms);
            if o.remaining.is_zero() {
                o.state = OrderState::Done(Terminal::Filled);
            } else if self.cfg.realism.depth {
                // Immediate-or-cancel: whatever the book could not supply is
                // gone rather than left resting.
                o.state = OrderState::Done(if o.filled.is_zero() {
                    Terminal::Unfillable
                } else {
                    Terminal::PartiallyFilled
                });
            } else {
                o.state = OrderState::Done(Terminal::Filled);
            }
            self.finish(&o);
            return;
        }

        // Passive and not marketable: rest.
        if !self.cfg.realism.queue {
            // The naive assumption: posting at the touch means being filled
            // at the touch, in full, at once.
            let price = o.order.limit_price;
            let qty = o.remaining;
            self.record_fill(&mut o, price, qty, now_ms, Liquidity::Maker);
            o.state = OrderState::Done(Terminal::Filled);
            self.finish(&o);
            return;
        }

        let ahead = book.qty_at(o.order.side, o.order.limit_price);
        o.queue_ahead = ahead;
        o.state = OrderState::Resting;
        let key = (
            o.order.token.clone(),
            o.order.side,
            o.order.limit_price.ticks(),
        );
        self.level_seen.insert(key, ahead);
        self.resting.push(o);
    }

    /// Consumes displayed liquidity for a marketable order.
    fn take_liquidity(&mut self, o: &mut LiveOrder, book: &OrderBook, now_ms: i64) {
        let opposite = o.order.side.opposite();
        if !self.cfg.realism.depth {
            // Ideal: the whole order clears at the touch, however large.
            let price = book
                .best(opposite)
                .map(|l| l.price)
                .unwrap_or(o.order.limit_price);
            let qty = o.remaining;
            self.record_fill(o, price, qty, now_ms, Liquidity::Taker);
            return;
        }
        let ladder = book.walk(opposite, Some(o.order.limit_price), o.remaining);
        for (level, take) in ladder {
            if take.is_zero() {
                continue;
            }
            self.record_fill(o, level.price, take, now_ms, Liquidity::Taker);
            if o.remaining.is_zero() {
                break;
            }
        }
    }

    /// Applies a real trade print to the resting orders it would have hit.
    ///
    /// `aggressor` is the taker side, verified against live data. A taker
    /// buy consumes resting asks; a taker sell consumes resting bids.
    pub fn on_trade(&mut self, token: &str, price: Price, qty: Qty, aggressor: Side, now_ms: i64) {
        let consumed_side = aggressor.opposite();
        let key = (token.to_owned(), consumed_side, price.ticks());
        if self.level_seen.contains_key(&key) {
            let credit = self.trade_credit.entry(key).or_insert(Qty::ZERO);
            *credit = Qty(credit.0 + qty.0);
        }

        let mut remaining_trade = qty;
        let mut newly_done = Vec::new();
        // Lifted out of the loop because `self.resting` is borrowed mutably
        // for its duration and the counter lives on `self`.
        let mut next_fill_id = self.next_fill_id;
        for o in &mut self.resting {
            if remaining_trade.is_zero() {
                break;
            }
            if o.order.token != token || o.order.side != consumed_side {
                continue;
            }
            // Would this print have reached our price?
            let reached = match o.order.side {
                Side::Sell => price >= o.order.limit_price,
                Side::Buy => price <= o.order.limit_price,
            };
            if !reached {
                continue;
            }

            // Trades consume the front of the queue first.
            let eaten = Qty(o.queue_ahead.0.min(remaining_trade.0));
            o.queue_ahead = o.queue_ahead.saturating_sub(eaten);
            remaining_trade = remaining_trade.saturating_sub(eaten);
            if remaining_trade.is_zero() {
                continue;
            }

            let take = Qty(o.remaining.0.min(remaining_trade.0));
            if take.is_zero() {
                continue;
            }
            remaining_trade = remaining_trade.saturating_sub(take);

            // Maker fills always happen at the resting order's own price.
            let fill_price = o.order.limit_price;
            let fee = if self.cfg.realism.fees {
                take.notional(fill_price).bps(self.cfg.maker_fee_bps)
            } else {
                Usdc::ZERO
            };
            o.remaining = o.remaining.saturating_sub(take);
            o.filled = Qty(o.filled.0 + take.0);
            self.stats.executed = Qty(self.stats.executed.0 + take.0);
            self.stats.maker_fills += 1;
            let fill_id = next_fill_id;
            next_fill_id += 1;
            self.fills.push(Fill {
                decision_id: o.order.decision_id,
                order_id: o.order.id,
                fill_id,
                token: o.order.token.clone(),
                side: o.order.side,
                price: fill_price,
                qty: take,
                ts_ms: now_ms,
                liquidity: Liquidity::Maker,
                fee,
                reference_price: o.order.reference_price,
            });
            if o.remaining.is_zero() {
                o.state = OrderState::Done(Terminal::Filled);
                newly_done.push(o.order.id);
            }
        }

        self.next_fill_id = next_fill_id;

        if !newly_done.is_empty() {
            let mut finished = Vec::new();
            self.resting.retain(|o| {
                if newly_done.contains(&o.order.id) {
                    finished.push(o.clone());
                    false
                } else {
                    true
                }
            });
            for o in finished {
                self.finish(&o);
            }
        }
    }

    /// Applies a level update, inferring cancellations ahead of our orders.
    pub fn on_level_update(&mut self, token: &str, side: Side, price: Price, new_qty: Qty) {
        let key = (token.to_owned(), side, price.ticks());
        let Some(&prev) = self.level_seen.get(&key) else {
            return;
        };
        self.level_seen.insert(key.clone(), new_qty);
        if new_qty >= prev {
            return;
        }

        let decrease = prev.saturating_sub(new_qty);
        // Attribute as much of the decrease as trade prints can explain;
        // the rest is cancellation.
        let credit = self.trade_credit.entry(key.clone()).or_insert(Qty::ZERO);
        let from_trades = Qty(credit.0.min(decrease.0));
        *credit = credit.saturating_sub(from_trades);
        let cancelled = decrease.saturating_sub(from_trades);
        if cancelled.is_zero() {
            return;
        }

        let model = self.cfg.queue_model;
        for o in &mut self.resting {
            if o.order.token != token
                || o.order.side != side
                || o.order.limit_price.ticks() != price.ticks()
            {
                continue;
            }
            o.queue_ahead = match model {
                QueueModel::Pessimistic => o.queue_ahead,
                QueueModel::Optimistic => o.queue_ahead.saturating_sub(cancelled),
                QueueModel::Proportional => {
                    if prev.is_zero() {
                        o.queue_ahead
                    } else {
                        let share =
                            (cancelled.0 as u128 * o.queue_ahead.0 as u128) / prev.0 as u128;
                        o.queue_ahead.saturating_sub(Qty(share as u64))
                    }
                }
            };
        }
    }

    /// Records a fill against an order.
    fn record_fill(
        &mut self,
        o: &mut LiveOrder,
        price: Price,
        qty: Qty,
        now_ms: i64,
        liquidity: Liquidity,
    ) {
        if qty.is_zero() {
            return;
        }
        let bps = match liquidity {
            Liquidity::Maker => self.cfg.maker_fee_bps,
            Liquidity::Taker => self.cfg.taker_fee_bps,
        };
        let fee = if self.cfg.realism.fees {
            qty.notional(price).bps(bps)
        } else {
            Usdc::ZERO
        };
        o.remaining = o.remaining.saturating_sub(qty);
        o.filled = Qty(o.filled.0 + qty.0);
        self.stats.executed = Qty(self.stats.executed.0 + qty.0);
        match liquidity {
            Liquidity::Maker => self.stats.maker_fills += 1,
            Liquidity::Taker => self.stats.taker_fills += 1,
        }
        let fill_id = self.next_fill_id;
        self.next_fill_id += 1;
        self.fills.push(Fill {
            decision_id: o.order.decision_id,
            order_id: o.order.id,
            fill_id,
            token: o.order.token.clone(),
            side: o.order.side,
            price,
            qty,
            ts_ms: now_ms,
            liquidity,
            fee,
            reference_price: o.order.reference_price,
        });
    }

    /// Books the terminal outcome of an order and releases its level tracking.
    fn finish(&mut self, o: &LiveOrder) {
        let terminal = match o.state {
            OrderState::Done(t) => t,
            _ if o.remaining.is_zero() && !o.filled.is_zero() => Terminal::Filled,
            _ if !o.filled.is_zero() => Terminal::PartiallyFilled,
            _ => Terminal::Expired,
        };
        match terminal {
            Terminal::Filled => self.stats.filled_full += 1,
            Terminal::PartiallyFilled => self.stats.filled_partial += 1,
            Terminal::Expired => self.stats.expired_unfilled += 1,
            Terminal::Unfillable => self.stats.unfillable += 1,
        }
        self.outcomes.push(OrderOutcome {
            decision_id: o.order.decision_id,
            order_id: o.order.id,
            requested: o.order.qty,
            executed: o.filled,
            terminal,
            decided_ms: o.order.decided_ms,
            arrive_ms: o.order.arrive_ms,
        });

        let key = (
            o.order.token.clone(),
            o.order.side,
            o.order.limit_price.ticks(),
        );
        let still_used = self.resting.iter().any(|r| {
            r.order.token == o.order.token
                && r.order.side == o.order.side
                && r.order.limit_price.ticks() == o.order.limit_price.ticks()
        });
        if !still_used {
            self.level_seen.remove(&key);
            self.trade_credit.remove(&key);
        }
    }

    /// Cancels every resting order, used when a session ends.
    pub fn flush(&mut self) {
        let leftovers = std::mem::take(&mut self.resting);
        for o in leftovers {
            self.finish(&o);
        }
        self.in_flight.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::event::{EventPayload, MarketEvent};
    use crate::market::orderbook::Level;

    fn p(s: &str) -> Price {
        Price::parse(s).unwrap()
    }

    /// A market with 200 shares resting at 0.50 bid and 150 at 0.51 ask.
    fn state_with_book() -> MarketState {
        let mut st = MarketState::new();
        st.apply(&MarketEvent {
            seq: 1,
            recv_ms: 1_000,
            exchange_ms: 1_000,
            payload: EventPayload::Snapshot {
                asset_id: "UP".into(),
                bids: vec![
                    Level {
                        price: p("0.49"),
                        qty: Qty::from_shares(300),
                    },
                    Level {
                        price: p("0.50"),
                        qty: Qty::from_shares(200),
                    },
                ],
                asks: vec![
                    Level {
                        price: p("0.51"),
                        qty: Qty::from_shares(150),
                    },
                    Level {
                        price: p("0.52"),
                        qty: Qty::from_shares(400),
                    },
                ],
                tick_size: Some(p("0.01")),
                hash: None,
            },
        });
        st
    }

    fn intent(side: Side, style: OrderStyle, limit: &str, shares: u64) -> OrderIntent {
        OrderIntent {
            decision_id: 1,
            token: "UP".into(),
            side,
            qty: Qty::from_shares(shares),
            limit_price: p(limit),
            style,
            reference_price: p("0.51"),
        }
    }

    fn sim(realism: Realism) -> ExecutionSimulator {
        ExecutionSimulator::new(ExecConfig {
            latency: LatencyModel {
                market_data_ms: 200,
                submit_ms: 100,
                cancel_ms: 100,
            },
            realism,
            ..ExecConfig::default()
        })
    }

    #[test]
    fn ideal_run_fills_everything_instantly_at_the_touch() {
        let st = state_with_book();
        let mut s = sim(Realism::IDEAL);
        // 1000 shares against only 150 displayed at the touch.
        s.submit(
            intent(Side::Buy, OrderStyle::Aggressive, "0.52", 1_000),
            1_000,
        );
        s.tick(1_000, &st);
        let fills = s.drain_fills();
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].qty, Qty::from_shares(1_000));
        assert_eq!(fills[0].price, p("0.51"), "ideal ignores depth entirely");
        assert_eq!(fills[0].fee, Usdc::ZERO);
        assert_eq!(s.stats().filled_full, 1);
    }

    #[test]
    fn depth_realism_walks_real_levels_and_produces_slippage() {
        let st = state_with_book();
        let mut s = sim(Realism::IDEAL.with(Factor::Depth, true));
        s.submit(
            intent(Side::Buy, OrderStyle::Aggressive, "0.52", 400),
            1_000,
        );
        s.tick(1_000, &st);
        let fills = s.drain_fills();
        // 150 at 0.51 then 250 at 0.52.
        assert_eq!(fills.len(), 2);
        assert_eq!(fills[0].qty, Qty::from_shares(150));
        assert_eq!(fills[1].price, p("0.52"));
        assert_eq!(fills[1].qty, Qty::from_shares(250));
        // Only the second level slipped, one tick on 250 shares = $2.50.
        assert_eq!(fills[0].slippage_cost(), Usdc::ZERO);
        assert_eq!(fills[1].slippage_cost(), Usdc(2_500_000));
    }

    #[test]
    fn depth_realism_caps_the_fill_at_available_liquidity() {
        let st = state_with_book();
        let mut s = sim(Realism::IDEAL.with(Factor::Depth, true));
        // A limit of 0.51 can only reach the 150 shares at the touch.
        s.submit(
            intent(Side::Buy, OrderStyle::Aggressive, "0.51", 1_000),
            1_000,
        );
        s.tick(1_000, &st);
        assert_eq!(s.drain_fills()[0].qty, Qty::from_shares(150));
        assert_eq!(s.stats().filled_partial, 1);
        assert!(s.stats().fill_ratio() < 0.2);
    }

    #[test]
    fn passive_order_without_queue_realism_fills_immediately() {
        let st = state_with_book();
        let mut s = sim(Realism::IDEAL);
        s.submit(intent(Side::Buy, OrderStyle::Passive, "0.50", 100), 1_000);
        s.tick(1_000, &st);
        let fills = s.drain_fills();
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].liquidity, Liquidity::Maker);
        assert_eq!(fills[0].price, p("0.50"));
    }

    #[test]
    fn passive_order_with_queue_realism_waits_behind_real_size() {
        let st = state_with_book();
        let mut s = sim(Realism::IDEAL.with(Factor::Queue, true));
        s.submit(intent(Side::Buy, OrderStyle::Passive, "0.50", 100), 1_000);
        s.tick(1_000, &st);
        assert!(s.drain_fills().is_empty(), "must not fill on arrival");
        assert_eq!(s.resting_count(), 1);

        // 200 shares rest ahead. A 150-share sell does not reach us.
        s.on_trade("UP", p("0.50"), Qty::from_shares(150), Side::Sell, 1_100);
        assert!(s.drain_fills().is_empty(), "still 50 shares ahead");

        // A further 120-share sell clears the remaining 50 and fills 70 of ours.
        s.on_trade("UP", p("0.50"), Qty::from_shares(120), Side::Sell, 1_200);
        let fills = s.drain_fills();
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].qty, Qty::from_shares(70));
        assert_eq!(fills[0].liquidity, Liquidity::Maker);
    }

    #[test]
    fn a_trade_on_the_wrong_side_never_fills_a_resting_order() {
        let st = state_with_book();
        let mut s = sim(Realism::IDEAL.with(Factor::Queue, true));
        s.submit(intent(Side::Buy, OrderStyle::Passive, "0.50", 100), 1_000);
        s.tick(1_000, &st);
        // A taker BUY consumes asks, so it cannot fill our resting bid.
        s.on_trade("UP", p("0.50"), Qty::from_shares(5_000), Side::Buy, 1_100);
        assert!(s.drain_fills().is_empty());
    }

    #[test]
    fn cancellations_are_told_apart_from_trades_by_the_queue_model() {
        let st = state_with_book();
        // Optimistic: cancels ahead of us shorten the queue.
        let mut s = ExecutionSimulator::new(ExecConfig {
            queue_model: QueueModel::Optimistic,
            realism: Realism::IDEAL.with(Factor::Queue, true),
            ..ExecConfig::default()
        });
        s.submit(intent(Side::Buy, OrderStyle::Passive, "0.50", 100), 1_000);
        s.tick(1_000, &st);
        // Level falls 200 -> 40 with no trade print: 160 cancelled.
        s.on_level_update("UP", Side::Buy, p("0.50"), Qty::from_shares(40));
        // 40 remain ahead; a 100-share sell clears them and fills 60 of ours.
        s.on_trade("UP", p("0.50"), Qty::from_shares(100), Side::Sell, 1_200);
        let fills = s.drain_fills();
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].qty, Qty::from_shares(60));
    }

    #[test]
    fn pessimistic_queue_model_ignores_cancellations_entirely() {
        let st = state_with_book();
        let mut s = sim(Realism::IDEAL.with(Factor::Queue, true)); // default pessimistic
        s.submit(intent(Side::Buy, OrderStyle::Passive, "0.50", 100), 1_000);
        s.tick(1_000, &st);
        s.on_level_update("UP", Side::Buy, p("0.50"), Qty::from_shares(40));
        // Queue still modelled as 200 ahead, so 100 shares of trade fills nothing.
        s.on_trade("UP", p("0.50"), Qty::from_shares(100), Side::Sell, 1_200);
        assert!(s.drain_fills().is_empty());
    }

    #[test]
    fn trade_driven_decreases_are_not_double_counted_as_cancellations() {
        let st = state_with_book();
        let mut s = ExecutionSimulator::new(ExecConfig {
            queue_model: QueueModel::Optimistic,
            realism: Realism::IDEAL.with(Factor::Queue, true),
            ..ExecConfig::default()
        });
        s.submit(intent(Side::Buy, OrderStyle::Passive, "0.50", 100), 1_000);
        s.tick(1_000, &st);
        // 150 trades away, then the level update reports the same 150 gone.
        s.on_trade("UP", p("0.50"), Qty::from_shares(150), Side::Sell, 1_100);
        s.on_level_update("UP", Side::Buy, p("0.50"), Qty::from_shares(50));
        // The queue should be 50 (200 - 150 traded), not 0.
        s.on_trade("UP", p("0.50"), Qty::from_shares(60), Side::Sell, 1_200);
        let fills = s.drain_fills();
        assert_eq!(fills.len(), 1);
        assert_eq!(
            fills[0].qty,
            Qty::from_shares(10),
            "60 traded less 50 still ahead leaves 10 for us"
        );
    }

    #[test]
    fn order_latency_can_turn_a_fill_into_a_miss() {
        let st = state_with_book();
        let mut s = sim(Realism::IDEAL.with(Factor::OrderLatency, true));
        // Marketable at decision time against the 0.51 ask.
        s.submit(
            intent(Side::Buy, OrderStyle::Aggressive, "0.51", 100),
            1_000,
        );
        s.tick(1_000, &st);
        assert!(s.drain_fills().is_empty(), "order is still in flight");

        // While in flight the ask lifts to 0.53, beyond our limit.
        let mut moved = MarketState::new();
        moved.apply(&MarketEvent {
            seq: 1,
            recv_ms: 1_050,
            exchange_ms: 1_050,
            payload: EventPayload::Snapshot {
                asset_id: "UP".into(),
                bids: vec![Level {
                    price: p("0.52"),
                    qty: Qty::from_shares(100),
                }],
                asks: vec![Level {
                    price: p("0.53"),
                    qty: Qty::from_shares(100),
                }],
                tick_size: Some(p("0.01")),
                hash: None,
            },
        });
        s.tick(1_100, &moved);
        assert!(s.drain_fills().is_empty(), "the market ran away from us");
        assert_eq!(s.stats().unfillable, 1);
        assert_eq!(s.stats().missed(), 1);
    }

    #[test]
    fn fees_are_charged_only_when_the_fee_factor_is_on() {
        let st = state_with_book();
        let cfg = ExecConfig {
            taker_fee_bps: 100, // 1%
            realism: Realism::IDEAL.with(Factor::Fees, true),
            ..ExecConfig::default()
        };
        let mut s = ExecutionSimulator::new(cfg);
        s.submit(
            intent(Side::Buy, OrderStyle::Aggressive, "0.52", 100),
            1_000,
        );
        s.tick(1_000, &st);
        let f = &s.drain_fills()[0];
        // 100 shares at 0.51 is $51.00; 1% is $0.51.
        assert_eq!(f.notional(), Usdc(51_000_000));
        assert_eq!(f.fee, Usdc(510_000));
    }

    #[test]
    fn resting_orders_expire_after_their_ttl_plus_cancel_latency() {
        let st = state_with_book();
        let mut s = ExecutionSimulator::new(ExecConfig {
            latency: LatencyModel {
                market_data_ms: 0,
                submit_ms: 0,
                cancel_ms: 100,
            },
            order_ttl_ms: 1_000,
            realism: Realism::IDEAL
                .with(Factor::Queue, true)
                .with(Factor::OrderLatency, true),
            ..ExecConfig::default()
        });
        s.submit(intent(Side::Buy, OrderStyle::Passive, "0.50", 100), 1_000);
        s.tick(1_000, &st);
        assert_eq!(s.resting_count(), 1);
        s.tick(2_050, &st); // TTL hit at 2000, cancel lands at 2100
        assert_eq!(s.resting_count(), 1, "cancel is still in flight");
        s.tick(2_150, &st);
        assert_eq!(s.resting_count(), 0);
        assert_eq!(s.stats().expired_unfilled, 1);
    }

    #[test]
    fn realism_sets_round_trip_through_factor_subsets() {
        assert_eq!(Realism::from_set(&Factor::ALL), Realism::REAL);
        assert_eq!(Realism::from_set(&[]), Realism::IDEAL);
        let r = Realism::from_set(&[Factor::Queue, Factor::Fees]);
        assert!(r.queue && r.fees);
        assert!(!r.depth && !r.md_latency && !r.order_latency);
        assert_eq!(r.enabled(), vec![Factor::Queue, Factor::Fees]);
    }
}
