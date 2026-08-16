//! Session file format: lossless capture and deterministic playback.
//!
//! # Format
//!
//! A session is newline-delimited JSON. The first line is a
//! [`SessionHeader`]; every subsequent line is a [`SessionRecord`].
//!
//! ```text
//! {"v":1,"kind":"header","tool":"...","started_ms":...,"markets":[...]}
//! {"f":1,"recv_ms":1786844302150,"raw":{"event_type":"book",...}}
//! {"f":2,"recv_ms":1786844302151,"raw":{"event_type":"price_change",...}}
//! ```
//!
//! # Why raw frames rather than normalized events
//!
//! Records store the exchange frame **verbatim**, alongside only the two
//! things the exchange cannot supply: a recorder frame index and a local
//! receive timestamp. Normalization happens on read, through the very same
//! [`Normalizer`] the live path uses.
//!
//! That choice buys two properties worth more than the disk space:
//!
//! * **No information loss.** Fields this crate does not model yet — and
//!   fields the exchange adds later — survive in the file. A session
//!   recorded today stays useful after the decoder learns more.
//! * **Replay fidelity by construction.** Live and replay cannot drift,
//!   because there is exactly one decoder and it runs in both paths. If
//!   normalization changes, old sessions re-normalize under the new rules
//!   instead of being frozen at whatever the recorder believed at the time.
//!
//! Sequence numbers are therefore *not* stored. They are re-derived on read
//! and are reproducible precisely because the decoder is deterministic.

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::market::event::{EventPayload, MarketEvent};
use crate::polymarket::api::ClockSample;
use crate::polymarket::market_discovery::{MarketDescriptor, Underlying};
use crate::polymarket::parser::{FrameError, Normalizer};

/// Current session format version.
pub const FORMAT_VERSION: u32 = 1;

/// First line of a session file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHeader {
    /// Format version.
    pub v: u32,
    /// Always `"header"`, so the first line is self-identifying.
    pub kind: String,
    /// Tool and version that produced the file.
    pub tool: String,
    /// Local wall-clock time at session start, ms since epoch.
    pub started_ms: i64,
    /// Clock offset measurement taken at session start, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock: Option<ClockSample>,
    /// Underlying followed.
    pub underlying: Underlying,
    /// WebSocket endpoint the data came from.
    pub source_url: String,
    /// Full metadata for every market followed.
    pub markets: Vec<MarketDescriptor>,
}

impl SessionHeader {
    /// Every token id across all followed markets, `Up` then `Down` per market.
    pub fn tokens(&self) -> Vec<String> {
        self.markets
            .iter()
            .flat_map(|m| [m.up_token.clone(), m.down_token.clone()])
            .collect()
    }

    /// The market a token belongs to.
    pub fn market_of(&self, token: &str) -> Option<&MarketDescriptor> {
        self.markets
            .iter()
            .find(|m| m.up_token == token || m.down_token == token)
    }
}

/// One line of a session body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SessionRecord {
    /// A verbatim exchange frame.
    Frame {
        /// Recorder frame index, from 1.
        f: u64,
        /// Local receive time, ms since epoch.
        recv_ms: i64,
        /// The frame exactly as the exchange sent it.
        raw: Value,
    },
    /// A recorder-synthesised lifecycle marker.
    ///
    /// The market channel publishes no open/close messages, so these are
    /// derived from real Gamma metadata and interleaved in receive order.
    Lifecycle {
        /// Recorder frame index, from 1.
        f: u64,
        /// Local time the marker was emitted, ms since epoch.
        recv_ms: i64,
        /// The lifecycle payload.
        lifecycle: EventPayload,
    },
}

impl SessionRecord {
    /// Recorder frame index.
    pub fn index(&self) -> u64 {
        match self {
            SessionRecord::Frame { f, .. } | SessionRecord::Lifecycle { f, .. } => *f,
        }
    }

    /// Local receive time.
    pub fn recv_ms(&self) -> i64 {
        match self {
            SessionRecord::Frame { recv_ms, .. } | SessionRecord::Lifecycle { recv_ms, .. } => {
                *recv_ms
            }
        }
    }
}

/// Buffered writer for a session file.
pub struct SessionWriter {
    out: BufWriter<File>,
    path: PathBuf,
    next_index: u64,
    bytes: u64,
}

impl SessionWriter {
    /// Creates `path` and writes the header line.
    pub fn create(path: impl AsRef<Path>, header: &SessionHeader) -> Result<SessionWriter> {
        let path = path.as_ref().to_path_buf();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let file = File::create(&path).with_context(|| format!("creating {}", path.display()))?;
        let mut w = SessionWriter {
            // 1 MiB of buffer keeps a ~310 event/second feed off the syscall
            // path; the writer is flushed explicitly on close.
            out: BufWriter::with_capacity(1 << 20, file),
            path,
            next_index: 1,
            bytes: 0,
        };
        w.write_line(&serde_json::to_string(header)?)?;
        Ok(w)
    }

    /// Appends a verbatim exchange frame.
    pub fn write_frame(&mut self, recv_ms: i64, raw: &Value) -> Result<u64> {
        let f = self.next_index;
        self.next_index += 1;
        let rec = SessionRecord::Frame {
            f,
            recv_ms,
            raw: raw.clone(),
        };
        self.write_line(&serde_json::to_string(&rec)?)?;
        Ok(f)
    }

    /// Appends a lifecycle marker.
    pub fn write_lifecycle(&mut self, recv_ms: i64, lifecycle: &EventPayload) -> Result<u64> {
        let f = self.next_index;
        self.next_index += 1;
        let rec = SessionRecord::Lifecycle {
            f,
            recv_ms,
            lifecycle: lifecycle.clone(),
        };
        self.write_line(&serde_json::to_string(&rec)?)?;
        Ok(f)
    }

    fn write_line(&mut self, s: &str) -> Result<()> {
        self.out.write_all(s.as_bytes())?;
        self.out.write_all(b"\n")?;
        self.bytes += s.len() as u64 + 1;
        Ok(())
    }

    /// Number of body records written so far.
    pub fn records(&self) -> u64 {
        self.next_index - 1
    }

    /// Bytes written so far, before filesystem overhead.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Path being written.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Flushes buffered records to disk.
    pub fn flush(&mut self) -> Result<()> {
        self.out.flush().context("flushing session file")
    }
}

impl Drop for SessionWriter {
    fn drop(&mut self) {
        // A recording killed by Ctrl-C must not lose its buffered tail.
        let _ = self.out.flush();
    }
}

/// Counters describing how a session file decoded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReadStats {
    /// Body lines read.
    pub records: u64,
    /// Normalized events produced.
    pub events: u64,
    /// Lines that were not valid JSON.
    pub malformed_lines: u64,
    /// Frames carrying an `event_type` this build does not model.
    pub unhandled_frames: u64,
    /// Frames that failed field decoding.
    pub bad_frames: u64,
}

/// Reads a session file, re-normalizing it into [`MarketEvent`]s.
pub struct SessionReader {
    header: SessionHeader,
    lines: std::io::Lines<BufReader<File>>,
    norm: Normalizer,
    pending: std::vec::IntoIter<MarketEvent>,
    stats: ReadStats,
}

impl SessionReader {
    /// Opens `path` and decodes its header.
    pub fn open(path: impl AsRef<Path>) -> Result<SessionReader> {
        let path = path.as_ref();
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mut lines = BufReader::with_capacity(1 << 20, file).lines();
        let first = lines
            .next()
            .transpose()?
            .ok_or_else(|| anyhow!("{} is empty", path.display()))?;
        let header: SessionHeader = serde_json::from_str(&first)
            .with_context(|| format!("{}: first line is not a session header", path.display()))?;
        if header.v != FORMAT_VERSION {
            return Err(anyhow!(
                "{}: session format v{}, this build reads v{FORMAT_VERSION}",
                path.display(),
                header.v
            ));
        }
        Ok(SessionReader {
            header,
            lines,
            norm: Normalizer::new(),
            pending: Vec::new().into_iter(),
            stats: ReadStats::default(),
        })
    }

    /// Session metadata.
    pub fn header(&self) -> &SessionHeader {
        &self.header
    }

    /// Decode counters accumulated so far.
    pub fn stats(&self) -> ReadStats {
        self.stats
    }
}

impl Iterator for SessionReader {
    type Item = MarketEvent;

    fn next(&mut self) -> Option<MarketEvent> {
        loop {
            if let Some(ev) = self.pending.next() {
                return Some(ev);
            }
            let line = match self.lines.next() {
                None => return None,
                Some(Err(_)) => return None,
                Some(Ok(l)) => l,
            };
            if line.trim().is_empty() {
                continue;
            }
            self.stats.records += 1;

            let rec: SessionRecord = match serde_json::from_str(&line) {
                Ok(r) => r,
                Err(_) => {
                    // A truncated final line is the normal result of a
                    // recording that was killed; skip it and keep the rest.
                    self.stats.malformed_lines += 1;
                    continue;
                }
            };

            let produced = match rec {
                SessionRecord::Frame { recv_ms, raw, .. } => {
                    match self.norm.normalize(&raw, recv_ms) {
                        Ok(evs) => evs,
                        Err(FrameError::Unhandled(_)) => {
                            self.stats.unhandled_frames += 1;
                            continue;
                        }
                        Err(_) => {
                            self.stats.bad_frames += 1;
                            continue;
                        }
                    }
                }
                SessionRecord::Lifecycle {
                    recv_ms, lifecycle, ..
                } => {
                    vec![self.norm.emit(recv_ms, recv_ms, lifecycle)]
                }
            };
            self.stats.events += produced.len() as u64;
            self.pending = produced.into_iter();
        }
    }
}

/// Builds a timestamped session filename inside `dir`.
pub fn session_path(dir: impl AsRef<Path>, underlying: Underlying, started_ms: i64) -> PathBuf {
    dir.as_ref().join(format!(
        "session_{}_{}.jsonl",
        underlying.prefix(),
        started_ms / 1000
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Price, Qty};

    fn header() -> SessionHeader {
        SessionHeader {
            v: FORMAT_VERSION,
            kind: "header".into(),
            tool: "test".into(),
            started_ms: 1_786_844_302_000,
            clock: Some(ClockSample {
                local_ms: 1_786_844_302_000,
                server_ms: 1_786_844_302_016,
                rtt_ms: 249,
                offset_ms: 16,
            }),
            underlying: Underlying::Btc,
            source_url: crate::polymarket::websocket::MARKET_WS_URL.into(),
            markets: vec![MarketDescriptor {
                slug: "btc-updown-5m-1786844100".into(),
                title: "Bitcoin Up or Down".into(),
                question: "Bitcoin Up or Down?".into(),
                condition_id: "0x3b6b".into(),
                open_ts: 1_786_844_100,
                close_ts: 1_786_844_400,
                up_token: "UP".into(),
                down_token: "DOWN".into(),
                tick_size: Price::parse("0.01").unwrap(),
                min_size: Qty::from_shares(5),
                accepting_orders: true,
            }],
        }
    }

    const BOOK: &str = r#"{"event_type":"book","asset_id":"UP","timestamp":"1786844302100",
        "hash":"h","bids":[{"price":"0.5","size":"100"}],"asks":[{"price":"0.51","size":"80"}]}"#;
    const CHANGE: &str = r#"{"event_type":"price_change","timestamp":"1786844302200",
        "price_changes":[{"asset_id":"UP","price":"0.5","size":"40","side":"BUY","hash":"h2"},
                         {"asset_id":"DOWN","price":"0.49","size":"10","side":"SELL","hash":"h3"}]}"#;

    fn write_session(path: &Path) {
        let mut w = SessionWriter::create(path, &header()).unwrap();
        w.write_frame(1_786_844_302_300, &serde_json::from_str(BOOK).unwrap())
            .unwrap();
        w.write_lifecycle(
            1_786_844_302_310,
            &EventPayload::MarketOpen {
                slug: "btc-updown-5m-1786844100".into(),
                condition_id: "0x3b6b".into(),
                up_token: "UP".into(),
                down_token: "DOWN".into(),
                close_ms: 1_786_844_400_000,
            },
        )
        .unwrap();
        w.write_frame(1_786_844_302_400, &serde_json::from_str(CHANGE).unwrap())
            .unwrap();
        w.flush().unwrap();
    }

    #[test]
    fn round_trips_a_session_through_disk() {
        let dir = std::env::temp_dir().join("pmclob_rt");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        write_session(&path);

        let r = SessionReader::open(&path).unwrap();
        assert_eq!(r.header().markets[0].slug, "btc-updown-5m-1786844100");
        assert_eq!(r.header().tokens(), vec!["UP", "DOWN"]);
        assert_eq!(r.header().clock.unwrap().offset_ms, 16);

        let evs: Vec<_> = r.collect();
        // book -> 1, lifecycle -> 1, price_change with 2 changes -> 2.
        assert_eq!(evs.len(), 4);
        assert_eq!(
            evs.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );
        assert_eq!(evs[0].kind(), "snapshot");
        assert_eq!(evs[1].kind(), "market_open");
        assert_eq!(evs[2].kind(), "level_update");
        assert_eq!(evs[3].kind(), "level_update");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn raw_frames_are_stored_verbatim_including_unmodelled_fields() {
        let dir = std::env::temp_dir().join("pmclob_verbatim");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");

        let mut w = SessionWriter::create(&path, &header()).unwrap();
        let exotic = serde_json::json!({
            "event_type": "book", "asset_id": "UP", "timestamp": "1",
            "bids": [], "asks": [],
            "field_this_build_ignores": {"nested": [1, 2, 3]}
        });
        w.write_frame(1, &exotic).unwrap();
        w.flush().unwrap();

        let body = std::fs::read_to_string(&path).unwrap();
        assert!(
            body.contains("field_this_build_ignores"),
            "unmodelled fields must survive to disk"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_truncated_tail_does_not_discard_earlier_records() {
        let dir = std::env::temp_dir().join("pmclob_trunc");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        write_session(&path);

        // Simulate a recording killed mid-write.
        let mut body = std::fs::read_to_string(&path).unwrap();
        body.push_str("{\"f\":9,\"recv_ms\":1786844302500,\"raw\":{\"event_ty");
        std::fs::write(&path, body).unwrap();

        let mut r = SessionReader::open(&path).unwrap();
        let n = r.by_ref().count();
        assert_eq!(n, 4, "the four complete events must still be read");
        assert_eq!(r.stats().malformed_lines, 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rereading_the_same_file_produces_identical_events() {
        let dir = std::env::temp_dir().join("pmclob_det");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        write_session(&path);

        let a: Vec<_> = SessionReader::open(&path).unwrap().collect();
        let b: Vec<_> = SessionReader::open(&path).unwrap().collect();
        assert_eq!(a, b, "replay normalization must be deterministic");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn refuses_a_future_format_version() {
        let dir = std::env::temp_dir().join("pmclob_ver");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        let mut h = header();
        h.v = 99;
        SessionWriter::create(&path, &h).unwrap().flush().unwrap();
        assert!(SessionReader::open(&path).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
