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

    /// Every on-CPU slice, as a distribution.
    ///
    /// A slice runs from a `TaskScheduled` to whichever of preempt / yield /
    /// sleep / completion ends it. The distribution — not just its mean — is
    /// needed because `Metric::MeanSliceLength`'s sample floor is only valid
    /// while the coefficient of variation stays low: a bimodal mixture of
    /// near-zero early exits and full slices can push the standard error of
    /// the mean out to the whole tolerance, at which point a pass means
    /// nothing. Callers are expected to report the CV alongside the mean.
    pub fn slice_durations(&self) -> Samples {
        let mut open: HashMap<Pid, u64> = HashMap::new();
        let mut out: Vec<u64> = Vec::new();
        let pids: Vec<Pid> = self.scenario.tasks.iter().map(|t| t.pid).collect();
        for e in self.trace.events() {
            match e.kind {
                TraceKind::TaskScheduled { pid } if pids.contains(&pid) => {
                    open.insert(pid, e.time_ns);
                }
                TraceKind::TaskPreempted { pid }
                | TraceKind::TaskYielded { pid }
                | TraceKind::TaskSlept { pid }
                | TraceKind::TaskCompleted { pid }
                    if pids.contains(&pid) =>
                {
                    if let Some(start) = open.remove(&pid) {
                        out.push(e.time_ns.saturating_sub(start));
                    }
                }
                _ => {}
            }
        }
        Samples::from_nanos(out)
    }

    /// Mean on-CPU slice length: workload CPU time over dispatch count.
    ///
    /// `None` when nothing was dispatched — a mean of no slices is not zero,
    /// it is undefined, and returning zero would let an empty run compare
    /// against a live one.
    pub fn mean_slice(&self) -> Option<DurationNs> {
        let n = self.context_switches();
        (n > 0).then(|| DurationNs(self.total_cpu_time().as_nanos() / n))
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
