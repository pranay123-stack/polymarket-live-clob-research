//! Event lineage: every result must trace back to the decision that caused it.
//!
//! ```text
//! Decision ──▶ Intent ──▶ Order ──▶ Fill ──▶ Position change ──▶ P&L impact
//! ```

mod common;

use std::collections::{HashMap, HashSet};
use std::process::Command;

use polymarket_live_clob_research::execution::matcher::{ExecConfig, Realism};
use polymarket_live_clob_research::market::event::{EventPayload, MarketEvent};
use polymarket_live_clob_research::market::orderbook::Level;
use polymarket_live_clob_research::market::state::MarketState;
use polymarket_live_clob_research::polymarket::market_discovery::MarketDescriptor;
use polymarket_live_clob_research::replay::engine::{run, RunConfig, RunResult};
use polymarket_live_clob_research::strategy::{
    ExecStyle, ImbalanceStrategy, SignalConfig, StrategyContext, STRATEGY_NAME,
};
use polymarket_live_clob_research::types::{Price, Qty, Side, Usdc};

const BIN: &str = env!("CARGO_BIN_EXE_polymarket-live-clob-research");

fn config(realism: Realism) -> RunConfig {
    RunConfig {
        exec: ExecConfig {
            realism,
            ..Default::default()
        },
        signal: SignalConfig::default(),
        starting_cash: Usdc::from_dollars(10_000),
        md_latency_override_ms: None,
        decision_horizon_ms: 3_000,
    }
}

fn go(cfg: &RunConfig) -> RunResult {
    run(common::reader(), &common::markets(), cfg).expect("replay must succeed")
}

// ---------------------------------------------------------------- decisions

#[test]
fn the_run_produces_decisions_with_unique_identities() {
    let r = go(&config(Realism::REAL));
    let ids: Vec<u64> = r.ledger.decisions().map(|d| d.id).collect();
    assert!(!ids.is_empty(), "the fixture must generate decisions");
    assert_eq!(
        ids.len(),
        ids.iter().collect::<HashSet<_>>().len(),
        "decision ids must be unique"
    );
    for d in r.ledger.decisions() {
        assert_eq!(d.strategy, STRATEGY_NAME);
        assert!(!d.token.is_empty());
        assert!(!d.market_slug.is_empty());
        assert!(d.ts_ms > 0);
        assert!(
            !d.intents.is_empty(),
            "a decision without orders is not a decision"
        );
    }
}

#[test]
fn a_recorded_decision_equals_the_strategy_intent_that_produced_it() {
    // Drive the strategy directly and compare against what a replay records.
    // Nothing downstream may alter, reorder or reinterpret a decision.
    let mut st = MarketState::new();
    st.apply(&MarketEvent {
        seq: 1,
        recv_ms: 0,
        exchange_ms: 0,
        payload: EventPayload::Snapshot {
            asset_id: "UP".into(),
            bids: vec![Level {
                price: Price::parse("0.50").unwrap(),
                qty: Qty::from_shares(900),
            }],
            asks: vec![Level {
                price: Price::parse("0.51").unwrap(),
                qty: Qty::from_shares(100),
            }],
            tick_size: Some(Price::parse("0.01").unwrap()),
            hash: None,
        },
    });
    let markets = [MarketDescriptor {
        slug: "btc-updown-5m-1786844100".into(),
        title: "t".into(),
        question: "q".into(),
        condition_id: "c".into(),
        open_ts: 1_786_844_100,
        close_ts: 1_786_844_400,
        up_token: "UP".into(),
        down_token: "DOWN".into(),
        tick_size: Price::parse("0.01").unwrap(),
        min_size: Qty::from_shares(5),
        accepting_orders: true,
    }];
    let zero = |_: &str| 0i64;
    let mut s = ImbalanceStrategy::new(SignalConfig::default());
    let decisions = s.on_tick(&StrategyContext {
        now_ms: 1_786_844_200_000,
        observed: &st,
        markets: &markets,
        position: &zero,
    });

    assert_eq!(decisions.len(), 1);
    let d = &decisions[0];
    // Side, token, size and timestamp are properties of the decision itself.
    assert_eq!(d.side, Side::Buy);
    assert_eq!(d.token, "UP");
    assert_eq!(d.qty, Qty::from_shares(100));
    assert_eq!(d.ts_ms, 1_786_844_200_000);
    // And every intent restates none of them independently.
    for i in &d.intents {
        assert_eq!(i.decision_id, d.id);
        assert_eq!(i.side, d.side);
        assert_eq!(i.token, d.token);
    }
    assert_eq!(
        Qty(d.intents.iter().map(|i| i.qty.0).sum()),
        d.qty,
        "the decision's size must be exactly what it authorised"
    );
}

// ------------------------------------------------------------------- orders

#[test]
fn every_order_belongs_to_a_decision_that_exists() {
    let r = go(&config(Realism::REAL));
    let known: HashSet<u64> = r.ledger.decisions().map(|d| d.id).collect();
    assert!(!r.ledger.outcomes().is_empty());
    for o in r.ledger.outcomes() {
        assert!(
            known.contains(&o.decision_id),
            "order {} cites decision {} which does not exist",
            o.order_id,
            o.decision_id
        );
        assert!(
            o.arrive_ms >= o.decided_ms,
            "an order cannot arrive before it was decided"
        );
        assert!(
            o.executed.0 <= o.requested.0,
            "executed more than requested"
        );
    }
}

#[test]
fn the_orders_of_a_decision_account_for_all_of_its_size() {
    let r = go(&config(Realism::REAL));
    let mut requested: HashMap<u64, u64> = HashMap::new();
    for o in r.ledger.outcomes() {
        *requested.entry(o.decision_id).or_default() += o.requested.0;
    }
    for d in r.ledger.decisions() {
        if let Some(total) = requested.get(&d.id) {
            assert_eq!(
                *total, d.qty.0,
                "decision {} authorised {} but its orders requested {}",
                d.id, d.qty.0, total
            );
        }
    }
}

// -------------------------------------------------------------------- fills

#[test]
fn every_fill_belongs_to_a_real_order_and_a_real_decision() {
    let r = go(&config(Realism::REAL));
    let decisions: HashSet<u64> = r.ledger.decisions().map(|d| d.id).collect();
    let orders: HashSet<(u64, u64)> = r
        .ledger
        .outcomes()
        .iter()
        .map(|o| (o.decision_id, o.order_id))
        .collect();

    assert!(!r.fills.is_empty(), "the fixture must produce fills");
    let mut fill_ids = HashSet::new();
    for f in &r.fills {
        assert!(
            decisions.contains(&f.decision_id),
            "fill cites unknown decision"
        );
        assert!(
            orders.contains(&(f.decision_id, f.order_id)),
            "fill {} cites order {} that does not belong to decision {}",
            f.fill_id,
            f.order_id,
            f.decision_id
        );
        assert!(fill_ids.insert(f.fill_id), "fill ids must be unique");
    }
}

#[test]
fn a_fills_side_and_token_match_the_decision_that_caused_it() {
    let r = go(&config(Realism::REAL));
    let by_id: HashMap<u64, _> = r.ledger.decisions().map(|d| (d.id, d)).collect();
    for f in &r.fills {
        let d = by_id[&f.decision_id];
        assert_eq!(
            f.side, d.side,
            "fill {} traded against its decision",
            f.fill_id
        );
        assert_eq!(
            f.token, d.token,
            "fill {} traded the wrong token",
            f.fill_id
        );
        assert_eq!(
            f.reference_price, d.reference_price,
            "slippage baseline must come from the decision"
        );
    }
}

#[test]
fn executed_size_per_decision_matches_the_sum_of_its_fills() {
    let r = go(&config(Realism::REAL));
    let mut from_fills: HashMap<u64, u64> = HashMap::new();
    for f in &r.fills {
        *from_fills.entry(f.decision_id).or_default() += f.qty.0;
    }
    for d in r.ledger.decisions() {
        let expected = from_fills.get(&d.id).copied().unwrap_or(0);
        assert_eq!(
            r.ledger.pnl(d.id).executed.0,
            expected,
            "decision {} reports a different executed size than its fills",
            d.id
        );
    }
}

// ---------------------------------------------------------- P&L attribution

#[test]
fn per_decision_pnl_reconciles_with_the_portfolio() {
    for realism in [Realism::IDEAL, Realism::REAL] {
        let r = go(&config(realism));
        let s = r.ledger.summary(r.pnl.total_pnl);
        assert_eq!(
            s.net + s.rounding,
            r.pnl.total_pnl,
            "the ledger must account for the portfolio's whole P&L"
        );
        // The residual comes only from splitting notionals at lot rather than
        // fill boundaries, so it is bounded by the number of fills.
        assert!(
            s.rounding.0.unsigned_abs() <= r.fills.len() as u64 + 1,
            "rounding residual {} is larger than fill-boundary rounding explains",
            s.rounding
        );
    }
}

#[test]
fn each_decisions_net_is_its_realized_plus_unrealized_less_fees() {
    let r = go(&config(Realism::REAL));
    for d in r.ledger.decisions() {
        let p = r.ledger.pnl(d.id);
        assert_eq!(
            p.net,
            p.realized + p.unrealized - p.fees,
            "decision {}",
            d.id
        );
    }
}

#[test]
fn every_shortfall_carries_an_explanation() {
    // A decision that did not get the size it asked for is exactly what the
    // platform exists to explain, so none may be left without a cause.
    use polymarket_live_clob_research::lineage::Cause;
    let r = go(&config(Realism::REAL));
    let short: Vec<_> = r
        .ledger
        .decisions()
        .filter(|d| {
            let p = r.ledger.pnl(d.id);
            p.executed.0 < p.requested.0
        })
        .collect();
    assert!(
        !short.is_empty(),
        "realistic execution must under-fill at least one decision on this fixture"
    );
    for d in short {
        assert_ne!(
            r.ledger.cause(d.id),
            Cause::NoExecutionLoss,
            "decision {} was under-filled yet reports no execution problem",
            d.id
        );
    }
}

#[test]
fn a_passive_only_run_leaves_decisions_with_no_fill_at_all() {
    // With no aggressive leg to guarantee a print, queueing produces genuine
    // total misses — the case where P&L is zero but the cause is not "none".
    use polymarket_live_clob_research::lineage::Cause;
    let mut cfg = config(Realism::REAL);
    cfg.signal.style = ExecStyle::Passive;
    let r = go(&cfg);

    let never: Vec<_> = r
        .ledger
        .decisions()
        .filter(|d| r.ledger.pnl(d.id).executed.is_zero())
        .collect();
    assert!(
        !never.is_empty(),
        "a passive-only run must miss entirely at least once"
    );
    for d in never {
        let p = r.ledger.pnl(d.id);
        assert_eq!(p.realized, Usdc::ZERO);
        assert_eq!(p.unrealized, Usdc::ZERO);
        assert_eq!(p.fees, Usdc::ZERO);
        assert_eq!(p.net, Usdc::ZERO);
        assert_ne!(
            r.ledger.cause(d.id),
            Cause::NoExecutionLoss,
            "decision {} filled nothing yet reports no execution problem",
            d.id
        );
    }
}

#[test]
fn the_ideal_run_leaves_no_decision_unexecuted() {
    let r = go(&config(Realism::IDEAL));
    for d in r.ledger.decisions() {
        let p = r.ledger.pnl(d.id);
        assert_eq!(
            p.executed, p.requested,
            "decision {} did not fully fill under ideal assumptions",
            d.id
        );
    }
}

// ------------------------------------------------------------- CSV auditing

fn cli(args: &[&str]) -> (String, bool) {
    let out = Command::new(BIN).args(args).output().expect("binary runs");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.success(),
    )
}

fn read_csv(path: &std::path::Path) -> Vec<HashMap<String, String>> {
    let body = std::fs::read_to_string(path).expect("csv readable");
    let mut lines = body.lines();
    let header: Vec<String> = lines
        .next()
        .unwrap()
        .split(',')
        .map(str::to_owned)
        .collect();
    lines
        .map(|l| {
            header
                .iter()
                .cloned()
                .zip(l.split(',').map(str::to_owned))
                .collect()
        })
        .collect()
}

#[test]
fn the_audit_csvs_join_on_decision_id_with_no_orphans() {
    let dir = std::env::temp_dir().join("pmclob_audit_join");
    std::fs::create_dir_all(&dir).unwrap();
    let session = common::fixture_path();
    let (_, ok) = cli(&[
        "analyze",
        "--file",
        session.to_str().unwrap(),
        "--csv-dir",
        dir.to_str().unwrap(),
    ]);
    assert!(ok, "analyze --csv-dir must succeed");

    let decisions = read_csv(&dir.join("decisions.csv"));
    let orders = read_csv(&dir.join("orders.csv"));
    let fills = read_csv(&dir.join("fills.csv"));
    let pnl = read_csv(&dir.join("pnl.csv"));

    assert!(!decisions.is_empty() && !orders.is_empty() && !fills.is_empty());
    let ids: HashSet<&str> = decisions
        .iter()
        .map(|r| r["decision_id"].as_str())
        .collect();

    for o in &orders {
        assert!(ids.contains(o["decision_id"].as_str()), "orphan order row");
    }
    let order_keys: HashSet<(&str, &str)> = orders
        .iter()
        .map(|o| (o["decision_id"].as_str(), o["order_id"].as_str()))
        .collect();
    for f in &fills {
        assert!(ids.contains(f["decision_id"].as_str()), "orphan fill row");
        assert!(
            order_keys.contains(&(f["decision_id"].as_str(), f["order_id"].as_str())),
            "fill cites an order absent from orders.csv"
        );
    }
    // Every decision must appear in pnl.csv exactly once.
    let pnl_ids: Vec<&str> = pnl
        .iter()
        .map(|r| r["decision_id"].as_str())
        .filter(|id| *id != "TOTAL" && *id != "ROUNDING")
        .collect();
    assert_eq!(pnl_ids.len(), decisions.len());
    assert_eq!(pnl_ids.iter().collect::<HashSet<_>>().len(), pnl_ids.len());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn csv_figures_recompute_from_their_own_rows() {
    let dir = std::env::temp_dir().join("pmclob_audit_recompute");
    std::fs::create_dir_all(&dir).unwrap();
    let session = common::fixture_path();
    let (_, ok) = cli(&[
        "analyze",
        "--file",
        session.to_str().unwrap(),
        "--csv-dir",
        dir.to_str().unwrap(),
    ]);
    assert!(ok);

    let fills = read_csv(&dir.join("fills.csv"));
    let pnl = read_csv(&dir.join("pnl.csv"));

    // 1. Slippage recomputes from expected vs actual price.
    let mut fee_by_decision: HashMap<String, f64> = HashMap::new();
    let mut slip_by_decision: HashMap<String, f64> = HashMap::new();
    for f in &fills {
        let sign = if f["side"] == "BUY" { 1.0 } else { -1.0 };
        let actual: f64 = f["actual_price"].parse().unwrap();
        let expected: f64 = f["expected_price"].parse().unwrap();
        let qty: f64 = f["quantity"].parse().unwrap();
        let reported: f64 = f["slippage"].parse().unwrap();
        assert!(
            ((actual - expected) * qty * sign - reported).abs() < 0.01,
            "slippage does not recompute from its own row"
        );
        *fee_by_decision.entry(f["decision_id"].clone()).or_default() +=
            f["fee"].parse::<f64>().unwrap();
        *slip_by_decision
            .entry(f["decision_id"].clone())
            .or_default() += reported;
    }

    // 2. Per-decision fees and slippage match the fills that produced them.
    for row in &pnl {
        let id = &row["decision_id"];
        if id == "TOTAL" || id == "ROUNDING" {
            continue;
        }
        let fees: f64 = row["fees"].parse().unwrap();
        let slip: f64 = row["slippage"].parse().unwrap();
        assert!(
            (fees - fee_by_decision.get(id).copied().unwrap_or(0.0)).abs() < 0.01,
            "decision {id}: fees disagree with fills.csv"
        );
        assert!(
            (slip - slip_by_decision.get(id).copied().unwrap_or(0.0)).abs() < 0.01,
            "decision {id}: slippage disagrees with fills.csv"
        );
        // 3. net == realized + unrealized - fees, per row.
        let (r, u, n): (f64, f64, f64) = (
            row["realized_pnl"].parse().unwrap(),
            row["unrealized_pnl"].parse().unwrap(),
            row["net_pnl"].parse().unwrap(),
        );
        assert!(
            (r + u - fees - n).abs() < 1e-6,
            "decision {id}: net does not balance"
        );
    }

    // 4. The per-decision rows plus the rounding residue equal the total.
    let sum: f64 = pnl
        .iter()
        .filter(|r| r["decision_id"] != "TOTAL")
        .map(|r| r["net_pnl"].parse::<f64>().unwrap_or(0.0))
        .sum();
    let total: f64 = pnl
        .iter()
        .find(|r| r["decision_id"] == "TOTAL")
        .map(|r| r["net_pnl"].parse().unwrap())
        .expect("a TOTAL row");
    assert!(
        (sum - total).abs() < 1e-6,
        "per-decision rows sum to {sum} but TOTAL says {total}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ------------------------------------------------------- end-to-end and CLI

#[test]
fn verify_replay_reports_an_identical_result() {
    let session = common::fixture_path();
    let (out, ok) = cli(&["verify-replay", "--file", session.to_str().unwrap()]);
    assert!(ok, "verify-replay must succeed:\n{out}");
    assert!(out.contains("IDENTICAL RESULT"));
    for stage in [
        "Order book:",
        "Strategy decisions:",
        "Orders:",
        "Fills:",
        "P&L:",
        "Attribution:",
    ] {
        assert!(out.contains(stage), "missing stage `{stage}`");
    }
    assert_eq!(out.matches("FAIL").count(), 0, "a stage failed:\n{out}");
}

#[test]
fn the_book_checksum_is_stable_across_runs_and_sensitive_to_content() {
    let a = go(&config(Realism::REAL));
    let b = go(&config(Realism::REAL));
    assert_eq!(a.book_checksum, b.book_checksum, "checksum must be stable");
    assert_ne!(a.book_checksum, 0);

    // A book that differs by one level must not collide.
    let mut s1 = MarketState::new();
    let mut s2 = MarketState::new();
    let snap = |qty: u64| EventPayload::Snapshot {
        asset_id: "UP".into(),
        bids: vec![Level {
            price: Price::parse("0.50").unwrap(),
            qty: Qty::from_shares(qty),
        }],
        asks: vec![],
        tick_size: None,
        hash: None,
    };
    s1.apply(&MarketEvent {
        seq: 1,
        recv_ms: 0,
        exchange_ms: 0,
        payload: snap(100),
    });
    s2.apply(&MarketEvent {
        seq: 1,
        recv_ms: 0,
        exchange_ms: 0,
        payload: snap(101),
    });
    assert_ne!(s1.checksum(), s2.checksum());
}

#[test]
fn the_whole_pipeline_runs_from_fixture_to_report() {
    // real_market_fixture.jsonl -> record format -> replay -> execution -> report
    let session = common::fixture_path();
    let path = session.to_str().unwrap();

    let (inspect, ok) = cli(&["inspect", "--file", path]);
    assert!(ok && inspect.contains("POLYMARKET SESSION INSPECT"));

    let (replay, ok) = cli(&["replay", "--file", path]);
    assert!(ok && replay.contains("accounting identity   exact"));

    let (verify, ok) = cli(&["verify-replay", "--file", path]);
    assert!(ok && verify.contains("IDENTICAL RESULT"));

    let (analyze, ok) = cli(&["analyze", "--file", path]);
    assert!(ok, "analyze failed");
    for section in [
        "POLYMARKET EXECUTION ANALYSIS",
        "Ideal execution",
        "Realistic execution",
        "EDGE LOSS:",
        "DECISION vs EXECUTION",
        "DECISION ERROR",
        "EXECUTION ERROR",
    ] {
        assert!(analyze.contains(section), "report missing `{section}`");
    }
}

#[test]
fn decision_style_changes_the_orders_but_never_breaks_lineage() {
    for style in [ExecStyle::Passive, ExecStyle::Aggressive, ExecStyle::Split] {
        let mut cfg = config(Realism::REAL);
        cfg.signal.style = style;
        let r = go(&cfg);
        let known: HashSet<u64> = r.ledger.decisions().map(|d| d.id).collect();
        for f in &r.fills {
            assert!(
                known.contains(&f.decision_id),
                "{style:?}: fill lost its decision"
            );
        }
        let expected_orders = match style {
            ExecStyle::Split => 2,
            _ => 1,
        };
        for d in r.ledger.decisions() {
            assert_eq!(
                d.intents.len(),
                expected_orders,
                "{style:?}: unexpected order count on decision {}",
                d.id
            );
        }
    }
}
