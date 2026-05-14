//! Trace event recording for the simulator.
//!
//! Every scheduling action (task scheduled, preempted, slept, woke, CPU idle)
//! is recorded as a `TraceEvent` with a simulated timestamp and CPU ID.

use crate::dsq::DsqManager;
use crate::engine::ExitKind;
use crate::fmt::FmtTs;
use crate::scenario::IrqType;
use crate::task::TaskDef;
use crate::types::{CpuId, DsqId, Pid, TimeNs, Vtime};

/// What triggered a DSQ length sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DsqSampleTrigger {
    /// Task was inserted into the DSQ.
    Insert,
    /// Task was moved from DSQ to local.
    Consume,
    /// Periodic sampling (scheduler tick).
    Tick,
}

/// A point-in-time sample of a DSQ's length.
#[derive(Debug, Clone)]
pub struct DsqLengthSample {
    /// Simulated time when sample was taken.
    pub time_ns: TimeNs,
    /// The DSQ that was sampled.
    pub dsq_id: DsqId,
    /// Number of tasks queued at sample time.
    pub length: usize,
    /// What triggered this sample.
    pub trigger: DsqSampleTrigger,
}

/// Summary statistics from a trace, useful for realism comparison.
///
/// This struct captures high-level metrics that can be compared between
/// simulated and real kernel traces to identify realism gaps.
#[derive(Debug, Clone, Default)]
pub struct TraceSummary {
    /// Total number of trace events recorded.
    pub total_events: usize,
    /// Total number of scheduler tick events across all CPUs.
    pub total_ticks: usize,
    /// Total number of task yield events (voluntary phase boundary).
    pub total_yields: usize,
    /// Total number of task preemption events (slice expiration).
    pub total_preempts: usize,
    /// Total number of task sleep events.
    pub total_sleeps: usize,
    /// Total number of task wake events.
    pub total_wakes: usize,
    /// Total number of CPU idle periods.
    pub total_idle_periods: usize,
    /// Total idle duration across all CPUs (nanoseconds).
    pub total_idle_duration_ns: u64,
    /// Count of dispatches to global DSQs.
    pub global_dsq_dispatches: usize,
    /// Count of dispatches to local (per-CPU) DSQs.
    pub local_dsq_dispatches: usize,
}

impl std::fmt::Display for TraceSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Trace Summary:")?;
        writeln!(f, "  total_events:          {}", self.total_events)?;
        writeln!(f, "  total_ticks:           {}", self.total_ticks)?;
        writeln!(f, "  total_yields:          {}", self.total_yields)?;
        writeln!(f, "  total_preempts:        {}", self.total_preempts)?;
        writeln!(f, "  total_sleeps:          {}", self.total_sleeps)?;
        writeln!(f, "  total_wakes:           {}", self.total_wakes)?;
        writeln!(f, "  total_idle_periods:    {}", self.total_idle_periods)?;
        writeln!(
            f,
            "  total_idle_duration:   {:.3}ms",
            self.total_idle_duration_ns as f64 / 1_000_000.0
        )?;
        writeln!(f, "  global_dsq_dispatches: {}", self.global_dsq_dispatches)?;
        writeln!(f, "  local_dsq_dispatches:  {}", self.local_dsq_dispatches)
    }
}

/// A single trace event produced by the simulator.
#[derive(Debug, Clone)]
pub struct TraceEvent {
    /// Simulated time in nanoseconds when this event occurred.
    pub time_ns: TimeNs,
    /// The CPU on which this event occurred.
    pub cpu: CpuId,
    /// The kind of event.
    pub kind: TraceKind,
}

/// The type of scheduling event recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceKind {
    /// A task was scheduled to run on this CPU.
    TaskScheduled { pid: Pid },
    /// A task was preempted (slice expired) on this CPU.
    TaskPreempted { pid: Pid },
    /// A task yielded (phase complete but still runnable) on this CPU.
    TaskYielded { pid: Pid },
    /// A task voluntarily slept on this CPU.
    TaskSlept { pid: Pid },
    /// A task woke up.
    TaskWoke { pid: Pid },
    /// A task completed all its phases.
    TaskCompleted { pid: Pid },
    /// The CPU became idle (no tasks to run).
    CpuIdle,
    /// Simulation ended while this task was still running on a CPU.
    /// Emitted for each CPU with a current task when the event loop exits.
    SimulationEnd { pid: Pid },

    // ----- Ops-level events (kernel context-switch path) -----
    /// A task was stopped on this CPU (put_prev_task → stopping()).
    PutPrevTask { pid: Pid, still_runnable: bool },
    /// A CPU was selected for a waking task (select_task_rq → select_cpu()).
    SelectTaskRq {
        pid: Pid,
        prev_cpu: CpuId,
        selected_cpu: CpuId,
    },
    /// A task was enqueued into the scheduler (enqueue_task → enqueue()).
    EnqueueTask { pid: Pid, enq_flags: u64 },
    /// The scheduler's dispatch() callback was invoked to fill the local DSQ.
    Balance { prev_pid: Option<Pid> },
    /// A task was popped from the local DSQ (pick_task).
    PickTask { pid: Pid },
    /// A task was handed to the CPU to run (set_next_task → running()).
    SetNextTask { pid: Pid },

    // ----- Kfunc-level events (BPF helper calls) -----
    /// scx_bpf_dsq_insert: FIFO insert into a DSQ.
    DsqInsert {
        pid: Pid,
        dsq_id: DsqId,
        slice: TimeNs,
    },
    /// scx_bpf_dsq_insert_vtime: vtime-ordered insert into a DSQ.
    DsqInsertVtime {
        pid: Pid,
        dsq_id: DsqId,
        slice: TimeNs,
        vtime: Vtime,
    },
    /// scx_bpf_dsq_move_to_local: move head of DSQ to the current CPU's local DSQ.
    DsqMoveToLocal { dsq_id: DsqId, success: bool },
    /// scx_bpf_kick_cpu: send scheduling IPI to a CPU.
    KickCpu { target_cpu: CpuId },
    /// A periodic scheduler tick fired on this CPU.
    Tick { pid: Pid },
    /// Dispatch to local DSQ rejected (cpumask violation).
    ///
    /// Emitted when a scheduler dispatches to `SCX_DSQ_LOCAL_ON | cpu` but
    /// the task cannot run on that CPU (cpumask or migration-disabled).
    DispatchRejected {
        pid: Pid,
        target_cpu: CpuId,
        reason: DispatchRejectReason,
    },

    // ----- Task-state-transition structops (LAVD per-state cgroup hooks) -----
    //
    // tg `bundle-implement-cpu-bw-critical-tracekind-easy-wins` (TOP-4 cluster
    // from `experiments/bpftrace_gap_classification_20260513/REPORT.md`):
    // surface the runnable / dequeue / quiescent callbacks the engine
    // already invokes (engine.rs runnable@~3275, dequeue@~2910/3580,
    // quiescent@~3592) so the live-vs-sim diff harness can validate
    // task-lifecycle parity. No fake approximation — args are the exact
    // values handed to `self.scheduler.<op>(...)`.
    /// `ops.runnable` — task became runnable (called before enqueue on wake).
    Runnable { pid: Pid, enq_flags: u64 },
    /// `ops.dequeue` — task left the runnable state.
    Dequeue { pid: Pid, deq_flags: u64 },
    /// `ops.quiescent` — task voluntarily slept; complement of runnable.
    Quiescent { pid: Pid, deq_flags: u64 },

    // ----- CPU idle-tracking structops (LAVD update_idle hook) -----
    //
    // tg `bundle-implement-cpu-bw-critical-tracekind-easy-wins` (TOP-3).
    // Engine drives this at 5 sites (init / cpu-online / cpu-acquire /
    // idle-enter / idle-exit) — all are LAVD's hooks for cgroup-bw
    // replenish triggers; the live-vs-sim diff cannot detect setup-time
    // idle-state divergence without these.
    /// `ops.update_idle` — CPU entered (idle=true) or exited (idle=false) idle.
    UpdateIdle { cpu: CpuId, idle: bool },

    // ----- Cgroup-lifecycle structops (cpu-bw-stall-bug critical path) -----
    //
    // tg `bundle-implement-cpu-bw-critical-tracekind-easy-wins`
    // (TOP-1 / TOP-2 / TOP-6). LAVD's cgroup_init is where it links a
    // cgroup to the cgroup_bw library; cgroup_set_bandwidth is the
    // configuration call that sets the bug-triggering cpu.max quota;
    // cgroup_move changes a task's throttle state mid-flight. Without
    // these, the live-vs-sim diff harness cannot prove live and sim
    // agreed on the bug-triggering cpu.max configuration nor on the
    // cgroup hierarchy populated under LAVD.
    /// `ops.cgroup_init` — scheduler should track a new cgroup. Emits
    /// JSONL entry+exit (rc carried in exit `ret`).
    CgroupInit {
        cgid: crate::cgroup::CgroupId,
        rc: i32,
    },
    /// `ops.cgroup_exit` — cgroup destroyed; scheduler should drop tracking.
    CgroupExit { cgid: crate::cgroup::CgroupId },
    /// `ops.cgroup_set_bandwidth` — cpu.max was written. **Critical for
    /// cpu-bw-stall-bug** since this is the configuration call that sets
    /// period/quota/burst on the cgroup. Without emitting it the diff
    /// cannot prove live and sim agree on the bug-triggering quota.
    CgroupSetBandwidth {
        cgid: crate::cgroup::CgroupId,
        period_us: u64,
        quota_us: u64,
        burst_us: u64,
    },
    /// `ops.cgroup_move` — task migrated between cgroups. Throttle state
    /// changes mid-flight; required to validate fixture migrations.
    CgroupMove {
        pid: Pid,
        from_cgid: crate::cgroup::CgroupId,
        to_cgid: crate::cgroup::CgroupId,
    },

    // ----- Task-lifecycle structops (TOP-5: fixture-load determinism) -----
    //
    // tg `bundle-implement-secondary-tracekind-easy-wins` (TOP-5 cluster
    // from `experiments/bpftrace_gap_classification_20260513/REPORT.md`
    // rows #4 / #5 / #6): surface the init_task / exit_task / enable
    // structops so the live-vs-sim diff harness can validate that sim
    // and live agree on the per-task scheduler handshake at fixture
    // load and shutdown. Engine already invokes all three at
    // engine.rs init_task@~1748, exit_task@~2060, enable@~4461; this
    // is mechanical wiring with no model surgery.
    /// `ops.init_task` — scheduler should track a new task. Carries the
    /// callback rc so the diff harness can spot init failures.
    InitTask { pid: Pid, rc: i32 },
    /// `ops.exit_task` — task is exiting scheduler control.
    ExitTask { pid: Pid },
    /// `ops.enable` — task is being enabled (made schedulable). One-shot
    /// per task on first run, required for full handshake parity.
    Enable { pid: Pid },

    // ----- Task affinity structop (TOP-7: affinity parity) -----
    //
    // tg `bundle-implement-secondary-tracekind-easy-wins` (TOP-7 row #8):
    // surface the set_cpumask call (engine.rs:~1761) so migration-disabled
    // / cpumask-violation bug classes can be diffed live-vs-sim. The
    // cpumask is rendered as a stable hex string (LSB = cpu 0) computed
    // from `bpf_cpumask_test_cpu` over the engine's nr_cpus.
    /// `ops.set_cpumask` — scheduler notified of a task's cpumask change.
    SetCpumask {
        pid: Pid,
        /// Hex bitstring with LSB = cpu 0; widths beyond u64 emit
        /// space-separated 16-hex-digit words from low to high.
        cpumask_hex: String,
    },

    // ----- BPF helpers (TOP-8: time-source + cgroup-helper visibility) -----
    //
    // tg `bundle-implement-secondary-tracekind-easy-wins` (TOP-8 rows
    // #30 / #42 / #40): surface the `scx_bpf_now` / `scx_bpf_task_cgroup`
    // / `scx_bpf_task_cpu` helpers. **High** cpu-bw-stall-bug relevance:
    // the bug's smoking gun is computed against `bpf_ktime_get_ns()`
    // snapshots; emitting `now` lets the diff harness validate the
    // time-source agrees with live. `task_cgroup` is called by LAVD on
    // every enqueue to look up cgroup state.
    /// `scx_bpf_now()` — read the simulated time-source. Carries the
    /// returned ns value so the diff harness can compare clocks.
    HelperNow { ret_ns: TimeNs },
    /// `scx_bpf_task_cgroup(p)` — look up the cgroup a task belongs to.
    /// Carries pid + the resolved cgid (or 0 for "root/unknown").
    HelperTaskCgroup {
        pid: Pid,
        cgid: crate::cgroup::CgroupId,
    },
    /// `scx_bpf_task_cpu(p)` — look up the CPU a task is assigned to
    /// (the kernel's `task_cpu(p)` semantics). Carries pid + the
    /// returned CPU id.
    HelperTaskCpu { pid: Pid, ret_cpu: CpuId },

    // ----- BPF DSQ-creation helpers (TOP-9: DSQ-creation visibility) -----
    //
    // tg `bundle-implement-secondary-tracekind-easy-wins` (TOP-9 rows
    // #31 / #32 / #33): surface `scx_bpf_create_dsq` / `_destroy_dsq`
    // / `_dsq_nr_queued` so the diff harness can confirm sim and live
    // agree on the scheduler-side per-cgroup DSQ topology — LAVD
    // creates per-cgroup DSQs at cgroup_init and missing-DSQ bugs hide
    // here.
    /// `scx_bpf_create_dsq(dsq_id, node)` — scheduler asked to create a
    /// DSQ. Carries the dsq_id, requested NUMA node, and the rc.
    CreateDsq { dsq_id: DsqId, node: i32, rc: i32 },
    /// `scx_bpf_destroy_dsq(dsq_id)` — scheduler asked to destroy a
    /// DSQ. Trace-only in scxsim today (the helper is a no-op stub),
    /// but recording the request still proves the scheduler asked.
    DestroyDsq { dsq_id: DsqId },
    /// `scx_bpf_dsq_nr_queued(dsq_id)` — scheduler probed a DSQ depth.
    /// Carries the queried dsq_id and the returned count.
    DsqNrQueued { dsq_id: DsqId, ret: i32 },

    // ----- IRQ events -----
    /// An interrupt starts on a CPU (hardirq or softirq).
    IrqStart { cpu: CpuId, irq_type: IrqType },
    /// An interrupt handler completes on a CPU.
    IrqEnd { cpu: CpuId },

    // ----- Cgroup bandwidth (cpu.max) events -----
    /// Trace marker: `delta_ns` of CPU time consumed by `pid` in `cgid`.
    /// Recorded by the engine on every task stop; the actual cpu.max
    /// accounting happens in the scheduler-side cgroup_bw library
    /// (`scx_cgroup_bw_consume`), not here.
    CgroupBwCharge {
        pid: Pid,
        cgid: crate::cgroup::CgroupId,
        delta_ns: TimeNs,
    },
    /// Engine refused to admit `pid` from a local DSQ because the
    /// scheduler-side cgroup_bw library reported its cgroup throttled
    /// (`scxsim_cgroup_bw_is_cgroup_throttled` returned true). The task
    /// remains queued and will be re-checked on the next dispatch.
    ///
    /// (Legacy lazy-throttle path — see `CgroupBwDequeueOnThrottle` /
    /// `CgroupBwReenqueueOnReplenish` for the eager-throttle replacement.)
    CgroupBwDenied {
        pid: Pid,
        cgid: crate::cgroup::CgroupId,
    },
    /// Eager throttle: the engine pulled `pid` from its local DSQ at the
    /// admission gate because its cgroup is throttled, called
    /// `ops.dequeue` + `ops.quiescent` to remove it from the BPF
    /// scheduler's queues, and stashed it in `bw_blocked[cgid]` to be
    /// re-runnabled on the next replenish.
    ///
    /// Mirrors what the kernel's bandwidth controller does when a task
    /// crosses a cgroup quota: dequeue the task entirely from
    /// sched_ext, not just refuse dispatch. tg
    /// `scxsim-eager-cgroup-bw-throttle-via-dequeue-wakeup-cycle`.
    CgroupBwDequeueOnThrottle {
        pid: Pid,
        cgid: crate::cgroup::CgroupId,
    },
    /// Eager throttle counterpart: when the cgroup_bw library marks a
    /// cgroup as no longer throttled (per-cgroup snapshot diff after
    /// `replenish_timerfn`), the engine drains `bw_blocked[cgid]` and
    /// schedules a `TaskWake` event for each `pid`. The task then
    /// re-enters via the normal wakeup path:
    /// `ops.runnable` → `ops.select_cpu` → `ops.enqueue` (which
    /// goes through LAVD's `can_direct_dispatch` and may take the
    /// simple-insert direct-dispatch fast path).
    CgroupBwReenqueueOnReplenish {
        pid: Pid,
        cgid: crate::cgroup::CgroupId,
    },
    /// The compiled-in `scx/lib/cgroup_bw.bpf.c` library performed a
    /// per-cgroup replenishment. Captures the smoking-gun fields the
    /// library computes inside `cbw_replenish_cgroup` (the bug's CAUSE
    /// at lib/cgroup_bw.bpf.c:1679).
    ///
    /// **Smoking-gun signature for the cpu-bw-stall-bug:**
    /// `keep_throttled == true && runtime_total_last == 0` for the same
    /// `cgid` across multiple consecutive replenishments. Means the
    /// cgroup's debt grew unbounded during a period in which the lib
    /// did no work to recover from the throttle -- the cgroup never
    /// escapes throttle.
    ///
    /// Fired only under LAVD with `enable_cpu_bw=true` (the only
    /// scheduler that compiles the cgroup_bw library in today). Skipped
    /// for unlimited-quota cgroups (`nquota_ub == CBW_RUNTUME_INF`),
    /// matching the lib's own `out_no_replenish` early-return path.
    ///
    /// tg `wprof-r2-add-cgroup-bw-replenish-tracekind-smoking-gun`
    /// (R2 HIGH from wprof-trace-baseline 2026-05-13).
    CgroupBwReplenish {
        cgid: crate::cgroup::CgroupId,
        /// Total runtime consumed during the just-completed period
        /// (lib field `cgx->runtime_total_last`, captured before the
        /// inner call). 0 means the period saw no work.
        runtime_total_last: i64,
        /// Effective quota for the period that just ended (lib field
        /// `cgx->period_budget` before the inner call). Used together
        /// with runtime_total_last to compute debt.
        period_budget_in: i64,
        /// `max(runtime_total_last - period_budget_in, 0)` -- the
        /// overspend the lib will subtract from the new period's
        /// budget. Mirrors lib/cgroup_bw.bpf.c:1679.
        debt: i64,
        /// `clamp(nquota - runtime_total_last, 0, burst_remaining)` --
        /// underspend carried forward as burst credit. Mirrors
        /// lib/cgroup_bw.bpf.c:1680.
        burst_credit: i64,
        /// New `period_budget = nquota_ub + burst_credit - debt` after
        /// the lib's WRITE_ONCE at lib/cgroup_bw.bpf.c:1697.
        period_budget_out: i64,
        /// `period_budget_out <= 0`: cgroup stays throttled into the
        /// next period because debt exceeded quota+burst. Lib field
        /// `cgx->is_throttled` is set to this value at line 1751.
        keep_throttled: bool,
    },

    // ----- BTQ park/unpark events (cpu-bw-stall-bug critical path) -----
    //
    // tg `add-cbw-put-aside-and-drain-btq-batch-tracekinds` (A1+A2 from
    // cgroup_bw audit). Net inflow / outflow of tasks to the per-cgroup
    // Backup-Task-Queue, observed via BEFORE/AFTER snapshot diff of
    // `CbwCgroupSnapshot::btq_total_len` around `fire_timer`.
    //
    // The lib's `cbw_put_aside` (lib/cgroup_bw.bpf.c:1532) parks tasks
    // into the BTQ when their cgroup is throttled at enqueue time, and
    // `cbw_drain_btq_batch` (lib/cgroup_bw.bpf.c:2088) unparks them
    // when the cgroup is replenished. Without these TraceKinds, the
    // diff harness cannot see WHY tasks remain parked across replenish
    // events — which is the heart of the cpu-bw-stall-bug.
    //
    // The events are emitted from the snapshot/diff helper in
    // `unsafe_impl::cgroup_bw_replenish` (alongside CgroupBwReplenish);
    // the same fire_timer hook in `engine.rs::handle_timer_fired`
    // captures both. Net deltas are coarsened: a sequence of N
    // put_asides followed by M drains between two snapshot points is
    // rendered as `count = N - M` (positive → CbwPutAside, negative
    // → CbwDrainBtqBatch). This is sufficient to detect the stall
    // signature ("BTQ length grows monotonically across replenish
    // events without ever being drained") which is the bug's
    // fingerprint.
    /// Net tasks added to the cgroup's BTQ between two snapshot
    /// points. Inferred from `btq_total_len_after - btq_total_len_before > 0`.
    /// Mirrors the lib's `cbw_put_aside` (lib/cgroup_bw.bpf.c:1532).
    CbwPutAside {
        cgid: crate::cgroup::CgroupId,
        /// Net number of tasks added to the BTQ during the window.
        count: u32,
        /// Aggregate BTQ length AFTER the snapshot window. The
        /// stall fingerprint is `btq_len_after` staying high or
        /// growing across consecutive `CbwPutAside` events on the
        /// same `cgid`.
        btq_len_after: u32,
    },
    /// Net tasks drained from the cgroup's BTQ between two snapshot
    /// points. Inferred from `btq_total_len_after - btq_total_len_before < 0`.
    /// Mirrors the lib's `cbw_drain_btq_batch` (lib/cgroup_bw.bpf.c:2088),
    /// which is called from `cbw_reenqueue_cgroup` on every per-period
    /// replenishment.
    CbwDrainBtqBatch {
        cgid: crate::cgroup::CgroupId,
        /// Net number of tasks drained from the BTQ during the window.
        count: u32,
        /// Aggregate BTQ length AFTER the drain. The recovery fingerprint
        /// is `btq_len_after` returning to 0 promptly.
        btq_len_after: u32,
    },

    /// Cgroup `is_throttled` state transition observed between two
    /// snapshot points. Mirrors the lib's `cbw_throttle_cgroups`
    /// (lib/cgroup_bw.bpf.c:1281), which propagates throttle state
    /// top-down across the cgroup hierarchy: for each cgroup with a
    /// throttled ancestor, the lib does
    /// `WRITE_ONCE(cur_cgx->is_throttled, true)`. The clear path
    /// (`is_throttled` flipped from 1→0) happens at the next
    /// replenish-period boundary.
    ///
    /// tg `add-cbw-throttle-cgroups-tracekind` (A3 from cgroup_bw
    /// audit, follow-up to closed A1+A2 PR #42).
    ///
    /// **Caveat: aliasing of throttle causes.** A 0→1 transition on
    /// `is_throttled` can come from either:
    ///
    /// * Step 1 — `cbw_update_runtime_total_sloppy` setting the flag
    ///   because the cgroup exhausted its OWN budget, OR
    /// * Step 2 — `cbw_throttle_cgroups` (this TraceKind's namesake)
    ///   propagating an ancestor's throttle state DOWN to descendants.
    ///
    /// The snapshot/diff helper cannot distinguish them without the
    /// full hierarchy snapshot. The TraceKind reports the OBSERVABLE
    /// transition; readers can disambiguate by joining with concurrent
    /// `CgroupBwReplenish` events on the same cgid (Step-1 transitions
    /// always coincide with a replenish-period budget-exhaustion event,
    /// Step-2 transitions appear on cgroups that did NOT individually
    /// exhaust budget). The cpu-bw-stall-bug fingerprint is `throttled`
    /// staying TRUE across multiple consecutive snapshots regardless of
    /// origin.
    CbwThrottleCgroups {
        cgid: crate::cgroup::CgroupId,
        /// New value of `cgx->is_throttled` (the AFTER snapshot's bit).
        /// `true` means the cgroup just became throttled (0→1 transition);
        /// `false` means it was just unthrottled (1→0 transition,
        /// typically at a replenish-period boundary).
        throttled: bool,
    },
}

/// Reason why a dispatch to a local DSQ was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchRejectReason {
    /// Target CPU is not in the task's cpumask.
    CpumaskViolation,
    /// Task is migration-disabled and cannot move to a different CPU.
    MigrationDisabled,
}

/// A complete simulation trace, containing all events in chronological order.
#[derive(Debug, Clone)]
pub struct Trace {
    events: Vec<TraceEvent>,
    pub(crate) nr_cpus: u32,
    task_names: Vec<(Pid, String)>,
    /// How the simulation terminated.
    exit_kind: ExitKind,
    /// DSQ length samples for analyzing queue behavior under load.
    dsq_samples: Vec<DsqLengthSample>,
    /// Warmup period: stats exclude events before this simulated time.
    warmup_ns: TimeNs,
}

impl Trace {
    #[allow(dead_code)]
    pub(crate) fn new(nr_cpus: u32, tasks: &[TaskDef]) -> Self {
        Self::with_warmup(nr_cpus, tasks, 0)
    }

    pub(crate) fn with_warmup(nr_cpus: u32, tasks: &[TaskDef], warmup_ns: TimeNs) -> Self {
        let task_names = tasks.iter().map(|t| (t.pid, t.name.clone())).collect();
        Self {
            events: Vec::new(),
            nr_cpus,
            task_names,
            exit_kind: ExitKind::Normal,
            dsq_samples: Vec::new(),
            warmup_ns,
        }
    }

    /// Resolve a PID to a task name, or `"???"` if unknown.
    pub(crate) fn task_name(&self, pid: Pid) -> &str {
        self.task_names
            .iter()
            .find(|(p, _)| *p == pid)
            .map(|(_, n)| n.as_str())
            .unwrap_or("???")
    }

    /// Set the exit kind for this trace.
    pub(crate) fn set_exit_kind(&mut self, kind: ExitKind) {
        self.exit_kind = kind;
    }

    /// Get the exit kind for this simulation.
    pub fn exit_kind(&self) -> &ExitKind {
        &self.exit_kind
    }

    /// Returns true if the simulation exited with an error.
    pub fn has_error(&self) -> bool {
        self.exit_kind.is_error()
    }

    pub(crate) fn record(&mut self, time_ns: TimeNs, cpu: CpuId, kind: TraceKind) {
        self.events.push(TraceEvent { time_ns, cpu, kind });
    }

    /// Get the warmup period in nanoseconds.
    pub fn warmup_ns(&self) -> TimeNs {
        self.warmup_ns
    }

    /// Get all events in chronological order.
    pub fn events(&self) -> &[TraceEvent] {
        &self.events
    }

    /// Calculate the total runtime (nanoseconds) for a given task PID.
    ///
    /// This sums up the intervals between `TaskScheduled` and the next
    /// `TaskPreempted`/`TaskSlept`/`TaskCompleted` for that PID. Open
    /// intervals (task still running at simulation end) are not counted;
    /// use longer durations or shorter phases to ensure tasks complete
    /// at least one cycle.
    pub fn total_runtime(&self, pid: Pid) -> TimeNs {
        let mut total: TimeNs = 0;
        let mut running_since: Option<TimeNs> = None;

        for event in &self.events {
            match &event.kind {
                TraceKind::TaskScheduled { pid: p } if *p == pid => {
                    running_since = Some(event.time_ns);
                }
                TraceKind::TaskPreempted { pid: p }
                | TraceKind::TaskYielded { pid: p }
                | TraceKind::TaskSlept { pid: p }
                | TraceKind::TaskCompleted { pid: p }
                    if *p == pid =>
                {
                    if let Some(start) = running_since.take() {
                        total += event.time_ns - start;
                    }
                }
                _ => {}
            }
        }

        total
    }

    /// Count the number of times a task was scheduled.
    pub fn schedule_count(&self, pid: Pid) -> usize {
        self.events
            .iter()
            .filter(|e| matches!(e.kind, TraceKind::TaskScheduled { pid: p } if p == pid))
            .count()
    }

    /// Count the number of times a CPU went idle.
    pub fn idle_count(&self, cpu: CpuId) -> usize {
        self.events
            .iter()
            .filter(|e| e.cpu == cpu && matches!(e.kind, TraceKind::CpuIdle))
            .count()
    }

    /// Count the number of `Balance` events on a CPU.
    pub fn balance_count(&self, cpu: CpuId) -> usize {
        self.events
            .iter()
            .filter(|e| e.cpu == cpu && matches!(e.kind, TraceKind::Balance { .. }))
            .count()
    }

    /// Count the number of `DsqInsert` or `DsqInsertVtime` events for a task.
    pub fn dsq_insert_count(&self, pid: Pid) -> usize {
        self.events
            .iter()
            .filter(|e| {
                matches!(
                    e.kind,
                    TraceKind::DsqInsert { pid: p, .. }
                    | TraceKind::DsqInsertVtime { pid: p, .. }
                    if p == pid
                )
            })
            .count()
    }

    /// Count the number of tick events on a CPU.
    pub fn tick_count(&self, cpu: CpuId) -> usize {
        self.events
            .iter()
            .filter(|e| e.cpu == cpu && matches!(e.kind, TraceKind::Tick { .. }))
            .count()
    }

    /// Count the number of yield events for a task.
    ///
    /// Yields occur when a task voluntarily gives up the CPU (phase boundary)
    /// but remains runnable. High yield counts for CPU-bound tasks may indicate
    /// a realism gap in the simulation model.
    pub fn yield_count(&self, pid: Pid) -> usize {
        self.events
            .iter()
            .filter(|e| matches!(e.kind, TraceKind::TaskYielded { pid: p } if p == pid))
            .count()
    }

    /// Count the number of preemption events for a task.
    pub fn preempt_count(&self, pid: Pid) -> usize {
        self.events
            .iter()
            .filter(|e| matches!(e.kind, TraceKind::TaskPreempted { pid: p } if p == pid))
            .count()
    }

    /// Compute run duration statistics for a task.
    ///
    /// Returns a tuple of (min, max, mean, count) for run durations in nanoseconds.
    /// This is useful for comparing simulated vs real traces where real systems
    /// show more variance due to system noise.
    pub fn run_duration_stats(&self, pid: Pid) -> Option<(TimeNs, TimeNs, TimeNs, usize)> {
        let mut durations: Vec<TimeNs> = Vec::new();
        let mut running_since: Option<TimeNs> = None;

        for event in &self.events {
            match &event.kind {
                TraceKind::TaskScheduled { pid: p } if *p == pid => {
                    running_since = Some(event.time_ns);
                }
                TraceKind::TaskPreempted { pid: p }
                | TraceKind::TaskYielded { pid: p }
                | TraceKind::TaskSlept { pid: p }
                | TraceKind::TaskCompleted { pid: p }
                    if *p == pid =>
                {
                    if let Some(start) = running_since.take() {
                        durations.push(event.time_ns - start);
                    }
                }
                _ => {}
            }
        }

        if durations.is_empty() {
            return None;
        }

        let min = *durations.iter().min().unwrap();
        let max = *durations.iter().max().unwrap();
        let sum: TimeNs = durations.iter().sum();
        let mean = sum / durations.len() as TimeNs;
        Some((min, max, mean, durations.len()))
    }

    /// Count dispatches through the global DSQ vs local DSQ.
    ///
    /// Returns (global_dsq_dispatches, local_dsq_dispatches).
    /// In real execution, CPU-bound tasks mostly use direct dispatch via
    /// SCX_DSQ_LOCAL from select_cpu, while simulated tasks may show more
    /// global DSQ usage due to yield cycles.
    pub fn dsq_dispatch_counts(&self) -> (usize, usize) {
        let mut global = 0usize;
        let mut local = 0usize;

        // DSQ ID 0x8000_0000_0000_0000 | cpu is local DSQ
        // Other IDs are global DSQs (e.g., 4096 for LAVD)
        const LOCAL_DSQ_MASK: u64 = 0xC000_0000_0000_0000;

        for event in &self.events {
            match &event.kind {
                TraceKind::DsqInsert { dsq_id, .. } | TraceKind::DsqInsertVtime { dsq_id, .. } => {
                    if dsq_id.0 & LOCAL_DSQ_MASK != 0 {
                        local += 1;
                    } else {
                        global += 1;
                    }
                }
                _ => {}
            }
        }

        (global, local)
    }

    /// Like `dsq_dispatch_counts` but only counts events at or after `after_ns`.
    fn dsq_dispatch_counts_after(&self, after_ns: TimeNs) -> (usize, usize) {
        let mut global = 0usize;
        let mut local = 0usize;
        const LOCAL_DSQ_MASK: u64 = 0xC000_0000_0000_0000;

        for event in &self.events {
            if event.time_ns < after_ns {
                continue;
            }
            match &event.kind {
                TraceKind::DsqInsert { dsq_id, .. } | TraceKind::DsqInsertVtime { dsq_id, .. } => {
                    if dsq_id.0 & LOCAL_DSQ_MASK != 0 {
                        local += 1;
                    } else {
                        global += 1;
                    }
                }
                _ => {}
            }
        }

        (global, local)
    }

    /// Get a summary of trace statistics useful for realism comparison.
    ///
    /// Returns a struct with key metrics for comparing simulated vs real traces.
    /// When `warmup_ns > 0`, events before that time are excluded from counts
    /// (but still tracked for state like idle start times).
    pub fn summary(&self) -> TraceSummary {
        use std::collections::HashMap;

        let warmup = self.warmup_ns;
        let mut total_ticks = 0usize;
        let mut total_yields = 0usize;
        let mut total_preempts = 0usize;
        let mut total_sleeps = 0usize;
        let mut total_wakes = 0usize;
        let mut total_idle_periods = 0usize;
        let mut total_idle_duration_ns = 0u64;

        // Track per-CPU idle start times for duration computation
        let mut cpu_idle_since: HashMap<crate::types::CpuId, crate::types::TimeNs> = HashMap::new();
        let mut last_event_time: crate::types::TimeNs = 0;

        for event in &self.events {
            last_event_time = last_event_time.max(event.time_ns);

            // Always track idle state transitions for correct duration accounting.
            match &event.kind {
                TraceKind::CpuIdle => {
                    cpu_idle_since.insert(event.cpu, event.time_ns);
                    if event.time_ns >= warmup {
                        total_idle_periods += 1;
                    }
                }
                TraceKind::TaskScheduled { .. } => {
                    if let Some(idle_start) = cpu_idle_since.remove(&event.cpu) {
                        // Only count idle duration within the post-warmup window.
                        let effective_start = idle_start.max(warmup);
                        if event.time_ns > effective_start {
                            total_idle_duration_ns += event.time_ns.saturating_sub(effective_start);
                        }
                    }
                }
                _ => {}
            }

            // Skip counting non-idle events before warmup.
            if event.time_ns < warmup {
                continue;
            }

            match &event.kind {
                TraceKind::Tick { .. } => total_ticks += 1,
                TraceKind::TaskYielded { .. } => total_yields += 1,
                TraceKind::TaskPreempted { .. } => total_preempts += 1,
                TraceKind::TaskSlept { .. } => total_sleeps += 1,
                TraceKind::TaskWoke { .. } => total_wakes += 1,
                // CpuIdle and TaskScheduled already handled above.
                _ => {}
            }
        }

        // Flush CPUs still idle at end of trace
        for idle_start in cpu_idle_since.values() {
            let effective_start = (*idle_start).max(warmup);
            if last_event_time > effective_start {
                total_idle_duration_ns += last_event_time.saturating_sub(effective_start);
            }
        }

        let total_events = if warmup > 0 {
            self.events.iter().filter(|e| e.time_ns >= warmup).count()
        } else {
            self.events.len()
        };

        let (global_dsq, local_dsq) = if warmup > 0 {
            self.dsq_dispatch_counts_after(warmup)
        } else {
            self.dsq_dispatch_counts()
        };

        TraceSummary {
            total_events,
            total_ticks,
            total_yields,
            total_preempts,
            total_sleeps,
            total_wakes,
            total_idle_periods,
            total_idle_duration_ns,
            global_dsq_dispatches: global_dsq,
            local_dsq_dispatches: local_dsq,
        }
    }

    /// Write the trace in Chrome Trace Event Format JSON, loadable in
    /// [ui.perfetto.dev](https://ui.perfetto.dev).
    pub fn write_perfetto_json(&self, writer: &mut impl std::io::Write) -> std::io::Result<()> {
        crate::perfetto::write_json(self, writer)
    }

    /// Write the trace as a wprof-compatible Perfetto protobuf
    /// (`TrackEvent` slices/instants with `debug_annotations`).
    ///
    /// The output is loadable by scxtop's `load_perfetto_trace` and
    /// uses the same category vocabulary as wprof (`ONCPU`, `WAKEE`,
    /// `SCX_DSQ`, `TIMER`, `HARDIRQ`, `SOFTIRQ`, `OFFCPU`,
    /// `IPI_SEND`) so a scxsim trace and a wprof trace can be
    /// loaded side-by-side and analyzed with the same tools. Events
    /// without a wprof counterpart use a `SCXSIM_*` category prefix.
    /// See `crates/scx_simulator/src/safe/perfetto_pb.rs` for the
    /// full vocabulary mapping.
    pub fn write_perfetto_pb(&self, writer: &mut impl std::io::Write) -> std::io::Result<()> {
        crate::perfetto_pb::write_pb(self, writer)
    }

    // -----------------------------------------------------------------------
    // DSQ length sampling
    // -----------------------------------------------------------------------

    /// Sample the length of a specific DSQ or all non-builtin DSQs.
    ///
    /// If `dsq_filter` is `Some(id)`, samples only that DSQ.
    /// If `None`, samples all non-builtin DSQs (used for tick sampling).
    pub(crate) fn sample_dsq_lengths(
        &mut self,
        time_ns: TimeNs,
        dsqs: &DsqManager,
        trigger: DsqSampleTrigger,
        dsq_filter: Option<DsqId>,
    ) {
        match dsq_filter {
            Some(dsq_id) => {
                // Sample a specific DSQ
                if !dsq_id.is_builtin() {
                    let length = dsqs.nr_queued(dsq_id);
                    self.dsq_samples.push(DsqLengthSample {
                        time_ns,
                        dsq_id,
                        length,
                        trigger,
                    });
                }
            }
            None => {
                // Sample all non-builtin DSQs in sorted order for determinism.
                for dsq_id in dsqs.sorted_dsq_ids() {
                    if !dsq_id.is_builtin() {
                        let length = dsqs.nr_queued(dsq_id);
                        self.dsq_samples.push(DsqLengthSample {
                            time_ns,
                            dsq_id,
                            length,
                            trigger,
                        });
                    }
                }
            }
        }
    }

    /// Get all DSQ length samples.
    pub fn dsq_samples(&self) -> &[DsqLengthSample] {
        &self.dsq_samples
    }

    /// Get the maximum observed length for a specific DSQ.
    pub fn max_dsq_length(&self, dsq_id: DsqId) -> Option<usize> {
        self.dsq_samples
            .iter()
            .filter(|s| s.dsq_id == dsq_id)
            .map(|s| s.length)
            .max()
    }

    /// Get the average observed length for a specific DSQ.
    pub fn avg_dsq_length(&self, dsq_id: DsqId) -> Option<f64> {
        let samples: Vec<_> = self
            .dsq_samples
            .iter()
            .filter(|s| s.dsq_id == dsq_id)
            .collect();
        if samples.is_empty() {
            return None;
        }
        let sum: usize = samples.iter().map(|s| s.length).sum();
        Some(sum as f64 / samples.len() as f64)
    }

    /// Get the DSQ length at or before a specific time.
    ///
    /// Returns the most recent sample for the DSQ at or before the given time.
    pub fn dsq_length_at_time(&self, dsq_id: DsqId, time_ns: TimeNs) -> Option<usize> {
        self.dsq_samples
            .iter()
            .rev()
            .find(|s| s.dsq_id == dsq_id && s.time_ns <= time_ns)
            .map(|s| s.length)
    }

    /// Pretty-print the trace for debugging.
    pub fn dump(&self) {
        let cpu_width = if self.nr_cpus == 0 {
            1u8
        } else {
            ((self.nr_cpus as f64).log10().floor() as u8) + 1
        };
        for event in &self.events {
            let desc = match &event.kind {
                TraceKind::TaskScheduled { pid } => format!("SCHED    pid={}", pid.0),
                TraceKind::TaskPreempted { pid } => format!("PREEMPT  pid={}", pid.0),
                TraceKind::TaskYielded { pid } => format!("YIELD    pid={}", pid.0),
                TraceKind::TaskSlept { pid } => format!("SLEEP    pid={}", pid.0),
                TraceKind::TaskWoke { pid } => format!("WAKE     pid={}", pid.0),
                TraceKind::TaskCompleted { pid } => format!("COMPLETE pid={}", pid.0),
                TraceKind::CpuIdle => "IDLE".to_string(),
                TraceKind::SimulationEnd { pid } => format!("SIM_END  pid={}", pid.0),
                TraceKind::PutPrevTask {
                    pid,
                    still_runnable,
                } => {
                    format!("PUT_PREV pid={} runnable={}", pid.0, still_runnable)
                }
                TraceKind::SelectTaskRq {
                    pid,
                    prev_cpu,
                    selected_cpu,
                } => {
                    format!(
                        "SELECT_CPU pid={} prev={} sel={}",
                        pid.0, prev_cpu.0, selected_cpu.0
                    )
                }
                TraceKind::EnqueueTask { pid, enq_flags } => {
                    format!("ENQUEUE  pid={} flags={:#x}", pid.0, enq_flags)
                }
                TraceKind::Balance { prev_pid } => {
                    let p = prev_pid.map_or(-1, |p| p.0);
                    format!("BALANCE  prev_pid={}", p)
                }
                TraceKind::PickTask { pid } => format!("PICK     pid={}", pid.0),
                TraceKind::SetNextTask { pid } => format!("SET_NEXT pid={}", pid.0),
                TraceKind::DsqInsert { pid, dsq_id, slice } => {
                    format!("DSQ_INS  pid={} dsq={:#x} slice={}", pid.0, dsq_id.0, slice)
                }
                TraceKind::DsqInsertVtime {
                    pid,
                    dsq_id,
                    slice,
                    vtime,
                } => {
                    format!(
                        "DSQ_INS_V pid={} dsq={:#x} slice={} vtime={}",
                        pid.0, dsq_id.0, slice, vtime.0
                    )
                }
                TraceKind::DsqMoveToLocal { dsq_id, success } => {
                    format!("DSQ_MOVE dsq={:#x} ok={}", dsq_id.0, success)
                }
                TraceKind::KickCpu { target_cpu } => {
                    format!("KICK     cpu={}", target_cpu.0)
                }
                TraceKind::Tick { pid } => format!("TICK     pid={}", pid.0),
                TraceKind::DispatchRejected {
                    pid,
                    target_cpu,
                    reason,
                } => {
                    let reason_str = match reason {
                        DispatchRejectReason::CpumaskViolation => "cpumask",
                        DispatchRejectReason::MigrationDisabled => "migration_disabled",
                    };
                    format!(
                        "DISPATCH_REJECT pid={} target_cpu={} reason={}",
                        pid.0, target_cpu.0, reason_str
                    )
                }
                TraceKind::IrqStart { cpu, irq_type } => {
                    let kind_str = match irq_type {
                        IrqType::HardIrq => "hardirq",
                        IrqType::SoftIrq => "softirq",
                    };
                    format!("IRQ_START cpu={} type={}", cpu.0, kind_str)
                }
                TraceKind::IrqEnd { cpu } => format!("IRQ_END  cpu={}", cpu.0),
                TraceKind::CgroupBwCharge {
                    pid,
                    cgid,
                    delta_ns,
                } => format!(
                    "CG_BW_CHRG pid={} cgid={} delta={}",
                    pid.0, cgid.0, delta_ns
                ),
                TraceKind::CgroupBwDenied { pid, cgid } => {
                    format!("CG_BW_DENY pid={} cgid={}", pid.0, cgid.0)
                }
                TraceKind::CgroupBwDequeueOnThrottle { pid, cgid } => {
                    format!("CG_BW_DEQ_THR pid={} cgid={}", pid.0, cgid.0)
                }
                TraceKind::CgroupBwReenqueueOnReplenish { pid, cgid } => {
                    format!("CG_BW_REENQ_RPL pid={} cgid={}", pid.0, cgid.0)
                }
                TraceKind::CgroupBwReplenish {
                    cgid,
                    runtime_total_last,
                    period_budget_in,
                    debt,
                    burst_credit,
                    period_budget_out,
                    keep_throttled,
                } => format!(
                    "CG_BW_RPLN cgid={} rtl={} pb_in={} debt={} bc={} pb_out={} kt={}",
                    cgid.0,
                    runtime_total_last,
                    period_budget_in,
                    debt,
                    burst_credit,
                    period_budget_out,
                    *keep_throttled as u8,
                ),
                // tg `bundle-implement-cpu-bw-critical-tracekind-easy-wins`:
                // pretty-printers for the 8 new TraceKinds.
                TraceKind::Runnable { pid, enq_flags } => {
                    format!("RUNNABLE pid={} enq=0x{:x}", pid.0, enq_flags)
                }
                TraceKind::Dequeue { pid, deq_flags } => {
                    format!("DEQUEUE  pid={} deq=0x{:x}", pid.0, deq_flags)
                }
                TraceKind::Quiescent { pid, deq_flags } => {
                    format!("QUIESCNT pid={} deq=0x{:x}", pid.0, deq_flags)
                }
                TraceKind::UpdateIdle { cpu, idle } => {
                    format!("UPD_IDLE cpu={} idle={}", cpu.0, idle)
                }
                TraceKind::CgroupInit { cgid, rc } => {
                    format!("CG_INIT  cgid={} rc={}", cgid.0, rc)
                }
                TraceKind::CgroupExit { cgid } => {
                    format!("CG_EXIT  cgid={}", cgid.0)
                }
                TraceKind::CgroupSetBandwidth {
                    cgid,
                    period_us,
                    quota_us,
                    burst_us,
                } => format!(
                    "CG_SET_BW cgid={} period_us={} quota_us={} burst_us={}",
                    cgid.0, period_us, quota_us, burst_us
                ),
                TraceKind::CgroupMove {
                    pid,
                    from_cgid,
                    to_cgid,
                } => format!(
                    "CG_MOVE  pid={} from_cgid={} to_cgid={}",
                    pid.0, from_cgid.0, to_cgid.0
                ),
                // tg `bundle-implement-secondary-tracekind-easy-wins`:
                // pretty-printers for the 10 new TraceKinds.
                TraceKind::InitTask { pid, rc } => {
                    format!("INIT_TSK pid={} rc={}", pid.0, rc)
                }
                TraceKind::ExitTask { pid } => {
                    format!("EXIT_TSK pid={}", pid.0)
                }
                TraceKind::Enable { pid } => {
                    format!("ENABLE   pid={}", pid.0)
                }
                TraceKind::SetCpumask { pid, cpumask_hex } => {
                    format!("SET_MASK pid={} cpus={}", pid.0, cpumask_hex)
                }
                TraceKind::HelperNow { ret_ns } => {
                    format!("HLP_NOW  ret_ns={}", ret_ns)
                }
                TraceKind::HelperTaskCgroup { pid, cgid } => {
                    format!("HLP_TCG  pid={} cgid={}", pid.0, cgid.0)
                }
                TraceKind::HelperTaskCpu { pid, ret_cpu } => {
                    format!("HLP_TCPU pid={} cpu={}", pid.0, ret_cpu.0)
                }
                TraceKind::CreateDsq { dsq_id, node, rc } => {
                    format!("DSQ_NEW  dsq=0x{:x} node={} rc={}", dsq_id.0, node, rc)
                }
                TraceKind::DestroyDsq { dsq_id } => {
                    format!("DSQ_DEL  dsq=0x{:x}", dsq_id.0)
                }
                TraceKind::DsqNrQueued { dsq_id, ret } => {
                    format!("DSQ_NRQ  dsq=0x{:x} ret={}", dsq_id.0, ret)
                }
                // tg `add-cbw-put-aside-and-drain-btq-batch-tracekinds`
                // (A1+A2 from cgroup_bw audit).
                TraceKind::CbwPutAside {
                    cgid,
                    count,
                    btq_len_after,
                } => format!(
                    "CBW_PARK cgid={} count={} btq_after={}",
                    cgid.0, count, btq_len_after
                ),
                TraceKind::CbwDrainBtqBatch {
                    cgid,
                    count,
                    btq_len_after,
                } => format!(
                    "CBW_DRAIN cgid={} count={} btq_after={}",
                    cgid.0, count, btq_len_after
                ),
                // tg `add-cbw-throttle-cgroups-tracekind` (A3 from
                // cgroup_bw audit). `THROTL` for newly throttled,
                // `UNTHRL` for newly unthrottled.
                TraceKind::CbwThrottleCgroups { cgid, throttled } => {
                    let kind_str = if *throttled { "THROTL" } else { "UNTHRL" };
                    format!("CBW_{kind_str} cgid={}", cgid.0)
                }
            };
            eprintln!(
                "[{}] cpu={:<3} {}",
                FmtTs::local(event.time_ns, Some(event.cpu), cpu_width),
                event.cpu.0,
                desc
            );
        }
    }
}
