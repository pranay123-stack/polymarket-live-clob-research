//! Replay determinism and fidelity, on a real recorded session.

mod common;

use polymarket_live_clob_research::execution::matcher::Realism;
use polymarket_live_clob_research::market::state::MarketState;
use polymarket_live_clob_research::replay::clock::VirtualClock;
use polymarket_live_clob_research::replay::engine::{run, RunConfig};
use polymarket_live_clob_research::types::{Side, Usdc};

fn config(realism: Realism) -> RunConfig {
    RunConfig {
        exec: polymarket_live_clob_research::execution::matcher::ExecConfig {
            realism,
            ..Default::default()
        },
        signal: Default::default(),
        starting_cash: Usdc::from_dollars(10_000),
        md_latency_override_ms: None,
        decision_horizon_ms: 30_000,
    }
}

#[test]
fn reading_the_same_session_twice_yields_identical_events() {
    let a = common::events();
    let b = common::events();
    assert_eq!(a.len(), b.len());
    assert_eq!(
        a, b,
        "normalization must not depend on anything but the file"
    );
}

#[test]
fn sequence_numbers_are_dense_and_start_at_one() {
    let evs = common::events();
    assert!(!evs.is_empty());
    for (i, e) in evs.iter().enumerate() {
        assert_eq!(e.seq, i as u64 + 1, "sequence numbers must be gapless");
    }
}

#[test]
fn replaying_twice_produces_identical_results() {
    let cfg = config(Realism::REAL);
    let markets = common::markets();
    let a = run(common::reader(), &markets, &cfg).expect("first replay");
    let b = run(common::reader(), &markets, &cfg).expect("second replay");

    assert_eq!(a.events, b.events);
    assert_eq!(a.decisions, b.decisions);
    assert_eq!(a.pnl, b.pnl);
    assert_eq!(a.exec, b.exec);
    assert_eq!(a.slippage.total_slippage, b.slippage.total_slippage);
    assert_eq!(a.slippage.fills, b.slippage.fills);
    assert_eq!(a.state, b.state);
}

#[test]
fn every_realism_configuration_is_individually_deterministic() {
    let markets = common::markets();
    for mask in 0..32u32 {
        let realism = Realism::IDEAL
            .with(
                polymarket_live_clob_research::execution::matcher::Factor::MdLatency,
                mask & 1 != 0,
            )
            .with(
                polymarket_live_clob_research::execution::matcher::Factor::OrderLatency,
                mask & 2 != 0,
            )
            .with(
                polymarket_live_clob_research::execution::matcher::Factor::Queue,
                mask & 4 != 0,
            )
            .with(
                polymarket_live_clob_research::execution::matcher::Factor::Depth,
                mask & 8 != 0,
            )
            .with(
                polymarket_live_clob_research::execution::matcher::Factor::Fees,
                mask & 16 != 0,
            );
        let cfg = config(realism);
        let a = run(common::reader(), &markets, &cfg).expect("replay a");
        let b = run(common::reader(), &markets, &cfg).expect("replay b");
        assert_eq!(
            a.pnl.total_pnl, b.pnl.total_pnl,
            "realism mask {mask} is not deterministic"
        );
        assert_eq!(
            a.exec, b.exec,
            "realism mask {mask} produced different fills"
        );
    }
}

#[test]
fn replay_reproduces_the_book_that_direct_application_produces() {
    // The replay engine and a plain state machine must agree on the market;
    // the engine only adds simulation on top of the same event application.
    let mut direct = MarketState::new();
    for e in common::events() {
        direct.apply(&e);
    }
    let result = run(common::reader(), &common::markets(), &config(Realism::REAL)).unwrap();
    assert_eq!(result.state.snapshots, direct.stats().snapshots);
    assert_eq!(result.state.level_updates, direct.stats().level_updates);
    assert_eq!(result.state.trades, direct.stats().trades);
    assert_eq!(result.events, common::events().len() as u64);
}

#[test]
fn event_time_never_moves_backwards_across_the_session() {
    let mut clock = VirtualClock::new();
    let mut last = 0;
    for e in common::events() {
        let now = clock.advance_to(e.exchange_ms);
        assert!(now >= last, "clock went backwards");
        last = now;
    }
    assert!(last > 0);
}

#[test]
fn replay_does_not_depend_on_the_wall_clock() {
    // Running the same replay after a real delay must not change anything.
    let cfg = config(Realism::REAL);
    let markets = common::markets();
    let a = run(common::reader(), &markets, &cfg).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(120));
    let b = run(common::reader(), &markets, &cfg).unwrap();
    assert_eq!(a.pnl.total_pnl, b.pnl.total_pnl);
    assert_eq!(a.span_ms, b.span_ms);
}

#[test]
fn the_delayed_view_lags_the_exchange_view_by_the_measured_delay() {
    // With market-data realism on, the strategy sees fewer events than the
    // truth at any instant, because each frame is held until its recv time.
    let markets = common::markets();
    let ideal = run(common::reader(), &markets, &config(Realism::IDEAL)).unwrap();
    let real = run(common::reader(), &markets, &config(Realism::REAL)).unwrap();
    assert_eq!(ideal.events, real.events, "both consume the whole session");
    // The recorded feed delay is a real, positive quantity.
    let p50 = real.feed_delay.percentile(50.0).expect("a median delay");
    assert!(
        (150..=400).contains(&p50),
        "median feed delay {p50}ms is outside the range this feed exhibits"
    );
}

#[test]
fn trades_in_the_session_carry_both_aggressor_sides() {
    // The queue model depends on `side` being the taker; a fixture with only
    // one side would leave half that logic untested.
    let mut buys = 0;
    let mut sells = 0;
    for e in common::events() {
        if let polymarket_live_clob_research::market::event::EventPayload::Trade { side, .. } =
            e.payload
        {
            match side {
                Side::Buy => buys += 1,
                Side::Sell => sells += 1,
            }
        }
    }
    assert!(
        buys > 0 && sells > 0,
        "fixture needs both taker sides, got {buys}/{sells}"
    );
}
