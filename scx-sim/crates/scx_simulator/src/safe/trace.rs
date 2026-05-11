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
    /// Dispatch to local DSQ rejected.
    ///
    /// Emitted when a scheduler dispatches to `SCX_DSQ_LOCAL` or
    /// `SCX_DSQ_LOCAL_ON | cpu` but the task cannot run on that CPU
    /// (cpumask or migration-disabled).
    DispatchRejected {
        kind: LocalDsqKind,
        pid: Pid,
        from_cpu: CpuId,
        target_cpu: CpuId,
        reason: DispatchRejectReason,
    },
    /// A scenario event changed a task's migration-disabled counter.
    MigrationDisabledSet { pid: Pid, value: u16 },

    // ----- IRQ events -----
    /// An interrupt starts on a CPU (hardirq or softirq).
    IrqStart { cpu: CpuId, irq_type: IrqType },
    /// An interrupt handler completes on a CPU.
    IrqEnd { cpu: CpuId },

    // ----- Cgroup bandwidth (cpu.max) events (Diff 3 wiring) -----
    /// Engine charged `delta_ns` of CPU time against `cgid`'s `cpu.max` quota
    /// (and any finite ancestors).
    CgroupBwCharge {
        pid: Pid,
        cgid: crate::cgroup::CgroupId,
        delta_ns: TimeNs,
    },
    /// `cgid`'s quota was exhausted; the cgroup is now throttled until refill.
    CgroupBwThrottle { cgid: crate::cgroup::CgroupId },
    /// Engine refused to admit `pid` from a local DSQ because its cgroup
    /// (or an ancestor) is currently throttled. The task remains queued.
    CgroupBwDenied {
        pid: Pid,
        cgid: crate::cgroup::CgroupId,
    },
    /// `cgid` reached a period boundary; quota was refilled and any
    /// throttled tasks are eligible to run again.
    CgroupBwRefill { cgid: crate::cgroup::CgroupId },
}

/// Local DSQ form that produced a dispatch decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalDsqKind {
    /// `SCX_DSQ_LOCAL`, resolved against the callback's local CPU.
    Local,
    /// `SCX_DSQ_LOCAL_ON | cpu`, resolved against an explicit CPU.
    LocalOn,
}

impl LocalDsqKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            LocalDsqKind::Local => "SCX_DSQ_LOCAL",
            LocalDsqKind::LocalOn => "SCX_DSQ_LOCAL_ON",
        }
    }
}

/// Reason why a dispatch to a local DSQ was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchRejectReason {
    /// Target CPU is not in the task's cpumask.
    CpumaskViolation,
    /// Task is migration-disabled and cannot move to a different CPU.
    MigrationDisabled,
}

/// Full details of a rejected local-DSQ dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispatchReject {
    pub kind: LocalDsqKind,
    pub pid: Pid,
    pub from_cpu: CpuId,
    pub target_cpu: CpuId,
    pub reason: DispatchRejectReason,
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
                    kind,
                    pid,
                    from_cpu,
                    target_cpu,
                    reason,
                } => {
                    let reason_str = match reason {
                        DispatchRejectReason::CpumaskViolation => "cpumask",
                        DispatchRejectReason::MigrationDisabled => "migration_disabled",
                    };
                    format!(
                        "DISPATCH_REJECT kind={} pid={} from_cpu={} target_cpu={} reason={}",
                        kind.label(),
                        pid.0,
                        from_cpu.0,
                        target_cpu.0,
                        reason_str
                    )
                }
                TraceKind::MigrationDisabledSet { pid, value } => {
                    format!("MIG_DIS  pid={} value={}", pid.0, value)
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
                TraceKind::CgroupBwThrottle { cgid } => {
                    format!("CG_BW_THR  cgid={}", cgid.0)
                }
                TraceKind::CgroupBwDenied { pid, cgid } => {
                    format!("CG_BW_DENY pid={} cgid={}", pid.0, cgid.0)
                }
                TraceKind::CgroupBwRefill { cgid } => {
                    format!("CG_BW_REF  cgid={}", cgid.0)
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
