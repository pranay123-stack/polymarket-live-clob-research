//! Event lineage: tracing every result back to the decision that caused it.
//!
//! ```text
//! Decision ──▶ Intent ──▶ Order ──▶ Fill ──▶ Position change ──▶ P&L impact
//! ```
//!
//! Every stage carries the [`DecisionId`] of its origin, so any figure in any
//! report can be walked back to the moment a strategy chose to act and to the
//! book it was looking at when it did.
//!
//! # The identity is structural, not reconstructed
//!
//! A [`Decision`] **owns** its [`OrderIntent`]s. There is no constructor that
//! produces a loose intent, so an order cannot exist without a decision to
//! attribute it to, and nothing downstream ever has to infer a decision's
//! side, token, size or timestamp from an execution result. That inference is
//! precisely the bug this design forecloses: an earlier version of shadow mode
//! guessed the side from the market rather than reading it from the intent,
//! and reported a directional hit rate of 100% because the guess was constant.
//!
//! # Per-decision P&L
//!
//! [`DecisionLedger`] keeps FIFO lots tagged with the decision that opened
//! them. When a later fill closes a lot, the round-trip profit is credited to
//! the **opening** decision — the one that took the risk — and the closing
//! fill's own exposure is what remains of it. Lots still open at the end are
//! marked to the final midpoint.
//!
//! This decomposition sums to the portfolio's total P&L up to integer
//! rounding, which is reported explicitly as
//! [`LedgerSummary::rounding`] rather than hidden. See `docs/SYSTEM_AUDIT.md`.

use std::collections::{BTreeMap, HashMap, VecDeque};

use serde::{Deserialize, Serialize};

use crate::execution::fills::Fill;
use crate::execution::orders::{OrderIntent, Terminal};
use crate::types::{Price, Qty, Side, Usdc};

/// Identifies one strategy decision within a run.
///
/// Sequential from 1, assigned by the strategy at the moment it decides.
/// Sequential rather than random so a run is readable and reproducible; use
/// [`Decision::natural_key`] to match decisions across two different runs,
/// where the counters need not line up.
pub type DecisionId = u64;

/// A strategy's choice to act, and the market it saw when it chose.
///
/// Owns the orders it produced, so lineage cannot be broken by construction.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    /// Identity of this decision.
    pub id: DecisionId,
    /// Event time at which the strategy decided, in milliseconds.
    pub ts_ms: i64,
    /// Name of the strategy that decided.
    pub strategy: &'static str,
    /// Market slug, e.g. `btc-updown-5m-1786844100`.
    pub market_slug: String,
    /// Token traded.
    pub token: String,
    /// Outcome the token represents, `Up` or `Down`.
    pub outcome: &'static str,
    /// Direction chosen.
    pub side: Side,
    /// Total size the decision wanted, across its orders.
    pub qty: Qty,
    /// Best bid on the **observed** book at decision time.
    pub best_bid: Option<Price>,
    /// Best ask on the observed book at decision time.
    pub best_ask: Option<Price>,
    /// Spread on the observed book at decision time.
    pub spread: Option<Price>,
    /// Midpoint of the observed book at decision time.
    pub mid: Option<Price>,
    /// Order-book imbalance that triggered the decision.
    pub imbalance: f64,
    /// Touch price the strategy was aiming at — the slippage baseline.
    pub reference_price: Price,
    /// The orders this decision produced.
    pub intents: Vec<OrderIntent>,
}

impl Decision {
    /// A key that identifies the same decision across two different runs.
    ///
    /// Decision counters restart per run and can diverge when execution
    /// differs — a position cap binds at a different moment, say — so
    /// comparing two runs matches on what the decision *was* rather than on
    /// the order it happened to be taken in.
    pub fn natural_key(&self) -> (String, i64, Side) {
        (self.token.clone(), self.ts_ms, self.side)
    }
}

/// What the market did after a decision, and why the money went where it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    /// The market subsequently moved the way the decision wanted.
    Correct,
    /// The market moved against the decision.
    Wrong,
    /// The market did not move, or there was no forward mark to judge against.
    Undetermined,
}

impl Verdict {
    /// Report label.
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Correct => "CORRECT",
            Verdict::Wrong => "WRONG",
            Verdict::Undetermined => "UNDETERMINED",
        }
    }
}

/// The dominant reason a decision's realised result differs from its ideal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cause {
    /// Execution matched the ideal; any loss came from the decision itself.
    NoExecutionLoss,
    /// The book moved past the order's limit while it was in flight.
    Latency,
    /// The order rested and was never reached before it was cancelled.
    QueuePosition,
    /// Only part of the requested size was available.
    PartialFill,
    /// The full size executed, but at worse prices than the touch.
    Slippage,
    /// Nothing was available to trade against on arrival.
    Liquidity,
    /// Fees were the only cost.
    Fees,
}

impl Cause {
    /// Report label.
    pub fn label(self) -> &'static str {
        match self {
            Cause::NoExecutionLoss => "no execution loss",
            Cause::Latency => "latency",
            Cause::QueuePosition => "queue position",
            Cause::PartialFill => "partial fill",
            Cause::Slippage => "slippage",
            Cause::Liquidity => "liquidity",
            Cause::Fees => "fees",
        }
    }
}

/// How one order ended, recorded for lineage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderOutcome {
    /// Decision that produced the order.
    pub decision_id: DecisionId,
    /// Order identity.
    pub order_id: u64,
    /// Size requested.
    pub requested: Qty,
    /// Size executed.
    pub executed: Qty,
    /// Terminal state.
    pub terminal: Terminal,
    /// When the strategy decided.
    pub decided_ms: i64,
    /// When the order reached the matching engine.
    pub arrive_ms: i64,
}

/// One FIFO lot, tagged with the decision that opened it.
#[derive(Debug, Clone, Copy)]
struct Lot {
    decision_id: DecisionId,
    qty: u64,
    price: Price,
    sign: i64,
}

/// Per-decision profit and loss.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionPnl {
    /// P&L booked when this decision's lots were closed by later fills.
    pub realized: Usdc,
    /// P&L on lots still open at the final mark.
    pub unrealized: Usdc,
    /// Fees this decision paid.
    pub fees: Usdc,
    /// `realized + unrealized - fees`.
    pub net: Usdc,
    /// Slippage against the decision's reference price.
    pub slippage: Usdc,
    /// Size requested across the decision's orders.
    pub requested: Qty,
    /// Size executed.
    pub executed: Qty,
}

impl DecisionPnl {
    /// Fraction of requested size that executed, in `[0, 1]`.
    pub fn fill_ratio(&self) -> f64 {
        if self.requested.is_zero() {
            return 0.0;
        }
        self.executed.0 as f64 / self.requested.0 as f64
    }
}

/// Totals across every decision in a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LedgerSummary {
    /// Decisions taken.
    pub decisions: usize,
    /// Orders produced.
    pub orders: usize,
    /// Fills produced.
    pub fills: usize,
    /// Sum of per-decision realized P&L.
    pub realized: Usdc,
    /// Sum of per-decision unrealized P&L.
    pub unrealized: Usdc,
    /// Sum of per-decision fees.
    pub fees: Usdc,
    /// Sum of per-decision net P&L.
    pub net: Usdc,
    /// Difference between the ledger's net and the portfolio's total P&L.
    ///
    /// Non-zero only through integer rounding, because the ledger splits
    /// notionals at lot boundaries while the portfolio splits them at fill
    /// boundaries. Reported so the table always balances against the
    /// portfolio rather than appearing to disagree with it.
    pub rounding: Usdc,
}

/// Records the full lineage of a run and computes per-decision P&L.
#[derive(Debug, Default, Clone)]
pub struct DecisionLedger {
    decisions: BTreeMap<DecisionId, Decision>,
    outcomes: Vec<OrderOutcome>,
    fills: Vec<Fill>,
    lots: HashMap<String, VecDeque<Lot>>,
    realized: BTreeMap<DecisionId, Usdc>,
    unrealized: BTreeMap<DecisionId, Usdc>,
    fees: BTreeMap<DecisionId, Usdc>,
    slippage: BTreeMap<DecisionId, Usdc>,
    executed: BTreeMap<DecisionId, Qty>,
    forward_mid: BTreeMap<DecisionId, (Price, i64)>,
    finalized: bool,
}

impl DecisionLedger {
    /// Creates an empty ledger.
    pub fn new() -> DecisionLedger {
        DecisionLedger::default()
    }

    /// Records a decision and the orders it authorised.
    pub fn record_decision(&mut self, d: Decision) {
        self.decisions.insert(d.id, d);
    }

    /// Records how an order ended.
    pub fn record_outcome(&mut self, o: OrderOutcome) {
        self.outcomes.push(o);
    }

    /// Records the market midpoint some time after a decision was taken.
    ///
    /// This is what separates *"the signal was wrong"* from *"the execution
    /// was bad"*. Without it a losing decision is uninterpretable: the two
    /// look identical on the P&L line.
    ///
    /// `elapsed_ms` is how much event time actually passed, which may be less
    /// than the configured horizon when a session ends first. It is recorded
    /// and exported rather than assumed, so a verdict drawn from a truncated
    /// window is visible as such. A zero or negative window is refused: it
    /// would produce a verdict from no information at all.
    pub fn record_forward_mid(&mut self, id: DecisionId, mid: Price, elapsed_ms: i64) {
        if elapsed_ms <= 0 {
            return;
        }
        self.forward_mid.entry(id).or_insert((mid, elapsed_ms));
    }

    /// Midpoint after the decision, if a forward window was observed.
    pub fn forward_mid(&self, id: DecisionId) -> Option<Price> {
        self.forward_mid.get(&id).map(|(p, _)| *p)
    }

    /// Event time that actually elapsed before the forward midpoint was taken.
    pub fn forward_elapsed_ms(&self, id: DecisionId) -> Option<i64> {
        self.forward_mid.get(&id).map(|(_, e)| *e)
    }

    /// Midpoint move after a decision, in price ticks, signed by its direction.
    ///
    /// Positive means the market moved the way the decision wanted.
    pub fn forward_ticks(&self, id: DecisionId) -> Option<i64> {
        let d = self.decisions.get(&id)?;
        let before = d.mid?;
        let (after, _) = self.forward_mid.get(&id)?;
        Some((after.ticks() as i64 - before.ticks() as i64) * d.side.sign())
    }

    /// Whether the market subsequently moved the way the decision wanted.
    ///
    /// Judged on the midpoint alone, deliberately: it is the one measure of
    /// the decision that is independent of how well the order was executed.
    pub fn verdict(&self, id: DecisionId) -> Verdict {
        match self.forward_ticks(id) {
            Some(t) if t > 0 => Verdict::Correct,
            Some(t) if t < 0 => Verdict::Wrong,
            _ => Verdict::Undetermined,
        }
    }

    /// Records a fill and folds it into the FIFO lots.
    ///
    /// Must be called in execution order; FIFO matching depends on it.
    pub fn record_fill(&mut self, fill: &Fill) {
        debug_assert!(!self.finalized, "fills must not arrive after finalize");
        let sign = fill.side.sign();
        let mut remaining = fill.qty.0;

        let queue = self.lots.entry(fill.token.clone()).or_default();
        // Close opposing lots first, oldest to newest.
        while remaining > 0 {
            let Some(front) = queue.front_mut() else {
                break;
            };
            if front.sign == sign {
                break;
            }
            let take = remaining.min(front.qty);
            let gross = Usdc(
                (Qty(take).notional(fill.price).0 - Qty(take).notional(front.price).0) * front.sign,
            );
            *self.realized.entry(front.decision_id).or_default() += gross;
            front.qty -= take;
            remaining -= take;
            if front.qty == 0 {
                queue.pop_front();
            }
        }
        // Whatever is left opens a new lot owned by this decision.
        if remaining > 0 {
            queue.push_back(Lot {
                decision_id: fill.decision_id,
                qty: remaining,
                price: fill.price,
                sign,
            });
        }

        *self.fees.entry(fill.decision_id).or_default() += fill.fee;
        *self.slippage.entry(fill.decision_id).or_default() += fill.slippage_cost();
        let e = self.executed.entry(fill.decision_id).or_default();
        *e = Qty(e.0 + fill.qty.0);
        self.fills.push(fill.clone());
    }

    /// Marks every still-open lot and closes the ledger.
    ///
    /// A token with no mark is held at cost, contributing zero unrealized
    /// P&L, matching how the portfolio values an unmarkable position.
    pub fn finalize(&mut self, marks: &dyn Fn(&str) -> Option<Price>) {
        for (token, queue) in &self.lots {
            let Some(mark) = marks(token) else { continue };
            for lot in queue {
                let gross = Usdc(
                    (Qty(lot.qty).notional(mark).0 - Qty(lot.qty).notional(lot.price).0) * lot.sign,
                );
                *self.unrealized.entry(lot.decision_id).or_default() += gross;
            }
        }
        self.finalized = true;
    }

    /// Every decision, in the order taken.
    pub fn decisions(&self) -> impl Iterator<Item = &Decision> {
        self.decisions.values()
    }

    /// A decision by id.
    pub fn decision(&self, id: DecisionId) -> Option<&Decision> {
        self.decisions.get(&id)
    }

    /// Every order outcome.
    pub fn outcomes(&self) -> &[OrderOutcome] {
        &self.outcomes
    }

    /// Every fill, in execution order.
    pub fn fills(&self) -> &[Fill] {
        &self.fills
    }

    /// P&L for one decision.
    pub fn pnl(&self, id: DecisionId) -> DecisionPnl {
        let realized = self.realized.get(&id).copied().unwrap_or_default();
        let unrealized = self.unrealized.get(&id).copied().unwrap_or_default();
        let fees = self.fees.get(&id).copied().unwrap_or_default();
        let requested = self.decisions.get(&id).map(|d| d.qty).unwrap_or(Qty::ZERO);
        DecisionPnl {
            realized,
            unrealized,
            fees,
            net: realized + unrealized - fees,
            slippage: self.slippage.get(&id).copied().unwrap_or_default(),
            requested,
            executed: self.executed.get(&id).copied().unwrap_or_default(),
        }
    }

    /// Totals across the run, with the rounding residual against `portfolio_total`.
    pub fn summary(&self, portfolio_total: Usdc) -> LedgerSummary {
        let realized: Usdc = self.realized.values().copied().sum();
        let unrealized: Usdc = self.unrealized.values().copied().sum();
        let fees: Usdc = self.fees.values().copied().sum();
        let net = realized + unrealized - fees;
        LedgerSummary {
            decisions: self.decisions.len(),
            orders: self.outcomes.len(),
            fills: self.fills.len(),
            realized,
            unrealized,
            fees,
            net,
            rounding: portfolio_total - net,
        }
    }

    /// The dominant reason this decision's execution fell short of ideal.
    ///
    /// Read from what actually happened to the decision's orders, in order of
    /// how much it cost: an order that never filled is a larger problem than
    /// one that filled expensively.
    pub fn cause(&self, id: DecisionId) -> Cause {
        let outcomes: Vec<&OrderOutcome> = self
            .outcomes
            .iter()
            .filter(|o| o.decision_id == id)
            .collect();
        if outcomes.is_empty() {
            return Cause::NoExecutionLoss;
        }
        let any = |t: Terminal| outcomes.iter().any(|o| o.terminal == t);
        let pnl = self.pnl(id);

        if any(Terminal::Unfillable) {
            // The book moved past the limit in flight, or offered nothing at
            // all. Both are recorded as unfillable; distinguish by whether
            // anything else about the decision executed.
            return if pnl.executed.is_zero() && outcomes.len() == 1 {
                Cause::Liquidity
            } else {
                Cause::Latency
            };
        }
        if any(Terminal::Expired) {
            return Cause::QueuePosition;
        }
        if any(Terminal::PartiallyFilled) {
            return Cause::PartialFill;
        }
        if pnl.slippage > Usdc::ZERO {
            return Cause::Slippage;
        }
        if pnl.fees > Usdc::ZERO {
            return Cause::Fees;
        }
        Cause::NoExecutionLoss
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::fills::Liquidity;

    fn p(s: &str) -> Price {
        Price::parse(s).unwrap()
    }

    fn decision(id: DecisionId, side: Side, shares: u64) -> Decision {
        Decision {
            id,
            ts_ms: 1_000 * id as i64,
            strategy: "test",
            market_slug: "btc-updown-5m-1".into(),
            token: "UP".into(),
            outcome: "Up",
            side,
            qty: Qty::from_shares(shares),
            best_bid: Some(p("0.50")),
            best_ask: Some(p("0.51")),
            spread: Some(p("0.01")),
            mid: Some(p("0.505")),
            imbalance: 0.5,
            reference_price: p("0.51"),
            intents: Vec::new(),
        }
    }

    fn fill(decision_id: DecisionId, side: Side, shares: u64, price: &str) -> Fill {
        Fill {
            decision_id,
            order_id: decision_id,
            fill_id: decision_id * 10,
            token: "UP".into(),
            side,
            price: p(price),
            qty: Qty::from_shares(shares),
            ts_ms: 0,
            liquidity: Liquidity::Taker,
            fee: Usdc::ZERO,
            reference_price: p("0.51"),
        }
    }

    #[test]
    fn round_trip_profit_is_credited_to_the_decision_that_opened_it() {
        let mut l = DecisionLedger::new();
        l.record_decision(decision(1, Side::Buy, 100));
        l.record_decision(decision(2, Side::Sell, 100));
        l.record_fill(&fill(1, Side::Buy, 100, "0.50"));
        l.record_fill(&fill(2, Side::Sell, 100, "0.60"));
        l.finalize(&|_| Some(p("0.60")));

        // Decision 1 took the risk and made the $10.
        assert_eq!(l.pnl(1).realized, Usdc::from_dollars(10));
        assert_eq!(l.pnl(1).unrealized, Usdc::ZERO);
        // Decision 2 closed it and holds nothing.
        assert_eq!(l.pnl(2).realized, Usdc::ZERO);
        assert_eq!(l.pnl(2).unrealized, Usdc::ZERO);
    }

    #[test]
    fn an_open_position_is_marked_to_the_final_price() {
        let mut l = DecisionLedger::new();
        l.record_decision(decision(1, Side::Buy, 100));
        l.record_fill(&fill(1, Side::Buy, 100, "0.50"));
        l.finalize(&|_| Some(p("0.55")));
        assert_eq!(l.pnl(1).unrealized, Usdc::from_dollars(5));
        assert_eq!(l.pnl(1).net, Usdc::from_dollars(5));
    }

    #[test]
    fn lots_close_oldest_first() {
        let mut l = DecisionLedger::new();
        for (id, price) in [(1u64, "0.40"), (2, "0.50")] {
            l.record_decision(decision(id, Side::Buy, 100));
            l.record_fill(&fill(id, Side::Buy, 100, price));
        }
        l.record_decision(decision(3, Side::Sell, 100));
        l.record_fill(&fill(3, Side::Sell, 100, "0.60"));
        l.finalize(&|_| Some(p("0.60")));

        // FIFO: the 0.40 lot closes first, for $20.
        assert_eq!(l.pnl(1).realized, Usdc::from_dollars(20));
        assert_eq!(l.pnl(1).unrealized, Usdc::ZERO);
        // The 0.50 lot is still open and marked at 0.60, for $10.
        assert_eq!(l.pnl(2).realized, Usdc::ZERO);
        assert_eq!(l.pnl(2).unrealized, Usdc::from_dollars(10));
    }

    #[test]
    fn an_unmarkable_token_contributes_no_invented_pnl() {
        let mut l = DecisionLedger::new();
        l.record_decision(decision(1, Side::Buy, 100));
        l.record_fill(&fill(1, Side::Buy, 100, "0.50"));
        l.finalize(&|_| None);
        assert_eq!(l.pnl(1).unrealized, Usdc::ZERO);
    }

    #[test]
    fn fees_are_charged_to_the_decision_that_paid_them() {
        let mut l = DecisionLedger::new();
        l.record_decision(decision(1, Side::Buy, 100));
        let mut f = fill(1, Side::Buy, 100, "0.50");
        f.fee = Usdc::from_dollars(1);
        l.record_fill(&f);
        l.finalize(&|_| Some(p("0.50")));
        let pnl = l.pnl(1);
        assert_eq!(pnl.fees, Usdc::from_dollars(1));
        assert_eq!(pnl.net, -Usdc::from_dollars(1));
    }

    #[test]
    fn natural_keys_match_the_same_decision_across_runs() {
        let a = decision(1, Side::Buy, 100);
        let mut b = decision(7, Side::Buy, 100);
        b.ts_ms = a.ts_ms;
        assert_eq!(
            a.natural_key(),
            b.natural_key(),
            "counters differ across runs; the decision is the same"
        );
    }

    #[test]
    fn cause_reads_the_terminal_state_of_the_decisions_orders() {
        let mut l = DecisionLedger::new();
        l.record_decision(decision(1, Side::Buy, 100));
        l.record_outcome(OrderOutcome {
            decision_id: 1,
            order_id: 1,
            requested: Qty::from_shares(100),
            executed: Qty::ZERO,
            terminal: Terminal::Expired,
            decided_ms: 0,
            arrive_ms: 0,
        });
        assert_eq!(l.cause(1), Cause::QueuePosition);

        let mut l2 = DecisionLedger::new();
        l2.record_decision(decision(2, Side::Buy, 100));
        l2.record_outcome(OrderOutcome {
            decision_id: 2,
            order_id: 2,
            requested: Qty::from_shares(100),
            executed: Qty::from_shares(40),
            terminal: Terminal::PartiallyFilled,
            decided_ms: 0,
            arrive_ms: 0,
        });
        assert_eq!(l2.cause(2), Cause::PartialFill);
    }
}
