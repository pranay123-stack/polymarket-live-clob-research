//! Event-time clock and the delay line that separates truth from observation.
//!
//! # Why replay never reads the wall clock
//!
//! A replay that consulted `SystemTime` would produce different results on a
//! loaded machine than on an idle one, which would make every number this
//! crate reports unreproducible. Time here advances only when an event says
//! it does.
//!
//! # Two views of one market
//!
//! Each recorded event carries two stamps: `exchange_ms`, when the exchange
//! published it, and `recv_ms`, when this recorder actually received it. The
//! gap between them is real, measured feed delay — a median of 213 ms on a
//! live BTC session, against a clock offset indistinguishable from zero.
//!
//! That gap is what the simulator exploits. The clock runs on exchange time,
//! so:
//!
//! * **truth** — every event whose `exchange_ms` has passed. This is what an
//!   arriving order actually meets.
//! * **observed** — every event whose `recv_ms` has passed. This is all the
//!   strategy could possibly have known.
//!
//! `observed` therefore trails `truth` by the genuine delay of each
//! individual frame, rather than by an assumed constant. Market-data latency
//! is *measured* on these markets, not parameterised — the
//! `--md-latency-ms` override exists only for sensitivity analysis.

use std::collections::VecDeque;

use crate::market::event::MarketEvent;

/// A monotonic clock driven by exchange timestamps.
#[derive(Debug, Clone, Copy, Default)]
pub struct VirtualClock {
    now_ms: i64,
}

impl VirtualClock {
    /// Creates a clock at time zero.
    pub fn new() -> VirtualClock {
        VirtualClock::default()
    }

    /// Current event time.
    pub fn now_ms(&self) -> i64 {
        self.now_ms
    }

    /// Advances to `ms`, never backwards.
    ///
    /// Out-of-order frames do occur — the feed's delay varies from 207 ms to
    /// several seconds — and letting one rewind the clock would re-open
    /// orders the simulator had already resolved.
    pub fn advance_to(&mut self, ms: i64) -> i64 {
        self.now_ms = self.now_ms.max(ms);
        self.now_ms
    }
}

/// Holds events back until the strategy could legitimately have seen them.
#[derive(Debug)]
pub struct DelayLine {
    /// `None` releases immediately; `Some(ms)` overrides with a fixed delay.
    override_ms: Option<i64>,
    /// Whether any delay is applied at all.
    enabled: bool,
    queue: VecDeque<MarketEvent>,
    released: u64,
}

impl DelayLine {
    /// Creates a delay line.
    ///
    /// `enabled` false makes the strategy omniscient: it sees each event the
    /// instant the exchange published it, which no participant can do. That
    /// is the ideal leg of the comparison.
    ///
    /// With `enabled` true and `override_ms` `None`, each event is released
    /// at its own recorded `recv_ms` — the delay this recorder really
    /// experienced. `Some(ms)` substitutes a fixed delay instead.
    pub fn new(enabled: bool, override_ms: Option<i64>) -> DelayLine {
        DelayLine {
            override_ms,
            enabled,
            queue: VecDeque::new(),
            released: 0,
        }
    }

    /// Number of events released so far.
    pub fn released(&self) -> u64 {
        self.released
    }

    /// Events still held back.
    pub fn pending(&self) -> usize {
        self.queue.len()
    }

    /// The event time at which `ev` becomes visible to the strategy.
    fn visible_at(&self, ev: &MarketEvent) -> i64 {
        match self.override_ms {
            Some(ms) => ev.exchange_ms + ms,
            None => ev.recv_ms,
        }
    }

    /// Queues an event, or returns it immediately when delay is disabled.
    pub fn push(&mut self, ev: MarketEvent) -> Option<MarketEvent> {
        if !self.enabled {
            self.released += 1;
            return Some(ev);
        }
        self.queue.push_back(ev);
        None
    }

    /// Releases every event whose visibility time has arrived.
    pub fn release(&mut self, now_ms: i64) -> Vec<MarketEvent> {
        if !self.enabled {
            return Vec::new();
        }
        let mut out = Vec::new();
        while let Some(front) = self.queue.front() {
            if self.visible_at(front) <= now_ms {
                out.push(self.queue.pop_front().expect("front just checked"));
            } else {
                break;
            }
        }
        self.released += out.len() as u64;
        out
    }

    /// Releases everything still held, for end of session.
    pub fn drain(&mut self) -> Vec<MarketEvent> {
        let out: Vec<_> = self.queue.drain(..).collect();
        self.released += out.len() as u64;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::event::EventPayload;
    use crate::types::Price;

    fn ev(seq: u64, exchange_ms: i64, recv_ms: i64) -> MarketEvent {
        MarketEvent {
            seq,
            recv_ms,
            exchange_ms,
            payload: EventPayload::TickSizeChange {
                asset_id: "UP".into(),
                new_tick: Price::parse("0.01").unwrap(),
            },
        }
    }

    #[test]
    fn the_clock_never_runs_backwards() {
        let mut c = VirtualClock::new();
        c.advance_to(1_000);
        assert_eq!(c.advance_to(900), 1_000);
        assert_eq!(c.now_ms(), 1_000);
    }

    #[test]
    fn a_disabled_delay_line_makes_the_strategy_omniscient() {
        let mut d = DelayLine::new(false, None);
        assert!(d.push(ev(1, 1_000, 1_213)).is_some());
        assert_eq!(d.pending(), 0);
    }

    #[test]
    fn events_become_visible_at_their_real_receive_time() {
        let mut d = DelayLine::new(true, None);
        // Stamped at 1000, actually received at 1213.
        assert!(d.push(ev(1, 1_000, 1_213)).is_none());
        assert!(d.release(1_100).is_empty(), "not received yet");
        assert!(d.release(1_212).is_empty());
        let out = d.release(1_213);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].seq, 1);
    }

    #[test]
    fn a_fixed_override_replaces_the_measured_delay() {
        let mut d = DelayLine::new(true, Some(50));
        d.push(ev(1, 1_000, 1_213));
        // Released 50ms after the exchange stamp, ignoring the real 213ms.
        assert_eq!(d.release(1_050).len(), 1);
    }

    #[test]
    fn release_preserves_order_and_stops_at_the_first_unripe_event() {
        let mut d = DelayLine::new(true, None);
        d.push(ev(1, 1_000, 1_100));
        d.push(ev(2, 1_010, 1_400));
        d.push(ev(3, 1_020, 1_150));
        let out = d.release(1_200);
        // Event 2 is not visible yet, so 3 must wait behind it.
        assert_eq!(out.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1]);
        assert_eq!(d.pending(), 2);
    }

    #[test]
    fn drain_releases_everything_left_at_end_of_session() {
        let mut d = DelayLine::new(true, None);
        d.push(ev(1, 1_000, 9_999));
        assert_eq!(d.drain().len(), 1);
        assert_eq!(d.released(), 1);
    }
}
