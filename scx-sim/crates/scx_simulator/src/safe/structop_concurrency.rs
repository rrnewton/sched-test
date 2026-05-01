//! Logical-time histogram for structop concurrency and non-structop CPU time.
//!
//! The engine records logical-time intervals where a CPU is executing either:
//! - a scheduler structop callback, or
//! - simulated kernel overhead outside structops
//!
//! Everything else in `[0, simulation_end)` is classified as userspace/idle.
//! This makes the final histogram invariant to callback preemption density and
//! lets the 0-structop bucket reflect actual simulated runtime rather than only
//! gaps between callback-accounting events.

use std::collections::BTreeMap;

use crate::fmt::FmtN;
use crate::types::{CpuId, TimeNs};

const DISPLAY_BUCKETS: usize = 5;
const OVERFLOW_BUCKET: usize = DISPLAY_BUCKETS - 1;

#[derive(Debug, Clone, Copy, Default)]
struct TimelineDelta {
    structops: i32,
    kernel: i32,
}

/// Sweep-line tracker over per-CPU logical-time intervals.
#[derive(Debug, Clone, Default)]
pub(crate) struct StructopConcurrencyTracker {
    deltas: BTreeMap<TimeNs, TimelineDelta>,
    cpu_tail_ns: BTreeMap<CpuId, TimeNs>,
}

impl StructopConcurrencyTracker {
    fn normalize_interval(
        &mut self,
        cpu: CpuId,
        start: TimeNs,
        end: TimeNs,
    ) -> Option<(TimeNs, TimeNs)> {
        if end <= start {
            return None;
        }
        let duration = end - start;
        let normalized_start = start.max(self.cpu_tail_ns.get(&cpu).copied().unwrap_or(0));
        let normalized_end = normalized_start + duration;

        self.cpu_tail_ns.insert(cpu, normalized_end);
        Some((normalized_start, normalized_end))
    }

    /// Record one structop interval on `cpu`.
    ///
    /// Intervals are serialized per CPU so one CPU can never contribute more
    /// than one active structop at a time, even if the engine later charges
    /// the callback cost to a different CPU's logical clock.
    pub(crate) fn record_structop_interval(&mut self, cpu: CpuId, start: TimeNs, end: TimeNs) {
        let Some((start, end)) = self.normalize_interval(cpu, start, end) else {
            return;
        };

        self.deltas.entry(start).or_default().structops += 1;
        self.deltas.entry(end).or_default().structops -= 1;
    }

    /// Record non-structop kernel time on `cpu`.
    pub(crate) fn record_kernel_interval(&mut self, cpu: CpuId, start: TimeNs, end: TimeNs) {
        let Some((start, end)) = self.normalize_interval(cpu, start, end) else {
            return;
        };

        self.deltas.entry(start).or_default().kernel += 1;
        self.deltas.entry(end).or_default().kernel -= 1;
    }

    /// Summarize the recorded intervals over the full simulation time span.
    pub(crate) fn summarize(&self, total_time: TimeNs) -> StructopConcurrencySummary {
        let mut exact_levels: Vec<TimeNs> = vec![0];
        let mut userspace_time: TimeNs = 0;
        let mut kernel_time: TimeNs = 0;
        let mut active_structops: i64 = 0;
        let mut active_kernel: i64 = 0;
        let mut prev_time: TimeNs = 0;

        for (&time, &delta) in &self.deltas {
            let segment_end = time.min(total_time);
            if segment_end > prev_time {
                let duration = segment_end - prev_time;
                if active_structops > 0 {
                    let active_level = usize::try_from(active_structops)
                        .expect("structop concurrency cannot be negative");
                    if active_level >= exact_levels.len() {
                        exact_levels.resize(active_level + 1, 0);
                    }
                    exact_levels[active_level] += duration;
                } else if active_kernel > 0 {
                    kernel_time += duration;
                } else {
                    userspace_time += duration;
                }
                prev_time = segment_end;
            }

            if time >= total_time {
                break;
            }

            active_structops += i64::from(delta.structops);
            active_kernel += i64::from(delta.kernel);
            debug_assert!(active_structops >= 0, "structop concurrency underflow");
            debug_assert!(active_kernel >= 0, "kernel concurrency underflow");
        }

        if total_time > prev_time {
            let duration = total_time - prev_time;
            if active_structops > 0 {
                let active_level = usize::try_from(active_structops)
                    .expect("structop concurrency cannot be negative");
                if active_level >= exact_levels.len() {
                    exact_levels.resize(active_level + 1, 0);
                }
                exact_levels[active_level] += duration;
            } else if active_kernel > 0 {
                kernel_time += duration;
            } else {
                userspace_time += duration;
            }
        }

        StructopConcurrencySummary {
            total_time,
            userspace_time,
            kernel_time,
            exact_levels,
        }
    }
}

/// Completed logical-time histogram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StructopConcurrencySummary {
    total_time: TimeNs,
    userspace_time: TimeNs,
    kernel_time: TimeNs,
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
    println!("Logical-time histogram:");
    println!("  {:>8}  {:>12}  {:>7}", "state", "logical", "share");

    for (label, duration) in [
        ("0/user", summary.userspace_time),
        ("kernel", summary.kernel_time),
        ("1", summary.display_buckets()[1]),
        ("2", summary.display_buckets()[2]),
        ("3", summary.display_buckets()[3]),
        ("4+", summary.display_buckets()[4]),
    ] {
        let pct = if summary.total_time == 0 {
            0.0
        } else {
            100.0 * duration as f64 / summary.total_time as f64
        };
        println!("  {:>8}  {:>12}  {:>6.1}%", label, FmtN(duration), pct);
    }
}

#[cfg(test)]
mod tests {
    use super::{StructopConcurrencyTracker, DISPLAY_BUCKETS};
    use crate::types::CpuId;

    #[test]
    fn overlapping_callbacks_count_shared_time_once() {
        let mut tracker = StructopConcurrencyTracker::default();
        tracker.record_structop_interval(CpuId(0), 100, 200);
        tracker.record_structop_interval(CpuId(1), 100, 200);

        let summary = tracker.summarize(200);
        assert_eq!(summary.userspace_time, 100);
        assert_eq!(summary.kernel_time, 0);
        assert_eq!(summary.display_buckets(), [0, 0, 100, 0, 0]);
    }

    #[test]
    fn adjacent_fragments_preserve_total_overlap() {
        let mut tracker = StructopConcurrencyTracker::default();
        tracker.record_structop_interval(CpuId(0), 100, 200);
        tracker.record_structop_interval(CpuId(1), 100, 150);
        tracker.record_structop_interval(CpuId(1), 150, 200);

        let summary = tracker.summarize(200);
        assert_eq!(summary.userspace_time, 100);
        assert_eq!(summary.kernel_time, 0);
        assert_eq!(summary.display_buckets(), [0, 0, 100, 0, 0]);
    }

    #[test]
    fn four_plus_bucket_aggregates_higher_levels() {
        let mut tracker = StructopConcurrencyTracker::default();
        for cpu in 0..5 {
            tracker.record_structop_interval(CpuId(cpu), 0, 10);
        }

        let buckets = tracker.summarize(10).display_buckets();
        assert_eq!(buckets.len(), DISPLAY_BUCKETS);
        assert_eq!(buckets, [0, 0, 0, 0, 10]);
    }

    #[test]
    fn userspace_bucket_covers_leading_and_trailing_time() {
        let mut tracker = StructopConcurrencyTracker::default();
        tracker.record_structop_interval(CpuId(0), 50, 75);

        let summary = tracker.summarize(100);
        assert_eq!(summary.userspace_time, 75);
        assert_eq!(summary.kernel_time, 0);
        assert_eq!(summary.display_buckets(), [0, 25, 0, 0, 0]);
    }

    #[test]
    fn same_cpu_callbacks_are_serialized() {
        let mut tracker = StructopConcurrencyTracker::default();
        tracker.record_structop_interval(CpuId(0), 0, 10);
        tracker.record_structop_interval(CpuId(0), 0, 10);

        let summary = tracker.summarize(20);
        assert_eq!(summary.userspace_time, 0);
        assert_eq!(summary.display_buckets(), [0, 20, 0, 0, 0]);
    }

    #[test]
    fn kernel_time_is_split_out_from_zero_structop_time() {
        let mut tracker = StructopConcurrencyTracker::default();
        tracker.record_kernel_interval(CpuId(0), 10, 30);
        tracker.record_structop_interval(CpuId(0), 30, 40);

        let summary = tracker.summarize(100);
        assert_eq!(summary.userspace_time, 70);
        assert_eq!(summary.kernel_time, 20);
        assert_eq!(summary.display_buckets(), [0, 10, 0, 0, 0]);
    }

    #[test]
    fn summary_clamps_to_simulation_end() {
        let mut tracker = StructopConcurrencyTracker::default();
        tracker.record_structop_interval(CpuId(0), 90, 120);
        tracker.record_kernel_interval(CpuId(0), 120, 150);

        let summary = tracker.summarize(100);
        assert_eq!(summary.userspace_time, 90);
        assert_eq!(summary.kernel_time, 0);
        assert_eq!(summary.display_buckets(), [0, 10, 0, 0, 0]);
    }
}
