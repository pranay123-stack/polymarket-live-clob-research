//! Finding live BTC 5-minute Up/Down markets.
//!
//! # Why there is no search call here
//!
//! Gamma exposes no working substring filter for event slugs — the
//! `slug_contains` parameter is accepted and then ignored, returning
//! unrelated events. Fortunately these markets do not need searching for:
//! their slugs are **deterministic**.
//!
//! ```text
//! btc-updown-5m-<unix_seconds>      unix_seconds % 300 == 0
//! ```
//!
//! The timestamp is the market's *open*; it closes exactly 300 seconds
//! later, which matches the `endDate` Gamma reports. Discovery is therefore
//! an exact-slug lookup per five-minute bucket rather than a scan, which is
//! both cheaper and immune to pagination races as markets roll over.
//!
//! Verified live: `btc-updown-5m-1786844100` is
//! *"Bitcoin Up or Down - August 15, 9:35PM-9:40PM ET"*, `endDate`
//! `2026-08-16T01:40:00Z`, outcomes `["Up", "Down"]`, tick `0.01`,
//! minimum order size `5`.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

use crate::polymarket::api::{GammaMarket, PolymarketClient};
use crate::types::{Price, Qty};

/// Length of one Up/Down round, in seconds.
pub const ROUND_SECS: i64 = 300;

/// Underlyings that run on the 5-minute Up/Down schedule.
///
/// BTC is the subject of this project; the others share the identical slug
/// and market structure and are accepted so a session can be widened without
/// touching code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Underlying {
    /// Bitcoin.
    Btc,
    /// Ether.
    Eth,
    /// Solana.
    Sol,
    /// XRP.
    Xrp,
    /// Dogecoin.
    Doge,
}

impl Underlying {
    /// The slug prefix this underlying uses.
    pub fn prefix(self) -> &'static str {
        match self {
            Underlying::Btc => "btc",
            Underlying::Eth => "eth",
            Underlying::Sol => "sol",
            Underlying::Xrp => "xrp",
            Underlying::Doge => "doge",
        }
    }

    /// Builds the event slug for the round opening at `open_ts`.
    pub fn slug_for(self, open_ts: i64) -> String {
        format!("{}-updown-5m-{}", self.prefix(), open_ts)
    }
}

/// Rounds `ts` down to the start of its 5-minute round.
pub fn align_round(ts: i64) -> i64 {
    ts - ts.rem_euclid(ROUND_SECS)
}

/// A discovered market, reduced to what the recorder and simulator need.
///
/// Serialised into every session file header so a replay carries the full
/// market context — token-to-outcome mapping, tick size, round boundaries —
/// without needing to call Gamma again for a market that has since resolved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MarketDescriptor {
    /// Event slug, e.g. `btc-updown-5m-1786844100`.
    pub slug: String,
    /// Human-readable title.
    pub title: String,
    /// The question being resolved.
    pub question: String,
    /// On-chain condition id.
    pub condition_id: String,
    /// Round open, seconds since epoch.
    pub open_ts: i64,
    /// Round close, seconds since epoch (`open_ts + 300`).
    pub close_ts: i64,
    /// Token id for the `Up` outcome.
    pub up_token: String,
    /// Token id for the `Down` outcome.
    pub down_token: String,
    /// Minimum price increment.
    pub tick_size: Price,
    /// Minimum order size.
    pub min_size: Qty,
    /// Whether the book was accepting orders at discovery time.
    pub accepting_orders: bool,
}

impl MarketDescriptor {
    /// Both token ids, `Up` first.
    pub fn tokens(&self) -> [&str; 2] {
        [&self.up_token, &self.down_token]
    }

    /// The outcome name a token corresponds to.
    pub fn outcome_of(&self, token: &str) -> Option<&'static str> {
        if token == self.up_token {
            Some("Up")
        } else if token == self.down_token {
            Some("Down")
        } else {
            None
        }
    }

    /// Builds a descriptor from Gamma metadata.
    ///
    /// The `Up`/`Down` token order is read from the market's `outcomes`
    /// field and matched positionally against `clobTokenIds` rather than
    /// assumed, because a silent swap would invert every signal downstream.
    pub fn from_gamma(slug: &str, open_ts: i64, m: &GammaMarket) -> Result<MarketDescriptor> {
        let tokens = m.token_ids()?;
        let outcomes = m.outcome_names()?;
        if tokens.len() != 2 || outcomes.len() != 2 {
            return Err(anyhow!(
                "{slug}: expected a binary market, got {} tokens and {} outcomes",
                tokens.len(),
                outcomes.len()
            ));
        }
        let idx_of = |name: &str| outcomes.iter().position(|o| o.eq_ignore_ascii_case(name));
        let up_i =
            idx_of("Up").ok_or_else(|| anyhow!("{slug}: no `Up` outcome in {outcomes:?}"))?;
        let down_i =
            idx_of("Down").ok_or_else(|| anyhow!("{slug}: no `Down` outcome in {outcomes:?}"))?;

        let tick_size = Price::parse(&m.tick_size.to_string())
            .map_err(|e| anyhow!("{slug}: tick size {}: {e}", m.tick_size))?;
        let min_size = Qty::parse(&m.min_size.to_string())
            .map_err(|e| anyhow!("{slug}: min size {}: {e}", m.min_size))?;

        Ok(MarketDescriptor {
            slug: slug.to_owned(),
            title: String::new(),
            question: m.question.clone(),
            condition_id: m.condition_id.clone(),
            open_ts,
            close_ts: open_ts + ROUND_SECS,
            up_token: tokens[up_i].clone(),
            down_token: tokens[down_i].clone(),
            tick_size,
            min_size,
            accepting_orders: m.accepting_orders,
        })
    }
}

/// Discovers consecutive 5-minute rounds starting at `from_ts`.
///
/// `from_ts` is aligned down to a round boundary. Rounds that do not exist
/// yet are skipped rather than treated as errors: the exchange creates them
/// on a rolling basis, so a lookahead that outruns creation is expected.
pub async fn discover(
    client: &PolymarketClient,
    underlying: Underlying,
    from_ts: i64,
    rounds: usize,
) -> Result<Vec<MarketDescriptor>> {
    let base = align_round(from_ts);
    let mut out = Vec::with_capacity(rounds);
    for i in 0..rounds as i64 {
        let open_ts = base + i * ROUND_SECS;
        let slug = underlying.slug_for(open_ts);
        let Some(event) = client
            .event_by_slug(&slug)
            .await
            .with_context(|| format!("discovering {slug}"))?
        else {
            continue;
        };
        let Some(m) = event.markets.first() else {
            continue;
        };
        let mut d = MarketDescriptor::from_gamma(&slug, open_ts, m)?;
        d.title = event.title.clone();
        out.push(d);
    }
    Ok(out)
}

/// Discovers the rounds covering a live recording window.
///
/// Includes the round already in progress, since a session that begins
/// mid-round still yields usable data for the remainder of it.
pub async fn discover_window(
    client: &PolymarketClient,
    underlying: Underlying,
    now_ts: i64,
    duration_secs: i64,
) -> Result<Vec<MarketDescriptor>> {
    let rounds = (duration_secs / ROUND_SECS + 2).max(1) as usize;
    discover(client, underlying, now_ts, rounds).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::polymarket::api::GammaEvent;

    const GAMMA_EVENT: &str = r#"{
      "id":"856752","slug":"btc-updown-5m-1786844100",
      "title":"Bitcoin Up or Down - August 15, 9:35PM-9:40PM ET",
      "markets":[{
        "question":"Bitcoin Up or Down?",
        "conditionId":"0x3b6b6807f535e0971af85f8e5c63399bc548d359cc02e347aaf01fe200a8a265",
        "clobTokenIds":"[\"93555969663711891451625685586929143462823436918129307927023356936446352666723\", \"94849117489514498694289722598425502085219999537404711666470172403883007236688\"]",
        "outcomes":"[\"Up\", \"Down\"]",
        "orderPriceMinTickSize":0.01,"orderMinSize":5,
        "acceptingOrders":true,"closed":false,"active":true,
        "endDate":"2026-08-16T01:40:00Z"}]}"#;

    #[test]
    fn slugs_are_deterministic_on_five_minute_boundaries() {
        assert_eq!(
            Underlying::Btc.slug_for(1_786_844_100),
            "btc-updown-5m-1786844100"
        );
        assert_eq!(align_round(1_786_844_223), 1_786_844_100);
        assert_eq!(align_round(1_786_844_100), 1_786_844_100);
        assert_eq!(align_round(1_786_844_399), 1_786_844_100);
    }

    #[test]
    fn descriptor_matches_the_live_market_we_verified() {
        let ev: GammaEvent = serde_json::from_str(GAMMA_EVENT).unwrap();
        let d =
            MarketDescriptor::from_gamma("btc-updown-5m-1786844100", 1_786_844_100, &ev.markets[0])
                .unwrap();
        // Close is exactly one round after open, matching Gamma's endDate
        // of 2026-08-16T01:40:00Z.
        assert_eq!(d.close_ts, 1_786_844_400);
        assert_eq!(d.tick_size, Price::parse("0.01").unwrap());
        assert_eq!(d.min_size, Qty::from_shares(5));
        assert!(d.up_token.starts_with("935559696637"));
        assert!(d.down_token.starts_with("948491174895"));
        assert_eq!(d.outcome_of(&d.up_token), Some("Up"));
        assert_eq!(d.outcome_of(&d.down_token), Some("Down"));
        assert_eq!(d.outcome_of("unrelated"), None);
    }

    #[test]
    fn token_order_follows_outcomes_rather_than_position() {
        // Same market with the outcome order reversed: the Up token must
        // follow the label, not the array index.
        let flipped = GAMMA_EVENT.replace(r#"[\"Up\", \"Down\"]"#, r#"[\"Down\", \"Up\"]"#);
        let ev: GammaEvent = serde_json::from_str(&flipped).unwrap();
        let d = MarketDescriptor::from_gamma("s", 1_786_844_100, &ev.markets[0]).unwrap();
        assert!(d.up_token.starts_with("948491174895"));
        assert!(d.down_token.starts_with("935559696637"));
    }

    #[test]
    fn non_binary_markets_are_rejected() {
        let bad = GAMMA_EVENT.replace(r#"[\"Up\", \"Down\"]"#, r#"[\"Up\", \"Down\", \"Flat\"]"#);
        let ev: GammaEvent = serde_json::from_str(&bad).unwrap();
        assert!(MarketDescriptor::from_gamma("s", 0, &ev.markets[0]).is_err());
    }
}
