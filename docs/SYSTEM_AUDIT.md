# System audit

A review of `polymarket-live-clob-research` for correctness, auditability,
event lineage and reproducibility. Written against the code as it stands, not
against its intent.

**Scope:** data flow, strategy intent flow, order lifecycle, fill lifecycle,
P&L attribution, shadow mode, CSV exports, replay determinism.

---

## 1. Areas reviewed

| Area | Verified by |
|------|-------------|
| Wire decoding | Live capture; fixtures are verbatim frames from production |
| Book reconstruction | 8,708 real level updates replayed with 0 stale rejections |
| Complementary consistency | Up bid + Down ask sums to exactly `1.0000` at equal size |
| Strategy intent flow | Decisions own their intents; no downstream inference path exists |
| Order lifecycle | Every order cites a decision that exists; arrival never precedes decision |
| Fill lifecycle | Every fill cites an order that exists; fill ids unique |
| P&L attribution | Portfolio identity exact under all 32 realism configurations |
| Per-decision P&L | Ledger reconciles to the portfolio; residual reported, measured at $0.00 |
| Replay determinism | Book checksum, decisions, orders, fills, P&L and attribution compared per run |
| CSV exports | Re-parsed and recomputed independently in tests |
| Read-only guarantee | Source scanned for key handling and CLOB write paths |

---

## 2. Architecture strengths

**Market-data latency is measured, not assumed.** Every recorded event carries
both `exchange_ms` and `recv_ms`. Replay builds two states from that one
stream — `truth`, which an arriving order meets, and `observed`, which is all
the strategy could have known. Most tools parameterise this; here it is a
property of the data. Measured p50 214 ms against a clock offset of
−6 ms ± 91 ms, so the delay is transport, not a mis-set clock.

**One decoder, two paths.** Sessions store verbatim exchange frames, and
normalization runs on read through the same `Normalizer` the live path uses.
Live and replay cannot drift, because there is exactly one decoder in the
crate. Unmodelled fields survive on disk, so a session recorded today stays
useful after the decoder learns more.

**Lineage is structural.** A `Decision` owns its `OrderIntent`s. There is no
constructor producing a loose intent, so an order cannot exist without a
decision, and nothing downstream can invent one.

**Accounting cannot silently drift.** Positions track a signed cost basis in
integer USDC rather than an average price, so reducing a position moves an
exact integer share and truncation stays inside the basis.
`equity − starting_cash == realized + unrealized − fees` holds exactly, with
no tolerance, and is asserted under all 32 realism configurations.

**Rounding is reported, never absorbed.** Shapley attribution and the
per-decision ledger both publish their residual as an explicit row. A table
that balanced by construction would hide exactly the arithmetic error worth
knowing about.

**The unknowable is a parameter, not a default.** Queue position cannot be
observed from public data. Rather than picking one assumption, `--queue-model`
exposes pessimistic / proportional / optimistic, and a test asserts the fill
ratios come out ordered, so running all three brackets the answer.

---

## 3. Hidden failure modes

Ordered by how much damage they could do before anyone noticed.

### 3.1 Trade-versus-cancel attribution is order-sensitive — *live*

The queue model credits a trade print against subsequent level decreases so
the same size is not counted twice. If a level decrease arrives **before** its
trade print, the decrease is booked as a cancellation and the credit is never
consumed.

*Effect:* under `optimistic`/`proportional` queue models, the queue advances
when it should not, overstating fills. `pessimistic` — the default — is
immune, because it ignores cancellations entirely.

*Mitigation:* default is pessimistic; behaviour is asserted by
`trade_driven_decreases_are_not_double_counted_as_cancellations`.
*Not fixed.* Correcting it needs a reorder buffer keyed on exchange
timestamp, which trades a real complexity cost for a bounded gain on a
non-default path.

### 3.2 Truth ordering assumes the file is in exchange order — *live*

`truth` is fed in file order, which is receive order. Feed delay varies from
207 ms to 4335 ms, so a badly delayed frame can carry an `exchange_ms` earlier
than a frame already applied.

*Effect:* the book rejects it as stale and counts it. On the reference session
this happened **0 times** in 131,460 updates, and the counter is printed by
`inspect` and `replay` so it can never be silently non-zero.

*Mitigation:* observable, counted, reported. *Not fixed:* a reorder window
would add latency to the live path to correct an event that has not yet been
observed to occur.

### 3.3 Decision streams diverge between runs — *live, and reported*

Per-decision comparison runs realistic and ideal execution over the same
observed view, so decisions should align. They can still diverge: the position
cap binds at different moments when fills differ.

*Effect:* some decisions are not comparable, and the share is **not small**:
2 of 13 on the committed fixture, but 9 of 19 on a live 75-second session.
Ideal execution fills in full, so the position cap binds sooner and the two
worlds drift apart faster the longer they run.

*Mitigation:* matched on `Decision::natural_key` — token, timestamp, side —
rather than on a counter, and the unmatched count is **printed** on every
report rather than dropped. This is a limitation of the question, not a
defect: a decision that exists in only one world has no counterpart to compare
against. Raising `--max-position` reduces the divergence, at the cost of
changing the strategy being measured. Read the per-decision table as a sample
of comparable decisions, not as the whole session — the session-level Shapley
attribution is what covers everything.

### 3.4 A short session cannot judge its own decisions — *live, and disclosed*

The verdict needs a forward midpoint. The committed fixture spans 13 seconds
of event time, shorter than the 30-second default horizon.

*Effect:* without handling, every verdict would read `UNDETERMINED` — which is
what it did before this audit.

*Mitigation:* outstanding decisions are judged against the last book that
existed, and `forward_elapsed_ms` records the window actually achieved, so a
verdict from a truncated window is visible as such in `decisions.csv`. A zero
window is refused outright.

### 3.5 No market impact — *by design, and material*

A hypothetical order never removes real liquidity, so the replayed book is
untouched. This **understates** the cost of large orders and is sound only for
sizes small against displayed depth.

*Mitigation:* `a_larger_order_never_fills_a_greater_fraction_of_itself`
asserts the direction of the effect. Documented in `EXECUTION_MODEL.md`.
*Cannot be fixed* from public data without modelling other participants'
reactions, which would replace a measurement with a simulation.

### 3.6 Per-decision P&L attributes round trips to the opener — *a choice*

FIFO lots are tagged with the decision that opened them; when a later fill
closes a lot, the profit is credited to the **opening** decision.

*Effect:* a decision that only exits a position books no P&L of its own, even
though exiting is a skill.

*Rationale:* the alternative — marking every fill to the final price — spreads
one round trip across two decisions and makes "did this decision make money?"
unanswerable. The choice is stated here and in `lineage.rs` because it is a
choice, not a fact. Both decompositions produce the same total.

### 3.7 The reference signal is not a strategy — *deliberate*

`imbalance_signal` is naive and is not claimed to be profitable. Its only
required property is producing realistic decisions at realistic moments so
execution has something to be measured on.

*Risk:* a reader mistaking the P&L for a result. *Mitigation:* stated in the
README, in `strategy.rs`, and in the footer of every `analyze` run.

---

## 4. Assumptions

| Assumption | Status | Where it bites |
|------------|--------|----------------|
| `price_change.size` is the level's new aggregate, not a delta | **Verified live** | Book would silently corrupt |
| `last_trade_price.side` is the aggressor | **Verified live**, 545:154 and 79:13 | Queue model inverts |
| Up/Down slugs are `<coin>-updown-5m-<5-min epoch>` | **Verified live** | Discovery finds nothing |
| Submission latency (120 ms default) | **Assumption** — unmeasurable without trading | Sweep it |
| Cancellation latency (120 ms default) | **Assumption** — same | Sweep it |
| Cancellations come from behind the queue | **Model**, default of three | Bracket with all three |
| Own orders do not move the market | **Model** | Large orders only |
| Local clock is disciplined | **Measured** each session, ±91 ms | Reported by `inspect` |
| Fees are 0 bps | **Observed** on this market family | Configurable |

---

## 5. Verification performed

```
cargo fmt --all --check                                   clean
cargo clippy --all-targets --all-features -- -D warnings  clean
cargo test --all                                          176 passed, 0 failed
verify-replay --file tests/fixtures/session_btc_sample.jsonl
                                                          IDENTICAL RESULT
```

Determinism is checked stage by stage rather than on final P&L alone, because
two offsetting execution differences can leave P&L unchanged while the book,
the decisions or the fills have diverged. The book checksum is FNV-1a over
every populated level in a fixed order.

The audit CSVs were re-parsed outside the binary: 0 orphan orders, 0 orphan
fills, all `fill_id` unique, every slippage figure recomputed from
`(actual − expected) × quantity × side`, and per-decision net P&L summing to
the portfolio total with a residual of **$0.00**.

---

## 6. Confidence assessment

| Property | Confidence | Basis |
|----------|-----------|-------|
| Wire decoding matches the exchange | **High** | Verified against production; fixtures verbatim |
| Book reconstruction is correct | **High** | Complementary Up/Down consistency is a strong independent check |
| Replay is deterministic | **High** | Six stages compared, all 32 realism configurations |
| Accounting is exact | **High** | Integer arithmetic, identity asserted with no tolerance |
| Lineage is complete | **High** | Structural, plus orphan checks on real data |
| Market-data latency is real | **High** | Measured, with clock offset separately bounded |
| Depth and slippage modelling | **Medium-high** | Real ladders, but no market impact |
| Queue position modelling | **Medium** | Trades measured, cancellations modelled; bracketed by three options |
| Order/cancel latency | **Low by construction** | Assumptions; labelled as such everywhere |
| Decision verdicts | **Medium** | Sound method, but needs sessions longer than the horizon |
| Per-decision comparability | **Medium-low** | Up to half of decisions may not exist in both worlds; count is reported |
| Strategy quality | **Not assessed** | Out of scope; deliberately naive |

**Overall.** The measurement infrastructure is sound and its numbers are
traceable end to end. The load-bearing weakness is not in the code: two of the
five execution factors rest on parameters that cannot be measured without
trading. The system's response is to label them at every point of use and make
sweeping them cheap, which is the correct response, but it means the
*absolute* edge-loss figure carries the uncertainty of those parameters. The
*relative* ranking of causes — which this platform exists to produce — is far
more robust, because queue position and depth are measured from real data.

Suitable for research and execution analysis. Not production trading
infrastructure, and no profitability is claimed for anything in it.
