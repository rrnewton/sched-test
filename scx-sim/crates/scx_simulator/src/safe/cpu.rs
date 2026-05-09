//! Simulated CPU state.

use std::collections::VecDeque;

use crate::types::{CpuId, Pid, TimeNs};

/// How the previous task on this CPU stopped running.
/// Used to determine context switch overhead (voluntary vs involuntary).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LastStopReason {
    /// Task yielded, slept, or completed (voluntary).
    Voluntary,
    /// Task was preempted by tick or higher-priority task (involuntary).
    Involuntary,
}

/// Current interrupt context on a CPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IrqContext {
    /// No interrupt context (normal task execution).
    #[default]
    None,
    /// In hardware interrupt handler (top half).
    HardIrq,
    /// In softirq handler (bottom half, inline).
    ServingSoftIrq,
}

/// A simulated CPU.
#[derive(Debug)]
pub struct SimCpu {
    /// CPU ID.
    pub id: CpuId,
    /// PID of the currently running task, or None if idle.
    pub current_task: Option<Pid>,
    /// PID of the previously running task (for dispatch `prev` argument).
    /// Set when a task stops running; cleared when a new task starts.
    pub prev_task: Option<Pid>,
    /// The CPU's local dispatch queue (FIFO).
    pub local_dsq: VecDeque<Pid>,
    /// Per-CPU logical clock (nanoseconds).
    ///
    /// Advances when this CPU processes events. With no scheduler overhead,
    /// equals the event queue time. When overhead is modeled, local clocks
    /// diverge across CPUs.
    pub local_clock: TimeNs,
    /// SMT sibling CPUs (including self). Empty if SMT is not configured.
    pub siblings: Vec<CpuId>,
    /// CPU performance level set by `scx_bpf_cpuperf_set`.
    /// Range: `[0, SCX_CPUPERF_ONE]` where `SCX_CPUPERF_ONE = 1024`.
    pub perf_lvl: u32,
    /// Timestamp when the current task started running on this CPU.
    /// Used by `preempt_current()` to compute how much of the slice was consumed.
    pub task_started_at: Option<TimeNs>,
    /// The slice value when the current task started running.
    /// Used by `preempt_current()` to set the remaining slice on the raw task.
    pub task_original_slice: Option<TimeNs>,
    /// Whether this CPU is online. Offline CPUs don't receive ticks or dispatch.
    pub is_online: bool,
    /// Current interrupt context on this CPU.
    pub irq_context: IrqContext,
    /// Accumulated IRQ time stolen from the current running task (ns).
    /// Reset when the task stops or when accounted for in phase/slice events.
    pub irq_stolen_ns: TimeNs,
    /// Cumulative IRQ time on this CPU since simulation start (ns).
    /// Never reset. Used by `sim_scx_clock_task` to compute task-only clock.
    pub irq_cumulative_ns: TimeNs,
    /// LLC (last-level cache) domain ID.
    ///
    /// CPUs sharing an LLC have the same `llc_id`. Used for LLC-aware
    /// migration penalty: cross-LLC migrations incur higher cache/TLB
    /// warming costs than intra-LLC migrations. Default: 0 (single domain).
    pub llc_id: u32,
    /// **EXPERIMENTAL** Local clock at which we last charged this CPU's
    /// running task against its cgroup `cpu.max` quota.
    ///
    /// Used by `ChargeGranularity::Tick` to compute the per-tick delta:
    /// at each Tick event, charge `local_clock - bw_last_charge_at_ns`,
    /// then update this field. At task-stop, charge the residual
    /// (`local_clock - bw_last_charge_at_ns`) since the last tick.
    ///
    /// Reset to the task's start time when a task starts running on
    /// this CPU. Unused when `ChargeGranularity::Stop` (the default).
    /// Branch experiment: `agent/charge-granularity-experiment`.
    pub bw_last_charge_at_ns: TimeNs,
}

impl SimCpu {
    pub fn new(id: CpuId) -> Self {
        SimCpu {
            id,
            current_task: None,
            prev_task: None,
            local_dsq: VecDeque::new(),
            local_clock: 0,
            siblings: Vec::new(),
            perf_lvl: 0,
            task_started_at: None,
            task_original_slice: None,
            is_online: true,
            irq_context: IrqContext::None,
            irq_stolen_ns: 0,
            irq_cumulative_ns: 0,
            llc_id: 0,
            bw_last_charge_at_ns: 0,
        }
    }

    pub fn is_idle(&self) -> bool {
        self.current_task.is_none()
    }
}
