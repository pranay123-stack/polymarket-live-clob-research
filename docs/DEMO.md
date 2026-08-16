# Demo walkthrough

Four steps, from live market to attributed report. Every figure below is real
output; nothing is illustrative.

Steps 2–4 run on the committed fixture, so they reproduce exactly from a
fresh clone with no network access.

---

## Step 1 — Capture real market data

```bash
cargo run --release -- record --seconds 600 --out data
```

```
POLYMARKET LIVE RECORDER

Market:
BTC 5 Minute Up/Down

Following 4 market(s), 8 token(s)

  t+   46s   frames     26075   markets   4   feed delay p50 178 ms
  t+   92s   frames     53018   markets   4   feed delay p50 177 ms

--- RECORDING COMPLETE ---

Frames:
68210

Feed delay (recv - exchange), ms:
n=68210 min=168 p50=176 p90=193 p99=361 max=3537

Clock offset at start:
+30 ms (+/-91 ms, rtt 182 ms) -> not distinguishable from zero

Saved:
data/session_btc_1786847735.jsonl
```

The clock probe is what makes the feed-delay number mean anything: the offset
is indistinguishable from zero, so the ~176 ms is genuine transport delay
rather than a mis-set local clock.

Markets roll every five minutes. The recorder re-discovers the upcoming window
and resubscribes when the token set changes; because the exchange republishes
a full snapshot per token every ~1.5 s, that costs a snapshot, not book
integrity.

---

## Step 2 — Replay the market

```bash
cargo run --release -- replay --file tests/fixtures/session_btc_sample.jsonl
```

```
Events replayed:
8784

Measured feed delay, ms:
n=8780 min=210 p50=214 p90=247 p99=423 max=4335

Realistic execution:
  P&L                   -$5.11
  decisions             13
  orders submitted      26
    filled in full      1
    filled in part      12
    expired unfilled    13
  fill ratio            34.4%
  accounting identity   exact

Book integrity:
  snapshots             17
  level updates         8708
  stale rejected        0
  crossed observed      2
```

`stale rejected 0` across 8,708 real updates is the check that the
reconstruction is not quietly dropping data.

### Prove the replay is deterministic

```bash
cargo run --release -- verify-replay --file tests/fixtures/session_btc_sample.jsonl
```

```
Order book:        PASS
Strategy decisions: PASS
Orders:            PASS
Fills:             PASS
P&L:               PASS
Attribution:       PASS

Book checksum:
b7eeaf56937f0e42

Compared:
13 decisions, 26 orders, 25 fills across 2 runs

IDENTICAL RESULT
```

Each stage is compared separately. Final P&L alone is a weak check: two
offsetting execution differences leave it unchanged while the book, the
decisions or the fills have diverged.

---

## Step 3 — Run shadow analysis

```bash
cargo run --release -- shadow --seconds 90
```

```
POLYMARKET SHADOW MODE

Observing 4 market(s) for 90 seconds.
No orders are sent. Nothing here can trade.

Hypothetical decisions:
47

Forward move resolved:
35 of 47 (13 moved favourably)

Ideal execution P&L:
$17.00

Realistic execution P&L:
-$61.06

EDGE LOSS:
$78.06

Fill ratio:
100.0% ideal -> 28.2% realistic

No orders were sent at any point.
```

Two simulators consume the same live stream — one under naive assumptions, one
under full realism — so the gap accrues in view. The recorded decisions are
the strategy's own `Decision` objects; side, token, size and timestamp are
read from them and never reconstructed from an execution result.

---

## Step 4 — Generate the attribution report

```bash
cargo run --release -- analyze \
  --file tests/fixtures/session_btc_sample.jsonl \
  --csv-dir audit/ --explain 2
```

```
Ideal execution (naive backtest assumptions):
  P&L                   $4.00
  fill ratio            100.0%

Realistic execution:
  P&L                   -$5.11
  fill ratio            34.4%

EDGE LOSS:
$9.11

CAUSE:

  Queue position / missed fills         $4.25    46.6%
  Stale market data                     $2.18    23.9%
  Order latency                         $1.61    17.7%
  Depth & slippage                      $1.06    11.7%
  Fees                                  $0.00     0.0%
  rounding                              $0.00
```

Then the question that matters — was the signal wrong, or was it right and
execution took the edge away?

```
DECISION vs EXECUTION

  Decisions compared      11
  Not comparable          2  (execution diverged the decision stream)
    prediction CORRECT    3
    prediction WRONG      3
    undetermined          5

  DECISION ERROR          3 decisions   realised -$1.15
    the market moved against the signal

  EXECUTION ERROR         2 decisions   realised -$1.17
    the signal was right; execution gave the edge back

  CLEAN                   1 decisions
    right signal, execution matched the ideal

  EXECUTION ERROR BREAKDOWN (dominant cause per decision)
    queue position         2 decisions   gap $2.17
```

And any single decision, in full lineage:

```
  Decision #5
    market              btc-updown-5m-1786845300
    strategy            imbalance_signal
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

**This is the whole thesis in one record.** The signal was right — it sold at
0.1650 and the market fell to 0.1350. Under naive backtest assumptions that
decision makes $2.00. In reality the passive order sat behind the queue and
filled 0.09 of the 100 shares it wanted, so it made nothing. The edge was
never in doubt; the execution simply never happened.

---

## The audit trail

```
audit/
  decisions.csv   13 rows
  orders.csv      26 rows
  fills.csv       25 rows
  pnl.csv         13 rows (+ ROUNDING, TOTAL)
```

All four join on `decision_id`, so the run can be reassembled and every
aggregate recomputed without this binary. Verified independently:

```
orphan orders                     0
orphan fills                      0
fill_id unique                    True
slippage rows failing recompute   0
per-decision net sum              -5.115150
  + rounding                       0.000000
TOTAL row                         -5.115150   balances
```

`decisions.csv` carries the observed book, the imbalance that triggered the
decision, the forward midpoint, the window actually achieved
(`forward_elapsed_ms`) and the verdict. `fills.csv` carries both the price the
strategy was looking at and the price it got, so slippage is re-derivable
rather than taken on trust.

---

## Notes on reading these numbers

The fixture is 13 seconds of one market's life. It demonstrates the machinery;
it is not a result. Run `record` for ten minutes and the picture changes —
sometimes substantially.

Realism is not guaranteed to cost money on a short session. On one live
120-second recording the net gap came out at **−$0.96**, with stale data
*gaining* $67.77 against queue position costing $65.88. When the factors
offset like that, `analyze` suppresses the percentage split rather than
reporting a share of a number smaller than its own parts.

Submission and cancellation latency are assumptions. Sweep them:

```bash
for l in 20 60 120 250 500; do
  cargo run --release -- analyze --file data/session.jsonl \
    --submit-latency-ms $l --cancel-latency-ms $l
done
```
