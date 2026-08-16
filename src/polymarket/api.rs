//! Read-only HTTP clients for Polymarket's CLOB and Gamma APIs.
//!
//! # Endpoints used
//!
//! | Host | Path | Purpose |
//! |------|------|---------|
//! | `clob.polymarket.com`  | `GET /time`                  | exchange clock, for skew estimation |
//! | `clob.polymarket.com`  | `GET /book?token_id=`        | REST book snapshot |
//! | `clob.polymarket.com`  | `GET /midpoint?token_id=`    | touch midpoint |
//! | `clob.polymarket.com`  | `GET /tick-size?token_id=`   | minimum price increment |
//! | `gamma-api.polymarket.com` | `GET /events?slug=`      | market metadata and token ids |
//!
//! Every one of these is a public read. Nothing here authenticates, signs, or
//! posts; the order-placement endpoints are deliberately not implemented.
//!
//! # User-Agent
//!
//! Gamma rejects some default client agents with `403`, so this client always
//! identifies itself explicitly. `reqwest` sends no `User-Agent` unless told
//! to, which is exactly the situation that trips the filter.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

use crate::market::orderbook::Level;
use crate::polymarket::websocket::now_ms;
use crate::types::{Price, Qty};

/// Default CLOB REST host.
pub const CLOB_BASE: &str = "https://clob.polymarket.com";
/// Default Gamma metadata host.
pub const GAMMA_BASE: &str = "https://gamma-api.polymarket.com";
/// Identifies this research tool to Polymarket's edge.
pub const USER_AGENT: &str = concat!(
    "polymarket-live-clob-research/",
    env!("CARGO_PKG_VERSION"),
    " (read-only market-data research)"
);

/// Read-only client for Polymarket's public HTTP surfaces.
#[derive(Debug, Clone)]
pub struct PolymarketClient {
    http: reqwest::Client,
    clob_base: String,
    gamma_base: String,
}

impl PolymarketClient {
    /// Builds a client against the production hosts.
    pub fn new() -> Result<PolymarketClient> {
        PolymarketClient::with_bases(CLOB_BASE, GAMMA_BASE)
    }

    /// Builds a client against explicit hosts, for tests and mirrors.
    pub fn with_bases(clob_base: &str, gamma_base: &str) -> Result<PolymarketClient> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(20))
            .build()
            .context("building HTTP client")?;
        Ok(PolymarketClient {
            http,
            clob_base: clob_base.trim_end_matches('/').to_owned(),
            gamma_base: gamma_base.trim_end_matches('/').to_owned(),
        })
    }

    /// Exchange clock, in seconds since epoch.
    ///
    /// Sampled at session start so recorded receive timestamps can be
    /// interpreted against the exchange's own clock rather than assuming the
    /// local machine is disciplined.
    pub async fn server_time(&self) -> Result<i64> {
        let url = format!("{}/time", self.clob_base);
        let body = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()?
            .text()
            .await?;
        body.trim()
            .trim_matches('"')
            .parse::<i64>()
            .with_context(|| format!("parsing server time from {body:?}"))
    }

    /// Fetches a REST book snapshot for one token.
    pub async fn book(&self, token_id: &str) -> Result<RestBook> {
        let url = format!("{}/book", self.clob_base);
        let raw: RawBook = self
            .http
            .get(&url)
            .query(&[("token_id", token_id)])
            .send()
            .await
            .with_context(|| format!("GET {url}?token_id={token_id}"))?
            .error_for_status()?
            .json()
            .await
            .context("decoding book response")?;
        raw.try_into()
    }

    /// Fetches the touch midpoint for one token.
    pub async fn midpoint(&self, token_id: &str) -> Result<Price> {
        #[derive(Deserialize)]
        struct Mid {
            mid: String,
        }
        let url = format!("{}/midpoint", self.clob_base);
        let m: Mid = self
            .http
            .get(&url)
            .query(&[("token_id", token_id)])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        // The midpoint sits between ticks when the spread is odd, so it can
        // carry finer resolution than the tick grid; that is representable.
        Price::parse(&m.mid).map_err(|e| anyhow!("midpoint {:?}: {e}", m.mid))
    }

    /// Fetches the minimum price increment for one token.
    pub async fn tick_size(&self, token_id: &str) -> Result<Price> {
        #[derive(Deserialize)]
        struct Tick {
            minimum_tick_size: f64,
        }
        let url = format!("{}/tick-size", self.clob_base);
        let t: Tick = self
            .http
            .get(&url)
            .query(&[("token_id", token_id)])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Price::parse(&t.minimum_tick_size.to_string())
            .map_err(|e| anyhow!("tick size {}: {e}", t.minimum_tick_size))
    }

    /// Estimates the offset between the local clock and the exchange's.
    ///
    /// Uses Cristian's algorithm against `GET /book`, whose `timestamp` field
    /// has millisecond resolution — `GET /time` only reports whole seconds,
    /// which is far too coarse to interpret a ~200 ms feed delay.
    ///
    /// For each sample, `offset = server - (t0 + t2) / 2` with uncertainty
    /// `±rtt/2`. The sample with the lowest round trip is returned, since a
    /// fast round trip bounds the error most tightly.
    ///
    /// This matters because [`crate::market::event::MarketEvent::feed_delay_ms`]
    /// measures `recv - exchange`, which conflates true feed delay with clock
    /// offset. Reporting the offset alongside lets a reader tell them apart.
    pub async fn probe_clock(&self, token_id: &str, samples: usize) -> Result<ClockSample> {
        let mut best: Option<ClockSample> = None;
        for _ in 0..samples.max(1) {
            let t0 = now_ms();
            let book = self.book(token_id).await?;
            let t2 = now_ms();
            if book.timestamp_ms == 0 {
                continue;
            }
            let s = ClockSample {
                local_ms: (t0 + t2) / 2,
                server_ms: book.timestamp_ms,
                rtt_ms: t2 - t0,
                offset_ms: book.timestamp_ms - (t0 + t2) / 2,
            };
            if best.as_ref().is_none_or(|b| s.rtt_ms < b.rtt_ms) {
                best = Some(s);
            }
        }
        best.ok_or_else(|| anyhow!("no usable clock sample from {token_id}"))
    }

    /// Looks up a Gamma event by its exact slug.
    ///
    /// Returns `None` when the slug does not exist, which is the normal
    /// answer for a 5-minute market that has not been created yet.
    pub async fn event_by_slug(&self, slug: &str) -> Result<Option<GammaEvent>> {
        let url = format!("{}/events", self.gamma_base);
        let events: Vec<GammaEvent> = self
            .http
            .get(&url)
            .query(&[("slug", slug)])
            .send()
            .await
            .with_context(|| format!("GET {url}?slug={slug}"))?
            .error_for_status()?
            .json()
            .await
            .with_context(|| format!("decoding Gamma event {slug}"))?;
        Ok(events.into_iter().next())
    }
}

/// One clock-offset measurement against the exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ClockSample {
    /// Local clock at the midpoint of the round trip, ms since epoch.
    pub local_ms: i64,
    /// Exchange clock as reported in the response, ms since epoch.
    pub server_ms: i64,
    /// Round-trip time of the probe, in ms.
    pub rtt_ms: i64,
    /// `server_ms - local_ms`; positive means the exchange clock reads ahead.
    ///
    /// Trustworthy only to within `±rtt_ms / 2`.
    pub offset_ms: i64,
}

impl ClockSample {
    /// Half the round trip: the uncertainty bound on [`Self::offset_ms`].
    pub fn uncertainty_ms(&self) -> i64 {
        self.rtt_ms / 2
    }

    /// Whether the measured offset is distinguishable from zero.
    ///
    /// When it is not, an observed feed delay can be read as genuine
    /// transport latency rather than a mis-set local clock.
    pub fn offset_is_significant(&self) -> bool {
        self.offset_ms.abs() > self.uncertainty_ms()
    }
}

/// A book snapshot as returned by `GET /book`.
#[derive(Debug, Clone)]
pub struct RestBook {
    /// On-chain condition id of the parent market.
    pub market: String,
    /// Token the book belongs to.
    pub asset_id: String,
    /// Exchange timestamp in milliseconds.
    pub timestamp_ms: i64,
    /// The exchange's book hash.
    pub hash: Option<String>,
    /// Resting bids.
    pub bids: Vec<Level>,
    /// Resting asks.
    pub asks: Vec<Level>,
}

#[derive(Debug, Deserialize)]
struct RawBook {
    #[serde(default)]
    market: String,
    #[serde(default)]
    asset_id: String,
    #[serde(default)]
    timestamp: String,
    #[serde(default)]
    hash: Option<String>,
    #[serde(default)]
    bids: Vec<RawLevel>,
    #[serde(default)]
    asks: Vec<RawLevel>,
}

#[derive(Debug, Deserialize)]
struct RawLevel {
    price: String,
    size: String,
}

impl TryFrom<RawBook> for RestBook {
    type Error = anyhow::Error;

    fn try_from(r: RawBook) -> Result<RestBook> {
        let conv = |ls: Vec<RawLevel>| -> Result<Vec<Level>> {
            ls.into_iter()
                .map(|l| {
                    Ok(Level {
                        price: Price::parse(&l.price)
                            .map_err(|e| anyhow!("book price {:?}: {e}", l.price))?,
                        qty: Qty::parse(&l.size)
                            .map_err(|e| anyhow!("book size {:?}: {e}", l.size))?,
                    })
                })
                .collect()
        };
        Ok(RestBook {
            timestamp_ms: r.timestamp.parse().unwrap_or(0),
            market: r.market,
            asset_id: r.asset_id,
            hash: r.hash,
            bids: conv(r.bids)?,
            asks: conv(r.asks)?,
        })
    }
}

/// A Gamma event: the container that holds one or more tradable markets.
#[derive(Debug, Clone, Deserialize)]
pub struct GammaEvent {
    /// Gamma's numeric event id, as a string.
    #[serde(default)]
    pub id: String,
    /// URL slug, e.g. `btc-updown-5m-1786844100`.
    #[serde(default)]
    pub slug: String,
    /// Human-readable title.
    #[serde(default)]
    pub title: String,
    /// Markets belonging to this event.
    #[serde(default)]
    pub markets: Vec<GammaMarket>,
}

/// A single tradable market inside a [`GammaEvent`].
#[derive(Debug, Clone, Deserialize)]
pub struct GammaMarket {
    /// Gamma's numeric market id, as a string.
    #[serde(default)]
    pub id: String,
    /// Market slug.
    #[serde(default)]
    pub slug: String,
    /// The question being resolved.
    #[serde(default)]
    pub question: String,
    /// On-chain condition id.
    #[serde(rename = "conditionId", default)]
    pub condition_id: String,
    /// JSON-encoded array of the two CLOB token ids.
    ///
    /// Gamma double-encodes this: the field is a *string* containing JSON.
    #[serde(rename = "clobTokenIds", default)]
    pub clob_token_ids: String,
    /// JSON-encoded array of outcome names, e.g. `["Up", "Down"]`.
    #[serde(default)]
    pub outcomes: String,
    /// Minimum price increment the exchange accepts.
    #[serde(rename = "orderPriceMinTickSize", default)]
    pub tick_size: f64,
    /// Minimum order size in shares.
    #[serde(rename = "orderMinSize", default)]
    pub min_size: f64,
    /// Whether the book is currently accepting orders.
    #[serde(rename = "acceptingOrders", default)]
    pub accepting_orders: bool,
    /// Whether the market has resolved.
    #[serde(default)]
    pub closed: bool,
    /// Whether the market is active.
    #[serde(default)]
    pub active: bool,
    /// ISO-8601 scheduled end.
    #[serde(rename = "endDate", default)]
    pub end_date: Option<String>,
}

impl GammaMarket {
    /// Decodes the double-encoded `clobTokenIds` field.
    pub fn token_ids(&self) -> Result<Vec<String>> {
        serde_json::from_str(&self.clob_token_ids)
            .with_context(|| format!("decoding clobTokenIds {:?}", self.clob_token_ids))
    }

    /// Decodes the double-encoded `outcomes` field.
    pub fn outcome_names(&self) -> Result<Vec<String>> {
        serde_json::from_str(&self.outcomes)
            .with_context(|| format!("decoding outcomes {:?}", self.outcomes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Captured verbatim from GET https://clob.polymarket.com/book.
    const REST_BOOK: &str = r#"{
      "market":"0x3b6b6807f535e0971af85f8e5c63399bc548d359cc02e347aaf01fe200a8a265",
      "asset_id":"104830988669153084352108937274802865572954927866102589054231572742216390377561",
      "timestamp":"1786844232261","hash":"242019da4e003779e83d2f1ff3de6da4a3585e16",
      "bids":[{"price":"0.01","size":"94104.93"},{"price":"0.02","size":"17928"}],
      "asks":[{"price":"0.51","size":"80"}]}"#;

    #[test]
    fn decodes_a_real_rest_book() {
        let raw: RawBook = serde_json::from_str(REST_BOOK).unwrap();
        let book: RestBook = raw.try_into().unwrap();
        assert_eq!(book.timestamp_ms, 1_786_844_232_261);
        assert_eq!(book.bids.len(), 2);
        assert_eq!(book.bids[0].qty, Qty::parse("94104.93").unwrap());
        assert_eq!(book.asks[0].price, Price::parse("0.51").unwrap());
        assert_eq!(
            book.hash.as_deref(),
            Some("242019da4e003779e83d2f1ff3de6da4a3585e16")
        );
    }

    // Captured verbatim from GET https://gamma-api.polymarket.com/events?slug=...
    const GAMMA_EVENT: &str = r#"{
      "id":"856752","slug":"btc-updown-5m-1786844100",
      "title":"Bitcoin Up or Down - August 15, 9:35PM-9:40PM ET",
      "markets":[{
        "id":"599123","slug":"btc-updown-5m-1786844100",
        "question":"Bitcoin Up or Down?",
        "conditionId":"0x3b6b6807f535e0971af85f8e5c63399bc548d359cc02e347aaf01fe200a8a265",
        "clobTokenIds":"[\"93555969663711891451625685586929143462823436918129307927023356936446352666723\", \"94849117489514498694289722598425502085219999537404711666470172403883007236688\"]",
        "outcomes":"[\"Up\", \"Down\"]",
        "orderPriceMinTickSize":0.01,"orderMinSize":5,
        "acceptingOrders":true,"closed":false,"active":true,
        "endDate":"2026-08-16T01:40:00Z"}]}"#;

    #[test]
    fn decodes_a_real_gamma_event_and_its_double_encoded_fields() {
        let ev: GammaEvent = serde_json::from_str(GAMMA_EVENT).unwrap();
        assert_eq!(ev.slug, "btc-updown-5m-1786844100");
        let m = &ev.markets[0];
        assert!(m.accepting_orders && !m.closed);
        assert_eq!(m.tick_size, 0.01);
        assert_eq!(m.min_size, 5.0);

        let toks = m.token_ids().unwrap();
        assert_eq!(toks.len(), 2);
        assert!(toks[0].starts_with("935559696637"));
        assert_eq!(m.outcome_names().unwrap(), vec!["Up", "Down"]);
    }

    #[test]
    fn unknown_gamma_fields_do_not_break_decoding() {
        // Gamma returns ~80 market fields; the client must survive additions.
        let ev: GammaEvent =
            serde_json::from_str(r#"{"slug":"x","brandNewField":123,"markets":[]}"#).unwrap();
        assert_eq!(ev.slug, "x");
        assert!(ev.markets.is_empty());
    }
}
