//! Shared helpers for tests that run on real recorded Polymarket data.
//!
//! Every test in this suite reads `tests/fixtures/session_btc_sample.jsonl`,
//! a slice of a live BTC 5-minute Up/Down recording made by this tool on
//! 2026-08-16: 8784 events across 8 tokens, comprising 17 book snapshots,
//! 8708 level updates and 55 trade prints (51 buy-aggressor, 4
//! sell-aggressor, so both sides of the queue model are exercised).
//!
//! Prices, sizes, timestamps and trades are verbatim. The only edit is that
//! most mid-session book snapshots were dropped to keep the file committable:
//! the exchange republishes a full snapshot per token roughly every 1.5
//! seconds, and since every delta between them is retained, the
//! reconstructed book is bit-identical either way. The first snapshot for
//! each token is always kept, because a book cannot be baselined without it.
//!
//! No market data in this repository is synthetic.

// This module is compiled independently into each integration test binary,
// so any helper a given binary does not call reads as dead code there.
#![allow(dead_code)]

use std::path::PathBuf;

use polymarket_live_clob_research::market::event::MarketEvent;
use polymarket_live_clob_research::polymarket::market_discovery::MarketDescriptor;
use polymarket_live_clob_research::recorder::storage::SessionReader;

/// Path to the recorded fixture session.
pub fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/session_btc_sample.jsonl")
}

/// Opens the fixture.
pub fn reader() -> SessionReader {
    SessionReader::open(fixture_path()).expect("fixture session must be readable")
}

/// Reads every event from the fixture.
pub fn events() -> Vec<MarketEvent> {
    reader().collect()
}

/// Market metadata recorded in the fixture header.
pub fn markets() -> Vec<MarketDescriptor> {
    reader().header().markets.clone()
}
