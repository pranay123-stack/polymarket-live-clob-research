# Architecture

## The pipeline

```
                         POLYMARKET
                             │
        ┌────────────────────┼────────────────────┐
        │                    │                    │
   Gamma API            CLOB REST          CLOB WebSocket
   /events?slug=        /book /time        wss://…/ws/market
   market metadata      /midpoint          book · price_change
   token ids            /tick-size         last_trade_price
        │                    │                    │
        └────────────────────┴────────────────────┘
                             │
                    ┌────────▼────────┐
                    │ market_discovery│  deterministic 5-minute slugs
                    └────────┬────────┘
                             │
                    ┌────────▼────────┐
                    │     parser      │  ONE decoder, live and replay
                    │   (Normalizer)  │  assigns sequence numbers
                    └────────┬────────┘
                             │
                     MarketEvent stream
                             │
        ┌────────────────────┴────────────────────┐
        │                                         │
┌───────▼────────┐                       ┌────────▼───────┐
│    recorder    │                       │  shadow engine │
│  session.jsonl │                       │  live, no order│
│  verbatim frames│                      └────────┬───────┘
└───────┬────────┘                                │
        │                                         │
┌───────▼────────┐                                │
│  replay engine │◀───────────────────────────────┘
│  event-time    │        same simulator both paths
│  clock         │
└───────┬────────┘
        │
   ┌────┴─────┐
   │          │
┌──▼───┐  ┌───▼────┐
│truth │  │observed│   two views of the same market
│ book │  │  book  │   exchange time vs receive time
└──┬───┘  └───┬────┘
   │          │
   │     ┌────▼─────┐
   │     │ strategy │  decides on `observed` only
   │     └────┬─────┘
   │          │ OrderIntent
   │     ┌────▼──────────────┐
   └────▶│ ExecutionSimulator│  fills against `truth`
         │ latency · queue   │
         │ depth · fees      │
         └────┬──────────────┘
              │ Fill
         ┌────▼─────┐
         │ portfolio│  exact integer accounting
         └────┬─────┘
              │
     ┌────────▼─────────┐
     │    analytics     │  slippage · Shapley attribution
     └────────┬─────────┘
              │
       Edge-loss report
```

## The one idea that makes this work

Every recorded event carries **two timestamps**:

| Stamp | Meaning |
|-------|---------|
| `exchange_ms` | when Polymarket published the event |
| `recv_ms` | when this recorder actually received it |

Their difference is real, measured feed delay — a median of **214 ms** on
the committed fixture, with a clock offset of **−6 ms ± 91 ms**, i.e.
indistinguishable from zero. That gap is not an assumption; it is in the data.

The replay engine keeps two market states from that one stream:

* **truth** — every event whose `exchange_ms` has passed. This is the book an
  arriving order actually meets.
* **observed** — every event whose `recv_ms` has passed. This is everything
  the strategy could possibly have known.

The strategy reads `observed`. The simulator fills against `truth`. The
distance between them is where a backtest's edge goes, and because both come
from the same recording, it is measured rather than modelled.

## Module map

| Module | Responsibility |
|--------|----------------|
| `types` | Fixed-point `Price`, `Qty`, `Usdc`. No floats in any accounting path. |
| `polymarket::api` | Read-only Gamma and CLOB clients; Cristian clock probe. |
| `polymarket::market_discovery` | Deterministic `btc-updown-5m-<ts>` slug lookup. |
| `polymarket::websocket` | Market-channel collector: subscribe, heartbeat, reconnect. |
| `polymarket::parser` | The single decoder. Wire frames → `MarketEvent`. |
| `market::orderbook` | Dense-ladder CLOB book, O(1) level updates. |
| `market::state` | Books, last trades and integrity counters for a session. |
| `recorder` | Live capture, rolling market discovery, session files. |
| `replay::clock` | Event-time clock and the delay line separating the two views. |
| `replay::engine` | The deterministic run loop. |
| `execution::matcher` | Latency, queue position, depth and fees. |
| `portfolio` | Position, cost basis, and the P&L identity. |
| `analytics` | Latency histograms, slippage, Shapley attribution. |
| `shadow::engine` | Live observation, two simulators, zero orders. |
| `strategy` | The reference imbalance signal. Not a trading strategy. |

## Design decisions worth defending

**A dense price ladder, not a tree.** Binary-outcome prices are bounded to
`[0, 1]` and quantised to `1e-4`, so the entire price domain is 10,001 ticks.
One array slot per tick makes a level update a single indexed write with no
allocation, hashing or rebalancing, at a fixed 160 KiB per book. Best bid and
ask are cached and repaired incrementally; only an update that empties the
touch walks, and it walks only to the next populated tick.

**Integers everywhere.** Accumulating `f64` rounding across millions of
replayed events makes P&L non-reproducible and breaks the accounting
identity. Prices are `1e-4`, quantities `1e-6`, cash `1e-6` USDC, and the
identity closes to the micro-dollar rather than to a tolerance.

**Sessions store verbatim frames, not decoded events.** Normalization runs on
read, through the same decoder the live path uses. That keeps unmodelled
fields on disk for later, and makes live and replay incapable of drifting
apart, because there is exactly one decoder in the crate.

**Sequence numbers are ours, not the exchange's.** Polymarket's market
channel publishes none. The `seq` on a `MarketEvent` is assigned by the
recorder in receive order. It orders a session and anchors replay
determinism; it cannot prove no message was lost in transit, and the
documentation says so wherever it appears.

**Read-only by construction.** No key material, no signing, no order
endpoints. `tests/integration_tests.rs` scans the source for private-key
handling and CLOB write paths and fails the build if any appear.
