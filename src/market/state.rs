//! Aggregate state for every token in a recorded or live session.
//!
//! [`MarketState`] is the single mutable object a replay produces. Feeding
//! the same event sequence into a fresh `MarketState` always yields the same
//! final state — the determinism property the replay tests assert.

use std::collections::HashMap;

use crate::market::event::{EventPayload, MarketEvent};
use crate::market::orderbook::{BookError, OrderBook};
use crate::types::{Price, Qty, Side};

/// The most recent public trade seen on a token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LastTrade {
    /// Execution price.
    pub price: Price,
    /// Executed size.
    pub qty: Qty,
    /// Aggressor side reported by the exchange.
    pub side: Side,
    /// Exchange timestamp of the print.
    pub exchange_ms: i64,
}

/// Counters describing how cleanly a session decoded.
///
/// Recorded and reported rather than hidden: a replay that silently drops
/// updates would produce confident, wrong analytics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StateStats {
    /// Events applied successfully.
    pub applied: u64,
    /// Snapshots applied.
    pub snapshots: u64,
    /// Level updates applied.
    pub level_updates: u64,
    /// Trades observed.
    pub trades: u64,
    /// Updates rejected for arriving out of timestamp order.
    pub stale_rejected: u64,
    /// Updates that left the book momentarily crossed.
    pub crossed_observed: u64,
    /// Level updates for a token with no snapshot yet.
    pub before_snapshot: u64,
}

/// All books and last-trade state for a session.
#[derive(Debug, Default)]
pub struct MarketState {
    books: HashMap<String, OrderBook>,
    last_trades: HashMap<String, LastTrade>,
    /// Tokens for which a snapshot has been seen.
    snapshotted: HashMap<String, bool>,
    stats: StateStats,
    /// Exchange timestamp of the most recently applied event.
    clock_ms: i64,
}

impl MarketState {
    /// Creates empty state.
    pub fn new() -> MarketState {
        MarketState::default()
    }

    /// Exchange timestamp of the most recently applied event.
    pub fn clock_ms(&self) -> i64 {
        self.clock_ms
    }

    /// Decode and application counters for the session so far.
    pub fn stats(&self) -> StateStats {
        self.stats
    }

    /// The book for `asset_id`, if any event has referenced it.
    pub fn book(&self, asset_id: &str) -> Option<&OrderBook> {
        self.books.get(asset_id)
    }

    /// Most recent public trade on `asset_id`.
    pub fn last_trade(&self, asset_id: &str) -> Option<LastTrade> {
        self.last_trades.get(asset_id).copied()
    }

    /// Every token seen so far, in unspecified order.
    pub fn tokens(&self) -> impl Iterator<Item = &str> {
        self.books.keys().map(String::as_str)
    }

    /// Number of books being tracked.
    pub fn book_count(&self) -> usize {
        self.books.len()
    }

    /// A deterministic checksum over every reconstructed book.
    ///
    /// FNV-1a over each token's populated levels, in a fixed order: tokens
    /// sorted, bids then asks, ascending price. Two replays of one session
    /// must agree on this exactly, which is what `verify-replay` asserts —
    /// comparing final P&L alone would let a book divergence hide behind two
    /// offsetting execution errors.
    pub fn checksum(&self) -> u64 {
        const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const PRIME: u64 = 0x0000_0100_0000_01b3;
        let mut h = OFFSET;
        let mix = |v: u64, h: &mut u64| {
            for b in v.to_le_bytes() {
                *h ^= b as u64;
                *h = h.wrapping_mul(PRIME);
            }
        };
        let mut tokens: Vec<&String> = self.books.keys().collect();
        tokens.sort();
        for t in tokens {
            for b in t.as_bytes() {
                h ^= *b as u64;
                h = h.wrapping_mul(PRIME);
            }
            let book = &self.books[t];
            for side in [Side::Buy, Side::Sell] {
                let mut levels = book.levels(side, usize::MAX);
                levels.sort_by_key(|l| l.price.ticks());
                for l in levels {
                    mix(l.price.ticks() as u64, &mut h);
                    mix(l.qty.0, &mut h);
                }
            }
        }
        h
    }

    /// Applies one event, returning any book-level objection.
    ///
    /// Objections are surfaced *and* counted rather than propagated as hard
    /// errors: a stale frame or a momentary cross is a fact about the live
    /// market, not a reason to abandon a session.
    pub fn apply(&mut self, ev: &MarketEvent) -> Option<BookError> {
        self.clock_ms = self.clock_ms.max(ev.exchange_ms);
        self.stats.applied += 1;

        match &ev.payload {
            EventPayload::Snapshot {
                asset_id,
                bids,
                asks,
                hash,
                ..
            } => {
                self.stats.snapshots += 1;
                self.books
                    .entry(asset_id.clone())
                    .or_insert_with(|| OrderBook::new(asset_id.clone()))
                    .apply_snapshot(bids, asks, ev.exchange_ms, hash.as_deref());
                self.snapshotted.insert(asset_id.clone(), true);
                None
            }

            EventPayload::LevelUpdate {
                asset_id,
                side,
                price,
                qty,
                hash,
                ..
            } => {
                self.stats.level_updates += 1;
                if !self.snapshotted.get(asset_id).copied().unwrap_or(false) {
                    // Applying deltas to a book with no baseline yields a
                    // book that is confidently wrong. Count and skip until
                    // the snapshot arrives.
                    self.stats.before_snapshot += 1;
                    return None;
                }
                let book = self
                    .books
                    .entry(asset_id.clone())
                    .or_insert_with(|| OrderBook::new(asset_id.clone()));
                match book.set_level(*side, *price, *qty, ev.exchange_ms, hash.as_deref()) {
                    Ok(()) => None,
                    Err(e) => {
                        match e {
                            BookError::StaleUpdate { .. } => self.stats.stale_rejected += 1,
                            BookError::Crossed { .. } => self.stats.crossed_observed += 1,
                        }
                        Some(e)
                    }
                }
            }

            EventPayload::Trade {
                asset_id,
                price,
                qty,
                side,
                ..
            } => {
                self.stats.trades += 1;
                self.last_trades.insert(
                    asset_id.clone(),
                    LastTrade {
                        price: *price,
                        qty: *qty,
                        side: *side,
                        exchange_ms: ev.exchange_ms,
                    },
                );
                None
            }

            // Tick-size changes and lifecycle markers carry no book mutation;
            // they are retained in the session file for provenance.
            EventPayload::TickSizeChange { .. }
            | EventPayload::MarketOpen { .. }
            | EventPayload::MarketClose { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::orderbook::Level;

    fn snapshot(seq: u64, asset: &str, ms: i64) -> MarketEvent {
        MarketEvent {
            seq,
            recv_ms: ms,
            exchange_ms: ms,
            payload: EventPayload::Snapshot {
                asset_id: asset.into(),
                bids: vec![Level {
                    price: Price::parse("0.50").unwrap(),
                    qty: Qty::from_shares(100),
                }],
                asks: vec![Level {
                    price: Price::parse("0.51").unwrap(),
                    qty: Qty::from_shares(100),
                }],
                tick_size: Some(Price::parse("0.01").unwrap()),
                hash: None,
            },
        }
    }

    fn update(seq: u64, asset: &str, ms: i64, side: Side, p: &str, q: u64) -> MarketEvent {
        MarketEvent {
            seq,
            recv_ms: ms,
            exchange_ms: ms,
            payload: EventPayload::LevelUpdate {
                asset_id: asset.into(),
                side,
                price: Price::parse(p).unwrap(),
                qty: Qty::from_shares(q),
                best_bid: None,
                best_ask: None,
                hash: None,
            },
        }
    }

    #[test]
    fn deltas_before_a_snapshot_are_skipped_not_applied() {
        let mut s = MarketState::new();
        assert!(s.apply(&update(1, "t", 10, Side::Buy, "0.40", 5)).is_none());
        assert_eq!(s.stats().before_snapshot, 1);
        assert!(s.book("t").is_none_or(|b| b.is_empty()));
    }

    #[test]
    fn snapshot_then_updates_build_a_live_book() {
        let mut s = MarketState::new();
        s.apply(&snapshot(1, "t", 10));
        s.apply(&update(2, "t", 11, Side::Buy, "0.50", 40));
        let b = s.book("t").unwrap();
        assert_eq!(b.best_bid().unwrap().qty, Qty::from_shares(40));
        assert_eq!(s.stats().snapshots, 1);
        assert_eq!(s.stats().level_updates, 1);
    }

    #[test]
    fn stale_updates_are_counted_and_rejected() {
        let mut s = MarketState::new();
        s.apply(&snapshot(1, "t", 100));
        let err = s.apply(&update(2, "t", 50, Side::Buy, "0.50", 40));
        assert!(matches!(err, Some(BookError::StaleUpdate { .. })));
        assert_eq!(s.stats().stale_rejected, 1);
        assert_eq!(
            s.book("t").unwrap().best_bid().unwrap().qty,
            Qty::from_shares(100)
        );
    }

    #[test]
    fn clock_advances_monotonically_with_exchange_time() {
        let mut s = MarketState::new();
        s.apply(&snapshot(1, "t", 100));
        s.apply(&update(2, "t", 50, Side::Buy, "0.50", 40));
        assert_eq!(s.clock_ms(), 100, "a stale frame must not rewind the clock");
    }
}
