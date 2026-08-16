//! Attributing edge loss to the execution factors that caused it.
//!
//! # The problem with a waterfall
//!
//! The obvious method is to switch factors on one at a time and record the
//! P&L drop at each step. It costs `k + 1` runs and is easy to explain, but
//! the answer depends on the order chosen: latency looks expensive when
//! enabled first and cheap when enabled last, because whichever factor goes
//! first absorbs all of the interaction between them. On a book where
//! latency and queue position interact strongly — which is exactly the case
//! here — the difference is not cosmetic.
//!
//! # Shapley values
//!
//! The Shapley value is the unique attribution that is order-independent,
//! gives identical factors identical credit, assigns nothing to a factor
//! that never changes the outcome, and sums exactly to the total. For factor
//! `i`:
//!
//! ```text
//! φ_i = Σ_{S ⊆ N∖{i}}  |S|! (n−|S|−1)! / n!  ·  [ v(S ∪ {i}) − v(S) ]
//! ```
//!
//! with `v(S)` the edge lost when exactly the factors in `S` are active, so
//! `v(∅) = 0` and `v(N)` is the full gap. It costs `2^k` runs — 32 for the
//! five factors here — which is affordable because a run is a single
//! streaming pass over the session.
//!
//! Arithmetic is exact: contributions accumulate as `i128` numerators over a
//! common denominator of `n!`, and whatever the final division truncates is
//! reported as [`Attribution::rounding`] rather than being quietly dropped,
//! so the parts always sum to the whole.

use crate::execution::matcher::{Factor, Realism};
use crate::types::Usdc;

/// How contributions were computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// Order-independent Shapley values over all `2^k` factor subsets.
    Shapley,
    /// Sequential waterfall in [`Factor::ALL`] order, costing `k + 1` runs.
    Waterfall,
}

impl Method {
    /// Number of runs the method requires for `k` factors.
    pub fn runs(self, k: u32) -> usize {
        match self {
            Method::Shapley => 1usize << k,
            Method::Waterfall => k as usize + 1,
        }
    }
}

/// A completed edge-loss attribution.
#[derive(Debug, Clone)]
pub struct Attribution {
    /// P&L with every realism factor off.
    pub ideal_pnl: Usdc,
    /// P&L with every realism factor on.
    pub real_pnl: Usdc,
    /// `ideal_pnl - real_pnl`; positive means realism destroyed edge.
    pub edge_loss: Usdc,
    /// Per-factor contribution, in report order. Sums to `edge_loss - rounding`.
    pub contributions: Vec<(Factor, Usdc)>,
    /// Residue left by integer division; always under one unit per factor.
    pub rounding: Usdc,
    /// Method used.
    pub method: Method,
    /// Number of simulation runs performed.
    pub runs: usize,
}

impl Attribution {
    /// Contributions ordered largest cost first.
    pub fn ranked(&self) -> Vec<(Factor, Usdc)> {
        let mut v = self.contributions.clone();
        v.sort_by_key(|(_, c)| std::cmp::Reverse(c.0));
        v
    }

    /// Whether percentage shares are worth reporting.
    ///
    /// Factors can offset one another — stale data occasionally *helps*,
    /// because a delayed signal is sometimes the luckier one. When they do,
    /// the total can be far smaller than the individual contributions, and a
    /// percentage of that total is noise multiplied rather than a share of
    /// anything. Callers should print the absolute figures and omit the
    /// percentages when this returns `false`.
    pub fn shares_are_meaningful(&self) -> bool {
        let largest = self
            .contributions
            .iter()
            .map(|(_, c)| c.0.abs())
            .max()
            .unwrap_or(0);
        // Equality is the cleanest case, not an ambiguous one: it means a
        // single factor accounts for the entire gap.
        self.edge_loss.0.abs() >= largest
    }

    /// A factor's share of the total edge loss, in `[0, 1]`.
    ///
    /// `None` when there is no edge loss to apportion. Check
    /// [`Self::shares_are_meaningful`] before presenting this as a
    /// percentage.
    pub fn share(&self, f: Factor) -> Option<f64> {
        if self.edge_loss.0 == 0 {
            return None;
        }
        self.contributions
            .iter()
            .find(|(g, _)| *g == f)
            .map(|(_, c)| c.0 as f64 / self.edge_loss.0 as f64)
    }
}

/// Computes an attribution by evaluating `pnl_of` on factor subsets.
///
/// `pnl_of` must be deterministic: the same [`Realism`] has to yield the same
/// P&L every time, or the Shapley terms will not telescope and the
/// contributions will not sum to the total.
pub fn attribute(
    factors: &[Factor],
    method: Method,
    mut pnl_of: impl FnMut(Realism) -> Usdc,
) -> Attribution {
    let n = factors.len();
    let ideal_pnl = pnl_of(Realism::IDEAL);
    let real_pnl = pnl_of(Realism::from_set(factors));
    let edge_loss = ideal_pnl - real_pnl;

    // v(S): edge lost when exactly the factors in S are active.
    let loss_of = |pnl: Usdc| Usdc(ideal_pnl.0 - pnl.0);

    let (contributions, runs) = match method {
        Method::Waterfall => {
            let mut out = Vec::with_capacity(n);
            let mut prev = ideal_pnl;
            let mut enabled: Vec<Factor> = Vec::new();
            for &f in factors {
                enabled.push(f);
                let pnl = pnl_of(Realism::from_set(&enabled));
                out.push((f, Usdc(prev.0 - pnl.0)));
                prev = pnl;
            }
            (out, n + 1)
        }

        Method::Shapley => {
            // Evaluate v(S) once per subset, indexed by bitmask.
            let total = 1usize << n;
            let mut v = vec![0i64; total];
            for (mask, slot) in v.iter_mut().enumerate() {
                let subset: Vec<Factor> = factors
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| mask & (1 << i) != 0)
                    .map(|(_, &f)| f)
                    .collect();
                *slot = loss_of(pnl_of(Realism::from_set(&subset))).0;
            }

            let n_fact = factorial(n as u64);
            let mut out = Vec::with_capacity(n);
            for (i, &f) in factors.iter().enumerate() {
                let bit = 1usize << i;
                let mut numerator: i128 = 0;
                for (mask, &without) in v.iter().enumerate() {
                    if mask & bit != 0 {
                        continue;
                    }
                    let s = (mask.count_ones()) as u64;
                    let weight = factorial(s) as i128 * factorial(n as u64 - s - 1) as i128;
                    let with = v[mask | bit];
                    numerator += weight * (with - without) as i128;
                }
                out.push((f, Usdc((numerator / n_fact as i128) as i64)));
            }
            (out, total)
        }
    };

    let attributed: i64 = contributions.iter().map(|(_, c)| c.0).sum();
    Attribution {
        ideal_pnl,
        real_pnl,
        edge_loss,
        contributions,
        rounding: Usdc(edge_loss.0 - attributed),
        method,
        runs,
    }
}

/// `n!` for the small `n` this module needs.
fn factorial(n: u64) -> u64 {
    (1..=n.max(1)).product()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(v: i64) -> Usdc {
        Usdc(v)
    }

    #[test]
    fn contributions_sum_to_the_total_edge_loss() {
        // Each active factor costs a fixed amount, plus an interaction term
        // whenever latency and queue are both on.
        let pnl = |r: Realism| {
            let mut p = 1_000_000i64;
            if r.md_latency {
                p -= 100_000
            }
            if r.order_latency {
                p -= 200_000
            }
            if r.queue {
                p -= 300_000
            }
            if r.depth {
                p -= 50_000
            }
            if r.fees {
                p -= 25_000
            }
            if r.md_latency && r.queue {
                p -= 400_000
            }
            f(p)
        };
        let a = attribute(&Factor::ALL, Method::Shapley, pnl);
        assert_eq!(a.edge_loss, f(1_075_000));
        let summed: i64 = a.contributions.iter().map(|(_, c)| c.0).sum();
        assert_eq!(summed + a.rounding.0, a.edge_loss.0);
        assert_eq!(a.runs, 32);
    }

    #[test]
    fn shapley_splits_interaction_evenly_where_a_waterfall_does_not() {
        // Two factors, each harmless alone, jointly costing 100.
        let pnl = |r: Realism| {
            f(if r.md_latency && r.queue {
                900_000
            } else {
                1_000_000
            })
        };
        let factors = [Factor::MdLatency, Factor::Queue];

        let s = attribute(&factors, Method::Shapley, pnl);
        assert_eq!(s.contributions[0].1, f(50_000));
        assert_eq!(
            s.contributions[1].1,
            f(50_000),
            "symmetric factors share equally"
        );

        let w = attribute(&factors, Method::Waterfall, pnl);
        assert_eq!(
            w.contributions[0].1,
            f(0),
            "first factor alone costs nothing"
        );
        assert_eq!(
            w.contributions[1].1,
            f(100_000),
            "the second absorbs it all"
        );
        assert_eq!(w.runs, 3);
    }

    #[test]
    fn a_factor_with_no_effect_receives_nothing() {
        let pnl = |r: Realism| f(if r.fees { 900_000 } else { 1_000_000 });
        let a = attribute(&Factor::ALL, Method::Shapley, pnl);
        assert_eq!(a.share(Factor::Fees), Some(1.0));
        for other in [
            Factor::MdLatency,
            Factor::OrderLatency,
            Factor::Queue,
            Factor::Depth,
        ] {
            assert_eq!(a.share(other), Some(0.0), "{other:?} changed nothing");
        }
    }

    #[test]
    fn realism_can_improve_pnl_and_the_loss_goes_negative() {
        // Nothing guarantees realism costs money; a passive fill that a naive
        // model missed can help. The report must not assume a sign.
        let pnl = |r: Realism| f(if r.queue { 1_200_000 } else { 1_000_000 });
        let a = attribute(&Factor::ALL, Method::Shapley, pnl);
        assert_eq!(a.edge_loss, f(-200_000));
        assert_eq!(a.share(Factor::Queue), Some(1.0));
    }

    #[test]
    fn ranked_puts_the_most_expensive_factor_first() {
        let pnl = |r: Realism| {
            let mut p = 0i64;
            if r.queue {
                p -= 500_000
            }
            if r.fees {
                p -= 100_000
            }
            f(p)
        };
        let a = attribute(&Factor::ALL, Method::Shapley, pnl);
        assert_eq!(a.ranked()[0].0, Factor::Queue);
    }

    #[test]
    fn offsetting_factors_suppress_meaningless_percentages() {
        // One factor costs 65, another gains 67: the total is -2, and a
        // percentage of it would read as thousands of percent.
        let pnl = |r: Realism| {
            let mut p = 0i64;
            if r.queue {
                p -= 65_000_000
            }
            if r.md_latency {
                p += 67_000_000
            }
            f(p)
        };
        let a = attribute(&Factor::ALL, Method::Shapley, pnl);
        assert_eq!(a.edge_loss, f(-2_000_000));
        assert!(
            !a.shares_are_meaningful(),
            "a total smaller than its parts cannot be apportioned as percentages"
        );

        // A clean case, where one factor dominates, stays reportable.
        let clean = attribute(&Factor::ALL, Method::Shapley, |r: Realism| {
            f(if r.queue { -65_000_000 } else { 0 })
        });
        assert!(clean.shares_are_meaningful());
    }

    #[test]
    fn zero_edge_loss_reports_no_shares_rather_than_dividing_by_zero() {
        let a = attribute(&Factor::ALL, Method::Shapley, |_| f(1_000));
        assert_eq!(a.edge_loss, Usdc::ZERO);
        assert_eq!(a.share(Factor::Queue), None);
    }
}
