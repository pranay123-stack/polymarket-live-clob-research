//! Per-token position and cost-basis accounting.
//!
//! # Why cost basis rather than average price
//!
//! Storing an average entry *price* forces a division on every fill, and the
//! rounding that division sheds is exactly the amount by which the P&L
//! identity later fails to close. This module instead tracks the signed
//! **cost basis in USDC** and moves an exact integer share of it whenever a
//! position is reduced:
//!
//! ```text
//! released   = cost_basis * closing_qty / |position_before|
//! realized  += -spent_on_closing_part - released
//! cost_basis -= released
//! ```
//!
//! Truncation from that division stays inside `cost_basis` instead of
//! escaping, so nothing is created or destroyed and
//!
//! ```text
//! equity - starting_cash == realized + unrealized - fees
//! ```
//!
//! holds to the micro-USDC, not merely to within a tolerance. The property
//! is asserted directly in [`crate::portfolio::pnl`] and in
//! `tests/execution_tests.rs`.
//!
//! # Signed positions
//!
//! Positions may go negative. Polymarket has no naked short: selling a token
//! you do not hold is economically buying its complement at `1 - p`. The
//! simulator models the signed position because it is the clearer accounting
//! object, and because this crate never sends an order, the distinction has
//! no execution consequence here. It would matter to any live implementation.

use serde::{Deserialize, Serialize};

use crate::types::{Price, Qty, Side, Usdc};

/// A signed position in one token, with its cost basis.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    /// Signed size in quantity units; positive is long.
    pub qty: i64,
    /// Signed money spent acquiring the open position.
    ///
    /// Positive for a long (cash paid out), negative for a short (cash taken in).
    pub cost_basis: Usdc,
    /// Cumulative realized profit and loss, excluding fees.
    pub realized: Usdc,
}

/// The cash and bookkeeping effect of applying one fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FillEffect {
    /// Signed money spent: positive when buying, negative when selling.
    pub spent: Usdc,
    /// Realized P&L booked by this fill.
    pub realized_delta: Usdc,
}

impl Position {
    /// An empty position.
    pub fn new() -> Position {
        Position::default()
    }

    /// True when flat.
    pub fn is_flat(&self) -> bool {
        self.qty == 0
    }

    /// Applies a fill, splitting it into its closing and opening parts.
    ///
    /// Notional is computed **separately for each part** rather than once for
    /// the whole fill and then apportioned. Both this method and the caller's
    /// cash update consume the same decomposition, so the two can never
    /// disagree by a rounding unit.
    pub fn apply_fill(&mut self, side: Side, qty: Qty, price: Price) -> FillEffect {
        let signed = side.sign();
        let incoming = qty.0 as i64;

        // How much of this fill reduces an opposing position.
        let closing = if self.qty != 0 && self.qty.signum() != signed {
            incoming.min(self.qty.abs())
        } else {
            0
        };
        let opening = incoming - closing;

        let mut realized_delta = Usdc::ZERO;
        let mut spent = Usdc::ZERO;

        if closing > 0 {
            let notional = Qty(closing as u64).notional(price);
            // Positive when buying back, negative when selling out.
            let spent_close = Usdc(notional.0 * signed);
            // Exact integer share of the basis being retired.
            let released = Usdc(
                ((self.cost_basis.0 as i128 * closing as i128) / self.qty.unsigned_abs() as i128)
                    as i64,
            );
            realized_delta = Usdc(-spent_close.0 - released.0);
            self.cost_basis -= released;
            self.realized += realized_delta;
            self.qty += signed * closing;
            spent += spent_close;
        }

        if opening > 0 {
            let notional = Qty(opening as u64).notional(price);
            let spent_open = Usdc(notional.0 * signed);
            self.cost_basis += spent_open;
            self.qty += signed * opening;
            spent += spent_open;
        }

        // A position that closed exactly flat must retire its whole basis;
        // otherwise truncation residue would masquerade as unrealized P&L on
        // a zero position.
        if self.qty == 0 && self.cost_basis != Usdc::ZERO {
            self.realized -= self.cost_basis;
            realized_delta -= self.cost_basis;
            self.cost_basis = Usdc::ZERO;
        }

        FillEffect {
            spent,
            realized_delta,
        }
    }

    /// Market value of the open position at `mark`.
    pub fn market_value(&self, mark: Price) -> Usdc {
        let v = Qty(self.qty.unsigned_abs()).notional(mark);
        Usdc(v.0 * self.qty.signum())
    }

    /// Unrealized P&L at `mark`: market value less what the position cost.
    pub fn unrealized(&self, mark: Price) -> Usdc {
        self.market_value(mark) - self.cost_basis
    }

    /// Average entry price, for reporting only.
    ///
    /// Derived from the basis on demand and never used in accounting, so its
    /// rounding cannot leak into P&L.
    pub fn avg_price(&self) -> Option<Price> {
        if self.qty == 0 {
            return None;
        }
        let ticks = (self.cost_basis.0 as i128 * 10_000) / self.qty as i128;
        Some(Price::from_ticks(ticks.clamp(0, 10_000) as u32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Price {
        Price::parse(s).unwrap()
    }

    #[test]
    fn buying_then_selling_higher_books_the_gain() {
        let mut pos = Position::new();
        pos.apply_fill(Side::Buy, Qty::from_shares(100), p("0.50"));
        assert_eq!(pos.qty, Qty::from_shares(100).0 as i64);
        assert_eq!(pos.cost_basis, Usdc::from_dollars(50));
        assert_eq!(pos.unrealized(p("0.50")), Usdc::ZERO);
        assert_eq!(pos.unrealized(p("0.55")), Usdc::from_dollars(5));

        pos.apply_fill(Side::Sell, Qty::from_shares(100), p("0.55"));
        assert!(pos.is_flat());
        assert_eq!(pos.realized, Usdc::from_dollars(5));
        assert_eq!(pos.cost_basis, Usdc::ZERO);
        assert_eq!(pos.unrealized(p("0.99")), Usdc::ZERO);
    }

    #[test]
    fn partial_close_retires_a_proportional_share_of_the_basis() {
        let mut pos = Position::new();
        pos.apply_fill(Side::Buy, Qty::from_shares(100), p("0.50"));
        pos.apply_fill(Side::Sell, Qty::from_shares(40), p("0.60"));
        // 40 shares bought at 0.50 sold at 0.60 is $4.00.
        assert_eq!(pos.realized, Usdc::from_dollars(4));
        assert_eq!(pos.qty, Qty::from_shares(60).0 as i64);
        assert_eq!(pos.cost_basis, Usdc::from_dollars(30));
        assert_eq!(pos.avg_price(), Some(p("0.50")));
    }

    #[test]
    fn shorting_then_covering_lower_books_the_gain() {
        let mut pos = Position::new();
        pos.apply_fill(Side::Sell, Qty::from_shares(100), p("0.60"));
        assert_eq!(pos.qty, -(Qty::from_shares(100).0 as i64));
        assert_eq!(pos.cost_basis, -Usdc::from_dollars(60));
        // A short gains when the mark falls.
        assert_eq!(pos.unrealized(p("0.50")), Usdc::from_dollars(10));

        pos.apply_fill(Side::Buy, Qty::from_shares(100), p("0.50"));
        assert!(pos.is_flat());
        assert_eq!(pos.realized, Usdc::from_dollars(10));
    }

    #[test]
    fn a_fill_that_flips_the_position_closes_then_reopens() {
        let mut pos = Position::new();
        pos.apply_fill(Side::Buy, Qty::from_shares(50), p("0.40"));
        // Sell 80: closes the 50 long at 0.50, opens a 30 short at 0.50.
        pos.apply_fill(Side::Sell, Qty::from_shares(80), p("0.50"));
        assert_eq!(pos.qty, -(Qty::from_shares(30).0 as i64));
        assert_eq!(pos.realized, Usdc::from_dollars(5));
        assert_eq!(pos.cost_basis, -Usdc::from_dollars(15));
    }

    #[test]
    fn closing_flat_leaves_no_residual_basis_at_awkward_prices() {
        // 3 shares at $0.33 does not divide evenly; the residue must be
        // booked to realized rather than lingering as phantom unrealized.
        let mut pos = Position::new();
        pos.apply_fill(Side::Buy, Qty::from_shares(3), p("0.3333"));
        pos.apply_fill(Side::Sell, Qty::from_shares(1), p("0.3333"));
        pos.apply_fill(Side::Sell, Qty::from_shares(1), p("0.3333"));
        pos.apply_fill(Side::Sell, Qty::from_shares(1), p("0.3333"));
        assert!(pos.is_flat());
        assert_eq!(pos.cost_basis, Usdc::ZERO);
        assert_eq!(pos.unrealized(p("0.99")), Usdc::ZERO);
        assert_eq!(
            pos.realized,
            Usdc::ZERO,
            "a round trip at one price is flat"
        );
    }

    #[test]
    fn spent_reported_by_a_fill_matches_the_side_convention() {
        let mut pos = Position::new();
        let e = pos.apply_fill(Side::Buy, Qty::from_shares(10), p("0.25"));
        assert_eq!(e.spent, Usdc::from_dollars(2) + Usdc(500_000));
        let e2 = pos.apply_fill(Side::Sell, Qty::from_shares(10), p("0.25"));
        assert_eq!(e2.spent, -(Usdc::from_dollars(2) + Usdc(500_000)));
    }
}
