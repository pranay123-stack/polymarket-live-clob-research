# polymarket-live-clob-research

A real-data Polymarket CLOB execution research platform that reconstructs live
market events and identifies where theoretical trading edge disappears under
realistic execution conditions.

Written in Rust. Records live Polymarket BTC 5-minute Up/Down order books,
replays them deterministically, simulates what a hypothetical order would
*actually* have done, and traces every resulting dollar back to the decision
that caused it.

Every number below is reproducible from a fresh clone. No synthetic markets,
no generated books, no invented price paths.

---

## Run it

```bash
git clone https://github.com/pranay123-stack/polymarket-live-clob-research
cd polymarket-live-clob-research
cargo build --release
```

Needs Rust 1.82+. Nothing else — no API key, no account, no config.

**Start here.** This runs on committed real market data, so it needs no
network and reproduces the numbers in this README exactly:

```bash
cargo run --release -- analyze --file tests/fixtures/session_btc_sample.jsonl
```

Takes ~20 seconds — it runs 32 replay passes to compute the Shapley
attribution. You should see `EDGE LOSS: $9.11` and a per-decision breakdown.

Three more, all offline:

```bash
# What's in the recording: events, markets, liquidity, feed delay, book integrity
cargo run --release -- inspect --file tests/fixtures/session_btc_sample.jsonl

# Prove the replay is deterministic — compares 6 stages across 2 runs
cargo run --release -- verify-replay --file tests/fixtures/session_btc_sample.jsonl

# Full audit trail: 4 joined CSVs + the 3 worst decisions in full lineage
cargo run --release -- analyze --file tests/fixtures/session_btc_sample.jsonl \
  --csv-dir audit/ --explain 3
```

And two that hit the live exchange (read-only — no orders, ever):

```bash
# Watch the ideal-vs-realistic gap accrue live. No arguments required.
cargo run --release -- shadow --seconds 120

# Capture your own session, then analyse it
cargo run --release -- record --seconds 300 --out data
cargo run --release -- analyze --file $(ls -t data/*.jsonl | head -1)
```

> **Reading the output:** the deliverable is the *measurement*, not a
> profitable strategy. The signal is deliberately naive — its only job is to
> generate realistic decisions so execution has something to be measured on.
> A losing P&L or a sub-50% hit rate is the tool working, not failing.

Full flag reference is in [Command reference](#command-reference) below.

---

## 1. Problem

A strategy looks profitable in simulation and loses money live. "Slippage" is
the usual explanation, and it is a single word standing in for at least five
distinct mechanisms that behave differently and cost different amounts.

Worse, the P&L line cannot tell you which happened. A losing trade where the
signal was wrong and a losing trade where the signal was right but the order
never filled look identical on a P&L statement — and they demand opposite
responses. One says fix the model. The other says fix the plumbing.

This platform separates them, on real market data.

## 2. Why backtests fail

A backtest that fills you at the touch, in full, instantly, for free is making
five errors at once:

| Assumption | Reality |
|-----------|---------|
| You see the current book | You see it ~210 ms late |
| Your order acts immediately | It arrives later, into a book that moved |
| Your passive order fills | You joined the back of a queue and may never be reached |
| You get the size you asked for | Depth is finite; you walk the ladder and pay for it |
| Trading is free | Fees apply |

Each is switchable here, so the cost of each is measurable rather than
conjectured. On the committed fixture the same strategy on the same data goes
from **+$4.00 at a 100% fill rate** to **−$5.11 at 34.4%**, and nearly half
the damage is queue position — the mechanism backtests model least and traders
discover last.

```
EDGE LOSS: $9.11

  Queue position / missed fills         $4.25    46.6%
  Stale market data                     $2.18    23.9%
  Order latency                         $1.61    17.7%
  Depth & slippage                      $1.06    11.7%
  Fees                                  $0.00     0.0%
```

On a longer 131,000-event session queue and missed fills reach 75.8%. This is
not a claim about a strategy. It is a claim about what execution costs.

## 3. Architecture

```
Polymarket ──▶ record ──▶ session.jsonl ──▶ replay ──▶ analyze
   live         (verbatim frames)         (deterministic)  (attribution)
     │                                          │
     │                                          └──▶ verify-replay
     └────────▶ shadow  (live, hypothetical orders, nothing sent)
```

| Command | What it does |
|---------|--------------|
| `record` | Captures real market data to a replayable session file. |
| `inspect` | Events, markets, liquidity, feed delay, book integrity. |
| `replay` | Deterministic replay with execution simulation. |
| `shadow` | Observes the live market, simulates orders, sends none. |
| `analyze` | Ideal vs realistic execution, attributed by factor and by decision. |
| `verify-replay` | Replays twice and proves every stage is identical. |

Design detail in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md); a full
review in [`docs/SYSTEM_AUDIT.md`](docs/SYSTEM_AUDIT.md).

## 4. Live data pipeline

Three public, read-only surfaces — Gamma metadata, CLOB REST reads, and the
public market WebSocket. Four things were verified against production rather
than taken from documentation, each of which would silently corrupt results:

- **`slug_contains` is accepted and ignored** by Gamma. Fortunately these
  slugs are deterministic — `btc-updown-5m-<5-minute epoch>`, where the
  timestamp is the round's open and close is exactly `+300` — so discovery is
  an exact lookup, not a search.
- **`price_change.size` is the level's new aggregate, not a delta.** Size `0`
  deletes.
- **`last_trade_price.side` is the aggressor**, measured rather than assumed:
  BUY prints preceded ask-side decreases 545 times against 154 bid-side; SELL
  preceded bid-side 79 against 13.
- **Gamma 403s on some default user agents**; `reqwest` sends none by default.

There are no sequence numbers on the market channel, so the recorder assigns
its own and says so wherever they appear. Full detail in
[`docs/DATA_MODEL.md`](docs/DATA_MODEL.md).

## 5. Replay engine

Sessions store **verbatim exchange frames**. Normalization runs on read
through the same decoder the live path uses, so live and replay cannot drift,
and fields this build does not model survive on disk for later.

The engine keeps two market states from that one stream:

- **truth** — everything the exchange had published by now. The book an
  arriving order actually meets.
- **observed** — everything that had reached us by now. All the strategy could
  possibly have known.

The strategy reads `observed`; the simulator fills against `truth`.
**Market-data latency therefore does not need to be a parameter** — it is in
the data. Measured p50 **214 ms**, p99 423 ms, against a clock offset of
**−6 ms ± 91 ms** (Cristian's algorithm against the millisecond-resolution
`/book` endpoint), so that delay is genuine transport, not a mis-set clock.

No wall clock, no randomness. `verify-replay` proves it stage by stage.

## 6. Execution model

Five switchable realism factors. What is measured, modelled and assumed is
stated at every point of use:

| Factor | Basis |
|--------|-------|
| Stale market data | **measured** from the recording's own timestamps |
| Order / cancel latency | **assumed** — unmeasurable without trading |
| Queue position | **modelled**, driven by real trade prints |
| Depth & slippage | **measured** by walking the real ladder |
| Fees | **configured**; observed at 0 bps on this market family |

Queue position deserves a note. Public data shows aggregate size per level,
never individual orders, so true queue position is unobservable. But trades
and cancellations *can* be told apart: a trade print says exactly how much
traded at what price on which side, and trades always consume the front. Any
level decrease beyond what trades explain is a cancellation — and where in the
queue a cancellation sat is genuinely unknowable. That residue is an explicit
`--queue-model` choice (`pessimistic` / `proportional` / `optimistic`) that
brackets the answer instead of hiding it.

[`docs/EXECUTION_MODEL.md`](docs/EXECUTION_MODEL.md) covers it line by line.

## 7. Shadow mode

Observes the live market and runs the strategy against it without sending
anything. Two simulators consume the same stream — one naive, one realistic —
so the reality gap accrues in view.

The recorded decisions are the strategy's own `Decision` objects. Side, token,
size and timestamp are read from them and are **never** reconstructed from an
execution result. That distinction is not academic: an earlier version
inferred the side from the market and reported a directional hit rate of 100%,
because the inference was constant. Reading the intent gives 37%.

Shadow mode cannot isolate market-data latency — live there is only one book,
the delayed one that arrived. Separating the two requires the recorded
`recv_ms`/`exchange_ms` pair, so that factor is measurable only in `replay`
and `analyze`.

## 8. Decision lineage

Every result traces back to the decision that caused it:

```
Decision ──▶ Intent ──▶ Order ──▶ Fill ──▶ Position change ──▶ P&L impact
```

The identity is **structural, not reconstructed**. A `Decision` owns its
`OrderIntent`s; there is no constructor that produces a loose intent, so an
order cannot exist without a decision to attribute it to. Fills carry
`decision_id`, `order_id` and their own `fill_id`, because one order walking
depth fills many times.

Four joined CSVs make the whole run auditable outside this binary:

```bash
cargo run --release -- analyze --file <session> --csv-dir audit/
```

| File | Columns |
|------|---------|
| `decisions.csv` | id, timestamp, market, strategy, side, token, observed book, imbalance, reference price, forward mid, verdict |
| `orders.csv` | decision_id, order_id, side, price, quantity, submit time, arrival time, terminal state |
| `fills.csv` | decision_id, order_id, fill_id, expected price, actual price, quantity, slippage, fee |
| `pnl.csv` | decision_id, realized, unrealized, fees, net, verdict, cause (+ ROUNDING, TOTAL) |

Tests re-parse these and recompute slippage, fees and P&L independently:
0 orphan rows, all `fill_id` unique, and per-decision net summing to the
portfolio total with a **$0.00** residual on the fixture.

## 9. Reality-gap attribution

**By factor**, using exact Shapley values across all `2^5 = 32` subsets. A
sequential waterfall is cheaper, but its answer depends on the order factors
are enabled. That is not a theoretical concern — on the committed fixture the
two methods **disagree about which cause is largest**:

| | Waterfall (6 runs) | Shapley (32 runs) |
|---|---|---|
| #1 cause | Stale market data **43.9%** | Queue position **46.6%** |
| #2 cause | Queue position 35.7% | Stale market data 23.9% |
| #3 cause | Depth & slippage 15.0% | Order latency 17.7% |
| **Total** | **$9.11** | **$9.11** |

Both are arithmetically correct and sum to the same figure. The waterfall
ranks stale data first because it happens to be enabled first and absorbs the
interaction with everything after it. Shapley is order-independent, so it is
the one to act on. Run both with `--attribution`.

The residue from integer division is reported as its own row rather than
absorbed.

**By decision**, answering the question a P&L line cannot:

```
  DECISION ERROR          3 decisions   realised -$1.15
    the market moved against the signal

  EXECUTION ERROR         2 decisions   realised -$1.17
    the signal was right; execution gave the edge back

  EXECUTION ERROR BREAKDOWN (dominant cause per decision)
    queue position         2 decisions   gap $2.17
```

Down to a single decision:

```
  Decision #5
    intent              SELL Up
    observed book       bid 0.1600 / ask 0.1700  spread 0.0100
    strategy prediction CORRECT
    price moved         0.1650 -> 0.1350
    requested / filled  100.00 / 0.09  (0%)
    ideal execution     $2.00
    actual execution    $0.00
    root cause          queue position
    estimated impact    -$2.00
```

The signal was right and the market moved 300 ticks its way. The passive order
sat behind the queue, filled 0.09 of 100 shares, and made nothing. The edge
existed; the execution never happened.

The per-decision comparison holds market-data latency on for both legs, so the
decision streams align and a like-for-like comparison is possible. Decisions
that still diverge — because a position cap bound differently — are matched on
token, timestamp and side, and the unmatched count is printed rather than
dropped.

Walkthrough with real output in [`docs/DEMO.md`](docs/DEMO.md).

## 10. Testing methodology

**177 tests — 121 unit, 56 integration — all running on real recorded
Polymarket data.**

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
cargo run --release -- verify-replay --file tests/fixtures/session_btc_sample.jsonl
```

The fixture is a verbatim slice of a live recording: 8,784 events, 8 tokens,
8,708 level updates, 55 trade prints across both aggressor sides.

What the suite actually pins down:

- **Determinism** across all 32 realism configurations, and stage by stage in
  `verify-replay` — book checksum, decisions, orders, fills, P&L, attribution.
- **Accounting** — `equity − cash == realized + unrealized − fees` exactly, no
  tolerance, under all 32 configurations.
- **Book correctness** — complementary Up/Down books must sum to exactly
  `1.0000` at equal size, an independent check the reconstruction cannot fake.
- **Lineage** — no orphan orders or fills, order sizes summing to their
  decision, fill sides matching their decision's side.
- **CSV integrity** — exported, re-parsed, and recomputed independently.
- **Read-only** — the source is scanned for key handling and CLOB write paths;
  the build fails if any appear.

[`docs/TESTING.md`](docs/TESTING.md) has the detail.

## 11. Limitations

Stated plainly, because a number resting on a hidden assumption is worse than
no number.

- **Submission and cancellation latency are assumptions, not measurements.**
  They cannot be observed without sending real orders. Defaults are 120 ms;
  the right use is to sweep them, and the report labels them as assumptions
  wherever it prints them. This is the load-bearing uncertainty in the
  *absolute* edge-loss figure. The *relative* ranking of causes is far more
  robust, because queue position and depth are measured.
- **No market impact.** A hypothetical order never removes real liquidity.
  This understates the cost of large orders and is sound only for sizes small
  against displayed depth.
- **No reaction from other participants.** Real makers would respond to a
  persistent counterparty.
- **Queue position is modelled**, and cancellation placement is unknowable
  from public data — run all three `--queue-model` options to bracket it.
- **Per-decision P&L credits round trips to the opening decision.** A pure
  exit books no P&L of its own. The alternative spreads one round trip across
  two decisions; both give the same total. It is a choice, and it is stated.
- **Decision verdicts need sessions longer than the horizon.** The committed
  fixture spans 13 seconds against a 30-second default, so its verdicts use a
  truncated window — recorded per row as `forward_elapsed_ms` so this is
  visible rather than implied.
- **The reference signal is not a trading strategy.** Deliberately naive
  order-book imbalance, present only to generate realistic decisions to
  measure execution on. **No profitability is claimed, for it or anything
  else.**
- **Realism does not always cost money on a single session.** Factors can
  offset; on one live 120-second recording the net gap was **−$0.96**, stale
  data *gaining* $67.77 against queue position costing $65.88. `analyze`
  suppresses the percentage split when the net is smaller than its parts
  rather than manufacturing a tidy number.
- **Results are vantage-point specific.** A ~210 ms feed delay reflects one
  network location. Co-located infrastructure would see a different floor —
  and would still face queue position and depth.
- **One market family.** BTC 5-minute Up/Down; the slug pattern generalises to
  ETH, SOL, XRP and DOGE via `--underlying`. Nothing else is claimed.

This is a research and execution-analysis platform. It is not production
trading infrastructure and makes no claim to be.

---

## Command reference

Every command supports `--help`. Only the file-reading commands require an
argument; `record` and `shadow` discover live markets themselves.

### `record` — capture live market data

```bash
cargo run --release -- record --seconds 600 --out data
```

| Flag | Default | Meaning |
|------|---------|---------|
| `--underlying` | `btc` | `btc`, `eth`, `sol`, `xrp`, `doge` |
| `--seconds` | `600` | How long to record |
| `--out` | `data` | Output directory |
| `--lookahead` | `3` | Future rounds to subscribe to, to catch market opens |
| `--refresh` | `60` | Seconds between market re-discovery passes |

Writes `data/session_<coin>_<ts>.jsonl`. Roughly 230 KB/s, so an hour is
~800 MB. Ctrl-C stops early and keeps the file.

### `inspect` — summarise a session

```bash
cargo run --release -- inspect --file <session.jsonl> [--detail]
```

Events by type, markets, per-token liquidity, measured feed delay, clock
offset, and book-integrity counters (stale rejections, crossed books, deltas
before snapshot).

### `replay` — deterministic replay with execution simulation

```bash
cargo run --release -- replay --file <session.jsonl>
```

Accepts every simulation flag below.

### `analyze` — the full report

```bash
cargo run --release -- analyze --file <session.jsonl> [--csv-dir DIR] [--explain N]
```

| Flag | Default | Meaning |
|------|---------|---------|
| `--csv-dir` | – | Write `decisions.csv`, `orders.csv`, `fills.csv`, `pnl.csv` |
| `--csv` | – | Write just the per-factor attribution table |
| `--fills-csv` | – | Write just the per-fill table |
| `--explain` | `3` | Print the N worst decisions in full lineage |
| `--attribution` | `shapley` | `shapley` (32 runs, order-independent) or `waterfall` (6 runs, cheaper) |

### `shadow` — live observation, nothing sent

```bash
cargo run --release -- shadow --seconds 300 [--record-to data]
```

| Flag | Default | Meaning |
|------|---------|---------|
| `--underlying` | `btc` | Which market family to follow |
| `--seconds` | `300` | How long to observe |
| `--record-to` | – | Also save the frames, so you can replay them later |

### `verify-replay` — prove determinism

```bash
cargo run --release -- verify-replay --file <session.jsonl> [--runs 3]
```

Compares order book checksum, decisions, orders, fills, P&L and attribution
across runs. Exits non-zero if any stage differs.

### Simulation flags

Shared by `replay`, `analyze`, `shadow` and `verify-replay`.

**Execution realism:**

| Flag | Default | Notes |
|------|---------|-------|
| `--md-latency-ms` | `-1` | `-1` means *measure it from the recording*. A positive value overrides with a fixed delay, for sensitivity analysis. |
| `--submit-latency-ms` | `120` | **Assumption** — cannot be measured without trading. Sweep it. |
| `--cancel-latency-ms` | `120` | **Assumption** — same. |
| `--queue-model` | `pessimistic` | `pessimistic`, `proportional`, `optimistic`. Run all three to bracket the answer. |
| `--taker-fee-bps` | `0` | Observed at 0 on this market family. |
| `--maker-fee-bps` | `0` | Same. |

**The strategy being measured:**

| Flag | Default | Notes |
|------|---------|-------|
| `--style` | `split` | `passive`, `aggressive`, or `split` (half of each) |
| `--order-shares` | `100` | Size per decision. Keep small against displayed depth — there is no market-impact model. |
| `--max-position` | `500` | Position cap per token |
| `--signal-threshold` | `0.35` | Imbalance magnitude required to act |
| `--signal-depth` | `5` | Book levels in the imbalance calculation |
| `--decision-cooldown-ms` | `2000` | Minimum gap between decisions on one token |
| `--order-ttl-ms` | `5000` | How long a resting order waits before cancelling |
| `--decision-horizon-ms` | `30000` | When the midpoint judges whether the decision was right |
| `--cash` | `10000` | Starting cash, in dollars |

### Worked examples

```bash
F=tests/fixtures/session_btc_sample.jsonl

# Sweep the one input that is an assumption rather than a measurement
for l in 20 120 500; do
  cargo run --release -- analyze --file $F \
    --submit-latency-ms $l --cancel-latency-ms $l
done

# Bracket the queue assumption
for q in pessimistic proportional optimistic; do
  cargo run --release -- replay --file $F --queue-model $q
done

# Passive-only: queue position with no aggressive leg to hide behind
cargo run --release -- replay --file $F --style passive

# See why the attribution method matters
cargo run --release -- analyze --file $F --attribution waterfall
cargo run --release -- analyze --file $F --attribution shapley

# Watch ETH instead, and keep the data
cargo run --release -- shadow --underlying eth --seconds 300 --record-to data
```

### Tests

```bash
cargo test --all          # 177 tests, ~90s
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
```

## Safety

Read-only by construction. No private keys, no signing, no wallet code, no
order placement, modification or cancellation endpoints — only public reads.
`shadow` is the closest thing to live trading here and it only ever writes to
a report. A test enforces this by scanning the source.

## Documentation

| | |
|---|---|
| [`docs/SYSTEM_AUDIT.md`](docs/SYSTEM_AUDIT.md) | Correctness review, failure modes, confidence assessment |
| [`docs/DEMO.md`](docs/DEMO.md) | Four-step walkthrough with real output |
| [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) | System design and module map |
| [`docs/DATA_MODEL.md`](docs/DATA_MODEL.md) | Verified wire formats, event model, session format |
| [`docs/EXECUTION_MODEL.md`](docs/EXECUTION_MODEL.md) | Latency, queue, depth, fees, attribution |
| [`docs/TESTING.md`](docs/TESTING.md) | Test strategy and the real-data fixture |
| [`data/README.md`](data/README.md) | Recording notes and file sizes |
