//! Periodic full-state snapshots of the scxsim engine.
//!
//! This module provides a *sample-based* view of the simulator state, in
//! contrast to the engine's existing event traces (`CgroupBwCharge`,
//! `CgroupBwThrottle`, `CgroupBwAdmit`, `CgroupBwRefill`) which are
//! *delta-based*. Event traces answer "what decisions fired"; state
//! snapshots answer "what did the world look like at simulated-time T".
//!
//! Snapshots are taken at fixed simulated-time intervals (default 100us)
//! and serialized as one JSON object per line (jsonl). The intent is
//! post-hoc analysis with line-oriented Python / `jq` tools — see
//! `experiments/lavd_cpubw_stalls_202604/overnight_2026-05-08/analyze_state_evolution.py`.
//!
//! # Wiring
//!
//! - [`SnapshotWriter::maybe_emit`] is called from the engine event loop
//!   between popped events. It is non-perturbing: it does not stage
//!   events, mutate scheduler state, or take any extra locks.
//! - The writer is attached to a `Simulator` via
//!   `Simulator::with_state_snapshot(writer)` (see `engine.rs`).
//!
//! # Determinism
//!
//! All keys in the snapshot are sorted (cgroup IDs, DSQ IDs, PIDs) so
//! that two runs with the same seed produce byte-identical output.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;

use serde::Serialize;

use crate::cgroup::{CgroupId, CgroupRegistry};
use crate::cgroup_bw::BandwidthManager;
use crate::cpu::{IrqContext, SimCpu};
use crate::dsq::DsqManager;
use crate::types::{CpuId, DsqId, Pid, TimeNs};

/// Maximum number of runnable tasks captured per snapshot.
///
/// Tasks are sorted by `runnable_for_ns` descending and the top-N are
/// kept. For the canonical Bug-1 repro this is more than the entire task
/// set; for stress workloads it bounds the per-snapshot JSON size.
pub const TASKS_RUNNABLE_CAP: usize = 32;

/// Maximum number of head PIDs reported per DSQ.
pub const DSQ_HEAD_PIDS: usize = 3;

/// One periodic snapshot of the simulator's full state.
#[derive(Debug, Clone, Serialize)]
pub struct StateSnapshot {
    /// Simulated clock at the moment of the sample (ns).
    pub t_ns: TimeNs,
    /// Monotonic snapshot index (0, 1, 2 ...). Useful as an x-axis when
    /// the sample interval is irregular due to long-running events.
    pub idx: u64,
    /// Roll-up counts for quick narrative scanning.
    pub summary: SnapshotSummary,
    /// Per-cgroup bandwidth state (only cgroups with finite quota).
    pub cgroups: Vec<CgroupSnapshot>,
    /// Per-CPU runtime state.
    pub cpus: Vec<CpuSnapshot>,
    /// Per-DSQ depth and head ordering.
    pub dsqs: Vec<DsqSnapshot>,
    /// Top-N runnable tasks by `runnable_for_ns` descending.
    pub tasks_runnable: Vec<TaskSnapshot>,
}

/// Aggregate counters cheap to compute and useful for narrative views.
#[derive(Debug, Clone, Serialize)]
pub struct SnapshotSummary {
    pub nr_runnable: u32,
    pub nr_running: u32,
    pub nr_sleeping: u32,
    pub nr_idle_cpus: u32,
    pub nr_throttled_cgroups: u32,
    /// Maximum `runnable_for_ns` across all currently-runnable tasks.
    /// This is the value the watchdog ultimately compares against.
    pub max_runnable_for_ns: u64,
    /// Sum of `len()` across all DSQs (including LOCAL).
    pub total_dsq_depth: u64,
    /// Total number of events sitting in the engine event queue.
    /// Stream C follow-up: needed to distinguish "queue is empty so the
    /// engine cannot make progress" from "queue has events but they fire
    /// after the next stop condition (e.g. watchdog)".
    pub event_queue_size: u32,
    /// Time of the next event in the queue (or None if empty). Lets the
    /// analysis script see when the next opportunity to act will arise.
    pub next_event_t_ns: Option<TimeNs>,
    /// **Steady-state investigation:** sum across ALL tasks of
    /// `repeat_iteration * phases_per_loop + phase_idx`. Cumulative
    /// count of phase boundaries crossed by the workload. A
    /// monotonically-increasing value means the workload IS making
    /// forward progress; a frozen value across many seconds is the
    /// canonical livelock signature.
    pub total_phase_completions: u64,
    /// **Steady-state investigation:** sum across ALL tasks of CPU
    /// time consumed since simulation start (computed from
    /// `task_struct.se.sum_exec_runtime`). A frozen value confirms
    /// tasks aren't running on-CPU; a growing value confirms they
    /// are getting at least some CPU time even if no individual
    /// phase finishes.
    pub total_run_ns_sum: u64,
}

/// Per-cgroup bandwidth snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct CgroupSnapshot {
    pub cgid: u64,
    /// Optional human-readable name (looked up from the cgroup registry).
    pub name: Option<String>,
    pub quota_ns: u64,
    pub period_ns: u64,
    pub runtime_remaining_ns: i64,
    pub throttled: bool,
    pub period_start_ns: TimeNs,
    pub throttled_pid_count: u32,
    /// Fraction of quota consumed in the current period in [0.0, 1.0+].
    pub utilization: f64,
    /// Cumulative observability counters (Stream C follow-up).
    /// `refills_count` was the missing piece for the original Stream C
    /// finding — `period_start_ns` not advancing was a *symptom*; the
    /// *mechanism* is `refills_count == 0` because the refill event
    /// never reached pop time before the watchdog fired.
    pub charges_count: u64,
    pub throttles_count: u64,
    pub refills_count: u64,
    /// Earliest scheduled CgroupBwRefill event time for this cgid in the
    /// engine's event queue, if any. `None` means no refill is queued
    /// (initial-load failure, or the cgroup was unconfigured).
    /// This is the smoking-gun observable for the follow-up question:
    /// "is the refill event scheduled but unreached?" vs "was it never
    /// scheduled at all?".
    pub next_refill_t_ns: Option<TimeNs>,
}

/// Per-CPU runtime snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct CpuSnapshot {
    pub cpu: u32,
    pub online: bool,
    pub idle: bool,
    pub current_pid: Option<i32>,
    pub prev_pid: Option<i32>,
    pub local_dsq_len: u32,
    pub local_clock_ns: TimeNs,
    pub perf_lvl: u32,
    pub irq_context: &'static str,
    /// How long the current task has been running on this CPU (ns).
    /// `None` if idle.
    pub current_run_for_ns: Option<u64>,
}

/// Per-DSQ depth snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct DsqSnapshot {
    pub dsq_id: u64,
    pub len: u32,
    /// First `DSQ_HEAD_PIDS` PIDs in priority/FIFO order.
    pub head_pids: Vec<i32>,
}

/// Per-runnable-task snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct TaskSnapshot {
    pub pid: i32,
    pub name: String,
    pub cgid: Option<u64>,
    /// "Runnable" or "Running"; sleeping/exited tasks are excluded.
    pub state: &'static str,
    /// 0 if `Running` (the task is on-CPU); otherwise simulated-clock
    /// minus the task's `runnable_at_ns`.
    pub runnable_for_ns: u64,
    /// Mirror of the kernel's `SCX_OPSS_*`: `"queued"` or `"none"`.
    pub ops_state: &'static str,
    pub last_cpu: Option<u32>,
    pub prev_cpu: u32,
    /// Progress observability (steady-state investigation):
    /// the task's current phase index in its scripted behavior.
    /// Combined with `repeat_iteration` and `phases_per_loop`, this
    /// gives "completed phases so far" for forward-progress detection.
    pub phase_idx: u32,
    /// Number of full phase-loop completions so far. Sum with
    /// `phase_idx` to compute total phases completed by this task.
    pub repeat_iteration: u32,
    /// Nanoseconds remaining in the current Run phase (0 for non-Run
    /// phases). Decreases as the task accumulates CPU time. A task
    /// making forward progress shows this counter ticking down.
    pub run_remaining_ns: u64,
}

/// Streaming writer that emits one [`StateSnapshot`] per line of output.
///
/// Owns the destination file handle. The engine event loop calls
/// [`Self::maybe_emit`] between events; the writer decides whether
/// enough simulated time has elapsed to take a sample.
pub struct SnapshotWriter {
    out: BufWriter<File>,
    interval_ns: u64,
    /// Next absolute simulated time at which a snapshot is due.
    next_ns: TimeNs,
    /// Monotonic snapshot index (assigned when emitting).
    idx: u64,
}

impl SnapshotWriter {
    /// Create a writer that will sample at fixed `interval_us` intervals,
    /// writing jsonl to `path`.
    pub fn create(path: &Path, interval_us: u64) -> io::Result<Self> {
        let file = File::create(path)?;
        Ok(SnapshotWriter {
            out: BufWriter::new(file),
            interval_ns: interval_us.saturating_mul(1_000),
            next_ns: 0,
            idx: 0,
        })
    }

    /// Sample interval in nanoseconds.
    pub fn interval_ns(&self) -> u64 {
        self.interval_ns
    }

    /// Number of snapshots emitted so far.
    pub fn count(&self) -> u64 {
        self.idx
    }

    /// Emit a snapshot if `now_ns` has crossed the next sample boundary.
    ///
    /// `now_ns` should be the engine's `clock` field (the time of the
    /// next event about to be processed). When the engine advances
    /// across many idle nanoseconds in a single jump, only one snapshot
    /// is emitted at the actual clock value; we do not back-fill missed
    /// boundaries because the state did not change in the gap.
    pub fn maybe_emit<F>(&mut self, now_ns: TimeNs, build: F) -> io::Result<()>
    where
        F: FnOnce(u64) -> StateSnapshot,
    {
        if self.interval_ns == 0 || now_ns < self.next_ns {
            return Ok(());
        }
        let snap = build(self.idx);
        // Use `to_writer` for streaming serialization (no intermediate
        // String allocation).
        serde_json::to_writer(&mut self.out, &snap)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        self.out.write_all(b"\n")?;
        self.idx += 1;
        // Advance to the next boundary at-or-after now_ns. This skips
        // missed boundaries when the engine jumps forward in time.
        let advance = ((now_ns - self.next_ns) / self.interval_ns) + 1;
        self.next_ns += advance * self.interval_ns;
        Ok(())
    }

    /// Flush the underlying buffered writer. Called at end of run.
    pub fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

/// Inputs needed to construct a [`StateSnapshot`].
///
/// Kept as a single struct of references so that the engine call site
/// stays compact and so that we can grow the inputs without rippling
/// changes through the event-loop signature.
pub struct SnapshotInputs<'a, T> {
    pub now_ns: TimeNs,
    pub cpus: &'a [SimCpu],
    pub dsqs: &'a DsqManager,
    pub bw_manager: &'a BandwidthManager,
    pub cgroup_registry: &'a CgroupRegistry,
    pub task_to_cgid: &'a HashMap<Pid, CgroupId>,
    /// All tasks. The engine carries these in a separate struct
    /// (`SimState.tasks`); we accept them via this trait so the
    /// snapshot module does not depend on the unsafe-impl `SimTask`.
    pub tasks: &'a T,
    /// Next scheduled refill time per cgid (queried from the engine
    /// event queue). `None` for any cgid without a queued refill.
    /// Stream C follow-up: this is the smoking-gun observable for the
    /// "is the refill scheduled but unreached?" hypothesis.
    pub next_refill_per_cgid: &'a HashMap<CgroupId, TimeNs>,
    /// Total events sitting in the engine event queue.
    pub event_queue_size: u32,
    /// Time of the next event in the queue, if any.
    pub next_event_t_ns: Option<TimeNs>,
}

/// Trait abstracting over the engine's task table for snapshot purposes.
///
/// The unsafe `SimTask` lives outside the safety boundary; this trait
/// gives `take_snapshot` the read-only fields it needs without pulling
/// the unsafe type into `safe/`.
pub trait TaskTable {
    /// Iterate `(pid, name, state, runnable_at_ns, prev_cpu, started_at_ns)`.
    ///
    /// `state` is `"Sleeping"`, `"Runnable"`, `"Running"`, or `"Exited"`.
    /// `runnable_at_ns` is `None` for non-runnable tasks.
    /// `started_at_ns` is `Some(t)` only for `Running` tasks (when the
    /// CPU last set `task_started_at`); used to compute current run-for.
    fn for_each_task(&self, f: &mut dyn FnMut(TaskRow<'_>));

    /// Fetch ops state for a PID — `"queued"` or `"none"`.
    fn ops_state(&self, pid: Pid) -> &'static str;

    /// Last CPU on which the task ran (`task_last_cpu`).
    fn last_cpu(&self, pid: Pid) -> Option<CpuId>;
}

/// Read-only view of one task row, passed to [`TaskTable::for_each_task`].
pub struct TaskRow<'a> {
    pub pid: Pid,
    pub name: &'a str,
    pub state: &'static str,
    pub runnable_at_ns: Option<TimeNs>,
    pub prev_cpu: CpuId,
    /// **Steady-state investigation:** the task's current phase index
    /// in its scripted behavior (0-based).
    pub phase_idx: u32,
    /// **Steady-state investigation:** how many times the task has
    /// completed its full phase loop. Combined with `phase_idx` and
    /// `phases_per_loop`, this is the per-task forward-progress
    /// counter.
    pub repeat_iteration: u32,
    /// **Steady-state investigation:** number of distinct phases per
    /// repeat iteration (i.e. `task.behavior.phases.len()`). Used by
    /// the snapshot to compute "total phases completed by this task".
    pub phases_per_loop: u32,
    /// **Steady-state investigation:** ns of CPU time still owed in
    /// the current Run phase (0 for non-Run phases). Decreases as
    /// the task accumulates CPU time.
    pub run_remaining_ns: u64,
    /// **Steady-state investigation:** total CPU time consumed by
    /// this task since simulation start (mirrors the kernel's
    /// `task_struct.se.sum_exec_runtime`). Aggregated across all
    /// tasks into `SnapshotSummary.total_run_ns_sum`.
    pub sum_exec_runtime_ns: u64,
}

/// Build a [`StateSnapshot`] from raw engine state.
pub fn take_snapshot<T: TaskTable>(inputs: &SnapshotInputs<'_, T>) -> StateSnapshot {
    let now_ns = inputs.now_ns;

    // ---- cgroups (sorted by cgid for determinism) ----
    let mut cgroup_pairs: Vec<_> = inputs.bw_manager.iter().collect();
    cgroup_pairs.sort_by_key(|(cgid, _)| cgid.0);
    let cgroups: Vec<CgroupSnapshot> = cgroup_pairs
        .iter()
        .map(|(cgid, st)| CgroupSnapshot {
            cgid: cgid.0,
            name: inputs
                .cgroup_registry
                .get(**cgid)
                .map(|info| info.name.clone()),
            quota_ns: st.quota_ns,
            period_ns: st.period_ns,
            runtime_remaining_ns: st.runtime_remaining_ns,
            throttled: st.throttled,
            period_start_ns: st.period_start_ns,
            throttled_pid_count: st.throttled_pids.len() as u32,
            utilization: st.utilization(),
            charges_count: st.charges_count,
            throttles_count: st.throttles_count,
            refills_count: st.refills_count,
            next_refill_t_ns: inputs.next_refill_per_cgid.get(*cgid).copied(),
        })
        .collect();
    let nr_throttled_cgroups = cgroups.iter().filter(|c| c.throttled).count() as u32;

    // ---- cpus (already in CpuId order in the Vec) ----
    let cpus: Vec<CpuSnapshot> = inputs
        .cpus
        .iter()
        .map(|cpu| CpuSnapshot {
            cpu: cpu.id.0,
            online: cpu.is_online,
            idle: cpu.is_idle(),
            current_pid: cpu.current_task.map(|p| p.0),
            prev_pid: cpu.prev_task.map(|p| p.0),
            local_dsq_len: cpu.local_dsq.len() as u32,
            local_clock_ns: cpu.local_clock,
            perf_lvl: cpu.perf_lvl,
            irq_context: irq_str(cpu.irq_context),
            current_run_for_ns: cpu
                .task_started_at
                .map(|t| now_ns.saturating_sub(t)),
        })
        .collect();

    // ---- dsqs (sorted by id) ----
    let dsq_ids = inputs.dsqs.sorted_dsq_ids();
    let mut total_dsq_depth: u64 = 0;
    let mut dsqs: Vec<DsqSnapshot> = Vec::with_capacity(dsq_ids.len());
    for dsq_id in dsq_ids {
        let len = inputs.dsqs.nr_queued(dsq_id) as u32;
        total_dsq_depth += len as u64;
        let mut head = inputs.dsqs.ordered_pids(dsq_id);
        head.truncate(DSQ_HEAD_PIDS);
        dsqs.push(DsqSnapshot {
            dsq_id: dsq_id.0,
            len,
            head_pids: head.into_iter().map(|p| p.0).collect(),
        });
    }
    // Local DSQ depth lives on each CPU and is not in DsqManager — fold it
    // into total_dsq_depth so the "where are tasks queued?" rollup is
    // complete.
    for cpu in inputs.cpus {
        total_dsq_depth += cpu.local_dsq.len() as u64;
    }
    let _ = (DsqId::GLOBAL, CpuId(0)); // Silence unused-import lint when types module shape changes.

    // ---- runnable tasks (top-N by runnable_for_ns) ----
    let mut runnable: Vec<(u64, TaskSnapshot)> = Vec::new();
    let mut nr_runnable: u32 = 0;
    let mut nr_running: u32 = 0;
    let mut nr_sleeping: u32 = 0;
    // Steady-state investigation roll-ups (computed across ALL tasks,
    // not just the top-N runnable, so they survive the truncation).
    let mut total_phase_completions: u64 = 0;
    let mut total_run_ns_sum: u64 = 0;
    inputs.tasks.for_each_task(&mut |row| {
        match row.state {
            "Runnable" => nr_runnable += 1,
            "Running" => nr_running += 1,
            "Sleeping" => nr_sleeping += 1,
            _ => {}
        }
        // Forward-progress accounting (all states, including Sleeping
        // and Exited): a task that completed phases and exited still
        // counts.
        total_phase_completions +=
            row.repeat_iteration as u64 * row.phases_per_loop as u64 + row.phase_idx as u64;
        total_run_ns_sum += row.sum_exec_runtime_ns;
        if row.state != "Runnable" && row.state != "Running" {
            return;
        }
        let runnable_for_ns = row
            .runnable_at_ns
            .map(|t| now_ns.saturating_sub(t))
            .unwrap_or(0);
        let snap = TaskSnapshot {
            pid: row.pid.0,
            name: row.name.to_string(),
            cgid: inputs.task_to_cgid.get(&row.pid).map(|c| c.0),
            state: row.state,
            runnable_for_ns,
            ops_state: inputs.tasks.ops_state(row.pid),
            last_cpu: inputs.tasks.last_cpu(row.pid).map(|c| c.0),
            prev_cpu: row.prev_cpu.0,
            phase_idx: row.phase_idx,
            repeat_iteration: row.repeat_iteration,
            run_remaining_ns: row.run_remaining_ns,
        };
        runnable.push((runnable_for_ns, snap));
    });
    // Sort by runnable_for_ns desc, then pid asc for determinism.
    runnable.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.pid.cmp(&b.1.pid)));
    let max_runnable_for_ns = runnable.first().map(|(t, _)| *t).unwrap_or(0);
    let tasks_runnable: Vec<TaskSnapshot> = runnable
        .into_iter()
        .take(TASKS_RUNNABLE_CAP)
        .map(|(_, s)| s)
        .collect();

    let nr_idle_cpus = inputs.cpus.iter().filter(|c| c.is_idle()).count() as u32;

    StateSnapshot {
        t_ns: now_ns,
        idx: 0, // assigned by SnapshotWriter::maybe_emit
        summary: SnapshotSummary {
            nr_runnable,
            nr_running,
            nr_sleeping,
            nr_idle_cpus,
            nr_throttled_cgroups,
            max_runnable_for_ns,
            total_dsq_depth,
            event_queue_size: inputs.event_queue_size,
            next_event_t_ns: inputs.next_event_t_ns,
            total_phase_completions,
            total_run_ns_sum,
        },
        cgroups,
        cpus,
        dsqs,
        tasks_runnable,
    }
}

fn irq_str(c: IrqContext) -> &'static str {
    match c {
        IrqContext::None => "none",
        IrqContext::HardIrq => "hardirq",
        IrqContext::ServingSoftIrq => "softirq",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cgroup::CgroupRegistry;
    use crate::cgroup_bw::BandwidthManager;
    use crate::cpu::SimCpu;
    use crate::dsq::DsqManager;
    use std::collections::HashMap;
    use tempfile::NamedTempFile;

    /// Toy task table for unit tests (no FFI involvement).
    struct ToyTasks(Vec<(Pid, String, &'static str, Option<TimeNs>, CpuId)>);

    impl TaskTable for ToyTasks {
        fn for_each_task(&self, f: &mut dyn FnMut(TaskRow<'_>)) {
            for (pid, name, state, runnable_at_ns, prev_cpu) in &self.0 {
                f(TaskRow {
                    pid: *pid,
                    name: name.as_str(),
                    state,
                    runnable_at_ns: *runnable_at_ns,
                    prev_cpu: *prev_cpu,
                    phase_idx: 0,
                    repeat_iteration: 0,
                    phases_per_loop: 1,
                    run_remaining_ns: 0,
                    sum_exec_runtime_ns: 0,
                });
            }
        }
        fn ops_state(&self, _pid: Pid) -> &'static str {
            "queued"
        }
        fn last_cpu(&self, _pid: Pid) -> Option<CpuId> {
            None
        }
    }

    fn empty_inputs() -> (
        Vec<SimCpu>,
        DsqManager,
        BandwidthManager,
        CgroupRegistry,
        HashMap<Pid, CgroupId>,
        HashMap<CgroupId, TimeNs>,
    ) {
        let cpus = vec![SimCpu::new(CpuId(0)), SimCpu::new(CpuId(1))];
        let dsqs = DsqManager::new();
        let bw_manager = BandwidthManager::new();
        let cgroup_registry = CgroupRegistry::new(2, 100);
        let task_to_cgid = HashMap::new();
        let next_refill_per_cgid: HashMap<CgroupId, TimeNs> = HashMap::new();
        (cpus, dsqs, bw_manager, cgroup_registry, task_to_cgid, next_refill_per_cgid)
    }

    #[test]
    fn empty_state_snapshot() {
        let (cpus, dsqs, bw, reg, t2c, refills) = empty_inputs();
        let tasks = ToyTasks(Vec::new());
        let inputs = SnapshotInputs {
            now_ns: 1_000,
            cpus: &cpus,
            dsqs: &dsqs,
            bw_manager: &bw,
            cgroup_registry: &reg,
            task_to_cgid: &t2c,
            tasks: &tasks,
            next_refill_per_cgid: &refills,
            event_queue_size: 0,
            next_event_t_ns: None,
        };
        let snap = take_snapshot(&inputs);
        assert_eq!(snap.t_ns, 1_000);
        assert_eq!(snap.cpus.len(), 2);
        assert!(snap.cgroups.is_empty());
        assert_eq!(snap.summary.nr_runnable, 0);
        assert_eq!(snap.summary.max_runnable_for_ns, 0);
    }

    #[test]
    fn runnable_tasks_sorted_desc() {
        let (cpus, dsqs, bw, reg, t2c, refills) = empty_inputs();
        let tasks = ToyTasks(vec![
            (Pid(10), "a".into(), "Runnable", Some(900), CpuId(0)),
            (Pid(11), "b".into(), "Runnable", Some(500), CpuId(0)),
            (Pid(12), "c".into(), "Sleeping", None, CpuId(0)),
        ]);
        let inputs = SnapshotInputs {
            now_ns: 1_000,
            cpus: &cpus,
            dsqs: &dsqs,
            bw_manager: &bw,
            cgroup_registry: &reg,
            task_to_cgid: &t2c,
            tasks: &tasks,
            next_refill_per_cgid: &refills,
            event_queue_size: 0,
            next_event_t_ns: None,
        };
        let snap = take_snapshot(&inputs);
        assert_eq!(snap.summary.nr_runnable, 2);
        assert_eq!(snap.summary.nr_sleeping, 1);
        assert_eq!(snap.tasks_runnable.len(), 2);
        // pid 11 has been runnable longer (1000-500=500) than pid 10 (1000-900=100)
        assert_eq!(snap.tasks_runnable[0].pid, 11);
        assert_eq!(snap.tasks_runnable[0].runnable_for_ns, 500);
        assert_eq!(snap.tasks_runnable[1].pid, 10);
        assert_eq!(snap.summary.max_runnable_for_ns, 500);
    }

    #[test]
    fn cgroup_throttle_state_captured() {
        let (cpus, dsqs, mut bw, reg, t2c, refills) = empty_inputs();
        // 50ms quota / 100ms period at t=0
        bw.configure(CgroupId(7), 100_000, 50_000, 0);
        // Charge 60ms — newly exhausted; mark throttled (matching engine logic)
        let exhausted = bw.charge(CgroupId(7), 60_000_000, |_| None);
        assert_eq!(exhausted, Some(CgroupId(7)));
        if let Some(s) = bw.get_mut(CgroupId(7)) {
            s.throttle(Pid(99));
        }
        let tasks = ToyTasks(Vec::new());
        let inputs = SnapshotInputs {
            now_ns: 60_000_000,
            cpus: &cpus,
            dsqs: &dsqs,
            bw_manager: &bw,
            cgroup_registry: &reg,
            task_to_cgid: &t2c,
            tasks: &tasks,
            next_refill_per_cgid: &refills,
            event_queue_size: 0,
            next_event_t_ns: None,
        };
        let snap = take_snapshot(&inputs);
        assert_eq!(snap.cgroups.len(), 1);
        assert_eq!(snap.cgroups[0].cgid, 7);
        assert!(snap.cgroups[0].throttled);
        assert_eq!(snap.cgroups[0].throttled_pid_count, 1);
        assert_eq!(snap.summary.nr_throttled_cgroups, 1);
        assert!(snap.cgroups[0].utilization > 1.0);
    }

    #[test]
    fn writer_emits_at_interval_boundaries() {
        let tmp = NamedTempFile::new().unwrap();
        let mut w = SnapshotWriter::create(tmp.path(), 100).unwrap(); // 100us = 100_000ns
        let (cpus, dsqs, bw, reg, t2c, refills) = empty_inputs();
        let tasks = ToyTasks(Vec::new());
        let make = |now_ns: TimeNs| {
            let inputs = SnapshotInputs {
                now_ns,
                cpus: &cpus,
                dsqs: &dsqs,
                bw_manager: &bw,
                cgroup_registry: &reg,
                task_to_cgid: &t2c,
                tasks: &tasks,
            next_refill_per_cgid: &refills,
            event_queue_size: 0,
            next_event_t_ns: None,
            };
            take_snapshot(&inputs)
        };

        // The first sample is intentionally taken at t=0 / first call so that
        // the analysis script always has a baseline snapshot. With next_ns=0
        // and any non-negative now_ns, the very first call emits.
        // Time 50us → emit idx 0 at t=50_000; next boundary becomes 100us.
        w.maybe_emit(50_000, |idx| {
            let mut s = make(50_000);
            s.idx = idx;
            s
        })
        .unwrap();
        assert_eq!(w.count(), 1);
        // Time 99us: before next boundary → no emit
        w.maybe_emit(99_999, |idx| {
            let mut s = make(99_999);
            s.idx = idx;
            s
        })
        .unwrap();
        assert_eq!(w.count(), 1);
        // Time 100us: at boundary → emit idx 1
        w.maybe_emit(100_000, |idx| {
            let mut s = make(100_000);
            s.idx = idx;
            s
        })
        .unwrap();
        assert_eq!(w.count(), 2);
        // Time 350us: skip across multiple boundaries, single emit idx 2.
        // Next due boundary after that should be 400us (350us / 100us = 3,
        // +1 = 4 → 400us).
        w.maybe_emit(350_000, |idx| {
            let mut s = make(350_000);
            s.idx = idx;
            s
        })
        .unwrap();
        assert_eq!(w.count(), 3);
        w.maybe_emit(399_999, |idx| {
            let mut s = make(399_999);
            s.idx = idx;
            s
        })
        .unwrap();
        assert_eq!(w.count(), 3, "should not emit before next boundary");
        w.maybe_emit(400_000, |idx| {
            let mut s = make(400_000);
            s.idx = idx;
            s
        })
        .unwrap();
        assert_eq!(w.count(), 4);
        w.flush().unwrap();

        // Verify file contains 4 jsonl lines parseable as JSON.
        let body = std::fs::read_to_string(tmp.path()).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 4);
        for l in &lines {
            let v: serde_json::Value = serde_json::from_str(l).expect("valid jsonl");
            assert!(v.get("t_ns").is_some());
            assert!(v.get("summary").is_some());
        }
    }

    #[test]
    fn writer_disabled_when_interval_zero() {
        let tmp = NamedTempFile::new().unwrap();
        let mut w = SnapshotWriter::create(tmp.path(), 0).unwrap();
        let (cpus, dsqs, bw, reg, t2c, refills) = empty_inputs();
        let tasks = ToyTasks(Vec::new());
        w.maybe_emit(1_000_000, |idx| {
            let inputs = SnapshotInputs {
                now_ns: 1_000_000,
                cpus: &cpus,
                dsqs: &dsqs,
                bw_manager: &bw,
                cgroup_registry: &reg,
                task_to_cgid: &t2c,
                tasks: &tasks,
            next_refill_per_cgid: &refills,
            event_queue_size: 0,
            next_event_t_ns: None,
            };
            let mut s = take_snapshot(&inputs);
            s.idx = idx;
            s
        })
        .unwrap();
        assert_eq!(w.count(), 0);
    }
}
