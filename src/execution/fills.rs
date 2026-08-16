//! Fills and the slippage each one carries.

use serde::{Deserialize, Serialize};

use crate::types::{Price, Qty, Side, Usdc};

/// Whether a fill provided or removed liquidity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Liquidity {
    /// Rested on the book and was hit.
    Maker,
    /// Crossed the spread.
    Taker,
}

/// One simulated execution.
///
/// Carries the whole lineage chain: which decision authorised it, which order
/// it came from, and its own identity, so a row in `fills.csv` can be joined
/// back to `orders.csv` and `decisions.csv` without inference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fill {
    /// Decision that ultimately caused this fill.
    pub decision_id: crate::lineage::DecisionId,
    /// Order that produced it.
    pub order_id: u64,
    /// Identity of this fill, unique within a run.
    ///
    /// One order can fill many times when it walks depth, so the order id
    /// alone does not identify a row.
    pub fill_id: u64,
    /// Token traded.
    pub token: String,
    /// Direction.
    pub side: Side,
    /// Execution price.
    pub price: Price,
    /// Executed size.
    pub qty: Qty,
    /// Event time of the fill (ms).
    pub ts_ms: i64,
    /// Whether the fill made or took liquidity.
    pub liquidity: Liquidity,
    /// Fee charged on this fill.
    pub fee: Usdc,
    /// Price the strategy saw when it decided.
    pub reference_price: Price,
}

impl Fill {
    /// Price movement against the order, in price ticks.
    ///
    /// Positive is adverse: a buy that paid more than it expected to, or a
    /// sell that received less.
    pub fn adverse_ticks(&self) -> i64 {
        (self.price.ticks() as i64 - self.reference_price.ticks() as i64) * self.side.sign()
    }

    /// Money given up relative to the reference price.
    ///
    /// Positive is a cost. Computed from the notional difference rather than
    /// from `adverse_ticks * qty` so it rounds identically to the cash
    /// figures that flow through the portfolio.
    pub fn slippage_cost(&self) -> Usdc {
        let at_fill = self.qty.notional(self.price);
        let at_reference = self.qty.notional(self.reference_price);
        Usdc((at_fill.0 - at_reference.0) * self.side.sign())
    }

    /// Notional value of the fill.
    pub fn notional(&self) -> Usdc {
        self.qty.notional(self.price)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(side: Side, price: &str, reference: &str) -> Fill {
        Fill {
            decision_id: 1,
            order_id: 1,
            fill_id: 1,
            token: "UP".into(),
            side,
            price: Price::parse(price).unwrap(),
            qty: Qty::from_shares(100),
            ts_ms: 0,
            liquidity: Liquidity::Taker,
            fee: Usdc::ZERO,
            reference_price: Price::parse(reference).unwrap(),
        }
    }

    #[test]
    fn buying_above_the_reference_is_adverse() {
        let f = fill(Side::Buy, "0.52", "0.50");
        assert_eq!(f.adverse_ticks(), 200);
        // 100 shares paying 2 cents more is $2.00 of slippage.
        assert_eq!(f.slippage_cost(), Usdc::from_dollars(2));
    }

    #[test]
    fn selling_below_the_reference_is_adverse() {
        let f = fill(Side::Sell, "0.48", "0.50");
        assert_eq!(f.adverse_ticks(), 200);
        assert_eq!(f.slippage_cost(), Usdc::from_dollars(2));
    }

    #[test]
    fn price_improvement_is_reported_as_negative_cost() {
        let f = fill(Side::Buy, "0.49", "0.50");
        assert_eq!(f.adverse_ticks(), -100);
        assert_eq!(f.slippage_cost(), -Usdc::from_dollars(1));
    }

    #[test]
    fn a_fill_at_the_reference_has_no_slippage() {
        assert_eq!(fill(Side::Buy, "0.50", "0.50").slippage_cost(), Usdc::ZERO);
    }
}
