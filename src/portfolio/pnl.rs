//! Multi-token portfolio and the P&L identity that guards it.
//!
//! The identity every simulated run must satisfy:
//!
//! ```text
//! equity - starting_cash == realized + unrealized - fees
//! ```
//!
//! where `equity = cash + Σ market value of open positions`. It is checked
//! by [`Portfolio::check_identity`] rather than assumed, and the check is
//! exact — no tolerance — because every operation underneath is integer
//! arithmetic with rounding residue explicitly conserved.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::portfolio::position::Position;
use crate::types::{Price, Qty, Side, Usdc};

/// Cash, positions and fees for one simulated run.
#[derive(Debug, Clone)]
pub struct Portfolio {
    starting_cash: Usdc,
    cash: Usdc,
    fees: Usdc,
    positions: BTreeMap<String, Position>,
}

impl Portfolio {
    /// Creates a portfolio holding only cash.
    pub fn new(starting_cash: Usdc) -> Portfolio {
        Portfolio {
            starting_cash,
            cash: starting_cash,
            fees: Usdc::ZERO,
            positions: BTreeMap::new(),
        }
    }

    /// Cash the portfolio started with.
    pub fn starting_cash(&self) -> Usdc {
        self.starting_cash
    }

    /// Cash currently held.
    pub fn cash(&self) -> Usdc {
        self.cash
    }

    /// Fees paid so far.
    pub fn fees(&self) -> Usdc {
        self.fees
    }

    /// Position in one token, if any.
    pub fn position(&self, token: &str) -> Option<&Position> {
        self.positions.get(token)
    }

    /// Every held position.
    pub fn positions(&self) -> impl Iterator<Item = (&String, &Position)> {
        self.positions.iter()
    }

    /// Applies a fill and its fee.
    pub fn apply_fill(&mut self, token: &str, side: Side, qty: Qty, price: Price, fee: Usdc) {
        let pos = self.positions.entry(token.to_owned()).or_default();
        let effect = pos.apply_fill(side, qty, price);
        self.cash -= effect.spent;
        self.cash -= fee;
        self.fees += fee;
    }

    /// Total realized P&L across tokens, before fees.
    pub fn realized(&self) -> Usdc {
        self.positions.values().map(|p| p.realized).sum()
    }

    /// Total unrealized P&L at the supplied marks.
    ///
    /// Tokens absent from `marks` are valued at their cost basis, i.e. zero
    /// unrealized. That is the honest default: a market whose book has gone
    /// empty offers no evidence about value, and marking it to an invented
    /// price would manufacture P&L.
    pub fn unrealized(&self, marks: &dyn Fn(&str) -> Option<Price>) -> Usdc {
        self.positions
            .iter()
            .map(|(token, pos)| match marks(token) {
                Some(m) => pos.unrealized(m),
                None => Usdc::ZERO,
            })
            .sum()
    }

    /// Cash plus the market value of open positions.
    pub fn equity(&self, marks: &dyn Fn(&str) -> Option<Price>) -> Usdc {
        let holdings: Usdc = self
            .positions
            .iter()
            .map(|(token, pos)| match marks(token) {
                Some(m) => pos.market_value(m),
                // Valued at cost when unmarkable, matching `unrealized`.
                None => pos.cost_basis,
            })
            .sum();
        self.cash + holdings
    }

    /// Total P&L: equity change since inception.
    pub fn total_pnl(&self, marks: &dyn Fn(&str) -> Option<Price>) -> Usdc {
        self.equity(marks) - self.starting_cash
    }

    /// A full snapshot at the supplied marks.
    pub fn snapshot(&self, marks: &dyn Fn(&str) -> Option<Price>) -> PnlSnapshot {
        PnlSnapshot {
            starting_cash: self.starting_cash,
            cash: self.cash,
            equity: self.equity(marks),
            realized: self.realized(),
            unrealized: self.unrealized(marks),
            fees: self.fees,
            total_pnl: self.total_pnl(marks),
            open_positions: self.positions.values().filter(|p| !p.is_flat()).count(),
        }
    }

    /// Verifies `equity - starting_cash == realized + unrealized - fees`.
    ///
    /// Returns the signed discrepancy, which must be exactly zero.
    pub fn check_identity(&self, marks: &dyn Fn(&str) -> Option<Price>) -> Usdc {
        self.total_pnl(marks) - (self.realized() + self.unrealized(marks) - self.fees)
    }
}

/// A reportable P&L snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PnlSnapshot {
    /// Cash at inception.
    pub starting_cash: Usdc,
    /// Cash now.
    pub cash: Usdc,
    /// Cash plus market value of positions.
    pub equity: Usdc,
    /// Realized P&L, before fees.
    pub realized: Usdc,
    /// Unrealized P&L at the marks used.
    pub unrealized: Usdc,
    /// Fees paid.
    pub fees: Usdc,
    /// Equity change since inception.
    pub total_pnl: Usdc,
    /// Number of tokens with a non-zero position.
    pub open_positions: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Price {
        Price::parse(s).unwrap()
    }

    fn no_marks(_: &str) -> Option<Price> {
        None
    }

    #[test]
    fn identity_holds_through_a_round_trip_with_fees() {
        let mut pf = Portfolio::new(Usdc::from_dollars(10_000));
        let marks = |t: &str| (t == "UP").then(|| p("0.55"));

        pf.apply_fill(
            "UP",
            Side::Buy,
            Qty::from_shares(100),
            p("0.50"),
            Usdc(12_345),
        );
        assert_eq!(pf.check_identity(&marks), Usdc::ZERO);

        pf.apply_fill(
            "UP",
            Side::Sell,
            Qty::from_shares(60),
            p("0.55"),
            Usdc(7_000),
        );
        assert_eq!(pf.check_identity(&marks), Usdc::ZERO);

        pf.apply_fill(
            "UP",
            Side::Sell,
            Qty::from_shares(40),
            p("0.52"),
            Usdc(3_333),
        );
        assert_eq!(pf.check_identity(&marks), Usdc::ZERO);
        assert_eq!(pf.check_identity(&no_marks), Usdc::ZERO);
    }

    #[test]
    fn identity_holds_across_many_awkward_prices_and_sizes() {
        let mut pf = Portfolio::new(Usdc::from_dollars(1_000));
        let marks = |_: &str| Some(p("0.4321"));
        // Deliberately non-dividing sizes and prices to stress the rounding
        // conservation in the cost-basis split.
        let script = [
            (Side::Buy, 7u64, "0.3333"),
            (Side::Buy, 11, "0.6667"),
            (Side::Sell, 5, "0.4999"),
            (Side::Sell, 9, "0.1234"),
            (Side::Buy, 3, "0.8888"),
            (Side::Sell, 7, "0.7777"),
        ];
        for (i, (side, shares, price)) in script.into_iter().enumerate() {
            pf.apply_fill(
                if i % 2 == 0 { "UP" } else { "DOWN" },
                side,
                Qty::from_shares(shares),
                p(price),
                Usdc(i as i64 * 137),
            );
            assert_eq!(
                pf.check_identity(&marks),
                Usdc::ZERO,
                "identity broke after fill {i}"
            );
        }
    }

    #[test]
    fn unmarkable_positions_contribute_no_invented_pnl() {
        let mut pf = Portfolio::new(Usdc::from_dollars(100));
        pf.apply_fill(
            "GONE",
            Side::Buy,
            Qty::from_shares(10),
            p("0.50"),
            Usdc::ZERO,
        );
        // With no mark the position is held at cost: zero unrealized, and
        // equity unchanged from inception.
        assert_eq!(pf.unrealized(&no_marks), Usdc::ZERO);
        assert_eq!(pf.total_pnl(&no_marks), Usdc::ZERO);
        assert_eq!(pf.check_identity(&no_marks), Usdc::ZERO);
    }

    #[test]
    fn fees_reduce_equity_and_are_reported_separately() {
        let mut pf = Portfolio::new(Usdc::from_dollars(100));
        let marks = |_: &str| Some(p("0.50"));
        pf.apply_fill(
            "UP",
            Side::Buy,
            Qty::from_shares(10),
            p("0.50"),
            Usdc::from_dollars(1),
        );
        let s = pf.snapshot(&marks);
        assert_eq!(s.fees, Usdc::from_dollars(1));
        assert_eq!(s.realized, Usdc::ZERO);
        assert_eq!(s.unrealized, Usdc::ZERO);
        assert_eq!(s.total_pnl, -Usdc::from_dollars(1));
        assert_eq!(pf.check_identity(&marks), Usdc::ZERO);
    }
}
