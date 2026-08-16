//! The three latencies that separate a backtest from a live fill.
//!
//! They are deliberately kept apart, because they act on different things:
//!
//! | Latency | Acts on | Can it be measured from public data? |
//! |---------|---------|--------------------------------------|
//! | market data | what the strategy *sees* | **Yes** — `recv - exchange` per frame |
//! | submission  | when the order *arrives* | No — requires actually trading |
//! | cancellation| when a cancel *lands*    | No — requires actually trading |
//!
//! Only the first is observable without sending orders, and this crate
//! sends none. Measured values from a real 200-second BTC session, recorded
//! from a residential connection in South Asia: median 213 ms, p90 226 ms,
//! p99 715 ms, floor 207 ms. A clock probe over the same session put the
//! local-to-exchange offset at −6 ms ±91 ms, i.e. indistinguishable from
//! zero, so that ~210 ms is genuine transport delay rather than a mis-set
//! clock.
//!
//! Submission and cancellation latency are therefore **parameters, not
//! measurements**, and the report labels them as such. Their defaults are
//! placeholders; the honest way to use this tool is to sweep them.

use serde::{Deserialize, Serialize};

/// Latency assumptions for one simulated run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatencyModel {
    /// How stale the strategy's view of the book is, in milliseconds.
    pub market_data_ms: i64,
    /// Delay between deciding and the order reaching the matching engine.
    pub submit_ms: i64,
    /// Delay between deciding to cancel and the cancel taking effect.
    pub cancel_ms: i64,
}

impl Default for LatencyModel {
    fn default() -> LatencyModel {
        LatencyModel {
            market_data_ms: 0,
            submit_ms: 120,
            cancel_ms: 120,
        }
    }
}

impl LatencyModel {
    /// A model with every latency set to zero: the naive backtest.
    pub const ZERO: LatencyModel = LatencyModel {
        market_data_ms: 0,
        submit_ms: 0,
        cancel_ms: 0,
    };

    /// Replaces the market-data latency with a value measured from a session.
    pub fn with_measured_md(self, measured_ms: i64) -> LatencyModel {
        LatencyModel {
            market_data_ms: measured_ms.max(0),
            ..self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measured_market_data_latency_replaces_the_placeholder() {
        let m = LatencyModel::default().with_measured_md(213);
        assert_eq!(m.market_data_ms, 213);
        assert_eq!(m.submit_ms, 120, "submission latency is independent");
    }

    #[test]
    fn negative_measurements_clamp_to_zero() {
        // A negative median would mean frames arriving before they were
        // stamped, which is a clock artefact rather than negative delay.
        assert_eq!(
            LatencyModel::default().with_measured_md(-40).market_data_ms,
            0
        );
    }
}
