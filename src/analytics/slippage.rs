//! Slippage accounting: what each fill cost against the price that was seen.
//!
//! Slippage is measured against the strategy's **reference price** — the
//! touch it was looking at when it decided — not against the mid, and not
//! against the price it eventually got. That baseline is deliberate: it is
//! exactly the price a naive backtest would have assumed it received, so the
//! number reported here is the error such a backtest makes.

use serde::{Deserialize, Serialize};

use crate::analytics::metrics::RunningStats;
use crate::execution::fills::{Fill, Liquidity};
use crate::types::Usdc;

/// Aggregate slippage and fee costs across a run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlippageReport {
    /// Fills recorded.
    pub fills: u64,
    /// Fills that provided liquidity.
    pub maker_fills: u64,
    /// Fills that took liquidity.
    pub taker_fills: u64,
    /// Total slippage cost; positive is money lost.
    pub total_slippage: Usdc,
    /// Slippage on maker fills.
    pub maker_slippage: Usdc,
    /// Slippage on taker fills.
    pub taker_slippage: Usdc,
    /// Fees paid across all fills.
    pub total_fees: Usdc,
    /// Total notional executed.
    pub notional: Usdc,
    /// Distribution of adverse price movement, in price ticks.
    pub adverse_ticks: RunningStats,
}

impl SlippageReport {
    /// Creates an empty report.
    pub fn new() -> SlippageReport {
        SlippageReport {
            adverse_ticks: RunningStats::new(),
            ..Default::default()
        }
    }

    /// Adds one fill.
    pub fn record(&mut self, f: &Fill) {
        let cost = f.slippage_cost();
        self.fills += 1;
        self.total_slippage += cost;
        self.total_fees += f.fee;
        self.notional += f.notional();
        self.adverse_ticks.push(f.adverse_ticks() as f64);
        match f.liquidity {
            Liquidity::Maker => {
                self.maker_fills += 1;
                self.maker_slippage += cost;
            }
            Liquidity::Taker => {
                self.taker_fills += 1;
                self.taker_slippage += cost;
            }
        }
    }

    /// Slippage as a fraction of notional traded, in basis points.
    ///
    /// `None` when nothing traded, which is distinct from zero slippage.
    pub fn slippage_bps(&self) -> Option<f64> {
        if self.notional.0 == 0 {
            return None;
        }
        Some(self.total_slippage.0 as f64 / self.notional.0 as f64 * 10_000.0)
    }

    /// Combined slippage and fee cost.
    pub fn total_cost(&self) -> Usdc {
        self.total_slippage + self.total_fees
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Price, Qty, Side};

    fn fill(liquidity: Liquidity, price: &str, reference: &str, fee: i64) -> Fill {
        Fill {
            decision_id: 1,
            order_id: 1,
            fill_id: 1,
            token: "UP".into(),
            side: Side::Buy,
            price: Price::parse(price).unwrap(),
            qty: Qty::from_shares(100),
            ts_ms: 0,
            liquidity,
            fee: Usdc(fee),
            reference_price: Price::parse(reference).unwrap(),
        }
    }

    #[test]
    fn splits_cost_between_maker_and_taker_fills() {
        let mut r = SlippageReport::new();
        r.record(&fill(Liquidity::Taker, "0.52", "0.50", 1_000));
        r.record(&fill(Liquidity::Maker, "0.50", "0.50", 0));
        assert_eq!(r.fills, 2);
        assert_eq!(r.taker_fills, 1);
        assert_eq!(r.maker_fills, 1);
        assert_eq!(r.taker_slippage, Usdc::from_dollars(2));
        assert_eq!(r.maker_slippage, Usdc::ZERO);
        assert_eq!(r.total_slippage, Usdc::from_dollars(2));
        assert_eq!(r.total_fees, Usdc(1_000));
        assert_eq!(r.total_cost(), Usdc::from_dollars(2) + Usdc(1_000));
    }

    #[test]
    fn slippage_in_basis_points_is_relative_to_notional() {
        let mut r = SlippageReport::new();
        // 100 shares at 0.52 = $52 notional, $2 slippage -> ~384.6 bps.
        r.record(&fill(Liquidity::Taker, "0.52", "0.50", 0));
        let bps = r.slippage_bps().unwrap();
        assert!((bps - 384.615).abs() < 0.01, "got {bps}");
    }

    #[test]
    fn an_empty_report_distinguishes_no_trades_from_zero_slippage() {
        assert_eq!(SlippageReport::new().slippage_bps(), None);
    }

    #[test]
    fn price_improvement_reduces_the_reported_cost() {
        let mut r = SlippageReport::new();
        r.record(&fill(Liquidity::Taker, "0.52", "0.50", 0));
        r.record(&fill(Liquidity::Maker, "0.48", "0.50", 0));
        assert_eq!(r.total_slippage, Usdc::ZERO, "the two offset exactly");
        assert_eq!(r.adverse_ticks.mean(), Some(0.0));
    }
}
