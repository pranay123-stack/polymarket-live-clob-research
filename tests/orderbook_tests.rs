//! Order book reconstruction, validated against a real recorded session.

mod common;

use std::collections::HashMap;

use polymarket_live_clob_research::market::event::EventPayload;
use polymarket_live_clob_research::market::orderbook::OrderBook;
use polymarket_live_clob_research::market::state::MarketState;
use polymarket_live_clob_research::types::{Price, Side, PRICE_SCALE};

#[test]
fn the_fixture_contains_the_real_data_the_suite_depends_on() {
    let evs = common::events();
    let mut snapshots = 0;
    let mut updates = 0;
    let mut trades = 0;
    for e in &evs {
        match e.payload {
            EventPayload::Snapshot { .. } => snapshots += 1,
            EventPayload::LevelUpdate { .. } => updates += 1,
            EventPayload::Trade { .. } => trades += 1,
            _ => {}
        }
    }
    assert!(snapshots >= 8, "need real snapshots, got {snapshots}");
    assert!(updates > 8_000, "need real level updates, got {updates}");
    assert!(trades > 50, "need real trade prints, got {trades}");
    assert!(
        !common::markets().is_empty(),
        "header must carry market metadata"
    );
}

#[test]
fn replaying_the_session_never_rejects_an_update() {
    let mut state = MarketState::new();
    for e in common::events() {
        state.apply(&e);
    }
    let s = state.stats();
    assert_eq!(
        s.stale_rejected, 0,
        "the recording is in order; rejections would mean a decoding bug"
    );
    assert_eq!(
        s.before_snapshot, 0,
        "every token should be snapshotted before its first delta"
    );
    assert!(s.snapshots > 0 && s.level_updates > 0);
}

#[test]
fn every_book_ends_in_a_self_consistent_state() {
    let mut state = MarketState::new();
    for e in common::events() {
        state.apply(&e);
    }
    let tokens: Vec<String> = state.tokens().map(str::to_owned).collect();
    assert!(
        tokens.len() >= 4,
        "expected several tokens, got {}",
        tokens.len()
    );

    for t in &tokens {
        let b = state.book(t).expect("token was listed by the state");
        if let (Some(bid), Some(ask)) = (b.best_bid(), b.best_ask()) {
            assert!(
                bid.price < ask.price,
                "{t}: book ended crossed, bid {} >= ask {}",
                bid.price,
                ask.price
            );
            assert_eq!(
                b.spread().unwrap(),
                Price(ask.price.ticks() - bid.price.ticks())
            );
            let mid = b.mid().unwrap();
            assert!(
                mid >= bid.price && mid <= ask.price,
                "{t}: mid outside the touch"
            );
        }
        // Levels must be ordered outward from the touch on both sides.
        let bids = b.levels(Side::Buy, 20);
        assert!(
            bids.windows(2).all(|w| w[0].price > w[1].price),
            "{t}: bids out of order"
        );
        let asks = b.levels(Side::Sell, 20);
        assert!(
            asks.windows(2).all(|w| w[0].price < w[1].price),
            "{t}: asks out of order"
        );
        assert!(
            bids.iter().all(|l| !l.qty.is_zero()),
            "{t}: empty level reported"
        );
    }
}

#[test]
fn complementary_up_and_down_books_mirror_each_other() {
    // On a binary market the Up and Down tokens are complements: a bid of p
    // on Up is economically an ask of 1-p on Down. Any drift between them
    // would mean the reconstruction has lost or misplaced size.
    let mut state = MarketState::new();
    for e in common::events() {
        state.apply(&e);
    }

    let mut checked = 0;
    for m in common::markets() {
        let (Some(up), Some(down)) = (state.book(&m.up_token), state.book(&m.down_token)) else {
            continue;
        };
        let (Some(up_bid), Some(down_ask)) = (up.best_bid(), down.best_ask()) else {
            continue;
        };
        assert_eq!(
            up_bid.price.ticks() + down_ask.price.ticks(),
            PRICE_SCALE,
            "{}: Up bid {} and Down ask {} are not complements",
            m.slug,
            up_bid.price,
            down_ask.price
        );
        assert_eq!(
            up_bid.qty, down_ask.qty,
            "{}: complementary levels carry different size",
            m.slug
        );
        checked += 1;
    }
    assert!(checked > 0, "no market had both books populated");
}

#[test]
fn snapshots_rebaseline_a_book_completely() {
    // Locate a token that receives a snapshot after updates, then confirm
    // the snapshot replaces rather than merges with prior state.
    let evs = common::events();
    let mut state = MarketState::new();
    let mut seen_update: HashMap<String, bool> = HashMap::new();

    for e in &evs {
        if let EventPayload::Snapshot {
            asset_id,
            bids,
            asks,
            ..
        } = &e.payload
        {
            if seen_update.get(asset_id).copied().unwrap_or(false) {
                state.apply(e);
                let book = state.book(asset_id).unwrap();
                let mut fresh = OrderBook::new(asset_id.clone());
                fresh.apply_snapshot(bids.as_slice(), asks.as_slice(), e.exchange_ms, None);
                assert_eq!(
                    book.best_bid().map(|l| l.price),
                    fresh.best_bid().map(|l| l.price),
                    "{asset_id}: snapshot did not fully rebaseline the book"
                );
                assert_eq!(
                    book.depth_qty(Side::Buy, 50),
                    fresh.depth_qty(Side::Buy, 50)
                );
                return;
            }
        }
        if let EventPayload::LevelUpdate { asset_id, .. } = &e.payload {
            seen_update.insert(asset_id.clone(), true);
        }
        state.apply(e);
    }
    panic!("fixture contained no snapshot following an update");
}

#[test]
fn walking_the_book_never_exceeds_displayed_liquidity() {
    let mut state = MarketState::new();
    for e in common::events() {
        state.apply(&e);
    }
    for t in state.tokens().map(str::to_owned).collect::<Vec<_>>() {
        let b = state.book(&t).unwrap();
        let available = b.depth_qty(Side::Sell, usize::MAX);
        let asked = polymarket_live_clob_research::types::Qty(available.0 * 2 + 1_000_000);
        let taken: u64 = b
            .walk(Side::Sell, None, asked)
            .iter()
            .map(|(_, q)| q.0)
            .sum();
        assert_eq!(
            taken, available.0,
            "{t}: walk returned more size than the book displayed"
        );
    }
}

#[test]
fn imbalance_stays_within_its_defined_range_on_real_books() {
    let mut state = MarketState::new();
    for e in common::events() {
        state.apply(&e);
    }
    for t in state.tokens().map(str::to_owned).collect::<Vec<_>>() {
        if let Some(i) = state.book(&t).unwrap().imbalance(5) {
            assert!((-1.0..=1.0).contains(&i), "{t}: imbalance {i} out of range");
        }
    }
}
