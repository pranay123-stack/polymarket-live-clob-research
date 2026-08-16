//! Live capture of real Polymarket market data to replayable session files.
//!
//! # Rolling markets
//!
//! A BTC Up/Down round lives for five minutes. Any recording longer than
//! that spans markets that did not exist when it started, so the recorder
//! re-discovers the upcoming window periodically and restarts the feed
//! whenever the token set changes. Because the exchange republishes a full
//! book snapshot per token every ~1.5 seconds, the brief gap across a
//! resubscribe costs a snapshot, not book integrity.
//!
//! # What is written
//!
//! Exchange frames go to disk verbatim (see [`storage`]). The only records
//! the recorder synthesises are market-open and market-close lifecycle
//! markers, which the market channel does not publish; those are derived
//! from real Gamma metadata and are tagged distinctly in the file so a
//! reader can always tell exchange data from recorder annotation.

pub mod storage;

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::analytics::metrics::{LatencyHistogram, LatencySummary};
use crate::market::event::EventPayload;
use crate::polymarket::api::{ClockSample, PolymarketClient};
use crate::polymarket::market_discovery::{self, MarketDescriptor, Underlying};
use crate::polymarket::websocket::{self, now_ms, FeedConfig, FeedMessage, MARKET_WS_URL};
use crate::recorder::storage::{session_path, SessionHeader, SessionWriter, FORMAT_VERSION};

/// How a recording session should run.
#[derive(Debug, Clone)]
pub struct RecorderConfig {
    /// Underlying to follow.
    pub underlying: Underlying,
    /// How long to record.
    pub duration: Duration,
    /// Directory to write the session file into.
    pub out_dir: PathBuf,
    /// How many future rounds to subscribe to ahead of the current one.
    ///
    /// Subscribing early captures a market's opening book, which is where
    /// the widest spreads and thinnest liquidity of the round appear.
    pub lookahead_rounds: usize,
    /// How often to re-discover markets.
    pub refresh: Duration,
    /// WebSocket endpoint.
    pub ws_url: String,
}

impl Default for RecorderConfig {
    fn default() -> RecorderConfig {
        RecorderConfig {
            underlying: Underlying::Btc,
            duration: Duration::from_secs(3_600),
            out_dir: PathBuf::from("data"),
            lookahead_rounds: 3,
            refresh: Duration::from_secs(60),
            ws_url: MARKET_WS_URL.to_owned(),
        }
    }
}

/// Outcome of a completed recording.
#[derive(Debug, Clone)]
pub struct RecordSummary {
    /// File written.
    pub path: PathBuf,
    /// Exchange frames written.
    pub frames: u64,
    /// Lifecycle markers written.
    pub lifecycle: u64,
    /// Distinct markets followed.
    pub markets: usize,
    /// Wall-clock duration of the recording, ms.
    pub elapsed_ms: i64,
    /// Bytes written.
    pub bytes: u64,
    /// Feed reconnections during the session.
    pub reconnects: u32,
    /// Distribution of `recv - exchange` across all stamped frames.
    pub feed_delay: LatencySummary,
    /// Clock offset measured at session start.
    pub clock: Option<ClockSample>,
}

/// Records a live session to disk.
///
/// Stops when `duration` elapses or `shutdown` flips, whichever is first.
pub async fn record(
    client: &PolymarketClient,
    cfg: RecorderConfig,
    mut shutdown: watch::Receiver<bool>,
) -> Result<RecordSummary> {
    let started_ms = now_ms();
    let deadline = tokio::time::Instant::now() + cfg.duration;

    // Discover an initial market set so the header has real context and the
    // clock probe has a real token to measure against.
    let now_s = client.server_time().await.unwrap_or(started_ms / 1000);
    let mut markets = market_discovery::discover(
        client,
        cfg.underlying,
        now_s,
        cfg.lookahead_rounds.max(1) + 1,
    )
    .await
    .context("initial market discovery")?;
    if markets.is_empty() {
        anyhow::bail!(
            "no live {} 5-minute markets found around {now_s}",
            cfg.underlying.prefix()
        );
    }

    let clock = match client.probe_clock(&markets[0].up_token, 5).await {
        Ok(c) => Some(c),
        Err(e) => {
            warn!(error = %e, "clock probe failed; feed delay will not be offset-corrected");
            None
        }
    };

    let path = session_path(&cfg.out_dir, cfg.underlying, started_ms);
    let header = SessionHeader {
        v: FORMAT_VERSION,
        kind: "header".into(),
        tool: format!("{}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
        started_ms,
        clock,
        underlying: cfg.underlying,
        source_url: cfg.ws_url.clone(),
        markets: markets.clone(),
    };
    let mut writer = SessionWriter::create(&path, &header)?;

    let mut seen_markets: HashSet<String> = HashSet::new();
    let mut lifecycle_written = 0u64;
    for m in &markets {
        write_open(&mut writer, m)?;
        lifecycle_written += 1;
        seen_markets.insert(m.slug.clone());
    }

    let mut delay = LatencyHistogram::new();
    let mut frames = 0u64;
    let mut reconnects = 0u32;
    let mut closed: HashSet<String> = HashSet::new();

    println!("POLYMARKET LIVE RECORDER\n");
    println!(
        "Market:\n{} 5 Minute Up/Down\n",
        cfg.underlying.prefix().to_uppercase()
    );
    println!("Recording:\n{} seconds\n", cfg.duration.as_secs());
    println!(
        "Following {} market(s), {} token(s)",
        markets.len(),
        markets.len() * 2
    );
    println!("Output:\n{}\n", path.display());

    'outer: loop {
        let tokens: Vec<String> = markets
            .iter()
            .flat_map(|m| [m.up_token.clone(), m.down_token.clone()])
            .collect();
        let token_set: HashSet<String> = tokens.iter().cloned().collect();

        let (feed_shutdown_tx, feed_shutdown_rx) = watch::channel(false);
        let (mut rx, handle) = websocket::spawn(
            FeedConfig {
                url: cfg.ws_url.clone(),
                tokens,
            },
            feed_shutdown_rx,
        );

        let mut refresh = tokio::time::interval(cfg.refresh);
        refresh.tick().await; // consume the immediate first tick

        let restart = loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {
                    let _ = feed_shutdown_tx.send(true);
                    drop(rx);
                    let _ = handle.await;
                    break 'outer;
                }

                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        println!("\nstopping on request…");
                        let _ = feed_shutdown_tx.send(true);
                        drop(rx);
                        let _ = handle.await;
                        break 'outer;
                    }
                }

                _ = refresh.tick() => {
                    let now_s = now_ms() / 1000;
                    match market_discovery::discover(
                        client, cfg.underlying, now_s, cfg.lookahead_rounds.max(1) + 1,
                    ).await {
                        Ok(found) if !found.is_empty() => {
                            // Emit close markers for rounds that have ended.
                            for m in &markets {
                                if now_s >= m.close_ts && closed.insert(m.slug.clone()) {
                                    write_close(&mut writer, m)?;
                                    lifecycle_written += 1;
                                }
                            }
                            let new_set: HashSet<String> = found
                                .iter()
                                .flat_map(|m| [m.up_token.clone(), m.down_token.clone()])
                                .collect();
                            if new_set != token_set {
                                for m in &found {
                                    if seen_markets.insert(m.slug.clone()) {
                                        write_open(&mut writer, m)?;
                                        lifecycle_written += 1;
                                    }
                                }
                                markets = found;
                                break true;
                            }
                        }
                        Ok(_) => {}
                        Err(e) => warn!(error = %e, "market refresh failed; keeping current set"),
                    }
                    writer.flush()?;
                    let secs = (now_ms() - started_ms) / 1000;
                    println!(
                        "  t+{secs:>5}s   frames {:>9}   markets {:>3}   feed delay p50 {} ms",
                        frames,
                        seen_markets.len(),
                        delay.percentile(50.0).map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
                    );
                }

                msg = rx.recv() => {
                    let Some(msg) = msg else { break true };
                    match msg {
                        FeedMessage::Frame { recv_ms, raw } => {
                            if let Some(ex) = raw.get("timestamp").and_then(as_ms) {
                                delay.record(recv_ms - ex);
                            }
                            writer.write_frame(recv_ms, &raw)?;
                            frames += 1;
                        }
                        FeedMessage::Connected { attempt } => {
                            if attempt > 1 { reconnects += 1; }
                            info!(attempt, "feed connected");
                        }
                        FeedMessage::Disconnected { reason } => {
                            warn!(%reason, "feed disconnected");
                        }
                    }
                }
            }
        };

        let _ = feed_shutdown_tx.send(true);
        drop(rx);
        let _ = handle.await;
        if !restart {
            break;
        }
        info!("resubscribing with a refreshed market set");
    }

    writer.flush()?;
    let summary = RecordSummary {
        path: writer.path().to_path_buf(),
        frames,
        lifecycle: lifecycle_written,
        markets: seen_markets.len(),
        elapsed_ms: now_ms() - started_ms,
        bytes: writer.bytes(),
        reconnects,
        feed_delay: delay.summary(),
        clock,
    };
    Ok(summary)
}

/// Writes a market-open lifecycle marker.
fn write_open(w: &mut SessionWriter, m: &MarketDescriptor) -> Result<()> {
    w.write_lifecycle(
        now_ms(),
        &EventPayload::MarketOpen {
            slug: m.slug.clone(),
            condition_id: m.condition_id.clone(),
            up_token: m.up_token.clone(),
            down_token: m.down_token.clone(),
            close_ms: m.close_ts * 1_000,
        },
    )?;
    Ok(())
}

/// Writes a market-close lifecycle marker.
fn write_close(w: &mut SessionWriter, m: &MarketDescriptor) -> Result<()> {
    w.write_lifecycle(
        now_ms(),
        &EventPayload::MarketClose {
            slug: m.slug.clone(),
            condition_id: m.condition_id.clone(),
        },
    )?;
    Ok(())
}

/// Reads a millisecond timestamp encoded as either a string or a number.
fn as_ms(v: &serde_json::Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

/// Wires `Ctrl-C` to a shutdown channel.
pub fn shutdown_signal() -> watch::Receiver<bool> {
    let (tx, rx) = watch::channel(false);
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = tx.send(true);
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_decode_from_both_wire_encodings() {
        assert_eq!(
            as_ms(&serde_json::json!("1786844302144")),
            Some(1_786_844_302_144)
        );
        assert_eq!(
            as_ms(&serde_json::json!(1786844302144i64)),
            Some(1_786_844_302_144)
        );
        assert_eq!(as_ms(&serde_json::json!("nope")), None);
    }

    #[test]
    fn default_config_records_an_hour_of_btc() {
        let c = RecorderConfig::default();
        assert_eq!(c.underlying, Underlying::Btc);
        assert_eq!(c.duration.as_secs(), 3_600);
        assert!(
            c.lookahead_rounds >= 1,
            "must subscribe ahead to catch market opens"
        );
    }
}
