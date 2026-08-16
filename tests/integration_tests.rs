//! End-to-end tests: the CLI, the storage round trip, and the safety
//! guarantee that nothing in this crate can trade.

mod common;

use std::process::Command;

use polymarket_live_clob_research::market::event::EventPayload;
use polymarket_live_clob_research::polymarket::parser::{split_frames, Normalizer};
use polymarket_live_clob_research::recorder::storage::{SessionReader, SessionWriter};

const BIN: &str = env!("CARGO_BIN_EXE_polymarket-live-clob-research");

fn cli(args: &[&str]) -> (String, String, bool) {
    let out = Command::new(BIN)
        .args(args)
        .output()
        .expect("binary must run");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

#[test]
fn the_binary_reports_its_subcommands() {
    let (stdout, _, ok) = cli(&["--help"]);
    assert!(ok);
    for verb in ["record", "inspect", "replay", "shadow", "analyze"] {
        assert!(stdout.contains(verb), "help is missing `{verb}`");
    }
}

#[test]
fn inspect_summarises_the_recorded_session() {
    let path = common::fixture_path();
    let (stdout, stderr, ok) = cli(&["inspect", "--file", path.to_str().unwrap()]);
    assert!(ok, "inspect failed: {stderr}");
    assert!(stdout.contains("POLYMARKET SESSION INSPECT"));
    assert!(
        stdout.contains("btc-updown-5m-"),
        "must name the real markets"
    );
    assert!(
        stdout.contains("Feed delay"),
        "must report measured latency"
    );
    assert!(stdout.contains("Book integrity"));
    assert!(
        stdout.contains("wss://ws-subscriptions-clob.polymarket.com/ws/market"),
        "must record where the data came from"
    );
}

#[test]
fn replay_runs_deterministically_from_the_command_line() {
    let path = common::fixture_path();
    let args = ["replay", "--file", path.to_str().unwrap()];
    let (a, stderr, ok) = cli(&args);
    assert!(ok, "replay failed: {stderr}");
    let (b, _, _) = cli(&args);
    assert_eq!(a, b, "two identical replays produced different output");
    assert!(a.contains("accounting identity   exact"));
}

#[test]
fn analyze_produces_a_complete_edge_loss_attribution() {
    let path = common::fixture_path();
    let (stdout, stderr, ok) = cli(&["analyze", "--file", path.to_str().unwrap()]);
    assert!(ok, "analyze failed: {stderr}");
    assert!(stdout.contains("POLYMARKET EXECUTION ANALYSIS"));
    assert!(stdout.contains("Ideal execution"));
    assert!(stdout.contains("Realistic execution"));
    assert!(stdout.contains("EDGE LOSS:"));
    for factor in [
        "Stale market data",
        "Order latency",
        "Queue position",
        "Depth & slippage",
        "Fees",
    ] {
        assert!(stdout.contains(factor), "attribution is missing `{factor}`");
    }
    // The report must say plainly which numbers are assumptions.
    assert!(stdout.contains("assumptions, not measurements"));
    assert!(stdout.contains("No profitability is claimed."));
}

#[test]
fn both_attribution_methods_are_reachable_and_agree_on_the_total() {
    // The waterfall was once unreachable: the flag was a bare bool, so
    // `--shapley false` errored and only Shapley could ever run.
    let path = common::fixture_path();
    let p = path.to_str().unwrap();

    let (shapley, _, ok) = cli(&["analyze", "--file", p, "--attribution", "shapley"]);
    assert!(ok, "shapley attribution must run");
    assert!(shapley.contains("Shapley (32 simulation runs)"));

    let (waterfall, _, ok) = cli(&["analyze", "--file", p, "--attribution", "waterfall"]);
    assert!(ok, "waterfall attribution must run");
    assert!(waterfall.contains("Waterfall (6 simulation runs)"));

    // Same total either way; only the split between factors differs.
    let edge = |out: &str| {
        out.lines()
            .skip_while(|l| !l.starts_with("EDGE LOSS:"))
            .nth(1)
            .map(str::to_owned)
            .expect("an EDGE LOSS figure")
    };
    assert_eq!(
        edge(&shapley),
        edge(&waterfall),
        "the total gap must not depend on how it is attributed"
    );
}

#[test]
fn analyze_writes_a_factor_csv_that_balances() {
    let dir = std::env::temp_dir().join("pmclob_csv_test");
    std::fs::create_dir_all(&dir).unwrap();
    let csv_path = dir.join("factors.csv");
    let path = common::fixture_path();
    let (_, stderr, ok) = cli(&[
        "analyze",
        "--file",
        path.to_str().unwrap(),
        "--csv",
        csv_path.to_str().unwrap(),
    ]);
    assert!(ok, "analyze --csv failed: {stderr}");

    let body = std::fs::read_to_string(&csv_path).unwrap();
    let mut rows: Vec<Vec<String>> = body
        .lines()
        .map(|l| l.split(',').map(str::to_owned).collect())
        .collect();
    let header = rows.remove(0);
    assert_eq!(header, ["factor", "edge_loss_usdc", "share"]);

    let total: f64 = rows
        .iter()
        .find(|r| r[0] == "TOTAL")
        .map(|r| r[1].parse().unwrap())
        .expect("a TOTAL row");
    let parts: f64 = rows
        .iter()
        .filter(|r| r[0] != "TOTAL")
        .map(|r| r[1].parse::<f64>().unwrap())
        .sum();
    assert!(
        (parts - total).abs() < 1e-6,
        "factor rows sum to {parts} but TOTAL says {total}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn analyze_exports_fills_whose_slippage_recomputes_from_the_row() {
    // Every aggregate in the report rests on these rows, so a reader must be
    // able to re-derive the slippage column rather than take it on trust.
    let dir = std::env::temp_dir().join("pmclob_fills_test");
    std::fs::create_dir_all(&dir).unwrap();
    let fills_path = dir.join("fills.csv");
    let path = common::fixture_path();
    let (_, stderr, ok) = cli(&[
        "analyze",
        "--file",
        path.to_str().unwrap(),
        "--fills-csv",
        fills_path.to_str().unwrap(),
    ]);
    assert!(ok, "analyze --fills-csv failed: {stderr}");

    let body = std::fs::read_to_string(&fills_path).unwrap();
    let mut lines = body.lines();
    let header: Vec<&str> = lines.next().unwrap().split(',').collect();
    let col = |name: &str| header.iter().position(|h| *h == name).expect(name);
    let (i_side, i_price, i_qty, i_ref, i_slip) = (
        col("side"),
        col("price"),
        col("qty"),
        col("reference_price"),
        col("slippage_usdc"),
    );

    let mut rows = 0;
    for line in lines {
        let f: Vec<&str> = line.split(',').collect();
        let sign = match f[i_side] {
            "BUY" => 1.0,
            "SELL" => -1.0,
            other => panic!("unexpected side {other}"),
        };
        let price: f64 = f[i_price].parse().unwrap();
        let qty: f64 = f[i_qty].parse().unwrap();
        let reference: f64 = f[i_ref].parse().unwrap();
        let reported: f64 = f[i_slip].parse().unwrap();
        let expected = (price - reference) * qty * sign;
        assert!(
            (reported - expected).abs() < 0.01,
            "row slippage {reported} does not match {expected} recomputed from its own fields"
        );
        rows += 1;
    }
    assert!(rows > 0, "the realistic run must produce fills to export");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_session_survives_a_write_and_read_round_trip() {
    let dir = std::env::temp_dir().join("pmclob_roundtrip_test");
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("copy.jsonl");

    // Re-record the fixture's raw frames through the writer, then confirm
    // the copy normalizes to exactly the same events.
    let src = common::reader();
    let header = src.header().clone();
    let original: Vec<_> = src.collect();

    let raw_lines: Vec<serde_json::Value> = std::fs::read_to_string(common::fixture_path())
        .unwrap()
        .lines()
        .skip(1)
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    let mut w = SessionWriter::create(&out, &header).unwrap();
    for rec in &raw_lines {
        let recv_ms = rec["recv_ms"].as_i64().unwrap();
        if let Some(raw) = rec.get("raw") {
            w.write_frame(recv_ms, raw).unwrap();
        } else {
            let lifecycle: EventPayload = serde_json::from_value(rec["lifecycle"].clone()).unwrap();
            w.write_lifecycle(recv_ms, &lifecycle).unwrap();
        }
    }
    w.flush().unwrap();

    let copied: Vec<_> = SessionReader::open(&out).unwrap().collect();
    assert_eq!(copied, original, "the round trip lost or altered events");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn the_recorded_frames_are_the_exchanges_own_bytes() {
    // Spot-check that stored frames still carry the exchange's own fields,
    // rather than a re-serialisation of this crate's model.
    let body = std::fs::read_to_string(common::fixture_path()).unwrap();
    let a_frame = body
        .lines()
        .skip(1)
        .find(|l| l.contains("\"price_change\""))
        .expect("fixture must contain a price_change frame");
    for field in [
        "\"asset_id\"",
        "\"best_bid\"",
        "\"best_ask\"",
        "\"hash\"",
        "\"market\"",
    ] {
        assert!(a_frame.contains(field), "verbatim frame lost {field}");
    }
}

#[test]
fn the_parser_handles_the_exact_envelope_the_exchange_sends() {
    // A real multi-event array frame, as captured from the live socket.
    let text = r#"[{"market":"0x101e","price_changes":[
        {"asset_id":"65637","price":"0.2","size":"60","side":"BUY","hash":"a",
         "best_bid":"0.5","best_ask":"0.51"}],
        "timestamp":"1786844302144","event_type":"price_change"},
       {"market":"0x3b6b","asset_id":"10483","price":"0.51","size":"20",
        "fee_rate_bps":"0","side":"BUY","timestamp":"1786844303130",
        "event_type":"last_trade_price","transaction_hash":"0x7fe6"}]"#;
    let frames = split_frames(text).expect("array envelope must decode");
    assert_eq!(frames.len(), 2);

    let mut n = Normalizer::new();
    let mut kinds = Vec::new();
    for f in &frames {
        for ev in n.normalize(f, 1_786_844_303_200).unwrap() {
            kinds.push(ev.kind());
        }
    }
    assert_eq!(kinds, vec!["level_update", "trade"]);
}

#[test]
fn no_order_placement_endpoint_is_reachable_from_this_crate() {
    // The crate is read-only by construction. This test guards that
    // property against a future change that quietly adds a write path.
    let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    let mut stack = vec![src_dir];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let body = std::fs::read_to_string(&path).unwrap();
            for line in body.lines() {
                let code = line.split("//").next().unwrap_or("");
                // Signing keys, private keys and the CLOB write endpoints.
                for bad in [
                    "private_key",
                    "PRIVATE_KEY",
                    "sign_order",
                    "\"/order\"",
                    "/orders\"",
                ] {
                    if code.contains(bad) {
                        offenders.push(format!("{}: {}", path.display(), line.trim()));
                    }
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "this crate must never gain a trading path:\n{}",
        offenders.join("\n")
    );
}
