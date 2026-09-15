//! Trace statistics for comparing real vs simulated scheduler behavior.
//!
//! This module provides statistical analysis of simulation traces to help
//! identify realism gaps. Compare metrics from simulated traces against
//! equivalent metrics from bpftrace captures of real kernel behavior.
//!
//! # Metrics Computed
//!
//! - **Run duration distribution**: Min/max/mean/stddev of task run times
//! - **Inter-arrival times**: Time between consecutive schedules of a task
//! - **Dispatch path frequency**: Direct dispatch vs enqueue path ratio
//! - **Tick frequency**: Actual tick interval vs expected
//! - **Yield/re-enqueue cycles**: Number of spurious yield patterns
//!
//! These metrics help identify the realism gaps documented in sim-6b003.

use std::collections::HashMap;

use crate::trace::{Trace, TraceKind};
use crate::types::{CpuId, DsqId, Pid, TimeNs};
use serde::{Deserialize, Serialize};

/// Summary statistics for a distribution of values.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DistributionStats {
    /// Number of samples.
    pub count: usize,
    /// Minimum value (or 0 if empty).
    pub min: TimeNs,
    /// Maximum value (or 0 if empty).
    pub max: TimeNs,
    /// Sum of all values.
    pub sum: TimeNs,
    /// Sum of squares (for variance calculation).
    sum_sq: u128,
}

impl DistributionStats {
    /// Create new empty statistics.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a sample value.
    pub fn add(&mut self, value: TimeNs) {
        if self.count == 0 {
            self.min = value;
            self.max = value;
        } else {
            self.min = self.min.min(value);
            self.max = self.max.max(value);
        }
        self.count += 1;
        self.sum += value;
        self.sum_sq += (value as u128) * (value as u128);
    }

    /// Mean value (or 0 if empty).
    pub fn mean(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.sum as f64 / self.count as f64
        }
    }

    /// Standard deviation (or 0 if empty or single sample).
    pub fn stddev(&self) -> f64 {
        if self.count < 2 {
            0.0
        } else {
            let mean = self.mean();
            let variance = (self.sum_sq as f64 / self.count as f64) - (mean * mean);
            variance.max(0.0).sqrt()
        }
    }

    /// Coefficient of variation (stddev / mean), as a percentage.
    /// Returns 0 if mean is 0.
    pub fn cv_percent(&self) -> f64 {
        let mean = self.mean();
        if mean == 0.0 {
            0.0
        } else {
            100.0 * self.stddev() / mean
        }
    }
}

/// Per-task statistics computed from a trace.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskStats {
    /// PID of the task.
    pub pid: Pid,
    /// Number of times the task was scheduled.
    pub schedule_count: usize,
    /// Distribution of run durations (time between scheduled and stopped).
    pub run_duration: DistributionStats,
    /// Distribution of inter-arrival times (time between consecutive schedules).
    pub inter_arrival: DistributionStats,
    /// Number of times the task was dispatched directly (via SCX_DSQ_LOCAL in select_cpu).
    pub direct_dispatch_count: usize,
    /// Number of times the task went through the enqueue path.
    pub enqueue_count: usize,
    /// Number of yield events (task yielded but remained runnable).
    pub yield_count: usize,
    /// Number of preemption events (slice expired).
    pub preempt_count: usize,
    /// Number of sleep events (voluntary block).
    pub sleep_count: usize,
    /// Scheduling latencies (enqueue-to-scheduled) in nanoseconds, sorted.
    ///
    /// Each sample is the wall-clock time from `EnqueueTask` to the next
    /// `TaskScheduled` for this PID. Sorted ascending after `from_trace()`.
    pub sched_latencies: Vec<TimeNs>,
}

impl TaskStats {
    /// Return a scheduling latency percentile from the sorted `sched_latencies`.
    ///
    /// `p` is in the range 0.0..=1.0 (e.g. 0.99 for p99). Returns 0 if empty.
    pub fn sched_latency_pctl(&self, p: f64) -> TimeNs {
        percentile(&self.sched_latencies, p)
    }
}

/// Compute a percentile from a pre-sorted slice. `p` in 0.0..=1.0. Returns 0 if empty.
pub fn percentile(sorted: &[TimeNs], p: f64) -> TimeNs {
    if sorted.is_empty() {
        return 0;
    }
    sorted[((sorted.len() as f64 * p) as usize).min(sorted.len() - 1)]
}

/// Per-CPU statistics computed from a trace.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CpuStats {
    /// CPU ID.
    pub cpu: CpuId,
    /// Number of tick events on this CPU.
    pub tick_count: usize,
    /// Distribution of tick intervals.
    pub tick_interval: DistributionStats,
    /// Number of times this CPU went idle.
    pub idle_count: usize,
    /// Total time this CPU spent idle (nanoseconds).
    pub idle_duration_ns: TimeNs,
    /// Number of dispatch (balance) calls on this CPU.
    pub balance_count: usize,
}

/// Global trace statistics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TraceStats {
    /// Per-task statistics.
    pub tasks: HashMap<Pid, TaskStats>,
    /// Per-CPU statistics.
    pub cpus: HashMap<CpuId, CpuStats>,
    /// Total simulation duration.
    pub duration_ns: TimeNs,
    /// Warmup window copied from the trace. Events before this time are
    /// excluded from every count above; retained so reporters can tell
    /// "nothing happened" apart from "everything was filtered out".
    pub warmup_ns: TimeNs,
    /// Number of DsqInsert events (FIFO).
    pub dsq_insert_count: usize,
    /// Number of DsqInsertVtime events (vtime-ordered).
    pub dsq_insert_vtime_count: usize,
    /// Number of DsqMoveToLocal events.
    pub dsq_move_to_local_count: usize,
    /// Number of kick_cpu calls.
    pub kick_cpu_count: usize,
    /// Dispatch counts per DSQ ID (which DSQ each task was inserted into).
    pub dsq_dispatch_histogram: HashMap<DsqId, usize>,
}

impl TraceStats {
    /// Compute statistics from a simulation trace.
    ///
    /// When the trace has a non-zero `warmup_ns`, events before that time
    /// are still processed for state tracking (e.g. which CPU a task last ran
    /// on) but are not counted in the final statistics.
    pub fn from_trace(trace: &Trace) -> Self {
        let mut stats = TraceStats::default();
        let warmup = trace.warmup_ns();
        stats.warmup_ns = warmup;

        // Track last events for interval computation
        let mut task_last_scheduled: HashMap<Pid, TimeNs> = HashMap::new();
        let mut task_running_since: HashMap<Pid, TimeNs> = HashMap::new();
        let mut cpu_last_tick: HashMap<CpuId, TimeNs> = HashMap::new();
        let mut cpu_idle_since: HashMap<CpuId, TimeNs> = HashMap::new();
        let mut last_event_time: TimeNs = 0;

        // Track if the last select_cpu did a direct dispatch
        let mut pending_select_cpu: HashMap<Pid, TimeNs> = HashMap::new();
        let mut pending_direct_dispatch: HashMap<Pid, bool> = HashMap::new();
        // Track enqueue time for sched_latency (enqueue-to-scheduled)
        let mut task_enqueue_time: HashMap<Pid, TimeNs> = HashMap::new();

        for event in trace.events() {
            last_event_time = last_event_time.max(event.time_ns);
            let post_warmup = event.time_ns >= warmup;

            // Ensure CPU entries exist
            stats.cpus.entry(event.cpu).or_insert_with(|| CpuStats {
                cpu: event.cpu,
                ..Default::default()
            });

            match &event.kind {
                TraceKind::TaskScheduled { pid } => {
                    let task_stats = stats.tasks.entry(*pid).or_insert_with(|| TaskStats {
                        pid: *pid,
                        ..Default::default()
                    });
                    if post_warmup {
                        task_stats.schedule_count += 1;
                    }

                    // Record sched_latency (enqueue-to-scheduled)
                    if let Some(enq_time) = task_enqueue_time.remove(pid) {
                        if post_warmup {
                            let latency = event.time_ns.saturating_sub(enq_time);
                            task_stats.sched_latencies.push(latency);
                        }
                    }

                    task_running_since.insert(*pid, event.time_ns);

                    // End idle interval for this CPU if it was idle
                    if let Some(idle_start) = cpu_idle_since.remove(&event.cpu) {
                        let effective_start = idle_start.max(warmup);
                        if post_warmup || event.time_ns > effective_start {
                            let idle_dur = event.time_ns.saturating_sub(effective_start);
                            let cpu_stats = stats.cpus.get_mut(&event.cpu).unwrap();
                            cpu_stats.idle_duration_ns += idle_dur;
                        }
                    }

                    // Compute inter-arrival time
                    if let Some(last_time) = task_last_scheduled.get(pid) {
                        if post_warmup {
                            let interval = event.time_ns.saturating_sub(*last_time);
                            task_stats.inter_arrival.add(interval);
                        }
                    }
                    task_last_scheduled.insert(*pid, event.time_ns);
                }

                TraceKind::TaskPreempted { pid }
                | TraceKind::TaskYielded { pid }
                | TraceKind::TaskSlept { pid }
                | TraceKind::TaskCompleted { pid } => {
                    let task_stats = stats.tasks.entry(*pid).or_insert_with(|| TaskStats {
                        pid: *pid,
                        ..Default::default()
                    });

                    // Compute run duration
                    if let Some(start_time) = task_running_since.remove(pid) {
                        if post_warmup {
                            let effective_start = start_time.max(warmup);
                            let duration = event.time_ns.saturating_sub(effective_start);
                            task_stats.run_duration.add(duration);
                        }
                    }

                    if post_warmup {
                        match &event.kind {
                            TraceKind::TaskPreempted { .. } => task_stats.preempt_count += 1,
                            TraceKind::TaskYielded { .. } => task_stats.yield_count += 1,
                            TraceKind::TaskSlept { .. } => task_stats.sleep_count += 1,
                            _ => {}
                        }
                    }
                }

                TraceKind::SelectTaskRq { pid, .. } => {
                    pending_select_cpu.insert(*pid, event.time_ns);
                    pending_direct_dispatch.insert(*pid, false);
                }

                TraceKind::DsqInsert { pid, dsq_id, .. } => {
                    if post_warmup {
                        stats.dsq_insert_count += 1;
                        *stats.dsq_dispatch_histogram.entry(*dsq_id).or_insert(0) += 1;
                    }

                    if (dsq_id.is_local() || dsq_id.is_local_on())
                        && pending_select_cpu.contains_key(pid)
                    {
                        pending_direct_dispatch.insert(*pid, true);
                    }
                }

                TraceKind::DsqInsertVtime { dsq_id, .. } if post_warmup => {
                    stats.dsq_insert_vtime_count += 1;
                    *stats.dsq_dispatch_histogram.entry(*dsq_id).or_insert(0) += 1;
                }

                TraceKind::EnqueueTask { pid, .. } => {
                    let task_stats = stats.tasks.entry(*pid).or_insert_with(|| TaskStats {
                        pid: *pid,
                        ..Default::default()
                    });

                    let was_direct = pending_direct_dispatch.remove(pid).unwrap_or(false);
                    pending_select_cpu.remove(pid);

                    if post_warmup {
                        if was_direct {
                            task_stats.direct_dispatch_count += 1;
                        } else {
                            task_stats.enqueue_count += 1;
                        }
                    }

                    // Always record enqueue time for sched_latency tracking.
                    // The warmup check happens at TaskScheduled time.
                    task_enqueue_time.insert(*pid, event.time_ns);
                }

                TraceKind::Balance { .. } if post_warmup => {
                    let cpu_stats = stats.cpus.get_mut(&event.cpu).unwrap();
                    cpu_stats.balance_count += 1;
                }

                TraceKind::CpuIdle => {
                    if post_warmup {
                        let cpu_stats = stats.cpus.get_mut(&event.cpu).unwrap();
                        cpu_stats.idle_count += 1;
                    }
                    cpu_idle_since.insert(event.cpu, event.time_ns);
                }

                TraceKind::Tick { .. } => {
                    let cpu_stats = stats.cpus.get_mut(&event.cpu).unwrap();
                    if post_warmup {
                        cpu_stats.tick_count += 1;

                        if let Some(last_tick) = cpu_last_tick.get(&event.cpu) {
                            let interval = event.time_ns.saturating_sub(*last_tick);
                            cpu_stats.tick_interval.add(interval);
                        }
                    }
                    cpu_last_tick.insert(event.cpu, event.time_ns);
                }

                TraceKind::DsqMoveToLocal { .. } if post_warmup => {
                    stats.dsq_move_to_local_count += 1;
                }

                TraceKind::KickCpu { .. } if post_warmup => {
                    stats.kick_cpu_count += 1;
                }

                _ => {}
            }
        }

        // Flush CPUs still idle at end of trace
        for (cpu, idle_start) in &cpu_idle_since {
            let effective_start = (*idle_start).max(warmup);
            if last_event_time > effective_start {
                let idle_dur = last_event_time.saturating_sub(effective_start);
                if let Some(cpu_stats) = stats.cpus.get_mut(cpu) {
                    cpu_stats.idle_duration_ns += idle_dur;
                }
            }
        }

        // Remove placeholder task entry
        stats.tasks.remove(&Pid(0));
        stats.duration_ns = if warmup > 0 {
            last_event_time.saturating_sub(warmup)
        } else {
            last_event_time
        };

        // Sort sched_latencies for percentile queries
        for task_stats in stats.tasks.values_mut() {
            task_stats.sched_latencies.sort_unstable();
        }

        stats
    }

    /// True when the trace had task activity but the warmup window excluded
    /// all of it, so every statistic below reads zero.
    ///
    /// Task entries are created for any `TaskScheduled` event, warmup or not,
    /// so a non-empty task map whose every `schedule_count` is zero means the
    /// events existed and were filtered — not that nothing ran.
    pub fn all_activity_filtered_by_warmup(&self) -> bool {
        self.warmup_ns > 0
            && !self.tasks.is_empty()
            && self.tasks.values().all(|t| t.schedule_count == 0)
    }

    /// Print a summary report to stdout.
    pub fn print_summary(&self) {
        println!("\n=== Trace Statistics ===\n");
        println!("Duration: {:.3}ms", self.duration_ns as f64 / 1_000_000.0);
        // Never let an all-zero report pass as a measurement. The CLI rejects
        // warmup >= duration up front, but scenarios built through the library
        // or loaded from JSON bypass that check and reach here.
        if self.all_activity_filtered_by_warmup() {
            println!(
                "\n*** WARNING: every recorded event fell inside the {:.3}ms warmup window, \
                 so all counts below are 0 by construction, NOT a measurement of zero activity. \
                 Shorten the warmup or lengthen the run. ***",
                self.warmup_ns as f64 / 1_000_000.0,
            );
        }
        println!();

        println!("--- Per-Task Statistics ---");
        let mut task_pids: Vec<_> = self.tasks.keys().copied().collect();
        task_pids.sort_by_key(|p| p.0);

        for pid in task_pids {
            let ts = &self.tasks[&pid];
            println!("  Task PID={}:", pid.0);
            println!("    Schedules:       {}", ts.schedule_count);
            println!(
                "    Run duration:    {:.3}ms mean, {:.3}ms stddev, CV={:.1}%",
                ts.run_duration.mean() / 1_000_000.0,
                ts.run_duration.stddev() / 1_000_000.0,
                ts.run_duration.cv_percent()
            );
            println!(
                "    Inter-arrival:   {:.3}ms mean, {:.3}ms stddev",
                ts.inter_arrival.mean() / 1_000_000.0,
                ts.inter_arrival.stddev() / 1_000_000.0
            );
            println!("    Direct dispatch: {}", ts.direct_dispatch_count);
            println!("    Enqueue calls:   {}", ts.enqueue_count);
            println!("    Yields:          {}", ts.yield_count);
            println!("    Preemptions:     {}", ts.preempt_count);
            println!("    Sleeps:          {}", ts.sleep_count);
            if !ts.sched_latencies.is_empty() {
                println!(
                    "    Sched latency:   p50={:.3}us p90={:.3}us p99={:.3}us p999={:.3}us max={:.3}us ({} samples)",
                    ts.sched_latency_pctl(0.50) as f64 / 1_000.0,
                    ts.sched_latency_pctl(0.90) as f64 / 1_000.0,
                    ts.sched_latency_pctl(0.99) as f64 / 1_000.0,
                    ts.sched_latency_pctl(0.999) as f64 / 1_000.0,
                    ts.sched_latencies.last().copied().unwrap_or(0) as f64 / 1_000.0,
                    ts.sched_latencies.len(),
                );
            }
        }
        println!();

        println!("--- Per-CPU Statistics ---");
        let mut cpu_ids: Vec<_> = self.cpus.keys().copied().collect();
        cpu_ids.sort_by_key(|c| c.0);

        for cpu in cpu_ids {
            let cs = &self.cpus[&cpu];
            println!("  CPU {}:", cpu.0);
            println!("    Ticks:         {}", cs.tick_count);
            println!(
                "    Tick interval: {:.3}ms mean, {:.3}ms stddev",
                cs.tick_interval.mean() / 1_000_000.0,
                cs.tick_interval.stddev() / 1_000_000.0
            );
            println!("    Balance calls: {}", cs.balance_count);
            println!("    Idle events:   {}", cs.idle_count);
            println!(
                "    Idle duration: {:.3}ms",
                cs.idle_duration_ns as f64 / 1_000_000.0
            );
            if self.duration_ns > 0 {
                let util = 1.0 - (cs.idle_duration_ns as f64 / self.duration_ns as f64);
                println!("    Utilization:   {:.1}%", util * 100.0);
            }
        }
        println!();

        // Overall utilization across all CPUs
        if !self.cpus.is_empty() && self.duration_ns > 0 {
            let total_idle: u64 = self.cpus.values().map(|c| c.idle_duration_ns).sum();
            let num_cpus = self.cpus.len() as u64;
            let total_capacity = num_cpus * self.duration_ns;
            let overall_util = 1.0 - (total_idle as f64 / total_capacity as f64);
            println!(
                "--- Overall CPU Utilization: {:.1}% ({} CPUs, {:.3}ms) ---",
                overall_util * 100.0,
                num_cpus,
                self.duration_ns as f64 / 1_000_000.0,
            );
            println!();
        }

        println!("--- Global Statistics ---");
        println!("  DSQ inserts (FIFO):    {}", self.dsq_insert_count);
        println!("  DSQ inserts (vtime):   {}", self.dsq_insert_vtime_count);
        println!("  DSQ move_to_local:     {}", self.dsq_move_to_local_count);
        println!("  Kick CPU calls:        {}", self.kick_cpu_count);

        // DSQ dispatch histogram — classify by type
        if !self.dsq_dispatch_histogram.is_empty() {
            let mut local_count = 0usize;
            let mut cpdom_count = 0usize;
            let mut percpu_count = 0usize;
            let mut other_count = 0usize;
            for (&dsq, &count) in &self.dsq_dispatch_histogram {
                if dsq.is_local() || dsq.is_local_on() {
                    local_count += count;
                } else if dsq.0 & (1 << 12) != 0 {
                    // LAVD_DSQ_TYPE_CPDOM (bit 12 set)
                    cpdom_count += count;
                } else if dsq.0 < 1024 {
                    // Per-CPU DSQ (small IDs)
                    percpu_count += count;
                } else {
                    other_count += count;
                }
            }
            let total = local_count + cpdom_count + percpu_count + other_count;
            println!("  DSQ routing:");
            if local_count > 0 {
                println!(
                    "    Local/direct:   {:>6} ({:.1}%)",
                    local_count,
                    100.0 * local_count as f64 / total as f64,
                );
            }
            if cpdom_count > 0 {
                println!(
                    "    Cpdom (shared): {:>6} ({:.1}%)",
                    cpdom_count,
                    100.0 * cpdom_count as f64 / total as f64,
                );
            }
            if percpu_count > 0 {
                println!(
                    "    Per-CPU:        {:>6} ({:.1}%)",
                    percpu_count,
                    100.0 * percpu_count as f64 / total as f64,
                );
            }
            if other_count > 0 {
                println!(
                    "    Other:          {:>6} ({:.1}%)",
                    other_count,
                    100.0 * other_count as f64 / total as f64,
                );
            }
        }
        println!();
    }

    /// Compute a realism score based on known gap indicators.
    ///
    /// Returns a score from 0-100 where 100 means most realistic.
    /// Deductions are made for:
    /// - High yield counts (Gap 1: spurious yield/re-enqueue)
    /// - Low tick variance (Gap 2: tick frequency mismatch)
    /// - Zero run duration variance (Gap 6: no timing jitter)
    pub fn realism_score(&self) -> f64 {
        let mut score = 100.0;

        // Gap 1: Spurious yield/re-enqueue cycles
        // Real CPU-bound tasks rarely yield; they get preempted.
        for ts in self.tasks.values() {
            if ts.schedule_count > 0 {
                let yield_ratio = ts.yield_count as f64 / ts.schedule_count as f64;
                // Deduct up to 20 points for high yield ratio (>50%)
                if yield_ratio > 0.5 {
                    score -= 20.0 * (yield_ratio - 0.5).min(0.5) / 0.5;
                }
            }
        }

        // Gap 2: Tick frequency mismatch
        // Expected 4ms ticks (HZ=250). Real kernel has some jitter.
        for cs in self.cpus.values() {
            let tick_cv = cs.tick_interval.cv_percent();
            // Very low CV (<1%) suggests unrealistic uniformity
            if tick_cv < 1.0 && cs.tick_count > 5 {
                score -= 10.0;
            }
        }

        // Gap 6: Run duration variance
        // Real tasks have variance from interrupts, overhead, etc.
        for ts in self.tasks.values() {
            let run_cv = ts.run_duration.cv_percent();
            // Zero CV suggests unrealistic determinism
            if run_cv == 0.0 && ts.run_duration.count > 2 {
                score -= 10.0;
            }
        }

        score.max(0.0)
    }
}

/// Comparison between two traces (real vs simulated).
#[derive(Debug, Serialize, Deserialize)]
pub struct TraceComparison {
    /// Statistics from the baseline trace (typically real).
    pub baseline: TraceStats,
    /// Statistics from the comparison trace (typically simulated).
    pub comparison: TraceStats,
}

impl TraceComparison {
    /// Create a comparison between two traces.
    pub fn new(baseline: &Trace, comparison: &Trace) -> Self {
        Self {
            baseline: TraceStats::from_trace(baseline),
            comparison: TraceStats::from_trace(comparison),
        }
    }

    /// Print a comparison report to stderr.
    pub fn print_comparison(&self) {
        eprintln!("\n=== Trace Comparison ===\n");

        eprintln!("--- Duration ---");
        eprintln!(
            "  Baseline:   {:.3}ms",
            self.baseline.duration_ns as f64 / 1_000_000.0
        );
        eprintln!(
            "  Comparison: {:.3}ms",
            self.comparison.duration_ns as f64 / 1_000_000.0
        );
        eprintln!();

        eprintln!("--- DSQ Operations ---");
        eprintln!(
            "  DSQ inserts:  {} vs {} ({:+})",
            self.baseline.dsq_insert_count,
            self.comparison.dsq_insert_count,
            self.comparison.dsq_insert_count as i64 - self.baseline.dsq_insert_count as i64
        );
        eprintln!(
            "  Kick CPU:     {} vs {} ({:+})",
            self.baseline.kick_cpu_count,
            self.comparison.kick_cpu_count,
            self.comparison.kick_cpu_count as i64 - self.baseline.kick_cpu_count as i64
        );
        eprintln!();

        eprintln!("--- Realism Scores ---");
        eprintln!("  Baseline:   {:.1}/100", self.baseline.realism_score());
        eprintln!("  Comparison: {:.1}/100", self.comparison.realism_score());
        eprintln!();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The result/stats types round-trip through serde so an embedder
    /// (ktstr) can persist + diff run outputs. Single-entry maps keep the JSON
    /// key order stable for the string-equality round-trip assertion, and
    /// exercise serde_json's handling of the newtype map keys (Pid/CpuId/DsqId).
    #[test]
    fn trace_stats_serde_round_trip() {
        let mut stats = TraceStats {
            duration_ns: 5_000_000,
            dsq_insert_count: 7,
            ..Default::default()
        };
        let ts = TaskStats {
            pid: Pid(42),
            schedule_count: 3,
            ..Default::default()
        };
        stats.tasks.insert(Pid(42), ts);
        stats.cpus.insert(CpuId(0), CpuStats::default());
        stats.dsq_dispatch_histogram.insert(DsqId(0), 2);

        let json = serde_json::to_string(&stats).expect("serialize TraceStats");
        let back: TraceStats = serde_json::from_str(&json).expect("deserialize TraceStats");
        let json2 = serde_json::to_string(&back).expect("re-serialize TraceStats");
        assert_eq!(json, json2, "TraceStats serde round-trip not stable");
    }

    #[test]
    fn test_distribution_stats_empty() {
        let stats = DistributionStats::new();
        assert_eq!(stats.count, 0);
        assert_eq!(stats.mean(), 0.0);
        assert_eq!(stats.stddev(), 0.0);
    }

    #[test]
    fn test_distribution_stats_single() {
        let mut stats = DistributionStats::new();
        stats.add(1000);
        assert_eq!(stats.count, 1);
        assert_eq!(stats.min, 1000);
        assert_eq!(stats.max, 1000);
        assert_eq!(stats.mean(), 1000.0);
        assert_eq!(stats.stddev(), 0.0);
    }

    #[test]
    fn test_distribution_stats_multiple() {
        let mut stats = DistributionStats::new();
        stats.add(100);
        stats.add(200);
        stats.add(300);
        assert_eq!(stats.count, 3);
        assert_eq!(stats.min, 100);
        assert_eq!(stats.max, 300);
        assert_eq!(stats.mean(), 200.0);
        // stddev of [100,200,300] is ~81.65
        assert!(stats.stddev() > 80.0 && stats.stddev() < 83.0);
    }

    #[test]
    fn test_cv_percent() {
        let mut stats = DistributionStats::new();
        stats.add(100);
        stats.add(100);
        stats.add(100);
        // Zero variance -> 0% CV
        assert_eq!(stats.cv_percent(), 0.0);

        let mut stats2 = DistributionStats::new();
        stats2.add(100);
        stats2.add(200);
        // CV = stddev/mean * 100
        assert!(stats2.cv_percent() > 30.0);
    }

    /// Regression for rc-issue-scxsim-stats-zero: a warmup that covers the
    /// whole run zeroes every statistic while the engine's unfiltered slice
    /// counter still reports activity. The all-zero report must be
    /// distinguishable from a genuine zero-activity run.
    #[test]
    fn test_all_activity_filtered_by_warmup() {
        let scheduled = |pid: i32| TaskStats {
            pid: Pid(pid),
            ..Default::default()
        };

        // Tasks present, every schedule_count 0, warmup set -> filtered.
        let mut filtered = TraceStats {
            warmup_ns: 5_000_000_000,
            ..Default::default()
        };
        filtered.tasks.insert(Pid(1), scheduled(1));
        filtered.tasks.insert(Pid(2), scheduled(2));
        assert!(filtered.all_activity_filtered_by_warmup());

        // Same shape but zero warmup: nothing can have been filtered, so the
        // zeros are a real (if empty) measurement.
        let mut no_warmup = TraceStats::default();
        no_warmup.tasks.insert(Pid(1), scheduled(1));
        assert!(!no_warmup.all_activity_filtered_by_warmup());

        // Any task with activity means the warmup did not swallow the run.
        let mut partial = TraceStats {
            warmup_ns: 1_000_000,
            ..Default::default()
        };
        partial.tasks.insert(Pid(1), scheduled(1));
        partial.tasks.insert(
            Pid(2),
            TaskStats {
                pid: Pid(2),
                schedule_count: 7,
                ..Default::default()
            },
        );
        assert!(!partial.all_activity_filtered_by_warmup());

        // No tasks at all: genuinely nothing ran, not a filtering artifact.
        let empty = TraceStats {
            warmup_ns: 5_000_000_000,
            ..Default::default()
        };
        assert!(!empty.all_activity_filtered_by_warmup());
    }
}
