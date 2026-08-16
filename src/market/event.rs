//! The normalized event model every downstream component consumes.
//!
//! Everything after this boundary — the book, the replay engine, the
//! execution simulator, the analytics — is written against [`MarketEvent`]
//! and has no idea whether the bytes arrived over a live WebSocket or were
//! read back off disk. That is what makes a replay faithful rather than
//! merely similar: live and replay share one decoder and one event type.
//!
//! # On sequence numbers
//!
//! Polymarket's public `market` channel does **not** carry sequence numbers.
//! The `seq` on a [`MarketEvent`] is assigned by *this* recorder, in receive
//! order, at capture time. It is a within-session ordering key and a replay
//! determinism anchor — it is not an exchange gap-detection primitive and
//! cannot be used to prove no message was lost in transit. See
//! `docs/DATA_MODEL.md`.

use serde::{Deserialize, Serialize};

use crate::market::orderbook::Level;
use crate::types::{Price, Qty, Side};

/// A single normalized market event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketEvent {
    /// Recorder-assigned monotonic sequence number, starting at 1.
    pub seq: u64,
    /// Local wall-clock time (ms since epoch) at which the frame was received.
    pub recv_ms: i64,
    /// Exchange-supplied timestamp (ms since epoch), or `recv_ms` when the
    /// message carries none.
    pub exchange_ms: i64,
    /// What actually happened.
    pub payload: EventPayload,
}

impl MarketEvent {
    /// The token this event concerns, when it concerns exactly one.
    pub fn asset_id(&self) -> Option<&str> {
        match &self.payload {
            EventPayload::Snapshot { asset_id, .. }
            | EventPayload::LevelUpdate { asset_id, .. }
            | EventPayload::Trade { asset_id, .. }
            | EventPayload::TickSizeChange { asset_id, .. } => Some(asset_id),
            EventPayload::MarketOpen { .. } | EventPayload::MarketClose { .. } => None,
        }
    }

    /// Observed one-way feed delay, `recv_ms - exchange_ms`.
    ///
    /// This mixes true network and exchange-side delay with the offset
    /// between the local clock and the exchange's. It is only a latency
    /// measurement to the extent the local clock is disciplined; the
    /// recorder samples the exchange clock at session start so the skew can
    /// be estimated and reported alongside. See [`crate::analytics::metrics`].
    pub fn feed_delay_ms(&self) -> i64 {
        self.recv_ms - self.exchange_ms
    }

    /// Short label used in inspection output.
    pub fn kind(&self) -> &'static str {
        self.payload.kind()
    }
}

/// The discriminated payload of a [`MarketEvent`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventPayload {
    /// A full book snapshot for one token, replacing all prior state.
    Snapshot {
        /// Token the snapshot describes.
        asset_id: String,
        /// Resting bids, as sent.
        bids: Vec<Level>,
        /// Resting asks, as sent.
        asks: Vec<Level>,
        /// Minimum price increment reported alongside the snapshot.
        tick_size: Option<Price>,
        /// The exchange's book hash, retained for provenance.
        hash: Option<String>,
    },
    /// A replacement of the aggregate resting size at one price level.
    ///
    /// `qty` is the new total at `price`, not a delta; `qty == 0` deletes.
    LevelUpdate {
        /// Token the update applies to.
        asset_id: String,
        /// Book side the level sits on.
        side: Side,
        /// Price of the level.
        price: Price,
        /// New aggregate resting size at `price`.
        qty: Qty,
        /// Exchange's view of the best bid at the time of the update.
        best_bid: Option<Price>,
        /// Exchange's view of the best ask at the time of the update.
        best_ask: Option<Price>,
        /// The exchange's book hash after applying the change.
        hash: Option<String>,
    },
    /// A public trade print.
    Trade {
        /// Token that traded.
        asset_id: String,
        /// Execution price.
        price: Price,
        /// Executed size.
        qty: Qty,
        /// Aggressor side as reported by the exchange.
        side: Side,
        /// Fee rate applied to the print, in basis points.
        fee_rate_bps: u32,
        /// Settlement transaction hash on Polygon.
        tx_hash: Option<String>,
    },
    /// The exchange changed a token's minimum price increment.
    TickSizeChange {
        /// Affected token.
        asset_id: String,
        /// New minimum increment.
        new_tick: Price,
    },
    /// Lifecycle marker: the recorder began following a market.
    ///
    /// Synthesised by the recorder from real Gamma metadata — the market
    /// channel itself publishes no lifecycle messages.
    MarketOpen {
        /// Event slug, e.g. `btc-updown-5m-1786844100`.
        slug: String,
        /// On-chain condition id.
        condition_id: String,
        /// Token id for the `Up` outcome.
        up_token: String,
        /// Token id for the `Down` outcome.
        down_token: String,
        /// Scheduled close time (ms since epoch) from market metadata.
        close_ms: i64,
    },
    /// Lifecycle marker: a followed market reached its scheduled close.
    MarketClose {
        /// Event slug that closed.
        slug: String,
        /// On-chain condition id.
        condition_id: String,
    },
}

impl EventPayload {
    /// Short label used in inspection output.
    pub fn kind(&self) -> &'static str {
        match self {
            EventPayload::Snapshot { .. } => "snapshot",
            EventPayload::LevelUpdate { .. } => "level_update",
            EventPayload::Trade { .. } => "trade",
            EventPayload::TickSizeChange { .. } => "tick_size_change",
            EventPayload::MarketOpen { .. } => "market_open",
            EventPayload::MarketClose { .. } => "market_close",
        }
    }
}

// `Level` lives with the book but travels inside events, so it is serialised
// here in the compact wire-like form rather than as a struct with named
// fields, keeping recorded sessions small.
impl Serialize for Level {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeTuple;
        let mut t = s.serialize_tuple(2)?;
        t.serialize_element(&self.price.ticks())?;
        t.serialize_element(&self.qty.0)?;
        t.end()
    }
}

impl<'de> Deserialize<'de> for Level {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Level, D::Error> {
        let (p, q) = <(u32, u64)>::deserialize(d)?;
        Ok(Level {
            price: Price::from_ticks(p),
            qty: Qty(q),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_round_trip_through_json_unchanged() {
        let ev = MarketEvent {
            seq: 42,
            recv_ms: 1_786_844_302_150,
            exchange_ms: 1_786_844_302_144,
            payload: EventPayload::LevelUpdate {
                asset_id:
                    "65637867123924978883300381859014997334719751092078905818953541938057691380772"
                        .into(),
                side: Side::Buy,
                price: Price::parse("0.2").unwrap(),
                qty: Qty::parse("60").unwrap(),
                best_bid: Some(Price::parse("0.5").unwrap()),
                best_ask: Some(Price::parse("0.51").unwrap()),
                hash: Some("ba530775bf217972f9e92867419a5a58e8b4a042".into()),
            },
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(serde_json::from_str::<MarketEvent>(&json).unwrap(), ev);
    }

    #[test]
    fn feed_delay_is_recv_minus_exchange() {
        let ev = MarketEvent {
            seq: 1,
            recv_ms: 1_000_120,
            exchange_ms: 1_000_000,
            payload: EventPayload::TickSizeChange {
                asset_id: "t".into(),
                new_tick: Price::parse("0.01").unwrap(),
            },
        };
        assert_eq!(ev.feed_delay_ms(), 120);
    }
}
