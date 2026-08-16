//! A transparent reference signal, used to generate orders to measure.
//!
//! # This is not a trading strategy
//!
//! The purpose of this crate is to measure *execution*, and measuring
//! execution requires a stream of order decisions to measure it on. This
//! module supplies the simplest defensible one: trade in the direction of
//! displayed order-book imbalance.
//!
//! It is deliberately naive and is **not** claimed to be profitable. Its
//! only required property is that it produces decisions at realistic moments
//! from realistic information, so that the ideal-versus-realistic comparison
//! is being run on something a person might plausibly have tried. Any
//! signal would do; a better one would not change the execution conclusions,
//! which is the point the platform exists to make.
//!
//! Only the `Up` token of each market is traded. `Up` and `Down` are
//! complements of one another, so trading both would double the same
//! position while appearing to diversify it.

use std::collections::HashMap;

use crate::execution::orders::{OrderIntent, OrderStyle};
use crate::lineage::{Decision, DecisionId};
use crate::market::state::MarketState;
use crate::polymarket::market_discovery::MarketDescriptor;
use crate::types::{Price, Qty, Side};

/// How the reference signal places its orders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ExecStyle {
    /// Rest at the touch and wait.
    Passive,
    /// Cross the spread for immediacy.
    Aggressive,
    /// Half passive, half aggressive — exercises every realism factor at once.
    Split,
}

/// Reference-signal parameters.
#[derive(Debug, Clone)]
pub struct SignalConfig {
    /// Imbalance magnitude required to act, in `[0, 1]`.
    pub threshold: f64,
    /// Book levels included in the imbalance calculation.
    pub depth_levels: usize,
    /// Minimum time between decisions on one token.
    pub cooldown_ms: i64,
    /// Size per decision, in whole shares.
    pub order_shares: u64,
    /// Cap on absolute position per token, in whole shares.
    pub max_position_shares: u64,
    /// How orders are placed.
    pub style: ExecStyle,
    /// Market ticks beyond the touch an aggressive order will pay.
    ///
    /// Counted in the market's own tick size (`0.01` on these markets), not
    /// in the finer `1e-4` price representation.
    pub aggression_ticks: u32,
}

impl Default for SignalConfig {
    fn default() -> SignalConfig {
        SignalConfig {
            threshold: 0.35,
            depth_levels: 5,
            cooldown_ms: 2_000,
            order_shares: 100,
            max_position_shares: 500,
            style: ExecStyle::Split,
            aggression_ticks: 1,
        }
    }
}

/// What the strategy is allowed to look at when deciding.
pub struct StrategyContext<'a> {
    /// Current event time, in milliseconds.
    pub now_ms: i64,
    /// The book as the strategy sees it — delayed when that factor is on.
    pub observed: &'a MarketState,
    /// Markets in this session.
    pub markets: &'a [MarketDescriptor],
    /// Current signed position in a token, in quantity units.
    pub position: &'a dyn Fn(&str) -> i64,
}

/// Name recorded on every decision this strategy takes.
pub const STRATEGY_NAME: &str = "imbalance_signal";

/// Order-book imbalance signal.
#[derive(Debug)]
pub struct ImbalanceStrategy {
    cfg: SignalConfig,
    last_decision_ms: HashMap<String, i64>,
    decisions: u64,
    next_decision_id: DecisionId,
}

impl ImbalanceStrategy {
    /// Creates the strategy.
    pub fn new(cfg: SignalConfig) -> ImbalanceStrategy {
        ImbalanceStrategy {
            cfg,
            last_decision_ms: HashMap::new(),
            decisions: 0,
            next_decision_id: 1,
        }
    }

    /// Number of decisions taken.
    pub fn decisions(&self) -> u64 {
        self.decisions
    }

    /// Evaluates every open market and returns the decisions taken.
    ///
    /// Returns [`Decision`]s rather than bare intents so every order is
    /// inseparable from the reasoning and the observed book that produced it.
    /// Callers read side, token, size and timestamp from the decision; there
    /// is no path by which they could infer those from an execution result.
    pub fn on_tick(&mut self, ctx: &StrategyContext<'_>) -> Vec<Decision> {
        let mut out = Vec::new();
        for m in ctx.markets {
            // A market past its close no longer trades.
            if ctx.now_ms >= m.close_ts * 1_000 {
                continue;
            }
            let token = &m.up_token;
            let Some(book) = ctx.observed.book(token) else {
                continue;
            };
            let (Some(bid), Some(ask)) = (book.best_bid(), book.best_ask()) else {
                continue;
            };
            let Some(imb) = book.imbalance(self.cfg.depth_levels) else {
                continue;
            };
            if imb.abs() < self.cfg.threshold {
                continue;
            }

            let last = self
                .last_decision_ms
                .get(token)
                .copied()
                .unwrap_or(i64::MIN);
            if ctx.now_ms.saturating_sub(last) < self.cfg.cooldown_ms {
                continue;
            }

            // Bid-heavy books lean up; ask-heavy books lean down.
            let side = if imb > 0.0 { Side::Buy } else { Side::Sell };

            // Respect the position cap, including the order about to be sent.
            let pos = (ctx.position)(token);
            let cap = Qty::from_shares(self.cfg.max_position_shares).0 as i64;
            let projected = pos + side.sign() * Qty::from_shares(self.cfg.order_shares).0 as i64;
            if projected.abs() > cap {
                continue;
            }

            self.last_decision_ms.insert(token.clone(), ctx.now_ms);
            self.decisions += 1;

            // Reference price is the touch the strategy is trying to hit —
            // the price a naive backtest would assume it received.
            let reference = match side {
                Side::Buy => ask.price,
                Side::Sell => bid.price,
            };
            let passive_price = match side {
                Side::Buy => bid.price,
                Side::Sell => ask.price,
            };
            // Aggression is expressed in the market's tick size, so one tick
            // on a 0.01 market moves the limit a full cent.
            let step = self.cfg.aggression_ticks * m.tick_size.ticks().max(1);
            let aggressive_price = match side {
                Side::Buy => Price::from_ticks(ask.price.ticks().saturating_add(step)),
                Side::Sell => Price::from_ticks(bid.price.ticks().saturating_sub(step)),
            };

            let id = self.next_decision_id;
            self.next_decision_id += 1;
            let shares = self.cfg.order_shares;

            let mk = |qty: u64, limit_price: Price, style: OrderStyle| OrderIntent {
                decision_id: id,
                token: token.clone(),
                side,
                qty: Qty::from_shares(qty),
                limit_price,
                style,
                reference_price: reference,
            };
            let intents = match self.cfg.style {
                ExecStyle::Passive => vec![mk(shares, passive_price, OrderStyle::Passive)],
                ExecStyle::Aggressive => {
                    vec![mk(shares, aggressive_price, OrderStyle::Aggressive)]
                }
                ExecStyle::Split => {
                    let half = shares / 2;
                    if half == 0 {
                        Vec::new()
                    } else {
                        vec![
                            mk(half, passive_price, OrderStyle::Passive),
                            mk(shares - half, aggressive_price, OrderStyle::Aggressive),
                        ]
                    }
                }
            };
            if intents.is_empty() {
                continue;
            }

            out.push(Decision {
                id,
                ts_ms: ctx.now_ms,
                strategy: STRATEGY_NAME,
                market_slug: m.slug.clone(),
                token: token.clone(),
                outcome: m.outcome_of(token).unwrap_or("Up"),
                side,
                qty: Qty(intents.iter().map(|i| i.qty.0).sum()),
                best_bid: Some(bid.price),
                best_ask: Some(ask.price),
                spread: book.spread(),
                mid: book.mid(),
                imbalance: imb,
                reference_price: reference,
                intents,
            });
        }
        out
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

    fn market() -> MarketDescriptor {
        MarketDescriptor {
            slug: "btc-updown-5m-1786844100".into(),
            title: "t".into(),
            question: "q".into(),
            condition_id: "c".into(),
            open_ts: 1_786_844_100,
            close_ts: 1_786_844_400,
            up_token: "UP".into(),
            down_token: "DOWN".into(),
            tick_size: p("0.01"),
            min_size: Qty::from_shares(5),
            accepting_orders: true,
        }
    }

    /// A book with `bid_qty` bid-side and `ask_qty` ask-side depth.
    fn state(bid_qty: u64, ask_qty: u64) -> MarketState {
        let mut st = MarketState::new();
        st.apply(&MarketEvent {
            seq: 1,
            recv_ms: 0,
            exchange_ms: 0,
            payload: EventPayload::Snapshot {
                asset_id: "UP".into(),
                bids: vec![Level {
                    price: p("0.50"),
                    qty: Qty::from_shares(bid_qty),
                }],
                asks: vec![Level {
                    price: p("0.51"),
                    qty: Qty::from_shares(ask_qty),
                }],
                tick_size: Some(p("0.01")),
                hash: None,
            },
        });
        st
    }

    fn ctx<'a>(
        now_ms: i64,
        st: &'a MarketState,
        ms: &'a [MarketDescriptor],
        pos: &'a dyn Fn(&str) -> i64,
    ) -> StrategyContext<'a> {
        StrategyContext {
            now_ms,
            observed: st,
            markets: ms,
            position: pos,
        }
    }

    #[test]
    fn a_balanced_book_produces_no_decision() {
        let st = state(100, 100);
        let ms = [market()];
        let zero = |_: &str| 0i64;
        let mut s = ImbalanceStrategy::new(SignalConfig::default());
        assert!(s
            .on_tick(&ctx(1_786_844_200_000, &st, &ms, &zero))
            .is_empty());
    }

    #[test]
    fn a_bid_heavy_book_buys_and_a_split_places_both_styles() {
        let st = state(900, 100);
        let ms = [market()];
        let zero = |_: &str| 0i64;
        let mut s = ImbalanceStrategy::new(SignalConfig::default());
        let decisions = s.on_tick(&ctx(1_786_844_200_000, &st, &ms, &zero));
        assert_eq!(decisions.len(), 1, "one signal is one decision");

        let d = &decisions[0];
        assert_eq!(d.side, Side::Buy);
        assert_eq!(d.intents.len(), 2, "a split decision carries two orders");
        assert_eq!(d.intents[0].style, OrderStyle::Passive);
        assert_eq!(d.intents[0].limit_price, p("0.50"), "passive joins the bid");
        assert_eq!(d.intents[1].style, OrderStyle::Aggressive);
        assert_eq!(
            d.intents[1].limit_price,
            p("0.52"),
            "aggressive pays through the ask"
        );
        // Slippage is measured against the touch a backtest would assume.
        assert!(d.intents.iter().all(|i| i.reference_price == p("0.51")));
        // The decision's size is the sum of the orders it authorised.
        assert_eq!(d.qty, Qty::from_shares(100));
    }

    #[test]
    fn every_intent_carries_the_id_of_the_decision_that_owns_it() {
        let st = state(900, 100);
        let ms = [market()];
        let zero = |_: &str| 0i64;
        let mut s = ImbalanceStrategy::new(SignalConfig::default());
        let decisions = s.on_tick(&ctx(1_786_844_200_000, &st, &ms, &zero));
        for d in &decisions {
            assert!(d.id > 0, "a decision must have an identity");
            for i in &d.intents {
                assert_eq!(
                    i.decision_id, d.id,
                    "an intent must be attributable to its own decision"
                );
                assert_eq!(i.token, d.token);
                assert_eq!(i.side, d.side);
            }
        }
    }

    #[test]
    fn the_decision_records_the_book_it_actually_observed() {
        let st = state(900, 100);
        let ms = [market()];
        let zero = |_: &str| 0i64;
        let mut s = ImbalanceStrategy::new(SignalConfig::default());
        let d = s
            .on_tick(&ctx(1_786_844_200_000, &st, &ms, &zero))
            .remove(0);
        assert_eq!(d.best_bid, Some(p("0.50")));
        assert_eq!(d.best_ask, Some(p("0.51")));
        assert_eq!(d.spread, Some(p("0.01")));
        assert_eq!(d.ts_ms, 1_786_844_200_000);
        assert_eq!(d.strategy, STRATEGY_NAME);
        assert_eq!(d.market_slug, "btc-updown-5m-1786844100");
        assert_eq!(d.outcome, "Up");
        assert!(d.imbalance > 0.0, "a bid-heavy book has positive imbalance");
    }

    #[test]
    fn decision_ids_are_unique_and_increasing_within_a_run() {
        let st = state(900, 100);
        let ms = [market()];
        let zero = |_: &str| 0i64;
        let mut s = ImbalanceStrategy::new(SignalConfig::default());
        let mut ids = Vec::new();
        for t in [0i64, 3_000, 6_000, 9_000] {
            for d in s.on_tick(&ctx(1_786_844_200_000 + t, &st, &ms, &zero)) {
                ids.push(d.id);
            }
        }
        assert!(ids.len() >= 3);
        assert!(
            ids.windows(2).all(|w| w[0] < w[1]),
            "ids must increase: {ids:?}"
        );
    }

    #[test]
    fn an_ask_heavy_book_sells() {
        let st = state(100, 900);
        let ms = [market()];
        let zero = |_: &str| 0i64;
        let mut s = ImbalanceStrategy::new(SignalConfig::default());
        let decisions = s.on_tick(&ctx(1_786_844_200_000, &st, &ms, &zero));
        assert!(decisions.iter().all(|d| d.side == Side::Sell));
        assert!(decisions
            .iter()
            .flat_map(|d| &d.intents)
            .all(|i| i.reference_price == p("0.50")));
    }

    #[test]
    fn the_cooldown_suppresses_repeat_decisions() {
        let st = state(900, 100);
        let ms = [market()];
        let zero = |_: &str| 0i64;
        let mut s = ImbalanceStrategy::new(SignalConfig::default());
        assert!(!s
            .on_tick(&ctx(1_786_844_200_000, &st, &ms, &zero))
            .is_empty());
        assert!(s
            .on_tick(&ctx(1_786_844_200_500, &st, &ms, &zero))
            .is_empty());
        assert!(!s
            .on_tick(&ctx(1_786_844_203_000, &st, &ms, &zero))
            .is_empty());
        assert_eq!(s.decisions(), 2);
    }

    #[test]
    fn the_position_cap_blocks_further_accumulation() {
        let st = state(900, 100);
        let ms = [market()];
        let at_cap = |_: &str| Qty::from_shares(500).0 as i64;
        let mut s = ImbalanceStrategy::new(SignalConfig::default());
        assert!(
            s.on_tick(&ctx(1_786_844_200_000, &st, &ms, &at_cap))
                .is_empty(),
            "already long the maximum"
        );
    }

    #[test]
    fn a_closed_market_is_not_traded() {
        let st = state(900, 100);
        let ms = [market()];
        let zero = |_: &str| 0i64;
        let mut s = ImbalanceStrategy::new(SignalConfig::default());
        // One millisecond past the round's close.
        assert!(s
            .on_tick(&ctx(1_786_844_400_001, &st, &ms, &zero))
            .is_empty());
    }

    #[test]
    fn only_the_up_token_is_traded() {
        let st = state(900, 100);
        let ms = [market()];
        let zero = |_: &str| 0i64;
        let mut s = ImbalanceStrategy::new(SignalConfig::default());
        let decisions = s.on_tick(&ctx(1_786_844_200_000, &st, &ms, &zero));
        assert!(
            decisions.iter().all(|d| d.token == "UP"),
            "Down is the complement of Up, not a second opportunity"
        );
    }
}
