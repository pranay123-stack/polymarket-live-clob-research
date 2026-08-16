//! Live collector for Polymarket's public `market` WebSocket channel.
//!
//! # Protocol
//!
//! Connect to `wss://ws-subscriptions-clob.polymarket.com/ws/market`, then
//! send a single subscription naming the tokens to follow:
//!
//! ```json
//! {"assets_ids": ["<token id>", ...], "type": "market"}
//! ```
//!
//! The server then streams JSON **arrays** of events. Text `PING` is
//! answered with text `PONG`; that application-level heartbeat is separate
//! from RFC 6455 ping/pong frames, and both are handled.
//!
//! # Resynchronisation
//!
//! The server republishes a full `book` snapshot per token roughly every
//! 1.5 seconds — measured at 1994 snapshots across 10 tokens in a 300-second
//! session. Recovery after a reconnect is therefore automatic: the book
//! re-baselines on the next snapshot without any explicit resync request.
//! [`crate::market::state::MarketState`] discards deltas for a token until
//! its first snapshot arrives, so the gap is skipped rather than mis-applied.
//!
//! This module is receive-only. There is no code path that sends anything
//! but the subscription and heartbeats.

use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

/// Default public market-channel endpoint.
pub const MARKET_WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";

/// Interval between application-level `PING` messages.
const HEARTBEAT: Duration = Duration::from_secs(10);
/// Longest gap tolerated with no inbound traffic before forcing a reconnect.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// First reconnect backoff step.
const BACKOFF_MIN: Duration = Duration::from_millis(500);
/// Longest reconnect backoff.
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// What the collector reports to its consumer.
#[derive(Debug, Clone)]
pub enum FeedMessage {
    /// The subscription was accepted on a fresh connection.
    Connected {
        /// 1 for the initial connection, incrementing per reconnect.
        attempt: u32,
    },
    /// One decoded frame object, with the local receive time stamped on it.
    Frame {
        /// Local receive time, ms since epoch.
        recv_ms: i64,
        /// The raw frame, exactly as sent.
        raw: Value,
    },
    /// The connection dropped; the collector will retry unless shutting down.
    Disconnected {
        /// Why the connection ended.
        reason: String,
    },
}

/// Configuration for a feed session.
#[derive(Debug, Clone)]
pub struct FeedConfig {
    /// WebSocket endpoint.
    pub url: String,
    /// Token ids to subscribe to.
    pub tokens: Vec<String>,
}

impl FeedConfig {
    /// Subscribes to `tokens` on the production endpoint.
    pub fn new(tokens: Vec<String>) -> FeedConfig {
        FeedConfig {
            url: MARKET_WS_URL.to_owned(),
            tokens,
        }
    }
}

/// Local wall-clock time in milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Runs the collector until `shutdown` flips to `true`.
///
/// Reconnects with exponential backoff on any transport failure. The task
/// ends cleanly — rather than propagating an error — when the consumer drops
/// its receiver, so a bounded recording can stop by dropping the channel.
pub async fn run(
    cfg: FeedConfig,
    tx: mpsc::Sender<FeedMessage>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let mut attempt: u32 = 0;
    let mut backoff = BACKOFF_MIN;

    loop {
        if *shutdown.borrow() {
            return Ok(());
        }
        attempt += 1;

        let connect = tokio::select! {
            r = tokio_tungstenite::connect_async(&cfg.url) => r,
            _ = shutdown.changed() => return Ok(()),
        };

        let stream = match connect {
            Ok((s, _resp)) => s,
            Err(e) => {
                warn!(attempt, error = %e, "connect failed; backing off");
                if tx
                    .send(FeedMessage::Disconnected {
                        reason: format!("connect: {e}"),
                    })
                    .await
                    .is_err()
                {
                    return Ok(());
                }
                if !sleep_or_shutdown(backoff, &mut shutdown).await {
                    return Ok(());
                }
                backoff = (backoff * 2).min(BACKOFF_MAX);
                continue;
            }
        };

        info!(attempt, tokens = cfg.tokens.len(), "market feed connected");
        match session(stream, &cfg, &tx, attempt, &mut shutdown).await {
            Ok(SessionEnd::Shutdown) => return Ok(()),
            Ok(SessionEnd::ConsumerGone) => return Ok(()),
            Ok(SessionEnd::Dropped(reason)) | Err(reason) => {
                warn!(attempt, %reason, "market feed dropped; reconnecting");
                if tx.send(FeedMessage::Disconnected { reason }).await.is_err() {
                    return Ok(());
                }
                if !sleep_or_shutdown(backoff, &mut shutdown).await {
                    return Ok(());
                }
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        }
    }
}

/// How a single connection ended.
enum SessionEnd {
    /// Shutdown was requested.
    Shutdown,
    /// The consumer dropped the receiving end.
    ConsumerGone,
    /// The transport failed and should be retried.
    Dropped(String),
}

/// Drives one connected session: subscribe, then pump frames.
async fn session<S>(
    mut stream: S,
    cfg: &FeedConfig,
    tx: &mpsc::Sender<FeedMessage>,
    attempt: u32,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<SessionEnd, String>
where
    S: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error>
        + StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    let sub = serde_json::json!({ "assets_ids": cfg.tokens, "type": "market" }).to_string();
    stream
        .send(Message::Text(sub))
        .await
        .map_err(|e| format!("subscribe: {e}"))?;

    if tx.send(FeedMessage::Connected { attempt }).await.is_err() {
        return Ok(SessionEnd::ConsumerGone);
    }

    let mut beat = tokio::time::interval(HEARTBEAT);
    beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick fires immediately; the subscription just went out, so
    // skip it rather than stacking a redundant heartbeat behind it.
    beat.tick().await;

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    let _ = stream.send(Message::Close(None)).await;
                    return Ok(SessionEnd::Shutdown);
                }
            }

            _ = beat.tick() => {
                if let Err(e) = stream.send(Message::Text("PING".to_owned())).await {
                    return Ok(SessionEnd::Dropped(format!("heartbeat: {e}")));
                }
            }

            item = tokio::time::timeout(IDLE_TIMEOUT, stream.next()) => {
                let recv_ms = now_ms();
                let msg = match item {
                    Err(_) => return Ok(SessionEnd::Dropped("idle timeout".into())),
                    Ok(None) => return Ok(SessionEnd::Dropped("stream closed".into())),
                    Ok(Some(Err(e))) => return Ok(SessionEnd::Dropped(format!("transport: {e}"))),
                    Ok(Some(Ok(m))) => m,
                };

                match msg {
                    Message::Text(text) => {
                        // The server answers our heartbeat with a bare PONG,
                        // which is not JSON and carries no market data.
                        if text.trim() == "PONG" {
                            debug!("heartbeat acknowledged");
                            continue;
                        }
                        match crate::polymarket::parser::split_frames(&text) {
                            Ok(frames) => {
                                for raw in frames {
                                    if tx.send(FeedMessage::Frame { recv_ms, raw }).await.is_err() {
                                        return Ok(SessionEnd::ConsumerGone);
                                    }
                                }
                            }
                            Err(e) => warn!(error = %e, "undecodable frame; skipped"),
                        }
                    }
                    Message::Ping(p) => {
                        if let Err(e) = stream.send(Message::Pong(p)).await {
                            return Ok(SessionEnd::Dropped(format!("pong: {e}")));
                        }
                    }
                    Message::Close(c) => {
                        return Ok(SessionEnd::Dropped(format!("server close: {c:?}")));
                    }
                    Message::Pong(_) | Message::Binary(_) | Message::Frame(_) => {}
                }
            }
        }
    }
}

/// Sleeps for `d`, returning `false` if shutdown was requested first.
async fn sleep_or_shutdown(d: Duration, shutdown: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(d) => true,
        _ = shutdown.changed() => !*shutdown.borrow(),
    }
}

/// Opens a channel and spawns the collector.
///
/// Returns the receiving end and a handle that resolves when the collector
/// stops. The queue is generously sized: a busy multi-market session was
/// measured at ~310 events/second, and a slow consumer must not be able to
/// silently drop market data.
pub fn spawn(
    cfg: FeedConfig,
    shutdown: watch::Receiver<bool>,
) -> (
    mpsc::Receiver<FeedMessage>,
    tokio::task::JoinHandle<Result<()>>,
) {
    let (tx, rx) = mpsc::channel(16_384);
    let handle = tokio::spawn(async move { run(cfg, tx, shutdown).await.context("market feed") });
    (rx, handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscription_matches_the_documented_wire_format() {
        let cfg = FeedConfig::new(vec!["111".into(), "222".into()]);
        let sub = serde_json::json!({ "assets_ids": cfg.tokens, "type": "market" });
        assert_eq!(
            serde_json::to_string(&sub).unwrap(),
            r#"{"assets_ids":["111","222"],"type":"market"}"#
        );
    }

    #[test]
    fn now_ms_is_a_plausible_epoch_millisecond_stamp() {
        // Sanity guard against a seconds/millis mix-up in timestamping.
        let t = now_ms();
        assert!(
            t > 1_700_000_000_000,
            "looks like seconds, not milliseconds"
        );
    }

    #[test]
    fn pong_is_not_json_and_must_be_filtered_before_parsing() {
        assert!(crate::polymarket::parser::split_frames("PONG").is_err());
    }
}
