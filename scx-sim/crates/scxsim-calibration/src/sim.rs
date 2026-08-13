//! Reading the simulated half: the same quantities, out of a [`Trace`].
//!
//! The counterpart to [`crate::vm`]. Both sides must be derived to the SAME
//! definition or the comparison measures the definitions rather than the
//! simulator, so each accessor below states the live definition it is matching.
//!
//! # Where the two estimators genuinely differ
//!
//! One does not reduce to the other, and pretending otherwise would hide it:
//!
//! * **Migrations.** ktstr counts them in userspace, by calling
//!   `sched_getcpu()` once per spin iteration and noticing when the answer
//!   changes (`workload/worker/mod.rs`). That is a SAMPLED estimator: a
//!   migration away and back between two samples is invisible to it, and it
//!   observes nothing while the task is off-CPU. The simulator counts every
//!   placement change the kernel actually made. On this scenario ktstr samples
//!   roughly every 35us, so the two converge closely, but they are not the same
//!   estimator and a large divergence should be read as such before it is read
//!   as infidelity.
//! * **Context switches.** ktstr's sidecar has no per-task count. Its
//!   `monitor.schedstat_deltas.total_sched_count` is VM-WIDE — every task in
//!   the guest, kernel threads and the monitor included. The simulator's
//!   `schedule_count` is per workload task. Different populations, so
//!   [`SimRun::context_switches`] exists but the calibration reports the metric
//!   as NotMeasured rather than comparing the two.
//!
//! # A known undercount, quantified rather than ignored
//!
//! [`Trace::total_runtime`] closes a task's running interval on preempt, yield,
//! sleep or completion, but not on `SimulationEnd`, so a task still on-CPU when
//! the simulation ends loses its final partial slice. That is bounded by one
//! slice (500us here) against a 12s run — 0.004%, and it biases CPU time DOWN
//! and off-CPU time UP. It is far below the off-CPU discrepancy this crate
//! measures, so it does not change any verdict, but it is the reason off-CPU
//! time in the simulator is not exactly zero for a task that never sleeps.

use std::collections::HashMap;

use scx_simulator::{CpuId, Pid, Scenario, Trace, TraceKind};

use crate::sample::Samples;
use crate::units::{DurationNs, Ratio};

/// The simulated counterpart of a [`crate::vm::VmRun`].
///
/// Borrows rather than copies: every quantity is derived on demand from the
/// trace, so there is no second representation to drift.
pub struct SimRun<'a> {
    scenario: &'a Scenario,
    trace: &'a Trace,
}

impl<'a> SimRun<'a> {
    pub fn new(scenario: &'a Scenario, trace: &'a Trace) -> Self {
        SimRun { scenario, trace }
    }

    /// Elapsed simulated time: the timestamp of the last event.
    ///
    /// This is the denominator for occupancy and the wall clock for off-CPU
    /// time, matching ktstr's per-worker `wall_time_ns`. Taken from the trace
    /// rather than from `scenario.duration_ns` because a simulation can end
    /// early, and using the requested duration would then silently inflate
    /// off-CPU time by the difference.
    pub fn elapsed(&self) -> DurationNs {
        DurationNs(
            self.trace
                .events()
                .iter()
                .map(|e| e.time_ns)
                .max()
                .unwrap_or(0),
        )
    }

    /// PIDs belonging to a cgroup, in scenario order.
    pub fn pids_in(&self, cgroup: &str) -> impl Iterator<Item = Pid> + '_ {
        let cgroup = cgroup.to_string();
        self.scenario
            .tasks
            .iter()
            .filter(move |t| t.cgroup_name.as_deref() == Some(cgroup.as_str()))
            .map(|t| t.pid)
    }

    /// CPU time for a cgroup. Matches ktstr's `total_cpu_time_ns`: summed over
    /// the cgroup's workers.
    pub fn cpu_time(&self, cgroup: &str) -> DurationNs {
        DurationNs(
            self.pids_in(cgroup)
                .map(|pid| self.trace.total_runtime(pid))
                .sum(),
        )
    }

    /// Total CPU time across every task in the scenario.
    pub fn total_cpu_time(&self) -> DurationNs {
        DurationNs(
            self.scenario
                .tasks
                .iter()
                .map(|t| self.trace.total_runtime(t.pid))
                .sum(),
        )
    }

    /// Off-CPU time as a fraction of wall, averaged over the cgroup's workers.
    ///
    /// Matches ktstr's `avg_off_cpu_pct / 100`, whose per-worker term is
    /// `off_cpu_ns / wall_time_ns` with `off_cpu_ns = wall - cpu` (saturating).
    pub fn off_cpu_fraction(&self, cgroup: &str) -> Option<Ratio> {
        let wall = self.elapsed().as_nanos() as f64;
        if wall == 0.0 {
            return None;
        }
        let mut n = 0u32;
        let mut acc = 0.0;
        for pid in self.pids_in(cgroup) {
            let cpu = self.trace.total_runtime(pid) as f64;
            acc += (wall - cpu).max(0.0) / wall;
            n += 1;
        }
        (n > 0).then(|| Ratio(acc / n as f64))
    }

    /// Occupancy: busy CPU time over available CPU time.
    pub fn occupancy(&self) -> Ratio {
        let capacity = self.elapsed().as_nanos() as f64 * self.scenario.nr_cpus as f64;
        if capacity == 0.0 {
            return Ratio(0.0);
        }
        Ratio(self.total_cpu_time().as_nanos() as f64 / capacity)
    }

    /// Migrations: placement changes between consecutive dispatches of a task.
    ///
    /// See the module note — this is the kernel's exact count, against ktstr's
    /// userspace sampled one.
    pub fn migrations(&self) -> u64 {
        let mut last: HashMap<Pid, CpuId> = HashMap::new();
        let mut count = 0u64;
        for e in self.trace.events() {
            if let TraceKind::TaskScheduled { pid } = e.kind {
                match last.insert(pid, e.cpu) {
                    Some(prev) if prev != e.cpu => count += 1,
                    _ => {}
                }
            }
        }
        count
    }

    /// Migrations for one cgroup's tasks.
    pub fn migrations_in(&self, cgroup: &str) -> u64 {
        let pids: Vec<Pid> = self.pids_in(cgroup).collect();
        let mut last: HashMap<Pid, CpuId> = HashMap::new();
        let mut count = 0u64;
        for e in self.trace.events() {
            if let TraceKind::TaskScheduled { pid } = e.kind {
                if !pids.contains(&pid) {
                    continue;
                }
                match last.insert(pid, e.cpu) {
                    Some(prev) if prev != e.cpu => count += 1,
                    _ => {}
                }
            }
        }
        count
    }

    /// Context switches across the scenario's tasks.
    ///
    /// Present for completeness and for the record in the report; NOT compared,
    /// because the live side counts a different population. See the module
    /// note.
    pub fn context_switches(&self) -> u64 {
        self.scenario
            .tasks
            .iter()
            .map(|t| self.trace.schedule_count(t.pid) as u64)
            .sum()
    }

    /// Scheduling delay: total time runnable-but-not-running, averaged over the
    /// cgroup's tasks. `None` when the cgroup has no task in the scenario.
    ///
    /// # The live definition this is matching, stated exactly
    ///
    /// ktstr's `mean_run_delay_us` is the mean over the cgroup's WORKERS of each
    /// worker's whole-run delta in `task->sched_info.run_delay`, read from
    /// `/proc/self/task/<tid>/schedstat` field 2 (`workload/worker/sched.rs`,
    /// `assert/reductions.rs`). The kernel accumulates that field in
    /// `sched_info_arrive()` at each dispatch, as `now - last_queued`, where
    /// `last_queued` is stamped by `sched_info_enqueue()` on every enqueue —
    /// including the re-enqueue of a preempted task. So: the summed length of
    /// every on-runqueue-but-not-on-CPU interval, per worker, then averaged
    /// across workers. It is a TOTAL per worker, not a per-dispatch mean;
    /// `worst_run_delay_us` is the worker with the largest total, not the worst
    /// single dispatch. (The kernel also exposes the dispatch count in field 3,
    /// but ktstr does not publish it per cgroup, so no per-episode figure is
    /// available on the live side and none is computed here.)
    ///
    /// This accumulates `TaskScheduled - EnqueueTask` per episode and sums, per
    /// task, then averages over tasks — the same shape, deliberately. The rule
    /// is the one `TraceStats::sched_latencies` uses and
    /// `tests/rundelay_tracking.rs` validates, including that samples accumulate
    /// across preempt/re-enqueue cycles, which is what makes it a total rather
    /// than a first-wakeup latency.
    ///
    /// # Where the two still differ
    ///
    /// Two known differences, both of which push the simulator DOWN. Neither
    /// determines the sign of an observed gap, and on the one run measured so
    /// far the simulator came out ABOVE one cgroup and BELOW the other — so do
    /// not read "sim is lower" as the expected outcome.
    ///
    /// * **Direct dispatch.** When `select_cpu` finds an idle CPU the simulator
    ///   never emits `EnqueueTask`, so that dispatch contributes no sample —
    ///   correctly zero, because in the simulator a wakeup costs no time. The
    ///   kernel enqueues unconditionally and charges the real wakeup-to-switch
    ///   path, single-digit microseconds, on every one of those. Measure the
    ///   exposure with [`SimRun::direct_dispatches`] rather than assuming it:
    ///   on `sched_basic_proportional` it is 1 dispatch in 24029, which rules
    ///   this out as an explanation for anything on that run.
    /// * **No interference.** The simulator has no IRQs, no timer ticks, no
    ///   kernel threads, no host, and only the scenario's own tasks exist. Its
    ///   scheduling delay is a FLOOR on what a real machine would show, not an
    ///   estimate of it. Useful for comparing scheduler policy; misleading for
    ///   anything about jitter or tails.
    ///
    /// What CAN dominate the sign is the schedulers being different. A global-
    /// DSQ policy in the simulator can queue tasks the guest's policy would not,
    /// which is a policy difference and not infidelity; on a calibration where
    /// both sides run the same scheduler that term disappears.
    ///
    /// The populations DO match, unlike context switches: both sides count the
    /// workload's workers and nothing else.
    pub fn run_delay(&self, cgroup: &str) -> Option<DurationNs> {
        let pids: Vec<Pid> = self.pids_in(cgroup).collect();
        if pids.is_empty() {
            return None;
        }
        let mut enqueued_at: HashMap<Pid, u64> = HashMap::new();
        let mut total: HashMap<Pid, u64> = HashMap::new();
        for e in self.trace.events() {
            match e.kind {
                TraceKind::EnqueueTask { pid, .. } if pids.contains(&pid) => {
                    // Refresh rather than insert: a second enqueue with no
                    // intervening dispatch restarts the wait, as
                    // `sched_info_enqueue` restamps `last_queued`.
                    enqueued_at.insert(pid, e.time_ns);
                }
                TraceKind::TaskScheduled { pid } if pids.contains(&pid) => {
                    if let Some(t0) = enqueued_at.remove(&pid) {
                        *total.entry(pid).or_default() += e.time_ns.saturating_sub(t0);
                    }
                }
                _ => {}
            }
        }
        // Tasks that never queued contribute a real zero, matching the live
        // side: ktstr pools one value per worker and a worker that never waited
        // reads a measured 0.0, not a missing sample.
        let sum: u64 = pids
            .iter()
            .map(|p| total.get(p).copied().unwrap_or(0))
            .sum();
        Some(DurationNs(sum / pids.len() as u64))
    }

    /// Dispatches of a cgroup's tasks that bypassed the enqueue path entirely.
    ///
    /// The size of the known downward bias in [`SimRun::run_delay`]: each of
    /// these contributes zero delay in the simulator and a real wakeup-path cost
    /// in the guest. Reported rather than corrected — inventing a per-wakeup
    /// overhead to close the gap would be manufacturing the agreement.
    /// Dispatches of a cgroup's tasks, the denominator for
    /// [`SimRun::direct_dispatches`].
    pub fn dispatches(&self, cgroup: &str) -> u64 {
        let pids: Vec<Pid> = self.pids_in(cgroup).collect();
        self.trace
            .events()
            .iter()
            .filter(|e| matches!(e.kind, TraceKind::TaskScheduled { pid } if pids.contains(&pid)))
            .count() as u64
    }

    pub fn direct_dispatches(&self, cgroup: &str) -> u64 {
        let pids: Vec<Pid> = self.pids_in(cgroup).collect();
        let mut queued: HashMap<Pid, bool> = HashMap::new();
        let mut count = 0u64;
        for e in self.trace.events() {
            match e.kind {
                TraceKind::EnqueueTask { pid, .. } if pids.contains(&pid) => {
                    queued.insert(pid, true);
                }
                TraceKind::TaskScheduled { pid }
                    if pids.contains(&pid) && !queued.remove(&pid).unwrap_or(false) =>
                {
                    count += 1;
                }
                _ => {}
            }
        }
        count
    }

    /// Wake-to-run latencies: each `TaskWoke` to that task's next
    /// `TaskScheduled`.
    ///
    /// The simulator CAN produce this distribution. Whether there is anything
    /// to compare it against is a question for the live side, which on this
    /// scenario did not measure it.
    pub fn wake_latencies(&self) -> Samples {
        let mut woke_at: HashMap<Pid, u64> = HashMap::new();
        let mut out: Vec<u64> = Vec::new();
        for e in self.trace.events() {
            match e.kind {
                TraceKind::TaskWoke { pid } => {
                    woke_at.insert(pid, e.time_ns);
                }
                TraceKind::TaskScheduled { pid } => {
                    if let Some(t0) = woke_at.remove(&pid) {
                        out.push(e.time_ns.saturating_sub(t0));
                    }
                }
                _ => {}
            }
        }
        Samples::from_nanos(out)
    }
}
