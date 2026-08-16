//! Hypothetical orders and their lifecycle.
//!
//! Nothing in this module reaches the network. An [`Order`] is an intent
//! evaluated against recorded or live market data; it is never signed,
//! serialised to the exchange, or submitted.

use serde::{Deserialize, Serialize};

use crate::types::{Price, Qty, Side};

/// How an order tries to trade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderStyle {
    /// Rest at the touch and wait to be hit — pays no spread, may never fill.
    Passive,
    /// Cross the spread for immediacy — always fills if depth allows.
    Aggressive,
}

/// Why an order stopped being active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Terminal {
    /// Completely filled.
    Filled,
    /// Filled in part, then cancelled or expired.
    PartiallyFilled,
    /// Expired at its time-to-live without any fill.
    Expired,
    /// Arrived when the book could not support it at all.
    Unfillable,
}

/// The live state of a simulated order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderState {
    /// Submitted, still travelling to the matching engine.
    InFlight,
    /// Resting on the book.
    Resting,
    /// Finished.
    Done(Terminal),
}

/// A hypothetical order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Order {
    /// Simulator-assigned identifier.
    pub id: u64,
    /// Decision that authorised this order.
    pub decision_id: crate::lineage::DecisionId,
    /// Token being traded.
    pub token: String,
    /// Direction.
    pub side: Side,
    /// Limit price. Aggressive orders still carry one, as a slippage cap.
    pub limit_price: Price,
    /// Size requested.
    pub qty: Qty,
    /// How the order seeks liquidity.
    pub style: OrderStyle,
    /// When the strategy decided, in event time (ms).
    pub decided_ms: i64,
    /// When the order reaches the matching engine (ms).
    pub arrive_ms: i64,
    /// When the order should be cancelled if still resting (ms).
    pub expire_ms: i64,
    /// Price the strategy saw when it decided, the baseline for slippage.
    pub reference_price: Price,
}

/// A strategy's request to trade, before the simulator assigns timing.
///
/// Always reached through the [`crate::lineage::Decision`] that owns it, so
/// `decision_id` is never absent and never guessed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderIntent {
    /// Decision that produced this intent.
    pub decision_id: crate::lineage::DecisionId,
    /// Token to trade.
    pub token: String,
    /// Direction.
    pub side: Side,
    /// Size.
    pub qty: Qty,
    /// Limit price.
    pub limit_price: Price,
    /// How to seek liquidity.
    pub style: OrderStyle,
    /// Price observed at decision time.
    pub reference_price: Price,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_states_distinguish_no_fill_from_partial_fill() {
        // The report separates these: an expired order costs nothing but
        // forgone edge, while a partial leaves inventory behind.
        assert_ne!(
            OrderState::Done(Terminal::Expired),
            OrderState::Done(Terminal::PartiallyFilled)
        );
    }

    #[test]
    fn intents_carry_the_price_the_strategy_actually_saw() {
        let i = OrderIntent {
            decision_id: 1,
            token: "UP".into(),
            side: Side::Buy,
            qty: Qty::from_shares(100),
            limit_price: Price::parse("0.51").unwrap(),
            style: OrderStyle::Aggressive,
            reference_price: Price::parse("0.50").unwrap(),
        };
        // Reference and limit differ: the strategy saw 0.50 and was willing
        // to pay up to 0.51. Slippage is measured against the former.
        assert_ne!(i.reference_price, i.limit_price);
    }
}
