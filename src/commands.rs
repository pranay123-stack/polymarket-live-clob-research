//! Implementations behind each CLI verb.

use anyhow::{Context, Result};

use crate::analytics::attribution::{attribute, Attribution, Method};
use crate::analytics::metrics::LatencyHistogram;
use crate::cli::QueueModelArg;
use crate::cli::{AnalyzeArgs, InspectArgs, RecordArgs, ReplayArgs, ShadowArgs};
use crate::execution::latency::LatencyModel;
use crate::execution::matcher::{ExecConfig, Factor, QueueModel, Realism};
use crate::market::state::MarketState;
use crate::polymarket::api::PolymarketClient;
use crate::recorder::storage::SessionReader;
use crate::recorder::{self, RecorderConfig};
use crate::replay::engine::{RunConfig, RunResult};
use crate::strategy::SignalConfig;
use crate::types::{Price, Side, Usdc};

/// Formats an epoch-millisecond stamp as UTC.
pub fn fmt_ms(ms: i64) -> String {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000)
        .ok()
        .and_then(|t| t.format(&Rfc3339).ok())
        .unwrap_or_else(|| ms.to_string())
}

/// Formats a duration in milliseconds as `HH:MM:SS`.
pub fn fmt_dur(ms: i64) -> String {
    let s = ms / 1000;
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// `record`: capture real market data to a session file.
pub async fn record(args: RecordArgs) -> Result<()> {
    let client = PolymarketClient::new()?;
    let cfg = RecorderConfig {
        underlying: args.underlying,
        duration: args.duration(),
        out_dir: args.out.clone(),
        lookahead_rounds: args.lookahead,
        refresh: std::time::Duration::from_secs(args.refresh.max(5)),
        ..RecorderConfig::default()
    };
    let shutdown = recorder::shutdown_signal();
    let s = recorder::record(&client, cfg, shutdown).await?;

    println!("\n--- RECORDING COMPLETE ---\n");
    println!("Markets:\n{}\n", s.markets);
    println!("Frames:\n{}\n", s.frames);
    println!("Lifecycle markers:\n{}\n", s.lifecycle);
    println!("Elapsed:\n{}\n", fmt_dur(s.elapsed_ms));
    println!("Reconnects:\n{}\n", s.reconnects);
    println!("Feed delay (recv - exchange), ms:\n{}\n", s.feed_delay);
    if let Some(c) = s.clock {
        println!(
            "Clock offset at start:\n{:+} ms (+/-{} ms, rtt {} ms) -> {}\n",
            c.offset_ms,
            c.uncertainty_ms(),
            c.rtt_ms,
            if c.offset_is_significant() {
                "significant; feed delay is offset-contaminated"
            } else {
                "not distinguishable from zero"
            }
        );
    }
    println!("Bytes:\n{}\n", s.bytes);
    println!("Saved:\n{}", s.path.display());
    Ok(())
}

/// `inspect`: summarise a recorded session without simulating anything.
pub fn inspect(args: InspectArgs) -> Result<()> {
    let mut reader = SessionReader::open(&args.file)
        .with_context(|| format!("opening {}", args.file.display()))?;
    let header = reader.header().clone();

    let mut state = MarketState::new();
    let mut delay = LatencyHistogram::new();
    let mut by_kind: std::collections::BTreeMap<&'static str, u64> = Default::default();
    let mut first_ms = i64::MAX;
    let mut last_ms = 0i64;
    let mut trade_notional = crate::types::Usdc::ZERO;
    let mut trade_shares = crate::types::Qty::ZERO;

    for ev in reader.by_ref() {
        *by_kind.entry(ev.kind()).or_default() += 1;
        first_ms = first_ms.min(ev.recv_ms);
        last_ms = last_ms.max(ev.recv_ms);
        // Lifecycle markers are recorder-stamped, so their delay is trivially
        // zero and would bias the feed-delay distribution.
        if !matches!(
            ev.payload,
            crate::market::event::EventPayload::MarketOpen { .. }
                | crate::market::event::EventPayload::MarketClose { .. }
        ) {
            delay.record(ev.feed_delay_ms());
        }
        if let crate::market::event::EventPayload::Trade { price, qty, .. } = &ev.payload {
            trade_notional += qty.notional(*price);
            trade_shares = crate::types::Qty(trade_shares.0 + qty.0);
        }
        state.apply(&ev);
    }
    let stats = reader.stats();

    println!("POLYMARKET SESSION INSPECT\n");
    println!("File:\n{}\n", args.file.display());
    println!("Tool:\n{}\n", header.tool);
    println!(
        "Underlying:\n{}\n",
        header.underlying.prefix().to_uppercase()
    );
    println!("Source:\n{}\n", header.source_url);
    println!("Started:\n{}\n", fmt_ms(header.started_ms));
    if first_ms <= last_ms {
        println!("Duration:\n{}\n", fmt_dur(last_ms - first_ms));
    }

    println!("Markets:\n{}", header.markets.len());
    for m in &header.markets {
        println!(
            "  {}  {}  open {}  close {}",
            m.slug,
            if m.title.is_empty() {
                &m.question
            } else {
                &m.title
            },
            fmt_ms(m.open_ts * 1000),
            fmt_ms(m.close_ts * 1000),
        );
    }
    println!();

    println!("Records:\n{}\n", stats.records);
    println!("Events:\n{}", stats.events);
    for (k, n) in &by_kind {
        println!("  {k:<16} {n:>10}");
    }
    println!();

    if stats.malformed_lines + stats.unhandled_frames + stats.bad_frames > 0 {
        println!("Decode issues:");
        println!("  malformed lines   {:>8}", stats.malformed_lines);
        println!("  unhandled frames  {:>8}", stats.unhandled_frames);
        println!("  bad frames        {:>8}", stats.bad_frames);
        println!();
    }

    println!("Feed delay (recv - exchange), ms:\n{}\n", delay.summary());
    if let Some(c) = header.clock {
        println!(
            "Clock offset at start:\n{:+} ms (+/-{} ms) -> {}\n",
            c.offset_ms,
            c.uncertainty_ms(),
            if c.offset_is_significant() {
                "significant"
            } else {
                "not distinguishable from zero"
            }
        );
    }

    let bs = state.stats();
    println!("Book integrity:");
    println!("  snapshots applied      {:>10}", bs.snapshots);
    println!("  level updates applied  {:>10}", bs.level_updates);
    println!("  trades observed        {:>10}", bs.trades);
    println!("  stale updates rejected {:>10}", bs.stale_rejected);
    println!("  crossed books observed {:>10}", bs.crossed_observed);
    println!("  deltas before snapshot {:>10}", bs.before_snapshot);
    println!();

    println!(
        "Traded volume observed:\n{} across {} shares\n",
        trade_notional, trade_shares
    );

    println!(
        "Liquidity at end of session ({} books):",
        state.book_count()
    );
    for m in &header.markets {
        for (label, token) in [("Up", &m.up_token), ("Down", &m.down_token)] {
            let Some(book) = state.book(token) else {
                continue;
            };
            if book.is_empty() {
                continue;
            }
            let bb = book.best_bid();
            let ba = book.best_ask();
            println!(
                "  {:<28} {:<4} bid {:<7} ask {:<7} spread {:<7} depth10 {:>12} / {:<12}",
                short(&m.slug),
                label,
                bb.map(|l| l.price.to_string())
                    .unwrap_or_else(|| "-".into()),
                ba.map(|l| l.price.to_string())
                    .unwrap_or_else(|| "-".into()),
                book.spread()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "-".into()),
                book.depth_qty(Side::Buy, 10).to_string(),
                book.depth_qty(Side::Sell, 10).to_string(),
            );
            if args.detail {
                for (side, name) in [(Side::Sell, "ask"), (Side::Buy, "bid")] {
                    for lv in book.levels(side, 5) {
                        println!("        {name} {} x {}", lv.price, lv.qty);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Trims the `<underlying>-updown-5m-` prefix for compact tables.
fn short(slug: &str) -> String {
    slug.rsplit_once('-')
        .map(|(_, ts)| format!("…{ts}"))
        .unwrap_or_else(|| slug.to_owned())
}

/// Builds simulation settings from the shared CLI flags.
pub fn run_config(exec: &crate::cli::ExecArgs, realism: Realism) -> RunConfig {
    RunConfig {
        exec: ExecConfig {
            latency: LatencyModel {
                market_data_ms: exec.md_latency_ms.max(0),
                submit_ms: exec.submit_latency_ms,
                cancel_ms: exec.cancel_latency_ms,
            },
            queue_model: match exec.queue_model {
                QueueModelArg::Pessimistic => QueueModel::Pessimistic,
                QueueModelArg::Proportional => QueueModel::Proportional,
                QueueModelArg::Optimistic => QueueModel::Optimistic,
            },
            taker_fee_bps: exec.taker_fee_bps,
            maker_fee_bps: exec.maker_fee_bps,
            order_ttl_ms: exec.order_ttl_ms,
            realism,
        },
        signal: SignalConfig {
            threshold: exec.signal_threshold,
            depth_levels: exec.signal_depth,
            cooldown_ms: exec.decision_cooldown_ms,
            order_shares: exec.order_shares,
            max_position_shares: exec.max_position,
            style: exec.style,
            aggression_ticks: 1,
        },
        starting_cash: Usdc::from_dollars(exec.cash),
        // A negative flag means "use the delay the recording actually
        // measured" rather than substituting a constant.
        md_latency_override_ms: (exec.md_latency_ms >= 0).then_some(exec.md_latency_ms),
        decision_horizon_ms: exec.decision_horizon_ms,
    }
}

/// Prints the shared header describing how a simulation was configured.
fn print_settings(cfg: &RunConfig, measured_md: Option<i64>) {
    println!("Execution settings:");
    match cfg.md_latency_override_ms {
        Some(ms) => println!("  market data latency   {ms} ms (fixed override)"),
        None => println!(
            "  market data latency   {} (measured per event from the recording)",
            measured_md
                .map(|m| format!("~{m} ms median"))
                .unwrap_or_else(|| "measured".into())
        ),
    }
    println!(
        "  submit latency        {} ms (assumption)",
        cfg.exec.latency.submit_ms
    );
    println!(
        "  cancel latency        {} ms (assumption)",
        cfg.exec.latency.cancel_ms
    );
    println!("  queue model           {:?}", cfg.exec.queue_model);
    println!(
        "  taker / maker fee     {} / {} bps",
        cfg.exec.taker_fee_bps, cfg.exec.maker_fee_bps
    );
    println!(
        "  order size            {} shares, style {:?}",
        cfg.signal.order_shares, cfg.signal.style
    );
    println!(
        "  signal                imbalance over {} levels, threshold {}",
        cfg.signal.depth_levels, cfg.signal.threshold
    );
    println!();
}

/// Prints the outcome of a single simulated run.
fn print_run(label: &str, r: &RunResult) {
    println!("{label}");
    println!("  P&L                   {}", r.pnl.total_pnl);
    println!("    realized            {}", r.pnl.realized);
    println!("    unrealized          {}", r.pnl.unrealized);
    println!("    fees                {}", r.pnl.fees);
    println!("  decisions             {}", r.decisions);
    println!("  orders submitted      {}", r.exec.submitted);
    println!("    filled in full      {}", r.exec.filled_full);
    println!("    filled in part      {}", r.exec.filled_partial);
    println!("    expired unfilled    {}", r.exec.expired_unfilled);
    println!("    never fillable      {}", r.exec.unfillable);
    println!(
        "  fill ratio            {:.1}%",
        r.exec.fill_ratio() * 100.0
    );
    println!(
        "  fills                 {} maker / {} taker",
        r.slippage.maker_fills, r.slippage.taker_fills
    );
    println!("  notional traded       {}", r.slippage.notional);
    println!(
        "  slippage              {}{}",
        r.slippage.total_slippage,
        r.slippage
            .slippage_bps()
            .map(|b| format!(" ({b:.1} bps)"))
            .unwrap_or_default()
    );
    println!(
        "  accounting identity   {}",
        if r.identity_error == Usdc::ZERO {
            "exact".to_string()
        } else {
            format!("BROKEN by {}", r.identity_error)
        }
    );
    println!();
}

/// Opens a session and returns its events plus market metadata.
fn open_session(
    path: &std::path::Path,
) -> Result<(
    SessionReader,
    Vec<crate::polymarket::market_discovery::MarketDescriptor>,
)> {
    let reader =
        SessionReader::open(path).with_context(|| format!("opening {}", path.display()))?;
    let markets = reader.header().markets.clone();
    Ok((reader, markets))
}

/// `replay`: deterministic replay with execution simulation.
pub fn replay(args: ReplayArgs) -> Result<()> {
    let (reader, markets) = open_session(&args.file)?;
    let cfg = run_config(&args.exec, Realism::REAL);

    println!("POLYMARKET DETERMINISTIC REPLAY\n");
    println!("File:\n{}\n", args.file.display());
    println!("Markets:\n{}\n", markets.len());
    print_settings(&cfg, None);

    let result = crate::replay::engine::run(reader, &markets, &cfg)?;

    println!("Events replayed:\n{}\n", result.events);
    println!("Event-time span:\n{}\n", fmt_dur(result.span_ms));
    println!(
        "Measured feed delay, ms:\n{}\n",
        result.feed_delay.summary()
    );
    print_run("Realistic execution:", &result);

    println!("Book integrity:");
    println!("  snapshots             {}", result.state.snapshots);
    println!("  level updates         {}", result.state.level_updates);
    println!("  trades                {}", result.state.trades);
    println!("  stale rejected        {}", result.state.stale_rejected);
    println!("  crossed observed      {}", result.state.crossed_observed);

    if result.identity_error != Usdc::ZERO {
        anyhow::bail!("accounting identity broken by {}", result.identity_error);
    }
    Ok(())
}

/// `shadow`: live observation with hypothetical orders.
pub async fn shadow(args: ShadowArgs) -> Result<()> {
    let client = PolymarketClient::new()?;
    let cfg = crate::shadow::engine::ShadowConfig {
        underlying: args.underlying,
        duration: std::time::Duration::from_secs(args.seconds),
        record_to: args.record_to.clone(),
        run: run_config(&args.exec, Realism::REAL),
        horizon_ms: 30_000,
    };
    let shutdown = recorder::shutdown_signal();
    let s = crate::shadow::engine::run(&client, cfg, shutdown).await?;

    println!("\n--- SHADOW SESSION COMPLETE ---\n");
    println!("Frames observed:\n{}\n", s.frames);
    println!("Events normalized:\n{}\n", s.events);
    println!("Hypothetical decisions:\n{}\n", s.decisions);
    println!(
        "Forward move resolved:\n{} of {} ({} moved favourably)\n",
        s.resolved, s.decisions, s.favourable
    );
    println!("Ideal execution P&L:\n{}\n", s.ideal_pnl);
    println!("Realistic execution P&L:\n{}\n", s.real_pnl);
    println!("EDGE LOSS:\n{}\n", s.edge_loss());
    println!(
        "Fill ratio:\n{:.1}% ideal -> {:.1}% realistic\n",
        s.ideal_fill_ratio * 100.0,
        s.real_fill_ratio * 100.0
    );
    if let Some(p) = &s.recorded_to {
        println!("Frames recorded to:\n{}\n", p.display());
    }
    println!("No orders were sent at any point.");
    Ok(())
}

/// `analyze`: ideal vs realistic execution report.
pub fn analyze(args: AnalyzeArgs) -> Result<()> {
    let (probe, markets) = open_session(&args.file)?;
    // One cheap pass to measure the feed delay the recording actually saw.
    let mut delay = LatencyHistogram::new();
    for ev in probe {
        if !matches!(
            ev.payload,
            crate::market::event::EventPayload::MarketOpen { .. }
                | crate::market::event::EventPayload::MarketClose { .. }
        ) {
            delay.record(ev.feed_delay_ms());
        }
    }
    let measured_md = delay.percentile(50.0);

    let base = run_config(&args.exec, Realism::REAL);
    println!("POLYMARKET EXECUTION ANALYSIS\n");
    println!("File:\n{}\n", args.file.display());
    println!("Markets:\n{}\n", markets.len());
    println!("Measured feed delay, ms:\n{}\n", delay.summary());
    print_settings(&base, measured_md);

    let method = match args.attribution {
        crate::cli::AttributionArg::Shapley => Method::Shapley,
        crate::cli::AttributionArg::Waterfall => Method::Waterfall,
    };
    println!(
        "Attribution method:\n{:?} ({} simulation runs)\n",
        method,
        method.runs(Factor::ALL.len() as u32)
    );

    // Cache runs by realism so the ideal and realistic legs, which the
    // attribution also visits, are not simulated twice.
    let mut cache: std::collections::HashMap<Realism, RunResult> = std::collections::HashMap::new();
    let file = args.file.clone();
    let exec_args = args.exec.clone();
    let markets_ref = markets.clone();

    let mut run_for = |realism: Realism| -> Usdc {
        if let Some(r) = cache.get(&realism) {
            return r.pnl.total_pnl;
        }
        let cfg = run_config(&exec_args, realism);
        let reader = match SessionReader::open(&file) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "re-opening session failed");
                return Usdc::ZERO;
            }
        };
        match crate::replay::engine::run(reader, &markets_ref, &cfg) {
            Ok(r) => {
                let pnl = r.pnl.total_pnl;
                cache.insert(realism, r);
                pnl
            }
            Err(e) => {
                tracing::error!(error = %e, "replay pass failed");
                Usdc::ZERO
            }
        }
    };

    let attribution = attribute(&Factor::ALL, method, &mut run_for);

    // One extra pass: ideal execution on the *same* observed view. Holding
    // market-data latency on keeps the decision stream identical to the
    // realistic run, which is what makes a per-decision comparison valid.
    let exec_ideal_realism = Realism::IDEAL.with(Factor::MdLatency, true);
    run_for(exec_ideal_realism);
    // `run_for` holds `cache` mutably; end its borrow before reading back.
    let _ = run_for;

    let ideal = cache.get(&Realism::IDEAL).cloned();
    let real = cache.get(&Realism::REAL).cloned();
    if let Some(r) = &ideal {
        print_run("Ideal execution (naive backtest assumptions):", r);
    }
    if let Some(r) = &real {
        print_run("Realistic execution:", r);
    }

    println!("EDGE LOSS:\n{}\n", attribution.edge_loss);
    println!("CAUSE:\n");
    let meaningful = attribution.shares_are_meaningful();
    let width = Factor::ALL
        .iter()
        .map(|f| f.label().len())
        .max()
        .unwrap_or(20);
    for (f, cost) in attribution.ranked() {
        let share = match meaningful {
            true => attribution
                .share(f)
                .map(|s| format!("{:>6.1}%", s * 100.0))
                .unwrap_or_else(|| "     -".into()),
            false => "     -".into(),
        };
        println!(
            "  {:<width$}  {:>12}  {share}",
            f.label(),
            cost.to_string(),
            width = width
        );
    }
    if attribution.rounding != Usdc::ZERO {
        println!(
            "  {:<width$}  {:>12}",
            "rounding",
            attribution.rounding.to_string(),
            width = width
        );
    }
    if !meaningful {
        println!();
        println!("  Percentages are omitted: the factors offset one another, so the net");
        println!("  edge loss is smaller than the largest single contribution. A share of");
        println!("  it would be noise multiplied rather than an apportionment. Read the");
        println!("  absolute figures instead, and note that realism is not guaranteed to");
        println!("  cost money on any one session - a delayed signal is sometimes the");
        println!("  luckier one. Longer sessions average this out.");
    }
    println!();

    if let (Some(i), Some(r)) = (&ideal, &real) {
        println!("Execution quality:");
        println!(
            "  fill ratio            {:.1}% ideal -> {:.1}% realistic",
            i.exec.fill_ratio() * 100.0,
            r.exec.fill_ratio() * 100.0
        );
        println!(
            "  orders never filled   {} ideal -> {} realistic",
            i.exec.missed(),
            r.exec.missed()
        );
        println!(
            "  slippage              {} ideal -> {} realistic",
            i.slippage.total_slippage, r.slippage.total_slippage
        );
        println!();
        for run in [i, r] {
            if run.identity_error != Usdc::ZERO {
                anyhow::bail!("accounting identity broken by {}", run.identity_error);
            }
        }
    }

    if let Some(csv_path) = &args.csv {
        write_factor_csv(csv_path, &attribution)?;
        println!("Factor table written to:\n{}\n", csv_path.display());
    }
    if let (Some(r), Some(ei)) = (
        cache.get(&Realism::REAL).cloned(),
        cache.get(&exec_ideal_realism).cloned(),
    ) {
        print_decision_attribution(&r, &ei, args.explain);
    }

    if let Some(dir) = &args.csv_dir {
        match &real {
            Some(r) => {
                write_audit_csvs(dir, r)?;
                let s = r.ledger.summary(r.pnl.total_pnl);
                println!(
                    "Audit set written to:\n{}\n  decisions.csv  {} rows\n  orders.csv     {} rows\n  fills.csv      {} rows\n  pnl.csv        {} rows (+ ROUNDING, TOTAL)\n",
                    dir.display(),
                    s.decisions,
                    s.orders,
                    s.fills,
                    s.decisions
                );
            }
            None => tracing::warn!("no realistic run to export"),
        }
    }

    if let Some(fills_path) = &args.fills_csv {
        match &real {
            Some(r) => {
                write_fills_csv(fills_path, &r.fills)?;
                println!(
                    "{} realistic fills written to:\n{}\n",
                    r.fills.len(),
                    fills_path.display()
                );
            }
            None => tracing::warn!("no realistic run to export fills from"),
        }
    }

    println!("NOTE");
    println!("  Submission and cancellation latency are assumptions, not measurements:");
    println!("  they cannot be observed without sending real orders, which this tool");
    println!("  never does. Market-data latency and every book, trade and price above");
    println!("  are measured from real Polymarket data. No profitability is claimed.");
    Ok(())
}

/// Writes one row per simulated fill.
///
/// This is the raw material behind every aggregate in the report: each row
/// carries the price the strategy was looking at alongside the price it
/// actually got, so the slippage column can be re-derived independently
/// rather than taken on trust.
fn write_fills_csv(path: &std::path::Path, fills: &[crate::execution::fills::Fill]) -> Result<()> {
    let mut w =
        csv::Writer::from_path(path).with_context(|| format!("creating {}", path.display()))?;
    w.write_record([
        "order_id",
        "ts_ms",
        "ts_utc",
        "token",
        "side",
        "liquidity",
        "price",
        "qty",
        "notional_usdc",
        "reference_price",
        "adverse_ticks",
        "slippage_usdc",
        "fee_usdc",
    ])?;
    for f in fills {
        w.write_record([
            f.order_id.to_string(),
            f.ts_ms.to_string(),
            fmt_ms(f.ts_ms),
            f.token.clone(),
            f.side.to_string(),
            format!("{:?}", f.liquidity),
            format!("{:.4}", f.price.to_f64()),
            format!("{:.6}", f.qty.to_f64()),
            format!("{:.6}", f.notional().to_f64()),
            format!("{:.4}", f.reference_price.to_f64()),
            f.adverse_ticks().to_string(),
            format!("{:.6}", f.slippage_cost().to_f64()),
            format!("{:.6}", f.fee.to_f64()),
        ])?;
    }
    w.flush()?;
    Ok(())
}

/// Writes the per-factor attribution table as CSV.
fn write_factor_csv(path: &std::path::Path, a: &Attribution) -> Result<()> {
    let mut w =
        csv::Writer::from_path(path).with_context(|| format!("creating {}", path.display()))?;
    w.write_record(["factor", "edge_loss_usdc", "share"])?;
    for (f, cost) in a.ranked() {
        w.write_record([
            f.label(),
            &format!("{:.6}", cost.to_f64()),
            &a.share(f).map(|s| format!("{s:.6}")).unwrap_or_default(),
        ])?;
    }
    w.write_record(["rounding", &format!("{:.6}", a.rounding.to_f64()), ""])?;
    w.write_record(["TOTAL", &format!("{:.6}", a.edge_loss.to_f64()), "1.0"])?;
    w.flush()?;
    Ok(())
}

/// Runs one replay pass, returning the full result.
fn one_pass(
    file: &std::path::Path,
    markets: &[crate::polymarket::market_discovery::MarketDescriptor],
    cfg: &RunConfig,
) -> Result<RunResult> {
    let reader =
        SessionReader::open(file).with_context(|| format!("opening {}", file.display()))?;
    crate::replay::engine::run(reader, markets, cfg)
}

/// A compact fingerprint of everything a run produced.
///
/// Compared field by field rather than as one hash so a mismatch names the
/// stage that drifted instead of merely reporting that something did.
#[derive(Debug, PartialEq, Eq)]
struct RunFingerprint {
    book_checksum: u64,
    decisions: Vec<(u64, i64, String, Side, u64)>,
    orders: Vec<(u64, u64, u64, u64, i64, i64)>,
    fills: Vec<(u64, u64, u64, u32, u64, i64)>,
    total_pnl: Usdc,
    realized: Usdc,
    unrealized: Usdc,
    fees: Usdc,
    attribution: Vec<(&'static str, Usdc)>,
}

impl RunFingerprint {
    fn of(r: &RunResult) -> RunFingerprint {
        RunFingerprint {
            book_checksum: r.book_checksum,
            decisions: r
                .ledger
                .decisions()
                .map(|d| (d.id, d.ts_ms, d.token.clone(), d.side, d.qty.0))
                .collect(),
            orders: r
                .ledger
                .outcomes()
                .iter()
                .map(|o| {
                    (
                        o.decision_id,
                        o.order_id,
                        o.requested.0,
                        o.executed.0,
                        o.decided_ms,
                        o.arrive_ms,
                    )
                })
                .collect(),
            fills: r
                .fills
                .iter()
                .map(|f| {
                    (
                        f.decision_id,
                        f.order_id,
                        f.fill_id,
                        f.price.ticks(),
                        f.qty.0,
                        f.ts_ms,
                    )
                })
                .collect(),
            total_pnl: r.pnl.total_pnl,
            realized: r.pnl.realized,
            unrealized: r.pnl.unrealized,
            fees: r.pnl.fees,
            attribution: r
                .ledger
                .decisions()
                .map(|d| (d.strategy, r.ledger.pnl(d.id).net))
                .collect(),
        }
    }
}

/// `verify-replay`: prove that replaying one session twice is identical.
///
/// Compares each stage separately. Final P&L alone is a weak check — two
/// offsetting execution differences can leave it unchanged while the book,
/// the decisions or the fills have quietly diverged.
pub fn verify_replay(args: crate::cli::VerifyReplayArgs) -> Result<()> {
    let (_, markets) = open_session(&args.file)?;
    let cfg = run_config(&args.exec, Realism::REAL);
    let runs = args.runs.max(2);

    println!("REPLAY VERIFICATION\n");
    println!("File:\n{}\n", args.file.display());
    println!("Runs:\n{runs}\n");
    print_settings(&cfg, None);

    let first = one_pass(&args.file, &markets, &cfg)?;
    let baseline = RunFingerprint::of(&first);
    let mut mismatches: Vec<String> = Vec::new();

    for n in 2..=runs {
        let next = one_pass(&args.file, &markets, &cfg)?;
        let f = RunFingerprint::of(&next);
        if f.book_checksum != baseline.book_checksum {
            mismatches.push(format!(
                "run {n}: order book checksum {:016x} != {:016x}",
                f.book_checksum, baseline.book_checksum
            ));
        }
        if f.decisions != baseline.decisions {
            mismatches.push(format!(
                "run {n}: {} decisions vs {}",
                f.decisions.len(),
                baseline.decisions.len()
            ));
        }
        if f.orders != baseline.orders {
            mismatches.push(format!("run {n}: orders differ"));
        }
        if f.fills != baseline.fills {
            mismatches.push(format!("run {n}: fills differ"));
        }
        if (f.total_pnl, f.realized, f.unrealized, f.fees)
            != (
                baseline.total_pnl,
                baseline.realized,
                baseline.unrealized,
                baseline.fees,
            )
        {
            mismatches.push(format!(
                "run {n}: P&L {} vs {}",
                f.total_pnl, baseline.total_pnl
            ));
        }
        if f.attribution != baseline.attribution {
            mismatches.push(format!("run {n}: per-decision attribution differs"));
        }
    }

    let pass = |ok: bool| if ok { "PASS" } else { "FAIL" };
    let has = |needle: &str| mismatches.iter().any(|m| m.contains(needle));

    println!("Order book:\n{}\n", pass(!has("checksum")));
    println!("Strategy decisions:\n{}\n", pass(!has("decisions")));
    println!("Orders:\n{}\n", pass(!has("orders")));
    println!("Fills:\n{}\n", pass(!has("fills")));
    println!("P&L:\n{}\n", pass(!has("P&L")));
    println!("Attribution:\n{}\n", pass(!has("attribution")));

    println!("Book checksum:\n{:016x}\n", baseline.book_checksum);
    println!(
        "Compared:\n{} decisions, {} orders, {} fills across {runs} runs\n",
        baseline.decisions.len(),
        baseline.orders.len(),
        baseline.fills.len()
    );

    if mismatches.is_empty() {
        println!("IDENTICAL RESULT");
        Ok(())
    } else {
        for m in &mismatches {
            println!("  {m}");
        }
        anyhow::bail!(
            "replay is not deterministic: {} mismatch(es)",
            mismatches.len()
        )
    }
}

/// Writes the four joined audit tables into `dir`.
///
/// `decisions.csv`, `orders.csv`, `fills.csv` and `pnl.csv` all carry
/// `decision_id`, so the whole run can be reassembled — and every aggregate
/// in the report recomputed — with a spreadsheet and no access to this binary.
fn write_audit_csvs(dir: &std::path::Path, r: &RunResult) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let ledger = &r.ledger;

    let mut w = csv::Writer::from_path(dir.join("decisions.csv"))?;
    w.write_record([
        "decision_id",
        "timestamp_ms",
        "timestamp_utc",
        "market",
        "strategy",
        "side",
        "token",
        "outcome",
        "quantity",
        "best_bid",
        "best_ask",
        "spread",
        "mid",
        "imbalance",
        "reference_price",
        "forward_mid",
        "forward_elapsed_ms",
        "forward_ticks",
        "verdict",
    ])?;
    let px = |v: Option<Price>| v.map(|p| format!("{:.4}", p.to_f64())).unwrap_or_default();
    for d in ledger.decisions() {
        w.write_record([
            d.id.to_string(),
            d.ts_ms.to_string(),
            fmt_ms(d.ts_ms),
            d.market_slug.clone(),
            d.strategy.to_string(),
            d.side.to_string(),
            d.token.clone(),
            d.outcome.to_string(),
            format!("{:.6}", d.qty.to_f64()),
            px(d.best_bid),
            px(d.best_ask),
            px(d.spread),
            px(d.mid),
            format!("{:.6}", d.imbalance),
            format!("{:.4}", d.reference_price.to_f64()),
            px(ledger.forward_mid(d.id)),
            ledger
                .forward_elapsed_ms(d.id)
                .map(|e| e.to_string())
                .unwrap_or_default(),
            ledger
                .forward_ticks(d.id)
                .map(|t| t.to_string())
                .unwrap_or_default(),
            ledger.verdict(d.id).label().to_string(),
        ])?;
    }
    w.flush()?;

    let mut w = csv::Writer::from_path(dir.join("orders.csv"))?;
    w.write_record([
        "decision_id",
        "order_id",
        "side",
        "price",
        "quantity",
        "submit_time",
        "arrival_time",
        "executed",
        "terminal",
    ])?;
    for o in ledger.outcomes() {
        // Side and price come from the decision that authorised the order,
        // never from whatever happened to execute.
        let d = ledger.decision(o.decision_id);
        let intent = d.and_then(|d| d.intents.iter().find(|i| i.qty == o.requested));
        w.write_record([
            o.decision_id.to_string(),
            o.order_id.to_string(),
            d.map(|d| d.side.to_string()).unwrap_or_default(),
            intent
                .map(|i| format!("{:.4}", i.limit_price.to_f64()))
                .unwrap_or_default(),
            format!("{:.6}", o.requested.to_f64()),
            o.decided_ms.to_string(),
            o.arrive_ms.to_string(),
            format!("{:.6}", o.executed.to_f64()),
            format!("{:?}", o.terminal),
        ])?;
    }
    w.flush()?;

    let mut w = csv::Writer::from_path(dir.join("fills.csv"))?;
    w.write_record([
        "decision_id",
        "order_id",
        "fill_id",
        "timestamp_ms",
        "token",
        "side",
        "liquidity",
        "expected_price",
        "actual_price",
        "quantity",
        "slippage",
        "fee",
    ])?;
    for f in &r.fills {
        w.write_record([
            f.decision_id.to_string(),
            f.order_id.to_string(),
            f.fill_id.to_string(),
            f.ts_ms.to_string(),
            f.token.clone(),
            f.side.to_string(),
            format!("{:?}", f.liquidity),
            format!("{:.4}", f.reference_price.to_f64()),
            format!("{:.4}", f.price.to_f64()),
            format!("{:.6}", f.qty.to_f64()),
            format!("{:.6}", f.slippage_cost().to_f64()),
            format!("{:.6}", f.fee.to_f64()),
        ])?;
    }
    w.flush()?;

    let mut w = csv::Writer::from_path(dir.join("pnl.csv"))?;
    w.write_record([
        "decision_id",
        "realized_pnl",
        "unrealized_pnl",
        "fees",
        "net_pnl",
        "slippage",
        "requested",
        "executed",
        "fill_ratio",
        "verdict",
        "cause",
    ])?;
    for d in ledger.decisions() {
        let pnl = ledger.pnl(d.id);
        w.write_record([
            d.id.to_string(),
            format!("{:.6}", pnl.realized.to_f64()),
            format!("{:.6}", pnl.unrealized.to_f64()),
            format!("{:.6}", pnl.fees.to_f64()),
            format!("{:.6}", pnl.net.to_f64()),
            format!("{:.6}", pnl.slippage.to_f64()),
            format!("{:.6}", pnl.requested.to_f64()),
            format!("{:.6}", pnl.executed.to_f64()),
            format!("{:.6}", pnl.fill_ratio()),
            ledger.verdict(d.id).label().to_string(),
            ledger.cause(d.id).label().to_string(),
        ])?;
    }
    // The residual against the portfolio keeps the table honest: the ledger
    // splits notionals at lot boundaries, the portfolio at fill boundaries.
    let summary = ledger.summary(r.pnl.total_pnl);
    w.write_record([
        "ROUNDING".to_string(),
        String::new(),
        String::new(),
        String::new(),
        format!("{:.6}", summary.rounding.to_f64()),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
    ])?;
    w.write_record([
        "TOTAL".to_string(),
        format!("{:.6}", summary.realized.to_f64()),
        format!("{:.6}", summary.unrealized.to_f64()),
        format!("{:.6}", summary.fees.to_f64()),
        format!("{:.6}", r.pnl.total_pnl.to_f64()),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
    ])?;
    w.flush()?;
    Ok(())
}

/// Prints the decision-versus-execution breakdown.
///
/// Answers the only question that matters when a trade loses money: was the
/// strategy wrong, or was it right and the execution took the edge away?
fn print_decision_attribution(real: &RunResult, exec_ideal: &RunResult, explain: usize) {
    use crate::lineage::{Cause, Verdict};

    /// One decision seen under both execution regimes.
    struct Comparison {
        id: crate::lineage::DecisionId,
        verdict: Verdict,
        ideal_net: Usdc,
        real_net: Usdc,
        /// `ideal_net - real_net`: what execution cost this decision.
        gap: Usdc,
        cause: Cause,
    }

    // Match by what the decision *was*, not by counter: execution differences
    // can shift the counters between two runs.
    let ideal_by_key: std::collections::HashMap<_, _> = exec_ideal
        .ledger
        .decisions()
        .map(|d| (d.natural_key(), exec_ideal.ledger.pnl(d.id).net))
        .collect();

    let mut rows: Vec<Comparison> = Vec::new();
    let mut unmatched = 0usize;
    for d in real.ledger.decisions() {
        let real_net = real.ledger.pnl(d.id).net;
        let Some(&ideal_net) = ideal_by_key.get(&d.natural_key()) else {
            unmatched += 1;
            continue;
        };
        rows.push(Comparison {
            id: d.id,
            verdict: real.ledger.verdict(d.id),
            ideal_net,
            real_net,
            gap: ideal_net - real_net,
            cause: real.ledger.cause(d.id),
        });
    }

    println!("DECISION vs EXECUTION\n");
    println!("  Decisions compared      {}", rows.len());
    if unmatched > 0 {
        println!("  Not comparable          {unmatched}  (execution diverged the decision stream)");
    }

    let count = |v: Verdict| rows.iter().filter(|r| r.verdict == v).count();
    println!("    prediction CORRECT    {}", count(Verdict::Correct));
    println!("    prediction WRONG      {}", count(Verdict::Wrong));
    println!("    undetermined          {}", count(Verdict::Undetermined));
    println!();

    let sum = |f: &dyn Fn(&Comparison) -> bool| -> (usize, Usdc) {
        let sel: Vec<&Comparison> = rows.iter().filter(|r| f(r)).collect();
        (sel.len(), sel.iter().map(|r| r.real_net).sum())
    };

    let (n_dec_err, pnl_dec_err) = sum(&|r| r.verdict == Verdict::Wrong);
    let (n_exec_err, pnl_exec_err) = sum(&|r| r.verdict == Verdict::Correct && r.gap > Usdc::ZERO);
    let (n_clean, _) = sum(&|r| r.verdict == Verdict::Correct && r.gap <= Usdc::ZERO);

    println!("  DECISION ERROR          {n_dec_err} decisions   realised {pnl_dec_err}");
    println!("    the market moved against the signal");
    println!();
    println!("  EXECUTION ERROR         {n_exec_err} decisions   realised {pnl_exec_err}");
    println!("    the signal was right; execution gave the edge back");
    println!();
    println!("  CLEAN                   {n_clean} decisions");
    println!("    right signal, execution matched the ideal");
    println!();

    if n_exec_err > 0 {
        println!("  EXECUTION ERROR BREAKDOWN (dominant cause per decision)");
        let mut by_cause: std::collections::BTreeMap<&'static str, (usize, Usdc)> =
            Default::default();
        for r in rows
            .iter()
            .filter(|r| r.verdict == Verdict::Correct && r.gap > Usdc::ZERO)
        {
            let e = by_cause.entry(r.cause.label()).or_insert((0, Usdc::ZERO));
            e.0 += 1;
            e.1 += r.gap;
        }
        let mut ordered: Vec<_> = by_cause.into_iter().collect();
        ordered.sort_by_key(|(_, (_, gap))| std::cmp::Reverse(gap.0));
        for (cause, (n, gap)) in ordered {
            println!("    {cause:<20} {n:>3} decisions   gap {gap}");
        }
        println!();
    }

    // The worst offenders, in full lineage.
    rows.sort_by_key(|r| std::cmp::Reverse(r.gap.0));
    for Comparison {
        id,
        verdict,
        ideal_net,
        real_net,
        gap,
        cause,
    } in rows.into_iter().take(explain)
    {
        if gap <= Usdc::ZERO {
            break;
        }
        let Some(d) = real.ledger.decision(id) else {
            continue;
        };
        let pnl = real.ledger.pnl(id);
        println!("  Decision #{id}");
        println!("    market              {}", d.market_slug);
        println!("    strategy            {}", d.strategy);
        println!("    intent              {} {}", d.side, d.outcome);
        println!(
            "    observed book       bid {} / ask {}  spread {}",
            d.best_bid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".into()),
            d.best_ask
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".into()),
            d.spread
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".into()),
        );
        println!("    strategy prediction {}", verdict.label());
        if let (Some(before), Some(after)) = (d.mid, real.ledger.forward_mid(id)) {
            println!("    price moved         {before} -> {after}");
        }
        println!(
            "    requested / filled  {} / {}  ({:.0}%)",
            pnl.requested,
            pnl.executed,
            pnl.fill_ratio() * 100.0
        );
        println!("    ideal execution     {ideal_net}");
        println!("    actual execution    {real_net}");
        println!("    root cause          {}", cause.label());
        println!("    estimated impact    {}", -gap);
        println!();
    }
}
