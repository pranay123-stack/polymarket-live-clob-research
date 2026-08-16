//! Streaming statistics for latency and other per-event measurements.
//!
//! Sessions run to millions of events, so nothing here retains samples. A
//! fixed-bucket histogram gives exact counts, an exact mean, and percentiles
//! accurate to the bucket width in constant memory.

use serde::{Deserialize, Serialize};

/// Values at or above this many milliseconds land in the overflow bucket.
const OVERFLOW_MS: i64 = 4_000;

/// A millisecond-resolution histogram with an overflow bucket.
///
/// Percentiles are exact for samples below [`OVERFLOW_MS`], which covers
/// every feed-delay observation seen on this feed by a wide margin — the
/// measured maximum across a 93,060-event session was 1551 ms.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatencyHistogram {
    buckets: Vec<u64>,
    overflow: u64,
    /// Count of samples below zero, which indicate clock offset rather than
    /// negative transport delay and are excluded from percentiles.
    negative: u64,
    count: u64,
    sum: i128,
    min: Option<i64>,
    max: Option<i64>,
}

impl Default for LatencyHistogram {
    fn default() -> LatencyHistogram {
        LatencyHistogram::new()
    }
}

impl LatencyHistogram {
    /// Creates an empty histogram.
    pub fn new() -> LatencyHistogram {
        LatencyHistogram {
            buckets: vec![0; OVERFLOW_MS as usize],
            overflow: 0,
            negative: 0,
            count: 0,
            sum: 0,
            min: None,
            max: None,
        }
    }

    /// Records one millisecond sample.
    pub fn record(&mut self, ms: i64) {
        self.count += 1;
        self.sum += ms as i128;
        self.min = Some(self.min.map_or(ms, |m| m.min(ms)));
        self.max = Some(self.max.map_or(ms, |m| m.max(ms)));
        if ms < 0 {
            self.negative += 1;
        } else if ms >= OVERFLOW_MS {
            self.overflow += 1;
        } else {
            self.buckets[ms as usize] += 1;
        }
    }

    /// Number of samples recorded.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Samples that were negative, i.e. received before their exchange stamp.
    pub fn negative(&self) -> u64 {
        self.negative
    }

    /// Arithmetic mean, or `None` when empty.
    pub fn mean(&self) -> Option<f64> {
        (self.count > 0).then(|| self.sum as f64 / self.count as f64)
    }

    /// Smallest sample.
    pub fn min(&self) -> Option<i64> {
        self.min
    }

    /// Largest sample.
    pub fn max(&self) -> Option<i64> {
        self.max
    }

    /// Percentile in `[0, 100]`, to bucket resolution.
    ///
    /// Computed over non-negative samples only; negative samples are a clock
    /// artefact rather than a delay and would distort the tail.
    pub fn percentile(&self, p: f64) -> Option<i64> {
        let usable = self.count - self.negative;
        if usable == 0 {
            return None;
        }
        let rank = ((p / 100.0) * usable as f64).ceil().max(1.0) as u64;
        let mut seen = 0u64;
        for (ms, &n) in self.buckets.iter().enumerate() {
            seen += n;
            if seen >= rank {
                return Some(ms as i64);
            }
        }
        (self.overflow > 0).then_some(OVERFLOW_MS)
    }

    /// Collapses to a reportable summary.
    pub fn summary(&self) -> LatencySummary {
        LatencySummary {
            count: self.count,
            negative: self.negative,
            min_ms: self.min,
            p50_ms: self.percentile(50.0),
            p90_ms: self.percentile(90.0),
            p99_ms: self.percentile(99.0),
            max_ms: self.max,
            mean_ms: self.mean(),
        }
    }
}

/// A reportable latency summary.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LatencySummary {
    /// Samples recorded.
    pub count: u64,
    /// Samples that were negative.
    pub negative: u64,
    /// Smallest sample, ms.
    pub min_ms: Option<i64>,
    /// Median, ms.
    pub p50_ms: Option<i64>,
    /// 90th percentile, ms.
    pub p90_ms: Option<i64>,
    /// 99th percentile, ms.
    pub p99_ms: Option<i64>,
    /// Largest sample, ms.
    pub max_ms: Option<i64>,
    /// Arithmetic mean, ms.
    pub mean_ms: Option<f64>,
}

impl std::fmt::Display for LatencySummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let g = |v: Option<i64>| v.map(|x| x.to_string()).unwrap_or_else(|| "-".into());
        write!(
            f,
            "n={} min={} p50={} p90={} p99={} max={}",
            self.count,
            g(self.min_ms),
            g(self.p50_ms),
            g(self.p90_ms),
            g(self.p99_ms),
            g(self.max_ms)
        )
    }
}

/// Running mean and variance via Welford's algorithm.
///
/// Used for per-order measurements — slippage, fill ratios — where the
/// sample count is small enough that a histogram would be wasteful but
/// numerical stability across a long session still matters.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct RunningStats {
    n: u64,
    mean: f64,
    m2: f64,
    min: f64,
    max: f64,
}

impl RunningStats {
    /// Creates empty statistics.
    pub fn new() -> RunningStats {
        RunningStats {
            n: 0,
            mean: 0.0,
            m2: 0.0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
        }
    }

    /// Adds one observation.
    pub fn push(&mut self, x: f64) {
        self.n += 1;
        let d = x - self.mean;
        self.mean += d / self.n as f64;
        self.m2 += d * (x - self.mean);
        self.min = self.min.min(x);
        self.max = self.max.max(x);
    }

    /// Number of observations.
    pub fn count(&self) -> u64 {
        self.n
    }

    /// Sample mean, or `None` when empty.
    pub fn mean(&self) -> Option<f64> {
        (self.n > 0).then_some(self.mean)
    }

    /// Sample standard deviation, or `None` with fewer than two observations.
    pub fn stddev(&self) -> Option<f64> {
        (self.n > 1).then(|| (self.m2 / (self.n - 1) as f64).sqrt())
    }

    /// Smallest observation.
    pub fn min(&self) -> Option<f64> {
        (self.n > 0).then_some(self.min)
    }

    /// Largest observation.
    pub fn max(&self) -> Option<f64> {
        (self.n > 0).then_some(self.max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_match_a_known_distribution() {
        let mut h = LatencyHistogram::new();
        for ms in 1..=100 {
            h.record(ms);
        }
        assert_eq!(h.count(), 100);
        assert_eq!(h.percentile(50.0), Some(50));
        assert_eq!(h.percentile(90.0), Some(90));
        assert_eq!(h.percentile(99.0), Some(99));
        assert_eq!(h.min(), Some(1));
        assert_eq!(h.max(), Some(100));
        assert!((h.mean().unwrap() - 50.5).abs() < 1e-9);
    }

    #[test]
    fn reproduces_the_measured_feed_delay_shape() {
        // The distribution observed live: a hard floor near 197 ms with a
        // thin tail. Percentiles must sit inside the body, not the tail.
        let mut h = LatencyHistogram::new();
        for _ in 0..900 {
            h.record(206);
        }
        for _ in 0..90 {
            h.record(220);
        }
        for _ in 0..10 {
            h.record(1_551);
        }
        assert_eq!(h.percentile(50.0), Some(206));
        assert_eq!(h.percentile(99.0), Some(220));
        assert_eq!(h.max(), Some(1_551));
    }

    #[test]
    fn overflow_samples_are_counted_and_reported_at_the_cap() {
        let mut h = LatencyHistogram::new();
        h.record(10);
        h.record(99_999);
        assert_eq!(h.max(), Some(99_999));
        assert_eq!(h.percentile(100.0), Some(OVERFLOW_MS));
    }

    #[test]
    fn negative_samples_are_excluded_from_percentiles_but_still_counted() {
        let mut h = LatencyHistogram::new();
        h.record(-50);
        h.record(10);
        h.record(20);
        assert_eq!(h.count(), 3);
        assert_eq!(h.negative(), 1);
        assert_eq!(h.min(), Some(-50));
        // Median of the two usable samples.
        assert_eq!(h.percentile(50.0), Some(10));
    }

    #[test]
    fn empty_histogram_reports_nothing_rather_than_zero() {
        let h = LatencyHistogram::new();
        assert_eq!(h.percentile(50.0), None);
        assert_eq!(h.mean(), None);
    }

    #[test]
    fn running_stats_match_hand_computed_values() {
        let mut s = RunningStats::new();
        for x in [2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0] {
            s.push(x);
        }
        assert!((s.mean().unwrap() - 5.0).abs() < 1e-12);
        // Sample (n-1) standard deviation of that classic set.
        assert!((s.stddev().unwrap() - 2.138_089_935_299_395).abs() < 1e-9);
        assert_eq!(s.min(), Some(2.0));
        assert_eq!(s.max(), Some(9.0));
    }
}
