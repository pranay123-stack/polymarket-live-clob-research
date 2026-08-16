# data/

Recorded sessions land here.

```bash
cargo run --release -- record --seconds 600 --out data
```

produces `data/session_btc_<unix_ts>.jsonl` — the header line, then one
verbatim exchange frame per line. Format is documented in
[`../docs/DATA_MODEL.md`](../docs/DATA_MODEL.md).

## These files are not in version control

A busy multi-market session runs about **310 events/second** and roughly
**230 KB/second**: a 200-second recording is ~47 MB, an hour is ~800 MB.
`.gitignore` excludes `data/*.jsonl` for that reason.

The committed sample lives at
[`../tests/fixtures/session_btc_sample.jsonl`](../tests/fixtures/session_btc_sample.jsonl)
(2.8 MB) so the test suite and every documented number are reproducible from
a fresh clone with no network access.

## Recording notes

**Markets roll.** A BTC Up/Down round lives five minutes, so any recording
longer than that spans markets that did not exist when it started. The
recorder re-discovers the upcoming window every `--refresh` seconds and
resubscribes when the token set changes. Because the exchange republishes a
full book snapshot per token every ~1.5 seconds, the gap across a resubscribe
costs a snapshot, not book integrity.

**Subscribe ahead.** `--lookahead` controls how many future rounds to follow.
Subscribing early captures a market's opening book, which is where the widest
spreads and thinnest liquidity of the round appear — often the most
interesting part of the session.

**Feed delay is real and worth checking per session.** Every recording
measures its own, and `inspect` reports it alongside a clock-offset probe so
you can tell transport delay from a mis-set local clock. On the reference
session: delay p50 214 ms, offset −6 ms ± 91 ms.

**Nothing here can trade.** The recorder holds no keys and has no order path.
