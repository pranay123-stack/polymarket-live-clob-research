# Testing

## The rule

Tests run on **recorded real Polymarket sessions**. No synthetic markets, no
generated books, no invented price paths. The only fabricated values in the
suite are small hand-built ladders in unit tests that check a single
arithmetic property in isolation, and those never stand in for a market.

## The fixture

`tests/fixtures/session_btc_sample.jsonl` — a slice of a live BTC 5-minute
Up/Down recording made by this tool on 2026-08-16:

| | |
|---|---|
| Events | 8,784 |
| Tokens | 8 (4 markets × Up/Down) |
| Book snapshots | 17 |
| Level updates | 8,708 |
| Trade prints | 55 (51 buy-aggressor, 4 sell-aggressor) |
| Measured feed delay | p50 214 ms, p99 423 ms |
| Size | 2.8 MB |

Prices, sizes, timestamps, hashes and trades are verbatim. The single edit:
most mid-session book snapshots were dropped to keep the file committable.
The exchange republishes a full snapshot per token roughly every 1.5 seconds,
and since every delta between them is retained, the reconstructed book is
identical either way. The first snapshot for each token is always kept,
because a book cannot be baselined without one.

Both aggressor sides appear, so both halves of the queue model are exercised.

## What each suite covers

### `tests/orderbook_tests.rs`
Reconstruction against real data: no stale rejections and no deltas before a
snapshot across the whole session; books end uncrossed with ordered levels
and a mid inside the touch; snapshots fully rebaseline a book that already
had updates; a walk never returns more than displayed liquidity; imbalance
stays in range.

The strongest check is **complementary consistency**: on a binary market a
bid of `p` on Up must be an ask of `1−p` on Down, at equal size. Any drift
would mean the reconstruction lost or misplaced size. The test asserts the
tick sums to exactly `1.0000` and the sizes match.

### `tests/replay_tests.rs`
Determinism. Two replays of one file produce identical events, fills, P&L and
counters — asserted for **all 32 realism configurations**, not just the
default. Sequence numbers are dense from 1. Event time never runs backwards.
A replay run after a real `sleep` gives the same answer, which is what
"no wall-clock dependency" actually means.

### `tests/execution_tests.rs`
The accounting identity holds **exactly** under all 32 realism
configurations. The ideal run fills everything; the realistic run fills less
and misses orders. Depth constraints never increase traded notional. Fees
change P&L by precisely the fees charged and never alter which fills occur.
Queue models come out ordered pessimistic ≤ proportional ≤ optimistic. Passive
orders only ever make, aggressive ones only ever take. Shapley contributions
sum to the measured gap, and Shapley and waterfall agree on the total.

### `tests/lineage_tests.rs`
Event lineage on real data: decision identities unique, every order citing a
decision that exists, every fill citing an order that exists, fill ids unique,
order sizes summing to their decision's size, and fill sides matching the
decision that caused them. Per-decision P&L reconciles to the portfolio with a
bounded residual. The audit CSVs are re-parsed and every figure recomputed
from its own row. `verify-replay` is asserted to report `IDENTICAL RESULT`
with no stage failing, and the book checksum is shown to be both stable across
runs and sensitive to a one-level difference.

### `tests/integration_tests.rs`
The CLI end to end: `inspect`, `replay` and `analyze` on the fixture, with
`replay` invoked twice to confirm byte-identical output. The `--csv` factor
table is parsed back and checked to balance against its own total. A session
survives a write-and-read round trip with events unchanged. Stored frames are
confirmed to still carry the exchange's own fields, not a re-serialisation of
this crate's model.

The last test is a **safety guard**: it scans `src/` for private-key
handling, order signing and CLOB write endpoints, and fails the build if any
appear. The read-only guarantee is enforced, not merely intended.

## Running

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
```

All three pass clean. `cargo test --all` runs 177 tests — 121 unit and 56
integration — in about a minute; the bulk of the time is the 32-run Shapley
passes, which are deliberately exhaustive.

## Recording fresh data

The fixture is a snapshot of one moment in a market that runs continuously.
To test against current conditions:

```bash
cargo run --release -- record --seconds 600 --out data
cargo run --release -- analyze --file data/session_btc_<ts>.jsonl
```

Results will differ — spreads, depth and trade intensity vary by time of day
and by how close a round is to expiry. That variation is the subject, not
noise: an execution model that only holds on one recording is not a model.

## What is not tested

Live network paths — reconnect and backoff behaviour — are exercised by
running `record` and `shadow` against the real exchange, not by an automated
test. Simulating a Polymarket server to test the client against would mean
testing against a fiction, which is the thing this project exists to avoid.
The reconnect path has been observed working during real recordings, where
the rolling market refresh resubscribes on a fresh token set every few
minutes.
