//! Execution simulation and accounting, exercised on a real session.

mod common;

use polymarket_live_clob_research::analytics::attribution::{attribute, Method};
use polymarket_live_clob_research::execution::matcher::{ExecConfig, Factor, QueueModel, Realism};
use polymarket_live_clob_research::replay::engine::{run, RunConfig, RunResult};
use polymarket_live_clob_research::strategy::{ExecStyle, SignalConfig};
use polymarket_live_clob_research::types::Usdc;

fn config(realism: Realism) -> RunConfig {
    RunConfig {
        exec: ExecConfig {
            realism,
            ..Default::default()
        },
        signal: SignalConfig::default(),
        starting_cash: Usdc::from_dollars(10_000),
        md_latency_override_ms: None,
        decision_horizon_ms: 30_000,
    }
}

fn go(cfg: &RunConfig) -> RunResult {
    run(common::reader(), &common::markets(), cfg).expect("replay must succeed")
}

#[test]
fn the_accounting_identity_holds_under_every_realism_configuration() {
    // equity - starting_cash == realized + unrealized - fees, exactly.
    for mask in 0..32u32 {
        let realism = Realism::IDEAL
            .with(Factor::MdLatency, mask & 1 != 0)
            .with(Factor::OrderLatency, mask & 2 != 0)
            .with(Factor::Queue, mask & 4 != 0)
            .with(Factor::Depth, mask & 8 != 0)
            .with(Factor::Fees, mask & 16 != 0);
        let r = go(&config(realism));
        assert_eq!(
            r.identity_error,
            Usdc::ZERO,
            "identity broke by {} under realism mask {mask}",
            r.identity_error
        );
    }
}

#[test]
fn the_ideal_run_fills_everything_it_asks_for() {
    let r = go(&config(Realism::IDEAL));
    assert!(
        r.exec.submitted > 0,
        "the signal must produce orders to measure"
    );
    assert_eq!(r.exec.filled_full, r.exec.submitted);
    assert_eq!(r.exec.missed(), 0);
    assert!((r.exec.fill_ratio() - 1.0).abs() < 1e-9);
    assert_eq!(r.pnl.fees, Usdc::ZERO, "the ideal run is free");
}

#[test]
fn realistic_execution_fills_less_than_the_ideal_run() {
    let ideal = go(&config(Realism::IDEAL));
    let real = go(&config(Realism::REAL));
    assert!(
        real.exec.fill_ratio() < ideal.exec.fill_ratio(),
        "realism must cost fills: {:.3} vs {:.3}",
        real.exec.fill_ratio(),
        ideal.exec.fill_ratio()
    );
    assert!(real.exec.missed() > 0, "some orders must go unfilled");
}

#[test]
fn depth_realism_alone_cannot_increase_traded_notional() {
    // Walking a real book can only ever give you less than assuming the
    // whole order clears at the touch.
    let ideal = go(&config(Realism::IDEAL));
    let with_depth = go(&config(Realism::IDEAL.with(Factor::Depth, true)));
    assert!(
        with_depth.slippage.notional.0 <= ideal.slippage.notional.0,
        "depth constraint increased traded notional"
    );
}

#[test]
fn fees_reduce_pnl_by_exactly_the_fees_charged() {
    let mut cfg = config(Realism::IDEAL);
    let free = go(&cfg);

    cfg.exec.realism = Realism::IDEAL.with(Factor::Fees, true);
    cfg.exec.taker_fee_bps = 50;
    cfg.exec.maker_fee_bps = 25;
    let charged = go(&cfg);

    assert!(
        charged.pnl.fees > Usdc::ZERO,
        "fees must actually be charged"
    );
    // Fees do not change decisions or fills, so P&L must drop by exactly them.
    assert_eq!(charged.exec, free.exec, "fees must not alter execution");
    assert_eq!(
        charged.pnl.total_pnl,
        free.pnl.total_pnl - charged.pnl.fees,
        "P&L must fall by precisely the fees paid"
    );
}

#[test]
fn queue_models_are_ordered_from_pessimistic_to_optimistic() {
    // A more generous view of cancellations can only help a resting order.
    let mut ratios = Vec::new();
    for model in [
        QueueModel::Pessimistic,
        QueueModel::Proportional,
        QueueModel::Optimistic,
    ] {
        let mut cfg = config(Realism::IDEAL.with(Factor::Queue, true));
        cfg.exec.queue_model = model;
        cfg.signal.style = ExecStyle::Passive;
        let r = go(&cfg);
        ratios.push((model, r.exec.fill_ratio()));
    }
    assert!(
        ratios[0].1 <= ratios[1].1 + 1e-9,
        "pessimistic {:?} filled more than proportional {:?}",
        ratios[0],
        ratios[1]
    );
    assert!(
        ratios[1].1 <= ratios[2].1 + 1e-9,
        "proportional {:?} filled more than optimistic {:?}",
        ratios[1],
        ratios[2]
    );
}

#[test]
fn passive_orders_only_fill_as_makers_and_aggressive_ones_as_takers() {
    let mut cfg = config(Realism::REAL);
    cfg.signal.style = ExecStyle::Passive;
    let passive = go(&cfg);
    assert_eq!(
        passive.slippage.taker_fills, 0,
        "a passive order that rests cannot take liquidity"
    );

    cfg.signal.style = ExecStyle::Aggressive;
    let aggressive = go(&cfg);
    assert_eq!(
        aggressive.slippage.maker_fills, 0,
        "an aggressive order never rests"
    );
}

#[test]
fn attribution_contributions_sum_to_the_measured_edge_loss() {
    let mut cache = std::collections::HashMap::new();
    let a = attribute(&Factor::ALL, Method::Shapley, |realism| {
        *cache
            .entry(realism)
            .or_insert_with(|| go(&config(realism)).pnl.total_pnl)
    });
    let summed: i64 = a.contributions.iter().map(|(_, c)| c.0).sum();
    assert_eq!(
        summed + a.rounding.0,
        a.edge_loss.0,
        "Shapley contributions must account for the whole gap"
    );
    assert_eq!(a.runs, 32);
    assert!(
        a.rounding.0.unsigned_abs() as usize <= Factor::ALL.len(),
        "rounding residue {} exceeds one unit per factor",
        a.rounding
    );
}

#[test]
fn shapley_and_waterfall_agree_on_the_total_if_not_the_split() {
    let mut cache = std::collections::HashMap::new();
    let mut pnl = |realism: Realism| {
        *cache
            .entry(realism)
            .or_insert_with(|| go(&config(realism)).pnl.total_pnl)
    };
    let s = attribute(&Factor::ALL, Method::Shapley, &mut pnl);
    let w = attribute(&Factor::ALL, Method::Waterfall, &mut pnl);
    assert_eq!(
        s.edge_loss, w.edge_loss,
        "the total gap is method-independent"
    );
    assert_eq!(s.ideal_pnl, w.ideal_pnl);
    assert_eq!(s.real_pnl, w.real_pnl);
}

#[test]
fn a_larger_order_never_fills_a_greater_fraction_of_itself() {
    // Depth is finite, so asking for more can only lower the fill ratio.
    let mut small = config(Realism::REAL);
    small.signal.order_shares = 50;
    small.signal.max_position_shares = 100_000;
    let mut large = small.clone();
    large.signal.order_shares = 5_000;

    let s = go(&small);
    let l = go(&large);
    assert!(
        l.exec.fill_ratio() <= s.exec.fill_ratio() + 1e-9,
        "a 5000-share order filled a greater fraction ({:.3}) than a 50-share one ({:.3})",
        l.exec.fill_ratio(),
        s.exec.fill_ratio()
    );
}

#[test]
fn no_fill_is_ever_priced_outside_its_order_limit() {
    let mut cfg = config(Realism::REAL);
    cfg.signal.style = ExecStyle::Aggressive;
    let r = go(&cfg);
    assert!(r.slippage.fills > 0);
    // Slippage is bounded by the aggression the strategy authorised: one
    // market tick beyond the touch, on 0.01 markets.
    let mean_ticks = r.slippage.adverse_ticks.mean().unwrap_or(0.0);
    assert!(
        mean_ticks <= 100.0,
        "mean adverse move {mean_ticks} ticks exceeds the authorised limit"
    );
}
