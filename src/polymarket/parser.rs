//! Decodes raw Polymarket `market`-channel frames into [`MarketEvent`]s.
//!
//! # Wire shapes
//!
//! Frames arrive as a JSON **array** of objects, each discriminated by
//! `event_type`. The three types observed on live BTC 5-minute markets:
//!
//! ```text
//! book              {market, asset_id, timestamp, hash, bids[], asks[], tick_size?}
//! price_change      {market, timestamp, price_changes[{asset_id, price, size,
//!                                                      side, hash, best_bid, best_ask}]}
//! last_trade_price  {market, asset_id, price, size, side, fee_rate_bps,
//!                    timestamp, transaction_hash}
//! ```
//!
//! A single `price_change` frame carries changes for several tokens at once,
//! so one frame fans out to several [`MarketEvent`]s.
//!
//! # Determinism
//!
//! [`Normalizer`] owns the sequence counter, and numbering depends only on
//! the frames it has been fed. Replaying a recorded session through a fresh
//! `Normalizer` therefore reproduces byte-identical events — the property the
//! replay determinism tests rest on.

use serde_json::Value;

use crate::market::event::{EventPayload, MarketEvent};
use crate::market::orderbook::Level;
use crate::types::{ParseError, Price, Qty, Side};

/// Failure decoding one frame.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    /// The frame had no `event_type`, so it cannot be dispatched.
    #[error("frame is missing event_type")]
    MissingEventType,
    /// A field the frame's type requires was absent or the wrong JSON type.
    #[error("missing or malformed field `{0}`")]
    BadField(&'static str),
    /// A numeric field failed fixed-point decoding.
    #[error("field `{field}`: {source}")]
    BadNumber {
        /// Name of the offending field.
        field: &'static str,
        /// Underlying fixed-point failure.
        #[source]
        source: ParseError,
    },
    /// A well-formed frame whose `event_type` this crate does not model.
    ///
    /// Carried as an error so callers can count and log unknown types rather
    /// than silently discarding data the exchange has started sending.
    #[error("unhandled event_type `{0}`")]
    Unhandled(String),
}

/// Stateful frame decoder that assigns sequence numbers.
#[derive(Debug, Default)]
pub struct Normalizer {
    next_seq: u64,
}

impl Normalizer {
    /// Creates a normalizer whose first event will be `seq = 1`.
    pub fn new() -> Normalizer {
        Normalizer { next_seq: 1 }
    }

    /// Sequence number the next emitted event will carry.
    pub fn next_seq(&self) -> u64 {
        self.next_seq.max(1)
    }

    /// Assigns the next sequence number to `payload`.
    pub fn emit(&mut self, recv_ms: i64, exchange_ms: i64, payload: EventPayload) -> MarketEvent {
        let seq = self.next_seq.max(1);
        self.next_seq = seq + 1;
        MarketEvent {
            seq,
            recv_ms,
            exchange_ms,
            payload,
        }
    }

    /// Decodes one frame object into zero or more events.
    ///
    /// `recv_ms` is the local receive time the recorder stamped on the frame.
    /// Frames without an exchange `timestamp` fall back to `recv_ms`, which
    /// keeps the event stream monotonic at the cost of a zero measured delay
    /// for that event.
    pub fn normalize(&mut self, raw: &Value, recv_ms: i64) -> Result<Vec<MarketEvent>, FrameError> {
        let event_type = raw
            .get("event_type")
            .and_then(Value::as_str)
            .ok_or(FrameError::MissingEventType)?;
        let exchange_ms = raw
            .get("timestamp")
            .and_then(as_i64_loose)
            .unwrap_or(recv_ms);

        match event_type {
            "book" => {
                let asset_id = str_field(raw, "asset_id")?;
                let bids = levels(raw.get("bids"), "bids")?;
                let asks = levels(raw.get("asks"), "asks")?;
                let tick_size = match raw.get("tick_size") {
                    Some(v) if !v.is_null() => Some(price_of(v, "tick_size")?),
                    _ => None,
                };
                let hash = raw.get("hash").and_then(Value::as_str).map(str::to_owned);
                Ok(vec![self.emit(
                    recv_ms,
                    exchange_ms,
                    EventPayload::Snapshot {
                        asset_id,
                        bids,
                        asks,
                        tick_size,
                        hash,
                    },
                )])
            }

            "price_change" => {
                let changes = raw
                    .get("price_changes")
                    .and_then(Value::as_array)
                    .ok_or(FrameError::BadField("price_changes"))?;
                let mut out = Vec::with_capacity(changes.len());
                for c in changes {
                    let asset_id = str_field(c, "asset_id")?;
                    let price = price_field(c, "price")?;
                    let qty = qty_field(c, "size")?;
                    let side = Side::parse(
                        c.get("side")
                            .and_then(Value::as_str)
                            .ok_or(FrameError::BadField("side"))?,
                    )
                    .map_err(|source| FrameError::BadNumber {
                        field: "side",
                        source,
                    })?;
                    let best_bid = opt_price(c, "best_bid")?;
                    let best_ask = opt_price(c, "best_ask")?;
                    let hash = c.get("hash").and_then(Value::as_str).map(str::to_owned);
                    out.push(self.emit(
                        recv_ms,
                        exchange_ms,
                        EventPayload::LevelUpdate {
                            asset_id,
                            side,
                            price,
                            qty,
                            best_bid,
                            best_ask,
                            hash,
                        },
                    ));
                }
                Ok(out)
            }

            "last_trade_price" => {
                let asset_id = str_field(raw, "asset_id")?;
                let price = price_field(raw, "price")?;
                let qty = qty_field(raw, "size")?;
                let side = Side::parse(
                    raw.get("side")
                        .and_then(Value::as_str)
                        .ok_or(FrameError::BadField("side"))?,
                )
                .map_err(|source| FrameError::BadNumber {
                    field: "side",
                    source,
                })?;
                let fee_rate_bps = raw
                    .get("fee_rate_bps")
                    .and_then(as_i64_loose)
                    .unwrap_or(0)
                    .max(0) as u32;
                let tx_hash = raw
                    .get("transaction_hash")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                Ok(vec![self.emit(
                    recv_ms,
                    exchange_ms,
                    EventPayload::Trade {
                        asset_id,
                        price,
                        qty,
                        side,
                        fee_rate_bps,
                        tx_hash,
                    },
                )])
            }

            "tick_size_change" => {
                let asset_id = str_field(raw, "asset_id")?;
                let new_tick = price_field(raw, "new_tick_size")?;
                Ok(vec![self.emit(
                    recv_ms,
                    exchange_ms,
                    EventPayload::TickSizeChange { asset_id, new_tick },
                )])
            }

            other => Err(FrameError::Unhandled(other.to_owned())),
        }
    }
}

/// Reads a required string field.
fn str_field(v: &Value, field: &'static str) -> Result<String, FrameError> {
    v.get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or(FrameError::BadField(field))
}

/// Accepts a number encoded either as a JSON number or as a string.
///
/// The feed is inconsistent about this: `timestamp` is a string while
/// `tick_size` is a number, and `fee_rate_bps` has been seen as both.
fn as_i64_loose(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
        .or_else(|| v.as_f64().map(|f| f as i64))
}

/// Decodes a price from either a JSON string or number.
fn price_of(v: &Value, field: &'static str) -> Result<Price, FrameError> {
    let s = match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return Err(FrameError::BadField(field)),
    };
    Price::parse(&s).map_err(|source| FrameError::BadNumber { field, source })
}

/// Reads a required price field.
fn price_field(v: &Value, field: &'static str) -> Result<Price, FrameError> {
    price_of(v.get(field).ok_or(FrameError::BadField(field))?, field)
}

/// Reads an optional price field, tolerating both `null` and absence.
fn opt_price(v: &Value, field: &'static str) -> Result<Option<Price>, FrameError> {
    match v.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(x) => price_of(x, field).map(Some),
    }
}

/// Reads a required quantity field.
fn qty_field(v: &Value, field: &'static str) -> Result<Qty, FrameError> {
    let raw = v.get(field).ok_or(FrameError::BadField(field))?;
    let s = match raw {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return Err(FrameError::BadField(field)),
    };
    Qty::parse(&s).map_err(|source| FrameError::BadNumber { field, source })
}

/// Decodes a `[{price, size}, ...]` array into book levels.
fn levels(v: Option<&Value>, field: &'static str) -> Result<Vec<Level>, FrameError> {
    let arr = v
        .and_then(Value::as_array)
        .ok_or(FrameError::BadField(field))?;
    let mut out = Vec::with_capacity(arr.len());
    for e in arr {
        out.push(Level {
            price: price_field(e, "price")?,
            qty: qty_field(e, "size")?,
        });
    }
    Ok(out)
}

/// Splits a raw WebSocket text payload into individual frame objects.
///
/// The exchange sends a JSON array per message, but tolerating a bare object
/// costs nothing and guards against a future single-event framing change.
pub fn split_frames(text: &str) -> Result<Vec<Value>, serde_json::Error> {
    match serde_json::from_str::<Value>(text)? {
        Value::Array(items) => Ok(items),
        other => Ok(vec![other]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every fixture below is a verbatim frame captured from
    // wss://ws-subscriptions-clob.polymarket.com/ws/market.

    const PRICE_CHANGE: &str = r#"{
      "market": "0x101ed8ef576fa987d45edf88551852ecfc5f85cf67302e541a619f3dbf23d607",
      "price_changes": [
        {"asset_id":"65637867123924978883300381859014997334719751092078905818953541938057691380772",
         "price":"0.2","size":"60","side":"BUY",
         "hash":"ba530775bf217972f9e92867419a5a58e8b4a042",
         "best_bid":"0.5","best_ask":"0.51"},
        {"asset_id":"114195042028531901061138597381801720656349527417762737125582806097729009801740",
         "price":"0.8","size":"60","side":"SELL",
         "hash":"3feca764f7303bf956167a9163dc2df912ab2662",
         "best_bid":"0.49","best_ask":"0.5"}
      ],
      "timestamp":"1786844302144","event_type":"price_change"}"#;

    const LAST_TRADE: &str = r#"{
      "market":"0x3b6b6807f535e0971af85f8e5c63399bc548d359cc02e347aaf01fe200a8a265",
      "asset_id":"104830988669153084352108937274802865572954927866102589054231572742216390377561",
      "price":"0.51","size":"20","fee_rate_bps":"0","side":"BUY",
      "timestamp":"1786844303130","event_type":"last_trade_price",
      "transaction_hash":"0x7fe686345957cd59e96b5b8aba518e1e3c08c3c1583c2794e67afc72f34320d8"}"#;

    const BOOK: &str = r#"{
      "market":"0x3b6b6807f535e0971af85f8e5c63399bc548d359cc02e347aaf01fe200a8a265",
      "asset_id":"87889240276906173592104621060527334299838911378771235560353538774356770591500",
      "timestamp":"1786844256381","hash":"d49fce0d4d337d7b96e7d6b9b48bf53b6b193cb9",
      "bids":[{"price":"0.01","size":"93541.6"},{"price":"0.5","size":"100"}],
      "asks":[{"price":"0.51","size":"250"},{"price":"0.99","size":"1000"}],
      "tick_size":"0.01","event_type":"book"}"#;

    #[test]
    fn one_price_change_frame_fans_out_to_one_event_per_token() {
        let mut n = Normalizer::new();
        let evs = n
            .normalize(
                &serde_json::from_str(PRICE_CHANGE).unwrap(),
                1_786_844_302_150,
            )
            .unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].seq, 1);
        assert_eq!(evs[1].seq, 2);
        assert_eq!(evs[0].exchange_ms, 1_786_844_302_144);
        assert_eq!(evs[0].feed_delay_ms(), 6);

        match &evs[0].payload {
            EventPayload::LevelUpdate {
                side,
                price,
                qty,
                best_ask,
                ..
            } => {
                assert_eq!(*side, Side::Buy);
                assert_eq!(*price, Price::parse("0.2").unwrap());
                assert_eq!(*qty, Qty::from_shares(60));
                assert_eq!(*best_ask, Some(Price::parse("0.51").unwrap()));
            }
            other => panic!("expected LevelUpdate, got {other:?}"),
        }
        assert!(matches!(
            evs[1].payload,
            EventPayload::LevelUpdate {
                side: Side::Sell,
                ..
            }
        ));
    }

    #[test]
    fn decodes_a_real_trade_print() {
        let mut n = Normalizer::new();
        let evs = n
            .normalize(
                &serde_json::from_str(LAST_TRADE).unwrap(),
                1_786_844_303_140,
            )
            .unwrap();
        match &evs[0].payload {
            EventPayload::Trade {
                price,
                qty,
                side,
                fee_rate_bps,
                tx_hash,
                ..
            } => {
                assert_eq!(*price, Price::parse("0.51").unwrap());
                assert_eq!(*qty, Qty::from_shares(20));
                assert_eq!(*side, Side::Buy);
                // Observed zero on live BTC 5-minute markets.
                assert_eq!(*fee_rate_bps, 0);
                assert!(tx_hash.as_deref().unwrap().starts_with("0x"));
            }
            other => panic!("expected Trade, got {other:?}"),
        }
    }

    #[test]
    fn decodes_a_real_snapshot_with_tick_size() {
        let mut n = Normalizer::new();
        let evs = n
            .normalize(&serde_json::from_str(BOOK).unwrap(), 1_786_844_256_400)
            .unwrap();
        match &evs[0].payload {
            EventPayload::Snapshot {
                bids,
                asks,
                tick_size,
                hash,
                ..
            } => {
                assert_eq!(bids.len(), 2);
                assert_eq!(asks.len(), 2);
                assert_eq!(bids[0].qty, Qty::parse("93541.6").unwrap());
                assert_eq!(*tick_size, Some(Price::parse("0.01").unwrap()));
                assert_eq!(
                    hash.as_deref(),
                    Some("d49fce0d4d337d7b96e7d6b9b48bf53b6b193cb9")
                );
            }
            other => panic!("expected Snapshot, got {other:?}"),
        }
    }

    #[test]
    fn sequence_numbers_are_dense_and_monotonic_across_frames() {
        let mut n = Normalizer::new();
        let mut seqs = Vec::new();
        for src in [BOOK, PRICE_CHANGE, LAST_TRADE] {
            for ev in n.normalize(&serde_json::from_str(src).unwrap(), 1).unwrap() {
                seqs.push(ev.seq);
            }
        }
        assert_eq!(seqs, vec![1, 2, 3, 4]);
    }

    #[test]
    fn unknown_event_types_surface_instead_of_vanishing() {
        let mut n = Normalizer::new();
        let err = n
            .normalize(&serde_json::json!({"event_type": "something_new"}), 1)
            .unwrap_err();
        assert_eq!(err, FrameError::Unhandled("something_new".into()));
        // A rejected frame must not consume a sequence number.
        assert_eq!(n.next_seq(), 1);
    }

    #[test]
    fn malformed_numbers_are_rejected_not_coerced() {
        let mut n = Normalizer::new();
        let bad = serde_json::json!({
            "event_type": "last_trade_price", "asset_id": "t",
            "price": "not-a-price", "size": "20", "side": "BUY", "timestamp": "1"
        });
        assert!(matches!(
            n.normalize(&bad, 1).unwrap_err(),
            FrameError::BadNumber { field: "price", .. }
        ));
    }

    #[test]
    fn splits_the_array_envelope_the_exchange_actually_sends() {
        let text = format!("[{BOOK},{LAST_TRADE}]");
        assert_eq!(split_frames(&text).unwrap().len(), 2);
        // A bare object is tolerated for forward compatibility.
        assert_eq!(split_frames(BOOK).unwrap().len(), 1);
    }
}
