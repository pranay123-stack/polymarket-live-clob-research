# Execution model

This document states exactly what is measured, what is modelled, and what is
assumed. The distinction is the whole value of the tool: a number that looks
precise but rests on an invented parameter is worse than no number.

## The counterfactual

Every run is parameterised by a set of realism factors. All off reproduces
the assumptions a naive backtest makes; all on applies every constraint that
can be defended from real data.

| Factor | Off (ideal) | On (realistic) | Basis |
|--------|-------------|----------------|-------|
| `MdLatency` | strategy sees the exchange-time book | sees it delayed by the recording's own per-event delay | **measured** |
| `OrderLatency` | orders act at decision time | arrive after `submit_ms`; cancels land after `cancel_ms` | **assumed** |
| `Queue` | passive orders fill in full, instantly | join behind real resting size; may never fill | **modelled** |
| `Depth` | full size at the touch price | walk real levels; partial fills; slippage | **measured** |
| `Fees` | free | charged per fill in bps | **configured** |

## 1. Market-data latency — measured

This is the factor most tools guess at, and the one this design does not have
to. Each recorded event carries both `exchange_ms` and `recv_ms`, so the
delay is a property of the data.

Measured on the committed fixture:

```
n=8780  min=210  p50=214  p90=247  p99=423  max=4335   (milliseconds)
```

That floor is not a clock artefact. A Cristian's-algorithm probe against
`GET /book` — chosen because its `timestamp` has millisecond resolution,
unlike `GET /time` — put the local-to-exchange offset at **−6 ms ± 91 ms**,
i.e. indistinguishable from zero. The ~210 ms is genuine transport delay from
this vantage point.

The delay line releases each event to the strategy's view at its own recorded
`recv_ms`, so the simulation uses the real per-event delay including its
tail, not a flat average. `--md-latency-ms` substitutes a fixed value for
sensitivity analysis only.

## 2. Order and cancellation latency — assumed

**These cannot be measured without sending real orders, which this tool never
does.** They are parameters, defaulting to 120 ms each, and the report labels
them as assumptions every time it prints them.

The honest way to use them is to sweep:

```bash
for l in 20 60 120 250 500; do
  cargo run --release -- analyze --file data/session.jsonl \
    --submit-latency-ms $l --cancel-latency-ms $l
done
```

Cancellation latency is not cosmetic. A cancel requested at the time-to-live
only lands after `cancel_ms`, and the order remains fillable throughout —
which is precisely the risk of trying to pull an order in a fast market.

## 3. Queue position — modelled, but driven by real events

Polymarket publishes aggregate size per level, never individual orders, so a
resting order's true queue position **is not observable**. It is modelled: on
arrival the order joins *behind* all size currently displayed at its price.

What lifts this above a guess is that trades and cancellations can be told
apart:

* A `last_trade_price` print says exactly how much traded, at what price, on
  which side. **Trades always consume the front of the queue**, so they
  reduce the size ahead of us by a known amount.
* Any level decrease beyond what trades explain is a **cancellation**, and a
  cancellation's position in the queue is genuinely unknowable.

Trade prints are credited against subsequent level decreases so the same size
is never counted twice — once as a trade and again as a cancellation.

The unknowable part is exposed as an explicit choice rather than buried:

| `--queue-model` | Assumption | Character |
|-----------------|------------|-----------|
| `pessimistic` (default) | every cancel came from behind; queue never improves | conservative bound |
| `proportional` | cancels split in proportion to queue ahead and behind | neutral |
| `optimistic` | every cancel came from ahead; queue improves fully | upper bound |

The default is pessimistic, and it is closer to reality than it first looks:
the orders most likely to cancel are informed ones near the front reacting to
news, which is adverse selection working against you.

Running all three brackets the answer. `tests/execution_tests.rs` asserts the
fill ratios come out ordered pessimistic ≤ proportional ≤ optimistic.

## 4. Depth and slippage — measured

An aggressive order walks the real recorded ladder outward from the touch,
stopping at its limit. It fills what was actually there, at the prices that
were actually displayed, and whatever the book cannot supply is a partial
fill.

`--fills-csv` exports one row per fill carrying both prices, so every
aggregate in the report can be recomputed from its own raw material.

Slippage is measured against the strategy's **reference price** — the touch
it was looking at when it decided. That baseline is chosen deliberately: it
is exactly the price a naive backtest assumes it received, so the reported
number is the error such a backtest makes.

## 5. Fees — configured

Every `fee_rate_bps` observed on live BTC 5-minute prints was `0`. That is a
measurement, not an assumption that trading is free: other Polymarket markets
do charge, so `--taker-fee-bps` and `--maker-fee-bps` exist and default to
the observed zero. On this market family the fee term is genuinely ~0% of
edge loss, and the report shows it as such rather than hiding it.

## Attributing the gap

Switching factors on one at a time — a waterfall — costs `k+1` runs but the
answer depends on the order chosen, because whichever factor goes first
absorbs all the interaction. Latency and queue position interact strongly
here, so the difference is not cosmetic.

The default is the **Shapley value**, the unique attribution that is
order-independent, gives identical factors identical credit, assigns nothing
to a factor that changes nothing, and sums exactly to the total:

```
φ_i = Σ_{S ⊆ N∖{i}}  |S|! (n−|S|−1)! / n!  ·  [ v(S ∪ {i}) − v(S) ]
```

with `v(S)` the edge lost when exactly the factors in `S` are active. It
costs `2^5 = 32` runs, affordable because a run is one streaming pass — about
0.8 seconds for 131,000 events.

Contributions accumulate as `i128` numerators over a common denominator, and
whatever the final division truncates is reported as an explicit `rounding`
row rather than quietly dropped, so the parts always sum to the whole.

Use `--shapley false` for the cheaper waterfall. Both report the same total;
only the split differs.

## Accounting

```
equity − starting_cash  ==  realized + unrealized − fees
```

Checked **exactly**, with no tolerance, after every run and under all 32
realism configurations in the test suite. It holds because positions track a
signed **cost basis in USDC** rather than an average price: reducing a
position moves an exact integer share of the basis, and the truncation stays
inside the basis instead of escaping as phantom P&L.

Unmarkable positions — a token whose book has gone empty — are held at cost,
contributing zero unrealized P&L. Marking them to an invented price would
manufacture profit.

## What this model does not do

**No market impact.** A hypothetical order never removes real liquidity, so
the replayed book is left untouched. Fills are read off the book that
actually existed. This understates the cost of large orders; it is sound for
sizes small against displayed depth, and `--order-shares` should be kept
there. A test asserts a 5,000-share order never fills a greater *fraction* of
itself than a 50-share one.

**No reaction from other participants.** Real market makers would respond to
a persistent counterparty. Nothing here models that.

**No adverse selection beyond what the tape shows.** The queue model captures
trades consuming the front, not the informational reason they arrived.

**Signed positions.** Polymarket has no naked short — selling a token you do
not hold is economically buying its complement at `1 − p`. The simulator
tracks a signed position because it is the clearer accounting object. Since
this crate never sends an order the distinction has no execution consequence
here; it would matter to any live implementation.

**Latency is symmetric and constant per run.** Real submission latency has a
distribution with a tail, and the tail is where the damage concentrates.
Sweeping the parameter is a substitute for modelling it, not an equal.
