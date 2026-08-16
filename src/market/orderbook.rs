//! A real Polymarket CLOB order book, reconstructed from the public feed.
//!
//! # Feed semantics
//!
//! This book is driven by two message types on the `market` channel:
//!
//! * `book` — a full snapshot of every resting level for one token.
//! * `price_change` — one or more `(asset_id, side, price, size)` tuples.
//!
//! The critical detail, confirmed against the live feed, is that
//! `price_change.size` is the **new aggregate resting size at that price**,
//! not a delta against the previous size. A level is deleted by reporting
//! size `0`. Treating it as a delta silently corrupts the book, so all
//! mutation goes through [`OrderBook::set_level`], which replaces.
//!
//! # Representation
//!
//! Prices on a binary market are bounded to `[0, 1]` and quantised to
//! `1e-4`, so the whole price domain is only 10001 ticks wide. That makes a
//! dense ladder — one array slot per tick — strictly better than a tree or
//! hash map: updates are a single indexed write with no allocation, hashing
//! or rebalancing, and the memory is a fixed 160 KiB per book.
//!
//! Best bid and best ask are cached and repaired incrementally. A level
//! update that does not touch the touch is O(1); one that empties the touch
//! walks inward only as far as the next populated tick.

use crate::types::{Price, Qty, Side, PRICE_SCALE};

/// Number of addressable price ticks, inclusive of both `0.0` and `1.0`.
const LADDER_LEN: usize = PRICE_SCALE as usize + 1;

/// Rejection reasons for an update that would corrupt the book.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BookError {
    /// An update arrived stamped earlier than the book's current state.
    ///
    /// The exchange does not publish sequence numbers on the market channel,
    /// so time is the only ordering signal available and out-of-order frames
    /// must be dropped rather than applied.
    #[error("stale update: exchange time {incoming}ms precedes book time {current}ms")]
    StaleUpdate {
        /// Exchange timestamp on the rejected update.
        incoming: i64,
        /// Exchange timestamp the book currently reflects.
        current: i64,
    },
    /// Applying the update would put the best bid at or above the best ask.
    #[error("update crosses the book: bid {bid} >= ask {ask}")]
    Crossed {
        /// Resulting best bid.
        bid: Price,
        /// Resulting best ask.
        ask: Price,
    },
}

/// One aggregated price level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Level {
    /// Price of the level.
    pub price: Price,
    /// Total resting size across every order at this price.
    pub qty: Qty,
}

/// A reconstructed limit order book for a single outcome token.
#[derive(Debug, Clone)]
pub struct OrderBook {
    /// The token (ERC-1155 asset id) this book belongs to.
    asset_id: String,
    /// Resting bid size indexed by price tick.
    bids: Vec<Qty>,
    /// Resting ask size indexed by price tick.
    asks: Vec<Qty>,
    /// Cached best bid tick, or `None` when the bid side is empty.
    best_bid: Option<u32>,
    /// Cached best ask tick, or `None` when the ask side is empty.
    best_ask: Option<u32>,
    /// Populated level counts, kept so emptiness is O(1).
    n_bids: u32,
    n_asks: u32,
    /// Exchange timestamp (ms) of the most recently applied message.
    last_exchange_ms: i64,
    /// The exchange's own book hash from the last message that carried one.
    last_hash: Option<String>,
    /// Number of updates applied since construction.
    updates: u64,
}

impl OrderBook {
    /// Creates an empty book for `asset_id`.
    pub fn new(asset_id: impl Into<String>) -> OrderBook {
        OrderBook {
            asset_id: asset_id.into(),
            bids: vec![Qty::ZERO; LADDER_LEN],
            asks: vec![Qty::ZERO; LADDER_LEN],
            best_bid: None,
            best_ask: None,
            n_bids: 0,
            n_asks: 0,
            last_exchange_ms: 0,
            last_hash: None,
            updates: 0,
        }
    }

    /// The token this book tracks.
    pub fn asset_id(&self) -> &str {
        &self.asset_id
    }

    /// Exchange timestamp (ms) of the last applied message.
    pub fn last_exchange_ms(&self) -> i64 {
        self.last_exchange_ms
    }

    /// The exchange's book hash from the last message that supplied one.
    ///
    /// Recorded for provenance. The algorithm the exchange uses to compute it
    /// is not published, so this crate stores the value but does not attempt
    /// to verify the book against it.
    pub fn last_hash(&self) -> Option<&str> {
        self.last_hash.as_deref()
    }

    /// Count of messages applied since construction.
    pub fn updates(&self) -> u64 {
        self.updates
    }

    /// True when neither side has any resting size.
    pub fn is_empty(&self) -> bool {
        self.n_bids == 0 && self.n_asks == 0
    }

    /// Replaces the entire book with a snapshot.
    ///
    /// Snapshots are authoritative and re-baseline the clock, so unlike an
    /// incremental update a snapshot is never rejected as stale — that is
    /// exactly how the book recovers after a reconnect.
    pub fn apply_snapshot(
        &mut self,
        bids: &[Level],
        asks: &[Level],
        exchange_ms: i64,
        hash: Option<&str>,
    ) {
        self.bids.iter_mut().for_each(|q| *q = Qty::ZERO);
        self.asks.iter_mut().for_each(|q| *q = Qty::ZERO);
        self.n_bids = 0;
        self.n_asks = 0;
        self.best_bid = None;
        self.best_ask = None;

        for lv in bids {
            if !lv.qty.is_zero() {
                let i = lv.price.ticks() as usize;
                if self.bids[i].is_zero() {
                    self.n_bids += 1;
                }
                self.bids[i] = lv.qty;
                self.best_bid = Some(match self.best_bid {
                    Some(b) => b.max(lv.price.ticks()),
                    None => lv.price.ticks(),
                });
            }
        }
        for lv in asks {
            if !lv.qty.is_zero() {
                let i = lv.price.ticks() as usize;
                if self.asks[i].is_zero() {
                    self.n_asks += 1;
                }
                self.asks[i] = lv.qty;
                self.best_ask = Some(match self.best_ask {
                    Some(a) => a.min(lv.price.ticks()),
                    None => lv.price.ticks(),
                });
            }
        }

        self.last_exchange_ms = exchange_ms;
        self.last_hash = hash.map(str::to_owned);
        self.updates += 1;
    }

    /// Applies one `price_change` tuple, replacing the aggregate size at
    /// `price` on `side`.
    ///
    /// Rejects updates stamped before the book's current time. A crossed
    /// result is reported to the caller but still applied: the feed is the
    /// source of truth, and a momentary cross is a real observation about
    /// the market rather than a decoding bug.
    pub fn set_level(
        &mut self,
        side: Side,
        price: Price,
        qty: Qty,
        exchange_ms: i64,
        hash: Option<&str>,
    ) -> Result<(), BookError> {
        if exchange_ms < self.last_exchange_ms {
            return Err(BookError::StaleUpdate {
                incoming: exchange_ms,
                current: self.last_exchange_ms,
            });
        }

        let tick = price.ticks();
        let i = tick as usize;
        let (ladder, count) = match side {
            Side::Buy => (&mut self.bids, &mut self.n_bids),
            Side::Sell => (&mut self.asks, &mut self.n_asks),
        };
        let was_populated = !ladder[i].is_zero();
        let now_populated = !qty.is_zero();
        ladder[i] = qty;
        match (was_populated, now_populated) {
            (false, true) => *count += 1,
            (true, false) => *count -= 1,
            _ => {}
        }

        // Repair the cached touch. Only three cases can move it: a new level
        // outside the current touch, or the touch itself emptying.
        match side {
            Side::Buy => match (self.best_bid, now_populated) {
                (_, true) if self.best_bid.is_none_or(|b| tick > b) => self.best_bid = Some(tick),
                (Some(b), false) if b == tick => self.best_bid = self.scan_down(tick),
                _ => {}
            },
            Side::Sell => match (self.best_ask, now_populated) {
                (_, true) if self.best_ask.is_none_or(|a| tick < a) => self.best_ask = Some(tick),
                (Some(a), false) if a == tick => self.best_ask = self.scan_up(tick),
                _ => {}
            },
        }

        self.last_exchange_ms = exchange_ms;
        if let Some(h) = hash {
            self.last_hash = Some(h.to_owned());
        }
        self.updates += 1;

        match (self.best_bid, self.best_ask) {
            (Some(b), Some(a)) if b >= a => Err(BookError::Crossed {
                bid: Price(b),
                ask: Price(a),
            }),
            _ => Ok(()),
        }
    }

    /// Finds the highest populated bid tick at or below `from`.
    fn scan_down(&self, from: u32) -> Option<u32> {
        (0..=from).rev().find(|&t| !self.bids[t as usize].is_zero())
    }

    /// Finds the lowest populated ask tick at or above `from`.
    fn scan_up(&self, from: u32) -> Option<u32> {
        (from..LADDER_LEN as u32).find(|&t| !self.asks[t as usize].is_zero())
    }

    /// Best bid level, if the bid side is populated.
    pub fn best_bid(&self) -> Option<Level> {
        self.best_bid.map(|t| Level {
            price: Price(t),
            qty: self.bids[t as usize],
        })
    }

    /// Best ask level, if the ask side is populated.
    pub fn best_ask(&self) -> Option<Level> {
        self.best_ask.map(|t| Level {
            price: Price(t),
            qty: self.asks[t as usize],
        })
    }

    /// Best price on `side`, if populated.
    pub fn best(&self, side: Side) -> Option<Level> {
        match side {
            Side::Buy => self.best_bid(),
            Side::Sell => self.best_ask(),
        }
    }

    /// Difference between best ask and best bid, when both sides are populated.
    pub fn spread(&self) -> Option<Price> {
        match (self.best_bid, self.best_ask) {
            (Some(b), Some(a)) if a >= b => Some(Price(a - b)),
            _ => None,
        }
    }

    /// Arithmetic midpoint of the touch.
    ///
    /// Rounds down on a half tick, which is deterministic and therefore
    /// replay-stable; the alternative of returning a half-tick price would
    /// not be representable on the exchange's grid.
    pub fn mid(&self) -> Option<Price> {
        match (self.best_bid, self.best_ask) {
            (Some(b), Some(a)) => Some(Price((b + a) / 2)),
            _ => None,
        }
    }

    /// Aggregate resting size at an exact price on `side`.
    pub fn qty_at(&self, side: Side, price: Price) -> Qty {
        let ladder = match side {
            Side::Buy => &self.bids,
            Side::Sell => &self.asks,
        };
        ladder[price.ticks() as usize]
    }

    /// The `depth` best levels on `side`, ordered outward from the touch.
    ///
    /// `depth` may exceed the ladder — `usize::MAX` is a legitimate way to
    /// ask for everything — so the reservation is clamped to the number of
    /// levels that can physically exist.
    pub fn levels(&self, side: Side, depth: usize) -> Vec<Level> {
        let mut out = Vec::with_capacity(depth.min(LADDER_LEN));
        match side {
            Side::Buy => {
                let start = match self.best_bid {
                    Some(b) => b,
                    None => return out,
                };
                for t in (0..=start).rev() {
                    let q = self.bids[t as usize];
                    if !q.is_zero() {
                        out.push(Level {
                            price: Price(t),
                            qty: q,
                        });
                        if out.len() == depth {
                            break;
                        }
                    }
                }
            }
            Side::Sell => {
                let start = match self.best_ask {
                    Some(a) => a,
                    None => return out,
                };
                for t in start..LADDER_LEN as u32 {
                    let q = self.asks[t as usize];
                    if !q.is_zero() {
                        out.push(Level {
                            price: Price(t),
                            qty: q,
                        });
                        if out.len() == depth {
                            break;
                        }
                    }
                }
            }
        }
        out
    }

    /// Total resting size within the `depth` best levels of `side`.
    pub fn depth_qty(&self, side: Side, depth: usize) -> Qty {
        Qty(self.levels(side, depth).iter().map(|l| l.qty.0).sum())
    }

    /// Order-book imbalance over the `depth` best levels, in `[-1, 1]`.
    ///
    /// Positive means bid-heavy. Returns `None` when both sides are empty,
    /// which is distinct from a genuinely balanced book at `0.0`.
    pub fn imbalance(&self, depth: usize) -> Option<f64> {
        let b = self.depth_qty(Side::Buy, depth).0 as f64;
        let a = self.depth_qty(Side::Sell, depth).0 as f64;
        if b + a == 0.0 {
            return None;
        }
        Some((b - a) / (b + a))
    }

    /// Walks the resting book to price an aggressive order **without
    /// mutating it**, returning `(level, filled)` pairs outward from the touch.
    ///
    /// `side` is the side of the *book* being consumed: a buyer lifts offers
    /// and so consumes [`Side::Sell`]. Fills stop at `limit`, or run to the
    /// end of the book when `limit` is `None`.
    ///
    /// The book is deliberately left untouched. This simulator observes a
    /// market it never traded in, so a hypothetical order cannot have removed
    /// real liquidity — see `docs/EXECUTION_MODEL.md` on market impact.
    pub fn walk(&self, side: Side, limit: Option<Price>, qty: Qty) -> Vec<(Level, Qty)> {
        let mut out = Vec::new();
        let mut remaining = qty;
        if remaining.is_zero() {
            return out;
        }
        let ticks: Box<dyn Iterator<Item = u32>> = match side {
            Side::Sell => match self.best_ask {
                Some(a) => Box::new(a..LADDER_LEN as u32),
                None => return out,
            },
            Side::Buy => match self.best_bid {
                Some(b) => Box::new((0..=b).rev()),
                None => return out,
            },
        };
        for t in ticks {
            if let Some(lim) = limit {
                let acceptable = match side {
                    // Consuming asks: pay no more than the limit.
                    Side::Sell => t <= lim.ticks(),
                    // Consuming bids: receive no less than the limit.
                    Side::Buy => t >= lim.ticks(),
                };
                if !acceptable {
                    break;
                }
            }
            let available = match side {
                Side::Buy => self.bids[t as usize],
                Side::Sell => self.asks[t as usize],
            };
            if available.is_zero() {
                continue;
            }
            let take = Qty(available.0.min(remaining.0));
            out.push((
                Level {
                    price: Price(t),
                    qty: available,
                },
                take,
            ));
            remaining = remaining.saturating_sub(take);
            if remaining.is_zero() {
                break;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lv(p: &str, q: &str) -> Level {
        Level {
            price: Price::parse(p).unwrap(),
            qty: Qty::parse(q).unwrap(),
        }
    }

    fn book() -> OrderBook {
        let mut b = OrderBook::new("tok");
        b.apply_snapshot(
            &[lv("0.48", "100"), lv("0.49", "200"), lv("0.50", "50")],
            &[lv("0.51", "80"), lv("0.52", "300")],
            1_000,
            Some("abc"),
        );
        b
    }

    #[test]
    fn snapshot_establishes_the_touch() {
        let b = book();
        assert_eq!(b.best_bid().unwrap().price, Price::parse("0.50").unwrap());
        assert_eq!(b.best_ask().unwrap().price, Price::parse("0.51").unwrap());
        assert_eq!(b.spread().unwrap(), Price::parse("0.01").unwrap());
        assert_eq!(b.mid().unwrap(), Price(5_050));
    }

    #[test]
    fn size_replaces_rather_than_accumulates() {
        let mut b = book();
        // The feed reports the new aggregate, not a delta.
        b.set_level(
            Side::Buy,
            Price::parse("0.49").unwrap(),
            Qty::from_shares(75),
            1_001,
            None,
        )
        .unwrap();
        assert_eq!(
            b.qty_at(Side::Buy, Price::parse("0.49").unwrap()),
            Qty::from_shares(75)
        );
    }

    #[test]
    fn zero_size_deletes_the_level_and_moves_the_touch() {
        let mut b = book();
        b.set_level(
            Side::Buy,
            Price::parse("0.50").unwrap(),
            Qty::ZERO,
            1_001,
            None,
        )
        .unwrap();
        assert_eq!(b.best_bid().unwrap().price, Price::parse("0.49").unwrap());
        assert_eq!(
            b.qty_at(Side::Buy, Price::parse("0.50").unwrap()),
            Qty::ZERO
        );
    }

    #[test]
    fn emptying_a_whole_side_clears_the_touch() {
        let mut b = book();
        for p in ["0.50", "0.49", "0.48"] {
            b.set_level(Side::Buy, Price::parse(p).unwrap(), Qty::ZERO, 1_002, None)
                .unwrap();
        }
        assert!(b.best_bid().is_none());
        assert!(b.spread().is_none());
        assert!(b.mid().is_none());
        assert!(!b.is_empty(), "the ask side is still populated");
    }

    #[test]
    fn rejects_updates_stamped_before_current_book_time() {
        let mut b = book();
        let err = b
            .set_level(
                Side::Buy,
                Price::parse("0.49").unwrap(),
                Qty::ZERO,
                999,
                None,
            )
            .unwrap_err();
        assert!(matches!(err, BookError::StaleUpdate { .. }));
        // The rejected update must not have been applied.
        assert_eq!(
            b.qty_at(Side::Buy, Price::parse("0.49").unwrap()),
            Qty::from_shares(200)
        );
    }

    #[test]
    fn reports_a_cross_but_still_reflects_the_feed() {
        let mut b = book();
        let err = b
            .set_level(
                Side::Buy,
                Price::parse("0.53").unwrap(),
                Qty::from_shares(10),
                1_003,
                None,
            )
            .unwrap_err();
        assert!(matches!(err, BookError::Crossed { .. }));
        assert_eq!(b.best_bid().unwrap().price, Price::parse("0.53").unwrap());
    }

    #[test]
    fn walk_consumes_outward_from_the_touch_and_respects_the_limit() {
        let b = book();
        // Buying 200 shares with a 0.52 limit takes all 80 at 0.51 then 120 at 0.52.
        let fills = b.walk(
            Side::Sell,
            Some(Price::parse("0.52").unwrap()),
            Qty::from_shares(200),
        );
        assert_eq!(fills.len(), 2);
        assert_eq!(fills[0].1, Qty::from_shares(80));
        assert_eq!(fills[1].1, Qty::from_shares(120));

        // A 0.51 limit stops after the touch: the rest is a missed fill.
        let capped = b.walk(
            Side::Sell,
            Some(Price::parse("0.51").unwrap()),
            Qty::from_shares(200),
        );
        assert_eq!(capped.len(), 1);
        assert_eq!(capped[0].1, Qty::from_shares(80));
    }

    #[test]
    fn walk_does_not_mutate_the_book() {
        let b = book();
        let before = b.best_ask().unwrap().qty;
        let _ = b.walk(Side::Sell, None, Qty::from_shares(1_000));
        assert_eq!(b.best_ask().unwrap().qty, before);
    }

    #[test]
    fn an_unbounded_depth_request_returns_the_whole_side() {
        let b = book();
        // usize::MAX must mean "everything", not a capacity overflow.
        assert_eq!(b.levels(Side::Buy, usize::MAX).len(), 3);
        assert_eq!(b.depth_qty(Side::Sell, usize::MAX), Qty::from_shares(380));
    }

    #[test]
    fn imbalance_is_signed_toward_the_heavier_side() {
        let b = book();
        // Bids 50+200+100=350 vs asks 80+300=380 within 3 levels.
        let imb = b.imbalance(3).unwrap();
        assert!(imb < 0.0, "ask-heavy book must report negative imbalance");
        assert!(OrderBook::new("empty").imbalance(3).is_none());
    }
}
