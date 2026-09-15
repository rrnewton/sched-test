//! Bail-to-next-schedule: how long a task waits between being parked by
//! cgroup bandwidth control and next actually running.
//!
//! # Why this exists as a first-class metric
//!
//! It is the only instrument that saw the scx#3618 pathology. For the same
//! run, scx-sim's runnable-stall watchdog reported 2.80s, 4.30s, and
//! eventually no stall at all, while the real worst wait was **40.28
//! seconds**. The watchdog is threshold-based and, since the throttle-aware
//! change, declines to report a task whose cgroup is throttled — which is
//! exactly the population this metric measures. This measurement is
//! threshold-independent and derived purely from the trace, so it is
//! unaffected by that decision either way.
//!
//! It previously lived as a private helper in
//! `tests/pr3618_cpumax_unbounded_wait.rs`, where only a human reading the
//! printed output ever saw it.
//!
//! # What is measured
//!
//! For each pid: the interval from its `LavdBailOnCgroupThrottle` — the real
//! `cgroup_bw.bpf.c` parking the task — to that pid's next `TaskScheduled`.
//! Repeated bails before the task next runs collapse into one interval
//! starting at the FIRST bail, because that is when the wait began.
//!
//! A pid still parked when the trace ends never got its schedule, so its wait
//! is a LOWER BOUND: `end_of_trace - first_bail`. Those pids are reported
//! separately by [`StarvationMetrics::never_rescheduled`] and must not be read
//! as converged values.
//!
//! Those lower bounds ARE included in the distribution and in
//! [`StarvationMetrics::worst_ns`]. Excluding them would drop precisely the
//! worst-affected tasks from the summary — the censoring failure this metric
//! exists to avoid — so they are counted, and
//! [`StarvationMetrics::never_rescheduled`] tells the reader how many of the
//! samples are bounds rather than completed waits.
//!
//! # The distribution matters more than the maximum
//!
//! Report [`StarvationMetrics::wait_pctl`] alongside
//! [`StarvationMetrics::worst_ns`]. On the #3618 reproducer the median wait is
//! sub-millisecond while the worst is 40 seconds; a single number cannot say
//! "tail phenomenon", and a metric exposing only a maximum re-creates exactly
//! the censoring problem this replaced.
//!
//! # The control
//!
//! [`StarvationMetrics::control_pctl`] measures the gap between consecutive
//! schedules for tasks that NEVER bailed — the unlimited competitor outside
//! the throttled cgroup. Its staying flat across every quota is what proves
//! the machine was not merely saturated. A starvation number without that
//! control invites the obvious objection.
//!
//! Note the control population is "never bailed", which is a trace-derived
//! PROXY for "not bandwidth-limited", not a cgroup-membership query. It is
//! accurate for the shapes we run (a limited cgroup plus an outside
//! competitor) and cheap, because it falls out of the same single pass.
//!
//! # This is measurement only
//!
//! Nothing here is wired into the watchdog or any pass/fail gate. Whether
//! anything should trip on these numbers depends on an unresolved question
//! about throttle suppression (see
//! `ai_docs/WATCHDOG_THROTTLE_SUPPRESSION_IS_A_FIDELITY_GAP_20260813.md`);
//! deciding it by implementation here would pre-empt that.

use crate::safe::stats::{percentile, DistributionStats};
use crate::safe::trace::{Trace, TraceKind};
use crate::types::{Pid, TimeNs};
use std::collections::BTreeMap;

/// One bail-to-next-schedule interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BailInterval {
    /// The task that was parked.
    pub pid: Pid,
    /// When it was FIRST parked in this episode.
    pub bailed_at_ns: TimeNs,
    /// How long it waited. For an unresolved interval this is
    /// `end_of_trace - bailed_at_ns`, i.e. a lower bound.
    pub wait_ns: TimeNs,
    /// `true` if the task was observed running again; `false` if it was still
    /// parked when the trace ended.
    pub resolved: bool,
}

/// Bail-to-next-schedule measurements over one trace, plus the
/// never-bailed control.
#[derive(Debug, Clone, Default)]
pub struct StarvationMetrics {
    intervals: Vec<BailInterval>,
    waits_sorted: Vec<TimeNs>,
    waits: DistributionStats,
    never_rescheduled: Vec<Pid>,
    control_sorted: Vec<TimeNs>,
    control: DistributionStats,
    control_pids: Vec<Pid>,
}

impl StarvationMetrics {
    /// Compute the metrics from a trace in a single pass.
    pub fn from_trace(trace: &Trace) -> Self {
        // First bail of the current episode, per pid.
        let mut pending: BTreeMap<Pid, TimeNs> = BTreeMap::new();
        let mut ever_bailed: BTreeMap<Pid, ()> = BTreeMap::new();
        let mut last_sched: BTreeMap<Pid, TimeNs> = BTreeMap::new();
        let mut control_gaps: BTreeMap<Pid, Vec<TimeNs>> = BTreeMap::new();
        let mut intervals: Vec<BailInterval> = Vec::new();
        let mut end_ns: TimeNs = 0;

        for ev in trace.events() {
            end_ns = end_ns.max(ev.time_ns);
            match &ev.kind {
                TraceKind::LavdBailOnCgroupThrottle { pid, .. } => {
                    // `or_insert`: a repeated bail before the task next runs
                    // does not restart the clock — the wait began at the first.
                    pending.entry(*pid).or_insert(ev.time_ns);
                    ever_bailed.insert(*pid, ());
                }
                TraceKind::TaskScheduled { pid } => {
                    if let Some(bailed_at_ns) = pending.remove(pid) {
                        intervals.push(BailInterval {
                            pid: *pid,
                            bailed_at_ns,
                            wait_ns: ev.time_ns.saturating_sub(bailed_at_ns),
                            resolved: true,
                        });
                    }
                    if let Some(prev) = last_sched.insert(*pid, ev.time_ns) {
                        control_gaps
                            .entry(*pid)
                            .or_default()
                            .push(ev.time_ns.saturating_sub(prev));
                    }
                }
                _ => {}
            }
        }

        // Still parked when the trace ended: the wait is a lower bound.
        let mut never_rescheduled: Vec<Pid> = Vec::new();
        for (pid, bailed_at_ns) in pending {
            intervals.push(BailInterval {
                pid,
                bailed_at_ns,
                wait_ns: end_ns.saturating_sub(bailed_at_ns),
                resolved: false,
            });
            never_rescheduled.push(pid);
        }

        let mut waits = DistributionStats::new();
        let mut waits_sorted: Vec<TimeNs> = Vec::with_capacity(intervals.len());
        for iv in &intervals {
            waits.add(iv.wait_ns);
            waits_sorted.push(iv.wait_ns);
        }
        waits_sorted.sort_unstable();

        // Control: consecutive-schedule gaps for pids that never bailed.
        let mut control = DistributionStats::new();
        let mut control_sorted: Vec<TimeNs> = Vec::new();
        let mut control_pids: Vec<Pid> = Vec::new();
        for (pid, gaps) in &control_gaps {
            if ever_bailed.contains_key(pid) {
                continue;
            }
            control_pids.push(*pid);
            for g in gaps {
                control.add(*g);
                control_sorted.push(*g);
            }
        }
        control_sorted.sort_unstable();

        Self {
            intervals,
            waits_sorted,
            waits,
            never_rescheduled,
            control_sorted,
            control,
            control_pids,
        }
    }

    /// Every measured interval, in the order the waits completed.
    pub fn intervals(&self) -> &[BailInterval] {
        &self.intervals
    }

    /// Number of measured intervals.
    pub fn sample_count(&self) -> usize {
        self.waits.count
    }

    /// Longest wait observed. 0 when nothing bailed.
    pub fn worst_ns(&self) -> TimeNs {
        self.waits.max
    }

    /// Shortest wait observed. 0 when nothing bailed.
    pub fn best_ns(&self) -> TimeNs {
        self.waits.min
    }

    /// Mean wait in nanoseconds. 0 when nothing bailed.
    pub fn mean_ns(&self) -> f64 {
        self.waits.mean()
    }

    /// A wait percentile. `p` in 0.0..=1.0 (0.5 for the median). 0 when empty.
    ///
    /// Quote this next to [`Self::worst_ns`]. The gap between them is the
    /// finding.
    ///
    /// Includes the lower-bound waits of tasks still parked at end of trace;
    /// see [`Self::never_rescheduled`] for how many those are.
    pub fn wait_pctl(&self, p: f64) -> TimeNs {
        percentile(&self.waits_sorted, p)
    }

    /// Pids still parked when the trace ended. Their intervals are lower
    /// bounds, not converged waits.
    pub fn never_rescheduled(&self) -> &[Pid] {
        &self.never_rescheduled
    }

    /// Worst wait per pid.
    pub fn per_pid_worst(&self) -> BTreeMap<Pid, TimeNs> {
        let mut m: BTreeMap<Pid, TimeNs> = BTreeMap::new();
        for iv in &self.intervals {
            let e = m.entry(iv.pid).or_insert(0);
            *e = (*e).max(iv.wait_ns);
        }
        m
    }

    /// Pids in the control population — those that never bailed.
    pub fn control_pids(&self) -> &[Pid] {
        &self.control_pids
    }

    /// Number of control gap samples.
    pub fn control_sample_count(&self) -> usize {
        self.control.count
    }

    /// Longest consecutive-schedule gap among tasks that never bailed.
    pub fn control_worst_ns(&self) -> TimeNs {
        self.control.max
    }

    /// A control-gap percentile. `p` in 0.0..=1.0. 0 when empty.
    pub fn control_pctl(&self, p: f64) -> TimeNs {
        percentile(&self.control_sorted, p)
    }

    /// One-line summary for test output and reports. Leads with the
    /// distribution rather than the maximum.
    pub fn summary(&self) -> String {
        format!(
            "bail->sched: n={} p50={:.3}ms p90={:.3}ms worst={:.3}ms never_resched={} | \
             control(never-bailed): n={} p50={:.3}ms worst={:.3}ms",
            self.sample_count(),
            self.wait_pctl(0.5) as f64 / 1e6,
            self.wait_pctl(0.9) as f64 / 1e6,
            self.worst_ns() as f64 / 1e6,
            self.never_rescheduled.len(),
            self.control_sample_count(),
            self.control_pctl(0.5) as f64 / 1e6,
            self.control_worst_ns() as f64 / 1e6,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safe::cgroup::CgroupId;
    use crate::safe::trace::Trace;
    use crate::types::CpuId;

    /// Build a trace from `(time_ns, kind)` pairs. All on CPU 0; this metric
    /// does not read the CPU field.
    fn trace_of(events: &[(TimeNs, TraceKind)]) -> Trace {
        let mut t = Trace::new(1, &[]);
        for (time_ns, kind) in events {
            t.record(*time_ns, CpuId(0), kind.clone());
        }
        t
    }

    fn bail(pid: u64) -> TraceKind {
        TraceKind::LavdBailOnCgroupThrottle {
            pid: Pid(pid as i32),
            cgid: CgroupId(1),
        }
    }

    fn sched(pid: u64) -> TraceKind {
        TraceKind::TaskScheduled {
            pid: Pid(pid as i32),
        }
    }

    #[test]
    fn measures_bail_to_the_next_schedule() {
        let m = StarvationMetrics::from_trace(&trace_of(&[(1_000, bail(7)), (5_000, sched(7))]));
        assert_eq!(m.sample_count(), 1);
        assert_eq!(m.worst_ns(), 4_000);
        assert!(m.intervals()[0].resolved);
        assert!(m.never_rescheduled().is_empty());
    }

    #[test]
    fn repeated_bails_before_running_collapse_to_the_first() {
        // The wait began at the FIRST bail. Restarting the clock on each
        // re-bail would under-report a long starvation as a series of short
        // ones -- which is the whole failure mode this metric exists to catch.
        let m = StarvationMetrics::from_trace(&trace_of(&[
            (1_000, bail(7)),
            (2_000, bail(7)),
            (3_000, bail(7)),
            (9_000, sched(7)),
        ]));
        assert_eq!(m.sample_count(), 1, "three bails, one wait");
        assert_eq!(
            m.worst_ns(),
            8_000,
            "the wait must be measured from the FIRST bail (1_000), not the last"
        );
    }

    #[test]
    fn a_task_still_parked_at_the_end_is_a_lower_bound_and_is_flagged() {
        let m = StarvationMetrics::from_trace(&trace_of(&[
            (1_000, bail(7)),
            (4_000, sched(9)), // someone else runs; 7 never does
        ]));
        assert_eq!(m.never_rescheduled(), &[Pid(7)]);
        let iv = m.intervals().iter().find(|i| i.pid == Pid(7)).unwrap();
        assert!(!iv.resolved, "must not be reported as a completed wait");
        assert_eq!(iv.wait_ns, 3_000, "lower bound = end_of_trace - first bail");
    }

    #[test]
    fn a_new_episode_after_running_is_measured_separately() {
        let m = StarvationMetrics::from_trace(&trace_of(&[
            (1_000, bail(7)),
            (2_000, sched(7)),
            (5_000, bail(7)),
            (11_000, sched(7)),
        ]));
        assert_eq!(m.sample_count(), 2);
        assert_eq!(m.worst_ns(), 6_000);
        assert_eq!(m.per_pid_worst()[&Pid(7)], 6_000);
    }

    #[test]
    fn the_distribution_separates_a_tail_from_the_body() {
        // The #3618 shape in miniature: a sub-millisecond median with a huge
        // worst case. Reporting only the max, or only the mean, loses the
        // distinction between "always slow" and "occasionally catastrophic".
        let mut evs: Vec<(TimeNs, TraceKind)> = Vec::new();
        let mut t = 0u64;
        for _ in 0..99 {
            evs.push((t, bail(7)));
            evs.push((t + 1_000, sched(7)));
            t += 10_000;
        }
        evs.push((t, bail(7)));
        evs.push((t + 40_000_000_000, sched(7)));
        let m = StarvationMetrics::from_trace(&trace_of(&evs));

        assert_eq!(m.sample_count(), 100);
        assert_eq!(m.wait_pctl(0.5), 1_000, "median is the body, not the tail");
        assert_eq!(m.worst_ns(), 40_000_000_000, "the tail is still visible");
        assert!(
            m.worst_ns() / m.wait_pctl(0.5) > 1_000_000,
            "p50 and worst must be reportable independently; a metric that \
             exposes only one of them cannot say 'tail phenomenon'"
        );
    }

    #[test]
    fn the_control_covers_only_tasks_that_never_bailed() {
        // pid 7 is throttled; pid 9 is the outside competitor. The control
        // must exclude 7 entirely, or it would launder the starvation it is
        // supposed to be a baseline against.
        let m = StarvationMetrics::from_trace(&trace_of(&[
            (1_000, bail(7)),
            (2_000, sched(9)),
            (5_000, sched(9)),
            (9_000, sched(9)),
            (50_000, sched(7)),
        ]));
        assert_eq!(m.control_pids(), &[Pid(9)]);
        assert_eq!(m.control_sample_count(), 2, "3 schedules = 2 gaps");
        assert_eq!(m.control_worst_ns(), 4_000);
        // And the starved task is still measured, on the other side.
        assert_eq!(m.worst_ns(), 49_000);
    }

    #[test]
    fn empty_trace_reports_zeroes_rather_than_panicking() {
        let m = StarvationMetrics::from_trace(&trace_of(&[]));
        assert_eq!(m.sample_count(), 0);
        assert_eq!(m.worst_ns(), 0);
        assert_eq!(m.wait_pctl(0.5), 0);
        assert_eq!(m.control_worst_ns(), 0);
        assert!(m.never_rescheduled().is_empty());
    }
}
