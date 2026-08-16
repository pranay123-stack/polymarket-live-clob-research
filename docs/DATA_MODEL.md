# Data model

## Verified wire formats

Everything below was confirmed against the live exchange, not taken from
documentation. Where the two disagreed, the live behaviour won.

### Market discovery — Gamma

```
GET https://gamma-api.polymarket.com/events?slug=btc-updown-5m-1786844100
```

BTC 5-minute Up/Down slugs are **deterministic**:

```
btc-updown-5m-<unix_seconds>        unix_seconds % 300 == 0
```

The timestamp is the round's **open**; it closes exactly 300 seconds later,
which matches Gamma's `endDate`. Discovery is therefore an exact-slug lookup
per five-minute bucket rather than a scan.

This matters because Gamma's `slug_contains` parameter is **accepted and then
ignored** — it returns unrelated events. There is no working substring search
for slugs, so anything built on one would silently return the wrong markets.

Two Gamma fields are **double-encoded**: `clobTokenIds` and `outcomes` are
JSON *strings* containing JSON arrays.

```json
"clobTokenIds": "[\"93555969663711891451625685586929143462823436918129307927023356936446352666723\", \"948491…\"]",
"outcomes": "[\"Up\", \"Down\"]"
```

The `Up` token is matched by **outcome name**, never by array position. A
silent swap would invert every signal downstream.

Observed market parameters: tick size `0.01`, minimum order size `5` shares,
outcomes `["Up", "Down"]`.

> **User-Agent.** Gamma rejects some default client agents with `403` —
> `Python-urllib/3.12` is blocked while a custom agent on the identical URL
> returns `200`. `reqwest` sends no `User-Agent` unless told to, so the
> client always sets one explicitly.

### Market data — CLOB WebSocket

```
wss://ws-subscriptions-clob.polymarket.com/ws/market
→ {"assets_ids": ["<token id>", …], "type": "market"}
```

Messages arrive as JSON **arrays** of events. Text `PING` is answered with
text `PONG`, which is not JSON and must be filtered before parsing.

| `event_type` | Payload |
|--------------|---------|
| `book` | `{market, asset_id, timestamp, hash, bids[], asks[], tick_size?}` |
| `price_change` | `{market, timestamp, price_changes[{asset_id, price, size, side, hash, best_bid, best_ask}]}` |
| `last_trade_price` | `{market, asset_id, price, size, side, fee_rate_bps, timestamp, transaction_hash}` |

Three facts about this feed carry real consequences:

**1. `price_change.size` is the new aggregate size at that level, not a
delta.** A level is deleted by reporting size `0`. Treating it as a delta
silently corrupts the book, so all mutation goes through a replace.

**2. One `price_change` frame carries changes for several tokens**, so one
frame fans out to several events.

**3. `last_trade_price.side` is the aggressor.** This was determined
empirically rather than assumed, by matching each trade print against the
next level change at the same token and price across a real session:

| Trade `side` | Ask side changed | Bid side changed |
|--------------|------------------|------------------|
| `BUY` | **545** | 154 |
| `SELL` | 13 | **79** |

A taker buy consumes resting asks; a taker sell consumes resting bids. The
entire queue model rests on this, which is why it was measured.

**Snapshots repeat.** The server republishes a full `book` per token roughly
every 1.5 seconds — 1994 snapshots across 10 tokens in a 300-second session.
Recovery after a reconnect is automatic; no resync request exists or is
needed.

**There are no sequence numbers.** The market channel publishes none. Gap
detection against the exchange is therefore not possible from public data.

### REST reads

| Endpoint | Use |
|----------|-----|
| `GET /book?token_id=` | snapshot; `timestamp` is milliseconds |
| `GET /time` | exchange clock, whole **seconds** only |
| `GET /midpoint?token_id=` | touch midpoint |
| `GET /tick-size?token_id=` | minimum increment |

The clock probe uses `/book` rather than `/time` precisely because `/time`'s
second resolution is far too coarse to interpret a ~200 ms feed delay.

## Fixed-point types

| Type | Unit | Scale |
|------|------|-------|
| `Price` | dollars per share | `1e-4` |
| `Qty` | shares | `1e-6` |
| `Usdc` | dollars | `1e-6` |

`1e-4` exactly represents both tick sizes Polymarket uses (`0.01` and
`0.001`). `1e-6` matches USDC's on-chain decimals. Decimal strings are parsed
by accumulating digits into the scale — never via `f64` — and a value needing
finer resolution than the scale is **rejected** rather than truncated.

## The event model

```rust
MarketEvent {
    seq,          // recorder-assigned, from 1
    recv_ms,      // local receive time
    exchange_ms,  // exchange-published time
    payload,
}
```

with payloads `Snapshot`, `LevelUpdate`, `Trade`, `TickSizeChange`,
`MarketOpen`, `MarketClose`.

`MarketOpen` and `MarketClose` are **recorder-synthesised** from real Gamma
metadata, because the market channel publishes no lifecycle messages. They
are stored under a distinct record key so a reader can always tell exchange
data from recorder annotation.

## Session file format

Newline-delimited JSON. First line is the header; every later line is a
record.

```json
{"v":1,"kind":"header","tool":"…","started_ms":…,"clock":{…},"underlying":"btc","source_url":"wss://…","markets":[…]}
{"f":1,"recv_ms":1786844302150,"raw":{"event_type":"book",…}}
{"f":2,"recv_ms":1786844302310,"lifecycle":{"type":"market_open",…}}
```

Records store the exchange frame **verbatim**, plus only the two things the
exchange cannot supply: a frame index and a local receive timestamp.
Sequence numbers are *not* stored — they are re-derived on read, which is
what makes replay reproducible rather than merely recorded.

The consequences are deliberate:

* **No information loss.** Fields this crate does not model, and fields the
  exchange adds later, survive in the file.
* **Replay fidelity by construction.** Live and replay share one decoder, so
  they cannot drift. If normalization improves, old sessions re-normalize
  under the new rules instead of being frozen.
* **Crash tolerance.** A recording killed mid-write leaves a truncated final
  line; the reader counts it and keeps every complete record before it.

### Size

A busy multi-market session runs about **310 events/second** and roughly
**230 KB/second** on disk. An hour of BTC 5-minute markets is therefore
around 800 MB. Recordings are excluded from version control; see
`data/README.md`.
