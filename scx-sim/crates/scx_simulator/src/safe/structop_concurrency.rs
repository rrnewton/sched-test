//! Logical-time histogram for concurrent structop execution.
//!
//! The engine charges each scheduler callback as a single logical-time interval
//! on the executing CPU's `local_clock`. This tracker records those intervals
//! and reconstructs how much logical time the simulation spent with 0, 1, 2,
//! 3, or 4+ structops active simultaneously.

use std::collections::BTreeMap;

use crate::fmt::FmtN;
use crate::types::{CpuId, TimeNs};

const DISPLAY_BUCKETS: usize = 5;
const OVERFLOW_BUCKET: usize = DISPLAY_BUCKETS - 1;

/// Sweep-line tracker over per-callback logical-time intervals.
#[derive(Debug, Clone, Default)]
pub(crate) struct StructopConcurrencyTracker {
    deltas: BTreeMap<TimeNs, i32>,
    cpu_tail_ns: BTreeMap<CpuId, TimeNs>,
}

impl StructopConcurrencyTracker {
    /// Record one structop interval on `cpu`.
    ///
    /// Intervals are serialized per CPU so one CPU can never contribute more
    /// than one active structop at a time, even if the engine later charges
    /// the callback cost to a different CPU's logical clock.
    pub(crate) fn record_interval(&mut self, cpu: CpuId, start: TimeNs, end: TimeNs) {
        if end <= start {
            return;
        }
        let duration = end - start;
        let normalized_start = start.max(self.cpu_tail_ns.get(&cpu).copied().unwrap_or(0));
        let normalized_end = normalized_start + duration;

        self.cpu_tail_ns.insert(cpu, normalized_end);
        *self.deltas.entry(normalized_start).or_insert(0) += 1;
        *self.deltas.entry(normalized_end).or_insert(0) -= 1;
    }

    /// Summarize the recorded intervals over the full simulation time span.
    pub(crate) fn summarize(&self, total_time: TimeNs) -> StructopConcurrencySummary {
        let effective_total = total_time.max(self.deltas.last_key_value().map_or(0, |(t, _)| *t));
        let mut exact_levels: Vec<TimeNs> = vec![0];
        let mut active: i64 = 0;
        let mut prev_time: TimeNs = 0;

        for (&time, &delta) in &self.deltas {
            if time > prev_time {
                let active_level =
                    usize::try_from(active).expect("structop concurrency cannot be negative");
                if active_level >= exact_levels.len() {
                    exact_levels.resize(active_level + 1, 0);
                }
                exact_levels[active_level] += time - prev_time;
                prev_time = time;
            }

            active += i64::from(delta);
            debug_assert!(active >= 0, "structop concurrency underflow");
        }

        if effective_total > prev_time {
            let active_level =
                usize::try_from(active).expect("structop concurrency cannot be negative");
            if active_level >= exact_levels.len() {
                exact_levels.resize(active_level + 1, 0);
            }
            exact_levels[active_level] += effective_total - prev_time;
        }

        StructopConcurrencySummary {
            total_time: effective_total,
            exact_levels,
        }
    }
}

/// Completed logical-time histogram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StructopConcurrencySummary {
    total_time: TimeNs,
    exact_levels: Vec<TimeNs>,
}

impl StructopConcurrencySummary {
    fn display_buckets(&self) -> [TimeNs; DISPLAY_BUCKETS] {
        let mut buckets = [0; DISPLAY_BUCKETS];
        for (level, &duration) in self.exact_levels.iter().enumerate() {
            let bucket = level.min(OVERFLOW_BUCKET);
            buckets[bucket] += duration;
        }
        buckets
    }
}

/// Print the final logical-time histogram.
pub(crate) fn print_structop_concurrency_histogram(
    tracker: &StructopConcurrencyTracker,
    total_time: TimeNs,
) {
    let summary = tracker.summarize(total_time);
    if summary.total_time == 0 {
        return;
    }

    println!();
    println!("Structop concurrency histogram:");
    println!("  {:>6}  {:>12}  {:>7}", "active", "logical", "share");

    for (label, duration) in ["0", "1", "2", "3", "4+"]
        .into_iter()
        .zip(summary.display_buckets())
    {
        let pct = if summary.total_time == 0 {
            0.0
        } else {
            100.0 * duration as f64 / summary.total_time as f64
        };
        println!("  {:>6}  {:>12}  {:>6.1}%", label, FmtN(duration), pct);
    }
}

#[cfg(test)]
mod tests {
    use super::{StructopConcurrencyTracker, DISPLAY_BUCKETS};
    use crate::types::CpuId;

    #[test]
    fn overlapping_callbacks_count_shared_time_once() {
        let mut tracker = StructopConcurrencyTracker::default();
        tracker.record_interval(CpuId(0), 100, 200);
        tracker.record_interval(CpuId(1), 100, 200);

        let buckets = tracker.summarize(200).display_buckets();
        assert_eq!(buckets, [100, 0, 100, 0, 0]);
    }

    #[test]
    fn adjacent_fragments_preserve_total_overlap() {
        let mut tracker = StructopConcurrencyTracker::default();
        tracker.record_interval(CpuId(0), 100, 200);
        tracker.record_interval(CpuId(1), 100, 150);
        tracker.record_interval(CpuId(1), 150, 200);

        let buckets = tracker.summarize(200).display_buckets();
        assert_eq!(buckets, [100, 0, 100, 0, 0]);
    }

    #[test]
    fn four_plus_bucket_aggregates_higher_levels() {
        let mut tracker = StructopConcurrencyTracker::default();
        for cpu in 0..5 {
            tracker.record_interval(CpuId(cpu), 0, 10);
        }

        let buckets = tracker.summarize(10).display_buckets();
        assert_eq!(buckets.len(), DISPLAY_BUCKETS);
        assert_eq!(buckets, [0, 0, 0, 0, 10]);
    }

    #[test]
    fn zero_bucket_covers_trailing_userspace_time() {
        let mut tracker = StructopConcurrencyTracker::default();
        tracker.record_interval(CpuId(0), 50, 75);

        let buckets = tracker.summarize(100).display_buckets();
        assert_eq!(buckets, [75, 25, 0, 0, 0]);
    }

    #[test]
    fn same_cpu_callbacks_are_serialized() {
        let mut tracker = StructopConcurrencyTracker::default();
        tracker.record_interval(CpuId(0), 0, 10);
        tracker.record_interval(CpuId(0), 0, 10);

        let buckets = tracker.summarize(20).display_buckets();
        assert_eq!(buckets, [0, 20, 0, 0, 0]);
    }
}
