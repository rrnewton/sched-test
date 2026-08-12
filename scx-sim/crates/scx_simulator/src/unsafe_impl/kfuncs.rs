//! Kfunc implementations for the simulator.
//!
//! These are `#[no_mangle] extern "C"` functions that the compiled scheduler
//! C code calls. They access the simulator state via `Arc<Mutex<SimState>>`.
//!
//! The pattern is:
//! 1. Before calling any scheduler ops, the simulator installs a `SimArc`
//!    in the thread-local `SIM_ARC` (via `install_sim_arc` or `enter_sim`).
//! 2. When the scheduler calls a kfunc, the kfunc locks the mutex via
//!    `with_sim()`.
//! 3. After the ops call returns, the simulator calls `exit_sim()` or
//!    `clear_sim_arc()`.
//!
//! # Safety
//!
//! Every public function in this module is `#[no_mangle] extern "C"` and
//! receives raw `*mut c_void` / `*const c_void` pointers from C scheduler
//! code. The thread-local `SIM_ARC` holds an `Arc<Mutex<SimState>>`;
//! `enter_sim` / `exit_sim` manage its installation. Soundness relies on
//! the caller (the simulation engine) guaranteeing that `SIM_ARC` is
//! installed before any kfunc is called and cleared after.

// These are extern "C" FFI entry points called from C code — the C caller
// is responsible for passing valid pointers, so marking them `unsafe` in Rust
// would be meaningless.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ffi::c_void;
use std::os::unix::io::RawFd;
use std::ptr;
use std::sync::{Arc, Mutex};

use rand::rngs::SmallRng;
use rand::RngCore;
use tracing::debug;

use crate::cgroup::{CgroupId, CgroupRegistry};
use crate::cpu::{LastStopReason, SimCpu};
use crate::dsq::DsqManager;
use crate::engine::{flush_staged_events, EventKind, EventQueue};
use crate::ffi;
use crate::fmt::FmtN;
use crate::perf::RbcCounter;
use crate::scenario::{NativeConcurrentConfig, NoiseConfig, OverheadConfig, PreemptiveConfig};
use crate::sim_task::SimTask;
use crate::task::OpsTaskState;
use crate::trace::{DispatchRejectReason, DsqSampleTrigger, Trace, TraceKind};
use crate::types::{CpuId, DsqId, KickFlags, Pid, TimeNs, Vtime};

/// Nanosecond cost for each simulated kernel function.
///
/// Each kfunc call during a scheduler ops invocation contributes its tier
/// cost to the CPU's `local_clock` via `rbc_kfunc_ns`, complementing the
/// RBC counter (which measures scheduler C code).
pub mod kfunc_cost {
    /// Field read/write: `scx_bpf_now`, `bpf_ktime_get_ns`, etc.
    pub const TRIVIAL: u64 = 10;
    /// Lookup/insert: `scx_bpf_create_dsq`, `bpf_task_from_pid`, etc.
    pub const SIMPLE: u64 = 50;
    /// PID lookup + trace: `scx_bpf_dsq_insert`, `scx_bpf_task_running`, etc.
    pub const MODERATE: u64 = 100;
    /// CPU scan, DSQ ops: `scx_bpf_select_cpu_dfl`, DSQ iterators, etc.
    pub const COMPLEX: u64 = 200;
}

/// Which scheduler ops callback is currently executing.
///
/// The kernel defers `scx_bpf_dsq_insert` — it never inserts immediately.
/// During `select_cpu`/`enqueue` the intent is recorded on the task and
/// executed after the callback returns. During `dispatch` inserts are
/// buffered and flushed after the callback. We track the context so
/// the engine can resolve `SCX_DSQ_LOCAL` correctly, and so the PMU
/// preemption handler records which callback was active at preemption.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum OpsContext {
    #[default]
    None = 0,
    SelectCpu = 1,
    Enqueue = 2,
    Dispatch = 3,
    Tick = 4,
    Stopping = 5,
    Running = 6,
    UpdateIdle = 7,
    FireTimer = 8,
    CpuOnline = 9,
    CpuOffline = 10,
    Runnable = 11,
    Quiescent = 12,
    Dequeue = 13,
    Enable = 14,
}

impl OpsContext {
    /// Short string name for trace output and serialization.
    pub fn short_name(self) -> &'static str {
        match self {
            OpsContext::None => "none",
            OpsContext::SelectCpu => "select_cpu",
            OpsContext::Enqueue => "enqueue",
            OpsContext::Dispatch => "dispatch",
            OpsContext::Tick => "tick",
            OpsContext::Stopping => "stopping",
            OpsContext::Running => "running",
            OpsContext::UpdateIdle => "update_idle",
            OpsContext::FireTimer => "fire_timer",
            OpsContext::CpuOnline => "cpu_online",
            OpsContext::CpuOffline => "cpu_offline",
            OpsContext::Runnable => "runnable",
            OpsContext::Quiescent => "quiescent",
            OpsContext::Dequeue => "dequeue",
            OpsContext::Enable => "enable",
        }
    }

    /// Reconstruct from discriminant (for atomic ring deserialization).
    pub fn from_discriminant(d: u8) -> Self {
        match d {
            0 => OpsContext::None,
            1 => OpsContext::SelectCpu,
            2 => OpsContext::Enqueue,
            3 => OpsContext::Dispatch,
            4 => OpsContext::Tick,
            5 => OpsContext::Stopping,
            6 => OpsContext::Running,
            7 => OpsContext::UpdateIdle,
            8 => OpsContext::FireTimer,
            9 => OpsContext::CpuOnline,
            10 => OpsContext::CpuOffline,
            11 => OpsContext::Runnable,
            12 => OpsContext::Quiescent,
            13 => OpsContext::Dequeue,
            14 => OpsContext::Enable,
            _ => OpsContext::None,
        }
    }

    /// Parse from short name string (for trace deserialization).
    pub fn from_short_name(s: &str) -> Self {
        match s {
            "select_cpu" => OpsContext::SelectCpu,
            "enqueue" => OpsContext::Enqueue,
            "dispatch" => OpsContext::Dispatch,
            "tick" => OpsContext::Tick,
            "stopping" => OpsContext::Stopping,
            "running" => OpsContext::Running,
            "update_idle" => OpsContext::UpdateIdle,
            "fire_timer" => OpsContext::FireTimer,
            "cpu_online" => OpsContext::CpuOnline,
            "cpu_offline" => OpsContext::CpuOffline,
            "runnable" => OpsContext::Runnable,
            "quiescent" => OpsContext::Quiescent,
            "dequeue" => OpsContext::Dequeue,
            "enable" => OpsContext::Enable,
            _ => OpsContext::None,
        }
    }
}

/// A deferred dispatch request recorded by `scx_bpf_dsq_insert[_vtime]`.
///
/// Mirrors the kernel's `p->scx.ddsp_dsq_id` / `ddsp_enq_flags` fields.
#[derive(Debug, Clone)]
pub struct PendingDispatch {
    pub pid: Pid,
    pub dsq_id: DsqId,
    pub enq_flags: u64,
    pub vtime: Option<Vtime>,
}

/// Active DSQ iterator state for `bpf_for_each(scx_dsq, ...)`.
///
/// Holds a snapshot of the DSQ contents at iteration start, plus the
/// current cursor position. This lets C loop code iterate a Rust-owned
/// DSQ via `sim_dsq_iter_begin` / `sim_dsq_iter_next` kfuncs.
#[derive(Debug)]
pub struct DsqIterState {
    pub dsq_id: DsqId,
    pub pids: Vec<Pid>,
    pub pos: usize,
}

/// Events staged by kfuncs during a callback for deferred processing.
///
/// Kfuncs cannot push directly to the event queue (they only have
/// `&mut SimulatorState`, not `&mut EventQueue`). Instead, they append
/// to `SimulatorState::staged_events`, which the engine flushes to the
/// event queue after each callback returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StagedEvent {
    /// A kick to be delivered to a target CPU after IPI latency.
    KickDelivered { cpu: CpuId, flags: KickFlags },
}

/// The subset of simulator state that kfuncs need access to.
///
/// Maximum number of distinct BPF timers a scheduler / library can arm
/// concurrently. The C side (each scheduler's `wrapper.c`) keeps a
/// fixed-size array of `(struct bpf_timer *, callback, map)` triples
/// indexed by slot id; `bpf_timer_init` allocates the next free slot
/// for the supplied `bpf_timer *`. The Rust side mirrors that with a
/// `pending_timers: [Option<(TimeNs, CpuId)>; MAX_BPF_TIMERS]` array on
/// `SimulatorState` and a `slot: u8` field on `EventKind::TimerFired`.
///
/// Phase 1 BPF infra scale-up items 1+2 (tg
/// `scxsim-bpf-infra-scale-up-phase1`). 8 was picked to comfortably
/// cover the production stack -- LAVD has 1 timer (`update_timer`),
/// the cgroup_bw library Phase 2 will compile in adds 1 more
/// (`cbw_replenish_timer`); future libraries may add a couple more.
/// Bump if a real workload ever exhausts it.
pub const MAX_BPF_TIMERS: usize = 8;

/// # Concurrency classification
///
/// Each field is classified for Phase 2 concurrent access:
/// - **PER-CPU**: Only accessed by the worker owning that CPU (safe without locking).
/// - **SHARED-READ**: Read by multiple workers but rarely mutated (RwLock candidate).
/// - **SHARED-MUTABLE**: Read and written by multiple workers (Mutex candidate).
/// - **PER-WORKER-TLS**: Should come from TLS in concurrent mode, not shared state.
pub struct SimulatorState {
    /// PER-CPU: Each worker accesses only `cpus[current_cpu]`. The Vec itself
    /// is fixed-size after init (no reallocation), so disjoint per-CPU access
    /// is safe. Cross-CPU reads (idle scan, kick) need SHARED-READ treatment.
    pub cpus: Vec<SimCpu>,
    /// SHARED-MUTABLE: DSQs are global state — multiple workers insert/consume
    /// concurrently. Requires Mutex or sharded locking in concurrent mode.
    pub dsqs: DsqManager,
    /// PER-WORKER-TLS: In concurrent mode, use `current_cpu_from_tls()`
    /// instead. This field is only safe under token-ring serialization.
    /// `enter_sim()` writes it on the shared state, which races when
    /// multiple workers call `enter_sim()` concurrently.
    ///
    /// Call sites reading `sim.current_cpu` inside kfuncs (Phase 2 migration):
    /// - `scx_bpf_dsq_insert` (trace recording)
    /// - `scx_bpf_dsq_insert_vtime` (trace recording)
    /// - `scx_bpf_dsq_move_to_local` (local DSQ target)
    /// - `scx_bpf_dsq_nr_queued` (LOCAL DSQ lookup)
    /// - `scx_bpf_now` / `bpf_ktime_get_ns` (per-CPU clock)
    /// - `bpf_get_smp_processor_id` / `sim_bpf_get_smp_processor_id`
    /// - `sim_bpf_in_hardirq` / `sim_bpf_in_serving_softirq` / `sim_bpf_in_interrupt`
    /// - `bpf_get_current_task_btf` (current task lookup)
    /// - `scx_bpf_kick_cpu` (trace recording)
    /// - `sim_scx_bpf_dsq_move` (LOCAL DSQ target)
    /// - `resolve_pending_dispatch` (LOCAL_ON rejection trace)
    pub current_cpu: CpuId,
    /// SHARED-MUTABLE: Trace is append-only but written by all workers.
    /// Could use a per-CPU trace buffer with post-merge, or a Mutex.
    pub trace: Trace,
    /// SHARED-READ: Global simulation clock (event queue time). Updated by
    /// the engine between events, read by kfuncs. Workers read but do not
    /// write during callback execution.
    pub clock: TimeNs,
    /// SHARED-READ: Task pointer maps are populated at init and during
    /// `enable`/task creation. Read by kfuncs for PID↔pointer translation.
    /// Rarely mutated after setup (RwLock candidate).
    ///
    /// Maps raw task_struct pointers to PIDs for reverse lookup.
    pub task_raw_to_pid: HashMap<usize, Pid>,
    /// SHARED-READ: Reverse of task_raw_to_pid. Same access pattern.
    ///
    /// Maps PIDs to raw task_struct pointers.
    pub task_pid_to_raw: HashMap<Pid, usize>,
    /// SHARED-MUTABLE: PRNG state is mutated on every call to
    /// `sim_bpf_get_prandom_u32`. Needs Mutex or per-worker PRNG.
    ///
    /// Deterministic PRNG for reproducible simulation.
    pub rng: SmallRng,
    /// PER-WORKER-TLS: Each worker sets this to the callback it is executing.
    /// Already duplicated in per-thread TLS via `set_current_ops_context()`.
    /// The shared field is used by the engine between callbacks.
    ///
    /// Which ops callback we are currently inside.
    pub ops_context: OpsContext,
    /// PER-CPU: Each worker records its own pending dispatches during
    /// `select_cpu`/`enqueue`/`dispatch`. Only the owning worker reads them
    /// back in `resolve_pending_dispatch`.
    ///
    /// Deferred dispatches recorded by `scx_bpf_dsq_insert*()`.
    /// The engine resolves `SCX_DSQ_LOCAL` and executes after the callback.
    pub pending_dispatches: VecDeque<PendingDispatch>,
    /// PER-CPU: DSQ iterator state is per-callback, used only by the
    /// worker that started the iteration.
    ///
    /// Active DSQ iterator for `bpf_for_each(scx_dsq, ...)`.
    pub dsq_iter: Option<DsqIterState>,
    /// Events staged by kfuncs during a callback for deferred processing.
    ///
    /// The engine flushes these to the event queue after each callback
    /// returns. This replaces direct kick processing for timed events.
    pub staged_events: Vec<(TimeNs, StagedEvent)>,
    /// SHARED-MUTABLE: Updated when any task starts running on any CPU,
    /// read by `scx_bpf_task_cpu`. Needs Mutex or per-task atomic.
    ///
    /// Per-task last CPU (set when a task starts running).
    /// Used by `scx_bpf_task_cpu` to return the correct value.
    pub task_last_cpu: HashMap<Pid, CpuId>,
    /// SHARED-MUTABLE: Updated by enqueue/dispatch on any CPU, read by
    /// dequeue gating. Needs Mutex or per-task atomic state.
    ///
    /// Per-task SCX ops_state (mirrors kernel's `SCX_OPSS_*`).
    /// Tracks whether the task is `Queued` (in the BPF scheduler) or `None`
    /// (dispatched / not queued). Used to gate `ops.dequeue()`.
    /// Uses BTreeMap for deterministic iteration order.
    pub task_ops_state: BTreeMap<Pid, OpsTaskState>,
    /// PER-CPU: Set by `scx_bpf_reenqueue_local` during `cpu_release`,
    /// consumed by the engine on the same CPU after the callback.
    ///
    /// Flag set by `scx_bpf_reenqueue_local` during `cpu_release`.
    /// The engine drains the local DSQ and re-enqueues tasks after the
    /// callback returns.
    pub reenqueue_local_requested: bool,
    /// SHARED-MUTABLE: Per-slot pending BPF timer state.
    ///
    /// `pending_timers[slot] = Some((fire_at_ns, armed_on_cpu))` means
    /// the BPF callback associated with `slot` re-armed itself or armed
    /// a sibling timer during the most recent dispatch. The engine
    /// drains ALL slots after each callback and inserts one
    /// `EventKind::TimerFired { cpu, slot }` per non-empty slot.
    ///
    /// Phase 1 BPF infra scale-up items 1+2 (tg
    /// `scxsim-bpf-infra-scale-up-phase1`, design doc section Phase 1
    /// items 1+2): bumped from a single `(pending_timer_ns,
    /// pending_timer_cpu)` pair to an array so that schedulers /
    /// libraries that arm multiple BPF timers concurrently don't lose
    /// arms (the production cgroup_bw library has its own
    /// `cbw_replenish_timer` separate from LAVD's `update_timer`;
    /// Phase 2 will compile that library in).
    ///
    /// Slot allocation is owned C-side per scheduler (see e.g.
    /// `schedulers/lavd/wrapper.c::lavd_timer_*` array). Slot 0 is
    /// reserved for the legacy single-timer code path; the
    /// backward-compat `sim_timer_start(nsecs)` entry point always
    /// writes slot 0 so existing single-timer schedulers (mitosis,
    /// cosmos, the pre-Phase-1 LAVD path) continue to work unchanged.
    ///
    /// In the kernel, BPF timers fire in softirq on the CPU that
    /// armed them (with `BPF_F_TIMER_CPU_PIN`). We capture
    /// `current_cpu` per slot for the same per-slot semantics.
    pub pending_timers: [Option<(TimeNs, CpuId)>; MAX_BPF_TIMERS],
    /// PER-CPU: Set by the engine before calling `select_cpu` on the
    /// waker's CPU, consumed by `bpf_get_current_task_btf` during that
    /// same callback. Only the owning worker reads/writes it.
    ///
    /// Raw pointer to the waker task during a wake-induced `select_cpu` call.
    /// In the kernel, `select_cpu` runs in the waker's context, so
    /// `bpf_get_current_task_btf()` returns the waker's task_struct.
    /// The engine sets this when processing a `Phase::Wake`-induced event.
    pub waker_task_raw: Option<usize>,
    /// SHARED-READ: Allocated once at startup, never mutated. All workers
    /// can safely read the pointer.
    ///
    /// Raw pointer to a synthetic idle task_struct (PF_IDLE, mm=NULL).
    /// In the real kernel, `bpf_get_current_task_btf()` never returns NULL —
    /// the idle task is always running when no real task is. The engine
    /// allocates this once at startup so kfuncs can return it as a fallback.
    pub idle_task_raw: *mut c_void,
    /// SHARED-READ: Configuration set at init, never mutated during simulation.
    ///
    /// Timing noise configuration (tick jitter).
    pub noise: NoiseConfig,
    /// SHARED-READ: Configuration set at init, never mutated during simulation.
    ///
    /// Context switch overhead configuration.
    pub overhead: OverheadConfig,
    /// PER-CPU: Each worker has its own RBC counter fd. In concurrent mode
    /// this would move to per-worker TLS.
    ///
    /// RBC counter for measuring scheduler C code overhead (None = disabled).
    ///
    /// In e9patch mode this is `None` — the e9 software counter provides a
    /// deterministic branch count instead of the PMU (which has signal skid).
    pub rbc_counter: Option<RbcCounter>,
    /// SHARED-READ: Configuration set at init, never mutated during simulation.
    ///
    /// Nanoseconds per retired conditional branch (None = disabled).
    pub sched_overhead_rbc_ns: Option<u64>,
    /// PER-CPU: Accumulated per-callback, reset per measurement window.
    /// Each worker tracks its own kfunc call count.
    ///
    /// Number of kfunc calls during the current RBC measurement window.
    pub rbc_kfunc_calls: u32,
    /// PER-CPU: Accumulated per-callback, reset per measurement window.
    /// Each worker tracks its own kfunc nanosecond cost.
    ///
    /// Accumulated kfunc nanosecond cost during the current RBC measurement window.
    pub rbc_kfunc_ns: u64,
    /// Snapshot of the e9 software counter at `start_rbc()` time.
    ///
    /// When e9patch mode is active, `start_rbc()` saves the current value of
    /// `E9SharedRbc.counter` here. `charge_sched_time()` then computes the
    /// number of Jcc instructions executed as `snapshot - current_counter`.
    /// This gives a deterministic branch count (unlike the PMU hardware counter).
    pub rbc_e9_snapshot: i64,
    /// Snapshot of the e9 counter at the last kfunc boundary (or structop start).
    ///
    /// Used to compute the RBC interval between successive kfuncs within a
    /// single structop. Reset in `start_rbc()` to the same value as
    /// `rbc_e9_snapshot`. Updated in `with_sim()` after each kfunc.
    pub rbc_e9_last_kfunc: i64,
    /// PMU counter reading at the last kfunc boundary (or structop start).
    ///
    /// Analogous to `rbc_e9_last_kfunc` but for the PMU hardware counter.
    /// The PMU counter accumulates (not decrements), so the interval is
    /// `current - last_kfunc`. Reset in `start_rbc()` to 0.
    pub rbc_pmu_last_kfunc: u64,
    /// Highest total RBC count observed for any single structop call.
    ///
    /// Updated in `charge_sched_time()` after each structop completes.
    /// Tracks the maximum across all CPUs and all ops callbacks.
    pub longest_structop_rbc: u64,
    /// Longest unbroken RBC interval between kfuncs within any structop.
    ///
    /// When a structop runs C code, it periodically calls kfuncs (which pause
    /// the counter). The interval between two kfunc calls is an unbroken run
    /// of scheduler C code. This tracks the maximum of those intervals.
    /// Updated in `with_sim()` and `charge_sched_time()`.
    pub longest_rbc_interval: u64,
    /// SHARED-MUTABLE: Any kfunc on any CPU can set a BPF error. First-write
    /// wins (subsequent errors are ignored). Needs Mutex or atomic Option.
    ///
    /// BPF error message set by `scx_bpf_error()`.
    /// When set, the engine should stop the simulation and report
    /// `ExitKind::ErrorBpf`. Only the first error is recorded.
    pub bpf_error: Option<String>,
    /// SHARED-READ: Configuration set at init, never mutated during simulation.
    ///
    /// Enable concurrent callback interleaving at kfunc yield points.
    pub interleave: bool,
    /// SHARED-READ: Enable stochastic BPF timer interleaving at cgroup_bw yield sites.
    pub stochastic_timer_interleave: bool,
    /// SHARED-READ: Timer fire-ahead window for stochastic timer interleaving.
    pub stochastic_timer_interleave_window_ns: TimeNs,
    /// SHARED-READ: Approximate rate for pulling eligible timers forward.
    pub stochastic_timer_interleave_one_in: u32,
    /// SHARED-READ: Force deterministic timer interleavings at targeted cgroup_bw sites.
    pub targeted_cbw_yield_sites: bool,
    /// SHARED-READ: Fire-ahead window for targeted cgroup_bw timer pulls.
    pub targeted_cbw_yield_window_ns: TimeNs,
    /// SHARED-READ: Maximum number of targeted cgroup_bw timer pulls.
    pub targeted_cbw_yield_limit: u32,
    /// SHARED-MUT: Number of targeted cgroup_bw timer pulls already forced.
    pub targeted_cbw_yield_count: u32,
    /// SHARED-READ: Configuration set at init, never mutated during simulation.
    ///
    /// Preemptive interleaving configuration (None = disabled).
    pub preemptive: Option<PreemptiveConfig>,
    /// SHARED-READ: Loaded at init, read-only during simulation. Workers
    /// read preemption targets but never mutate the trace.
    ///
    /// Optional preemption trace for replay mode.
    /// When set, preemptive dispatch uses the recorded trace instead of
    /// random PMU timeslices, enabling exact reproduction of preemption
    /// points via hardware breakpoints.
    pub replay_trace: Option<crate::preempt::trace::PreemptionTrace>,
    /// SHARED-MUTABLE: Per-worker cursors are advanced during dispatch.
    /// In concurrent mode, each worker's cursor is independent, but the
    /// backend itself is shared.
    ///
    /// Persistent replay backend (created once, reused across dispatch rounds).
    /// The `ReplayBackend` holds per-worker `ReplayCursor`s that track
    /// which preemption target is next. Creating a fresh backend each
    /// round would reset cursors to index 0, causing targets from all
    /// rounds to be replayed from the beginning every time.
    pub(crate) replay_backend: Option<crate::backend::replay::ReplayBackend>,
    /// Persistent e9patch replay backend (created once, reused across rounds).
    ///
    /// Used when replaying a trace with e9patch software branch counting
    /// instead of PMU + HW breakpoints. Holds per-worker cursors and
    /// accumulated RBC counters.
    pub(crate) e9_replay_backend: Option<crate::backend::e9patch::E9PatchReplayBackend>,
    /// SHARED-READ: Set once during `Simulator::run()`, read-only afterward.
    ///
    /// Resolved e9patch C trampoline function pointers (from the loaded `.so`).
    /// Set during `Simulator::run()` when e9patch mode is active.
    pub(crate) e9_fns: Option<crate::backend::e9patch::E9PatchFns>,
    /// PER-CPU: Indexed by CpuId.0 — each worker accesses only its own
    /// accumulator. The Vec is fixed-size after init.
    ///
    /// Per-CPU structop accumulators that persist across dispatch rounds.
    /// Indexed by CpuId.0. Seeded into worker thread-locals at the start
    /// of each dispatch round and drained back at the end.
    pub structop_accum: Vec<crate::preempt::StructopInfo>,
    /// SHARED-READ: Configuration set at init, never mutated during simulation.
    ///
    /// Native concurrency backend configuration (None = disabled).
    /// When set, workers run truly concurrently with real locks and
    /// window-based clock throttling instead of token-ring serialization.
    pub native_concurrent: Option<NativeConcurrentConfig>,
    /// SHARED-MUTABLE: Tasks that the engine eagerly removed from BPF
    /// scheduler queues (via `ops.dequeue` + `ops.quiescent`) when their
    /// cgroup_bw quota was exhausted. Indexed by `CgroupId`. The engine
    /// drains the per-cgroup queue and re-runnables each task (via the
    /// normal wakeup path: `ops.runnable` + `ops.select_cpu` +
    /// `ops.enqueue`) when the cgroup_bw library marks the cgroup as no
    /// longer throttled.
    ///
    /// This replaces the previous "lazy / head-of-line-blocking" admission
    /// gate that left throttled tasks in the local DSQ — making scxsim's
    /// throttle handling kernel-faithful (the kernel's bandwidth controller
    /// fully dequeues throttled tasks rather than blocking dispatch).
    ///
    /// Insertion order is preserved per-cgroup (VecDeque) so that
    /// re-enqueue happens in roughly the same order tasks were originally
    /// admitted. Cross-cgroup ordering is stable per the BTreeMap.
    ///
    /// tg `scxsim-eager-cgroup-bw-throttle-via-dequeue-wakeup-cycle`.
    pub bw_blocked: BTreeMap<crate::cgroup::CgroupId, VecDeque<Pid>>,
}

/// Bundle of all shared simulator state, protected by a single Mutex.
///
/// The engine creates this at simulation start and wraps it in `Arc<Mutex<_>>`.
/// Kfuncs acquire the lock via the `SIM_ARC` thread-local. The engine releases
/// the lock before calling into C scheduler code, and kfuncs reacquire it
/// through the thread-local Arc.
pub(crate) struct SimState {
    /// Core scheduler state (CPUs, DSQs, trace, etc.).
    pub sim: SimulatorState,
    /// All simulated tasks, keyed by PID.
    pub tasks: HashMap<Pid, SimTask>,
    /// The event priority queue.
    pub events: EventQueue,
    /// The cgroup hierarchy.
    pub cgroup_registry: CgroupRegistry,
    /// PID → cgroup-id resolution for charging.
    ///
    /// Built once at scenario load from `Scenario.tasks` (`cgroup_name`).
    /// Tasks not present in this map belong to the root cgroup and are
    /// not charged against any tracked bandwidth state.
    pub task_to_cgid: HashMap<Pid, CgroupId>,
}

/// Split-borrowed references to all SimState fields.
///
/// `MutexGuard<SimState>::DerefMut` returns a single `&mut SimState`,
/// preventing the borrow checker from splitting borrows of distinct
/// fields. Use `guard.fields()` to get independently-borrowable
/// references:
///
/// ```ignore
/// let f = guard.fields();
/// start_rbc(&mut f.sim);          // borrows f.sim
/// f.tasks.get_mut(&pid);          // borrows f.tasks — no conflict
/// f.sim.trace.record(...);        // borrows f.sim — fine
/// ```
#[allow(dead_code)]
pub(crate) struct SimFields<'a> {
    pub sim: &'a mut SimulatorState,
    pub tasks: &'a mut HashMap<Pid, SimTask>,
    pub events: &'a mut EventQueue,
    pub cgroup_registry: &'a mut CgroupRegistry,
    pub task_to_cgid: &'a mut HashMap<Pid, CgroupId>,
}

impl SimState {
    /// Get split-borrowable references to all fields.
    #[allow(dead_code)]
    pub(crate) fn fields(&mut self) -> SimFields<'_> {
        SimFields {
            sim: &mut self.sim,
            tasks: &mut self.tasks,
            events: &mut self.events,
            cgroup_registry: &mut self.cgroup_registry,
            task_to_cgid: &mut self.task_to_cgid,
        }
    }
}

// SAFETY: SimState contains raw pointers (idle_task_raw in SimulatorState,
// raw C pointers in CgroupInfo/SimTask) but these are only accessed while
// the Mutex is held. The Arc<Mutex<>> ensures exclusive access.
unsafe impl Send for SimState {}
unsafe impl Sync for SimState {}

/// Shared-ownership handle to the mutex-protected simulation state.
pub(crate) type SimArc = Arc<Mutex<SimState>>;

/// Per-callback identity context that the signal handler needs to
/// save/restore without locking the Mutex.
///
/// Stored in a `Cell<Option<CallbackContext>>` thread-local so it is
/// async-signal-safe (plain Copy read/write).
#[derive(Clone, Copy)]
pub struct CallbackContext {
    pub current_cpu: CpuId,
    pub ops_context: OpsContext,
    pub waker_task_raw: Option<usize>,
}

/// Kernel value of `SCX_TASK_QUEUED` from `enum scx_task_state`.
/// Propagated to `p->scx.flags` so scheduler C code can check it.
pub const SCX_TASK_QUEUED: u32 = 1;

impl SimulatorState {
    /// Consume a task from a global DSQ into a CPU's local DSQ.
    ///
    /// Uses raw pointer arithmetic to split-borrow `cpus[cpu_idx]` and
    /// `dsqs` simultaneously, which is necessary because `move_to_local`
    /// needs `&mut SimCpu` while we also hold `&mut DsqManager`.
    ///
    /// Skips tasks whose cpumask does not include the target CPU, mirroring
    /// the kernel's `consume_dispatch_q()` behavior.
    ///
    /// Returns true if a task was consumed.
    pub fn consume_dsq_to_local(
        &mut self,
        dsq_id: crate::types::DsqId,
        cpu: crate::types::CpuId,
    ) -> bool {
        let cpu_idx = cpu.0 as usize;
        // SAFETY: cpu_idx is within bounds (validated by the engine).
        // The split borrow is sound because cpus_ptr[cpu_idx] and dsqs
        // are disjoint fields of SimulatorState.
        let cpus_ptr = self.cpus.as_mut_ptr();
        let sim_cpu = unsafe { &mut *cpus_ptr.add(cpu_idx) };
        let task_map = &self.task_pid_to_raw;
        self.dsqs.move_to_local_filtered(dsq_id, sim_cpu, |pid| {
            let Some(&raw) = task_map.get(&pid) else {
                return true;
            };
            let task_ptr = raw as *mut c_void;
            let task_cpus = unsafe { ffi::sim_task_get_cpus_ptr(task_ptr) };
            task_cpus.is_null() || unsafe { ffi::bpf_cpumask_test_cpu(cpu.0, task_cpus) }
        })
    }

    /// Set the per-task ops_state and, when transitioning to Queued,
    /// also set `SCX_TASK_QUEUED` in `p->scx.flags`.
    ///
    /// `SCX_TASK_QUEUED` is **not** cleared here when going to None
    /// (dispatch resolution) — the kernel keeps the flag set until the
    /// task starts running or goes to sleep.  The engine clears it
    /// via [`clear_task_queued`] at those points.
    pub fn set_task_ops_state(&mut self, pid: Pid, new_state: OpsTaskState) {
        self.task_ops_state.insert(pid, new_state);
        if new_state == OpsTaskState::Queued {
            self.set_scx_flag(pid, SCX_TASK_QUEUED);
        }
    }

    /// Clear `SCX_TASK_QUEUED` in `p->scx.flags`.
    ///
    /// Called when a task starts running or goes to sleep — matching
    /// kernel semantics where the flag is cleared in `scx_ops_dequeue_task`
    /// and when the task is picked to run.
    pub fn clear_task_queued(&mut self, pid: Pid) {
        if let Some(&raw) = self.task_pid_to_raw.get(&pid) {
            unsafe {
                let flags = crate::ffi::sim_task_get_scx_flags(raw as *mut c_void);
                if flags & SCX_TASK_QUEUED != 0 {
                    crate::ffi::sim_task_set_scx_flags(
                        raw as *mut c_void,
                        flags & !SCX_TASK_QUEUED,
                    );
                }
            }
        }
    }

    fn set_scx_flag(&self, pid: Pid, flag: u32) {
        if let Some(&raw) = self.task_pid_to_raw.get(&pid) {
            unsafe {
                let flags = crate::ffi::sim_task_get_scx_flags(raw as *mut c_void);
                if flags & flag == 0 {
                    crate::ffi::sim_task_set_scx_flags(raw as *mut c_void, flags | flag);
                }
            }
        }
    }

    pub fn cpu_is_idle(&self, cpu: CpuId) -> bool {
        self.cpus
            .get(cpu.0 as usize)
            .is_some_and(|c| c.is_idle() && c.local_dsq.is_empty())
    }

    pub fn find_any_idle_cpu(&self) -> Option<CpuId> {
        self.cpus
            .iter()
            .find(|c| c.is_idle() && c.local_dsq.is_empty())
            .map(|c| c.id)
    }

    /// Update the idle SMT mask when a CPU transitions busy→idle.
    ///
    /// If all SMT siblings of `cpu` are idle, sets all of them in the
    /// idle SMT mask (the core is fully idle). Called by the engine when
    /// a CPU becomes idle.
    pub fn update_smt_mask_idle(&self, cpu: CpuId) {
        let siblings = &self.cpus[cpu.0 as usize].siblings;
        if siblings.is_empty() {
            return;
        }
        // Check if all siblings (including self) are idle
        let all_idle = siblings
            .iter()
            .all(|&sib| self.cpus[sib.0 as usize].is_idle());
        if all_idle {
            for &sib in siblings {
                unsafe { crate::ffi::scx_test_set_idle_smtmask(sib.0 as i32) };
            }
        }
    }

    /// Update the idle SMT mask when a CPU transitions idle→busy.
    ///
    /// Clears all SMT siblings from the idle SMT mask (the core is no
    /// longer fully idle). Called by the engine when a task starts
    /// running on a CPU.
    pub fn update_smt_mask_busy(&self, cpu: CpuId) {
        let siblings = &self.cpus[cpu.0 as usize].siblings;
        if siblings.is_empty() {
            return;
        }
        for &sib in siblings {
            unsafe { crate::ffi::scx_test_clear_idle_smtmask(sib.0 as i32) };
        }
    }

    pub fn task_pid_from_raw(&self, p: *mut c_void) -> Pid {
        *self
            .task_raw_to_pid
            .get(&(p as usize))
            .unwrap_or_else(|| panic!("unknown task_struct pointer {:?}", p))
    }

    /// Deterministic PRNG — delegates to `rand::rngs::SmallRng`.
    pub fn next_prng(&mut self) -> u32 {
        self.rng.next_u32()
    }

    /// Advance a CPU's local clock to at least the event queue time.
    ///
    /// # NOTE TIMING_MODEL
    ///
    /// This simulator is **not** cycle-accurate. Real CPU cores share a
    /// clock and run in lockstep, but our discrete-event loop jumps between
    /// unrelated concurrent events. Each CPU maintains a `local_clock` that
    /// tracks how far that CPU has progressed in simulated time.
    ///
    /// The essential correctness invariant: it is safe for CPU A's local
    /// clock to run ahead of CPU B's, **as long as the interval does not
    /// involve interaction between A and B**. When an event does require
    /// cross-CPU coordination (wake, kick, etc.), the event queue timestamp
    /// establishes a lower bound, and this function synchronizes:
    ///
    /// ```text
    /// local_clock = max(local_clock, event_queue_time)
    /// ```
    ///
    /// - If `local_clock > event_queue_time`: the CPU was busy and has
    ///   already advanced past the event. The local clock is preserved.
    ///   This is the common case for back-to-back task execution.
    ///
    /// - If `local_clock < event_queue_time`: the CPU was idle or behind.
    ///   We pull its clock forward to the event time. Any per-CPU overhead
    ///   (e.g. context switch cost) that was applied to `local_clock`
    ///   before the idle period is correctly absorbed — it represents real
    ///   time consumed on that CPU, which elapsed during the idle gap.
    ///
    /// Corollary: per-CPU timing adjustments (CSW overhead, tick jitter)
    /// should be applied directly to `local_clock` at the point they
    /// occur. They affect only subsequent events on that CPU and cannot
    /// influence other CPUs' clocks.
    pub fn advance_cpu_clock(&mut self, cpu: CpuId) {
        let idx = cpu.0 as usize;
        self.cpus[idx].local_clock = self.cpus[idx].local_clock.max(self.clock);
        clock_window_check(cpu, self.cpus[idx].local_clock);
    }

    /// Sample approximately-normal noise using Irwin-Hall (sum of 4 uniforms).
    /// Returns a value centered around 0 with the given stddev.
    pub fn sample_normal_ns(&mut self, stddev: TimeNs) -> i64 {
        if stddev == 0 {
            return 0;
        }
        // Sum of 4 uniform [0,1000) values → mean=2000, σ ≈ 577 (in milli-units)
        let sum: u64 = (0..4).map(|_| (self.next_prng() as u64) % 1000).sum();
        let centered = sum as i64 - 2000;
        // Scale: centered/577 * stddev
        (centered * stddev as i64) / 577
    }

    /// Sample wakeup latency from a heavy-tailed distribution.
    ///
    /// Production scheduling latencies follow a log-normal base distribution
    /// with occasional heavy-tail spikes (from lock contention, cache storms,
    /// runqueue imbalance). This produces realistic p99/p50 ratios:
    ///
    /// - Normal:    p99/p50 ≈ 2-3x  (too thin)
    /// - Log-normal: p99/p50 ≈ 5-10x (matches LAVD production)
    /// - With spikes: p99/p50 ≈ 10-50x (matches EEVDF production)
    ///
    /// Returns a positive latency value in nanoseconds.
    pub fn sample_wakeup_latency_ns(&mut self, floor_ns: TimeNs) -> u64 {
        if floor_ns == 0 {
            return 0;
        }

        // Generate approximate log-normal using exp(normal).
        // We use the Irwin-Hall normal approx: sum of 4 uniforms → mean=2000, σ≈577.
        let sum: u64 = (0..4).map(|_| (self.next_prng() as u64) % 1000).sum();
        // Map to normal(0,1) range: (sum - 2000) / 577
        // Then to log-normal: exp(μ + σ*z) where μ=ln(floor), σ=0.5
        // Approximation: floor * (1 + σ*z + 0.5*σ²*z²) for moderate z
        let z = (sum as i64 - 2000) as f64 / 577.0;
        let sigma = 0.5_f64;
        let log_normal_multiplier = (sigma * z).exp(); // e^(σz), centered around 1.0
        let base = (floor_ns as f64 * log_normal_multiplier).max(1.0) as u64;

        // Heavy-tail spike: ~4% probability of a large outlier.
        // Models: runqueue lock contention, cache line storms, TLB shootdown
        // batches, and bursty multi-thread wakeup contention.
        // Spike magnitude follows Pareto distribution: floor * U^(-1/α)
        // with α=0.6, calibrated to match production LAVD:
        //   p99/p50 ≈ 9x, p999/p50 ≈ 47x
        let spike_roll = self.next_prng() % 1000;
        if spike_roll < 35 {
            // 3.5% chance of heavy-tail spike
            let u = (self.next_prng() % 900 + 100) as f64 / 1000.0; // uniform(0.1, 1.0)
            let alpha = 0.55_f64;
            let pareto_multiplier = u.powf(-1.0 / alpha); // 1.0 to ~4.6 at p99
            let spike = (floor_ns as f64 * pareto_multiplier) as u64;
            base + spike
        } else {
            base
        }
    }

    /// Compute tick jitter (added to next tick interval).
    pub fn tick_jitter(&mut self) -> i64 {
        if !self.noise.enabled || !self.noise.tick_jitter {
            return 0;
        }
        self.sample_normal_ns(self.noise.tick_jitter_stddev_ns)
    }

    /// Compute context switch overhead for the given stop reason.
    pub fn csw_overhead(&mut self, reason: LastStopReason) -> TimeNs {
        if !self.overhead.enabled {
            return 0;
        }
        let (enabled, base) = match reason {
            LastStopReason::Voluntary => {
                (self.overhead.voluntary_csw, self.overhead.voluntary_csw_ns)
            }
            LastStopReason::Involuntary => (
                self.overhead.involuntary_csw,
                self.overhead.involuntary_csw_ns,
            ),
        };
        if !enabled {
            return 0;
        }
        let jitter = if self.overhead.csw_jitter {
            self.sample_normal_ns(self.overhead.csw_jitter_stddev_ns)
        } else {
            0
        };
        // Clamp to [0, 2*base] to avoid negative overhead
        (base as i64 + jitter).clamp(0, 2 * base as i64) as TimeNs
    }

    /// Execute pending deferred dispatches, resolving `SCX_DSQ_LOCAL` to
    /// the given `local_cpu`.
    ///
    /// This mirrors the kernel's post-callback dispatch resolution:
    /// after `select_cpu`, `local_cpu` is the CPU that `select_cpu` returned;
    /// after `enqueue`, it is the task's assigned CPU.
    ///
    /// Returns the first CPU where a local DSQ dispatch was resolved (the
    /// engine should try to run on that CPU), or None. All queued dispatches
    /// are processed in FIFO order before returning.
    pub fn resolve_pending_dispatch(&mut self, local_cpu: CpuId) -> Option<CpuId> {
        let mut first_local_dispatch = None;

        while let Some(pd) = self.pending_dispatches.pop_front() {
            let local_dispatch = self.resolve_one_pending_dispatch(local_cpu, pd);
            if first_local_dispatch.is_none() {
                first_local_dispatch = local_dispatch;
            }
        }

        first_local_dispatch
    }

    fn resolve_one_pending_dispatch(
        &mut self,
        local_cpu: CpuId,
        pd: PendingDispatch,
    ) -> Option<CpuId> {
        // Task has been dispatched — no longer in BPF scheduler's queue.
        self.set_task_ops_state(pd.pid, OpsTaskState::None);

        let dsq = pd.dsq_id;
        if dsq.is_local() {
            self.cpus[local_cpu.0 as usize].local_dsq.push_back(pd.pid);
            debug!(pid = pd.pid.0, "resolved SCX_DSQ_LOCAL");
            Some(local_cpu)
        } else if dsq.is_local_on() {
            let target_cpu = dsq.local_on_cpu();

            // Validate that the task can run on the target CPU (kernel behavior).
            // The kernel validates in task_can_run_on_remote_rq() and rejects
            // dispatches to CPUs not in the task's cpumask.
            if let Some(reason) = self.check_local_on_dispatch(pd.pid, target_cpu) {
                // Record trace event for rejected dispatch
                let cpu = self.current_cpu;
                let local_t = self.cpus[cpu.0 as usize].local_clock;
                self.trace.record(
                    local_t,
                    cpu,
                    TraceKind::DispatchRejected {
                        pid: pd.pid,
                        target_cpu,
                        reason,
                    },
                );

                // Set BPF error (matches kernel's scx_bpf_error behavior)
                let msg = match reason {
                    DispatchRejectReason::CpumaskViolation => {
                        format!(
                            "SCX_DSQ_LOCAL_ON cannot dispatch task {} to CPU {} \
                             (not in cpumask)",
                            pd.pid.0, target_cpu.0
                        )
                    }
                    DispatchRejectReason::MigrationDisabled => {
                        format!(
                            "SCX_DSQ_LOCAL_ON cannot move migration disabled {} \
                             to CPU {}",
                            pd.pid.0, target_cpu.0
                        )
                    }
                };
                debug!(pid = pd.pid.0, target_cpu = target_cpu.0, %msg, "dispatch rejected");
                if self.bpf_error.is_none() {
                    self.bpf_error = Some(msg);
                }
                return None;
            }

            if (target_cpu.0 as usize) < self.cpus.len() {
                self.cpus[target_cpu.0 as usize].local_dsq.push_back(pd.pid);
            }
            Some(target_cpu)
        } else if let Some(vtime) = pd.vtime {
            self.dsqs.insert_vtime(dsq, pd.pid, vtime);
            // Sample DSQ length after vtime insert
            let local_t = self.cpus[local_cpu.0 as usize].local_clock;
            self.trace
                .sample_dsq_lengths(local_t, &self.dsqs, DsqSampleTrigger::Insert, Some(dsq));
            None
        } else {
            self.dsqs.insert_fifo(dsq, pd.pid);
            // Sample DSQ length after FIFO insert
            let local_t = self.cpus[local_cpu.0 as usize].local_clock;
            self.trace
                .sample_dsq_lengths(local_t, &self.dsqs, DsqSampleTrigger::Insert, Some(dsq));
            None
        }
    }

    /// Check if a dispatch to `SCX_DSQ_LOCAL_ON | target_cpu` is valid.
    ///
    /// Returns `Some(reason)` if the dispatch should be rejected, `None` if valid.
    /// Mirrors the kernel's `task_can_run_on_remote_rq()` validation.
    fn check_local_on_dispatch(&self, pid: Pid, target_cpu: CpuId) -> Option<DispatchRejectReason> {
        // Get the task's raw pointer to access cpumask
        let task_raw = self.task_pid_to_raw.get(&pid)?;
        let task_ptr = *task_raw as *mut c_void;

        // Check cpumask: task must be allowed to run on target CPU
        let cpus_ptr = unsafe { ffi::sim_task_get_cpus_ptr(task_ptr) };
        if !cpus_ptr.is_null() {
            let allowed = unsafe { ffi::bpf_cpumask_test_cpu(target_cpu.0, cpus_ptr) };
            if !allowed {
                return Some(DispatchRejectReason::CpumaskViolation);
            }
        }

        // Check migration_disabled: task with migration_disabled > 0 cannot be
        // dispatched to a different CPU than its last known CPU.
        let migration_disabled = unsafe { ffi::sim_task_get_migration_disabled(task_ptr) };
        if migration_disabled > 0 {
            // Migration-disabled task: must dispatch to its last known CPU.
            if let Some(&last_cpu) = self.task_last_cpu.get(&pid) {
                if target_cpu != last_cpu {
                    return Some(DispatchRejectReason::MigrationDisabled);
                }
            }
        }

        None
    }

    /// Compute a hash of scheduler-visible state for determinism checking.
    ///
    /// This hashes:
    /// - All DSQ contents (scheduler-created and builtin, in sorted order)
    /// - Per-CPU current task and local DSQ
    /// - Task ops state
    ///
    /// The hash is used to detect state divergence between runs.
    pub fn compute_state_hash(&self) -> u64 {
        use crate::preempt::{fnv1a_combine, fnv1a_hash_u64};

        let mut hash = fnv1a_hash_u64(0xdeadbeef0001u64);

        // Hash ALL DSQ contents in sorted order (not just GLOBAL).
        // Schedulers create custom DSQs (e.g., tickless uses SHARED_DSQ=0)
        // that must be included for complete state comparison.
        let dsq_ids = self.dsqs.sorted_dsq_ids();
        hash = fnv1a_combine(hash, fnv1a_hash_u64(dsq_ids.len() as u64));
        for dsq_id in dsq_ids {
            hash = fnv1a_combine(hash, fnv1a_hash_u64(dsq_id.0));
            let pids = self.dsqs.ordered_pids(dsq_id);
            hash = fnv1a_combine(hash, fnv1a_hash_u64(pids.len() as u64));
            for pid in pids {
                hash = fnv1a_combine(hash, fnv1a_hash_u64(pid.0 as u64));
            }
        }

        // Hash per-CPU state
        for (i, cpu) in self.cpus.iter().enumerate() {
            // CPU index
            hash = fnv1a_combine(hash, fnv1a_hash_u64(i as u64));
            // Current task
            let current = cpu.current_task.map(|p| p.0 as u64).unwrap_or(0);
            hash = fnv1a_combine(hash, fnv1a_hash_u64(current));
            // Local DSQ contents
            hash = fnv1a_combine(hash, fnv1a_hash_u64(cpu.local_dsq.len() as u64));
            for pid in &cpu.local_dsq {
                hash = fnv1a_combine(hash, fnv1a_hash_u64(pid.0 as u64));
            }
            // Online status
            hash = fnv1a_combine(hash, fnv1a_hash_u64(cpu.is_online as u64));
        }

        // Hash task ops state (BTreeMap iterates in sorted key order)
        hash = fnv1a_combine(hash, fnv1a_hash_u64(self.task_ops_state.len() as u64));
        for (pid, state) in &self.task_ops_state {
            hash = fnv1a_combine(hash, fnv1a_hash_u64(pid.0 as u64));
            hash = fnv1a_combine(hash, fnv1a_hash_u64(*state as u64));
        }

        hash
    }
}

// ---------------------------------------------------------------------------
// Thread-local simulator state access
// ---------------------------------------------------------------------------

/// Thread-local simulation context for the trace formatter.
///
/// Carries the current CPU's local clock, the active CPU ID (if any),
/// and the zero-padding width for CPU IDs in log output.
#[derive(Debug, Clone, Copy)]
pub struct SimContext {
    pub clock: TimeNs,
    pub cpu: Option<CpuId>,
    pub cpu_width: u8,
}

impl SimContext {
    const fn new() -> Self {
        Self {
            clock: 0,
            cpu: None,
            cpu_width: 1,
        }
    }
}

thread_local! {
    static SIM_CONTEXT: std::cell::Cell<SimContext> = const { std::cell::Cell::new(SimContext::new()) };
    /// Arc-based thread-local for kfuncs to access the shared SimState.
    static SIM_ARC: RefCell<Option<SimArc>> = const { RefCell::new(None) };
    /// Per-callback identity context saved/restored across yield points.
    /// Async-signal-safe: Cell<Copy> read/write.
    static CALLBACK_CTX: std::cell::Cell<Option<CallbackContext>> = const { std::cell::Cell::new(None) };
    /// Original callback context while a Phase-3 interleaved timer is running.
    static INTERLEAVED_TIMER_SAVED_CTX: std::cell::Cell<Option<CallbackContext>> = const { std::cell::Cell::new(None) };
    /// Engine-level SimArc stored so that `enter_sim` can install `SIM_ARC`
    /// for cgroup callbacks and concurrent worker threads.
    static ENGINE_SIM_ARC: RefCell<Option<SimArc>> = const { RefCell::new(None) };
}

/// Install a SimArc into the current thread's thread-local.
///
/// Called by the engine before entering scheduler C code so that kfuncs
/// (which reacquire the lock) can find the Arc.
pub(crate) fn install_sim_arc(arc: &SimArc) {
    SIM_ARC.with(|c| {
        *c.borrow_mut() = Some(Arc::clone(arc));
    });
}

/// Clear the SimArc from the current thread's thread-local.
///
/// Called by the engine after scheduler C code returns.
pub(crate) fn clear_sim_arc() {
    SIM_ARC.with(|c| {
        *c.borrow_mut() = None;
    });
}

/// Clone the SimArc from the thread-local (for passing to sub-modules).
///
/// Returns `None` if not inside a simulator context.
#[allow(dead_code)]
pub(crate) fn clone_sim_arc() -> Option<SimArc> {
    SIM_ARC.with(|c| c.borrow().clone())
}

/// Install per-callback identity context into the thread-local.
pub fn install_callback_ctx(ctx: CallbackContext) {
    CALLBACK_CTX.with(|c| c.set(Some(ctx)));
}

/// Clear per-callback identity context.
pub fn clear_callback_ctx() {
    CALLBACK_CTX.with(|c| c.set(None));
}

/// Read per-callback identity context (async-signal-safe).
pub fn get_callback_ctx() -> Option<CallbackContext> {
    CALLBACK_CTX.with(|c| c.get())
}

/// Set the engine-level SimArc for this thread.
///
/// Called once by the engine at simulation start. `enter_sim` will
/// automatically install this Arc into `SIM_ARC`, bridging the old
/// raw-pointer path with the new Arc path for cgroup callbacks and
/// concurrent worker threads.
#[allow(dead_code)]
pub(crate) fn set_engine_sim_arc(arc: &SimArc) {
    ENGINE_SIM_ARC.with(|c| {
        *c.borrow_mut() = Some(Arc::clone(arc));
    });
}

/// Clear the engine-level SimArc from this thread.
///
/// Called by the engine when simulation ends.
#[allow(dead_code)]
pub(crate) fn clear_engine_sim_arc() {
    ENGINE_SIM_ARC.with(|c| {
        *c.borrow_mut() = None;
    });
}

/// Get a clone of the engine-level SimArc (for worker threads).
#[allow(dead_code)]
pub(crate) fn get_engine_sim_arc() -> Option<SimArc> {
    ENGINE_SIM_ARC.with(|c| c.borrow().clone())
}

/// Prepare for a C scheduler callback by installing the SimArc in the
/// thread-local so kfuncs and cgroup callbacks can lock it.
/// Prepare for a C scheduler callback by installing the SimArc in the
/// thread-local so kfuncs and cgroup callbacks can lock it.
///
/// Sets `state.current_cpu` and syncs `SIM_CONTEXT` so the trace formatter
/// shows the correct CPU in the timestamp suffix. Also installs
/// `CALLBACK_CTX` so yield functions can save/restore identity without
/// raw pointer access.
///
/// Installs `SIM_ARC` from `ENGINE_SIM_ARC` so that `with_sim()` and
/// cgroup callbacks can lock the mutex to access state.
///
/// # Safety
/// The caller must ensure `state` remains valid and unaliased for the
/// duration between `enter_sim` and `exit_sim`.
pub unsafe fn enter_sim(state: &mut SimulatorState, cpu: CpuId) {
    state.current_cpu = cpu;
    set_sim_clock(state.cpus[cpu.0 as usize].local_clock, Some(cpu));
    install_callback_ctx(CallbackContext {
        current_cpu: state.current_cpu,
        ops_context: state.ops_context,
        waker_task_raw: state.waker_task_raw,
    });
    // Install SIM_ARC from ENGINE_SIM_ARC so kfuncs and cgroup callbacks
    // can lock the mutex.
    if let Some(arc) = get_engine_sim_arc() {
        install_sim_arc(&arc);
    }
}

/// Remove the simulator state pointer after ops callbacks complete.
///
/// Also pauses the preemption timer — the timer may still be armed from
/// the last `resume_timer()` inside `with_sim()`. We must disable it
/// before returning to Rust engine code to prevent PMU signals from
/// firing outside of C scheduler code.
///
/// Clears `ops_context` after pausing the timer so that any pending PMU
/// signal (delayed by hardware skid) still sees the correct callback
/// context. Without this, signals delivered after a manual
/// `ops_context = None` but before `pause_timer()` would record
/// `ops=none` instead of the true callback context.
///
/// **Not safe for concurrent paths** where another worker may already
/// hold the token after `finish()`. Use [`exit_sim_no_clear_ops`] in
/// those cases and clear `ops_context` manually before `finish()`.
pub fn exit_sim() {
    // Restore per-callback context from CALLBACK_CTX back to SimulatorState.
    // The yield functions may have saved/restored CALLBACK_CTX across token
    // passes, so we need to sync it back before clearing.
    if let Some(ctx) = get_callback_ctx() {
        if let Some(arc) = clone_sim_arc() {
            crate::preempt::inhibit_preemption();
            let mut guard = arc.lock().unwrap();
            guard.sim.current_cpu = ctx.current_cpu;
            guard.sim.ops_context = ctx.ops_context;
            guard.sim.waker_task_raw = ctx.waker_task_raw;
            drop(guard);
            crate::preempt::allow_preemption();
        }
    }
    clear_callback_ctx();
    crate::preempt::pause_timer();
    if let Some(arc) = clone_sim_arc() {
        crate::preempt::inhibit_preemption();
        let mut guard = arc.lock().unwrap();
        guard.sim.ops_context = OpsContext::None;
        drop(guard);
        crate::preempt::allow_preemption();
    }
    // Also clear per-thread TLS so structop boundary detection and the
    // signal handler see None between callbacks.
    crate::preempt::set_current_ops_context(OpsContext::None);
    clear_sim_arc();
}

/// Like [`exit_sim`] but does **not** clear `ops_context`.
///
/// Used in concurrent dispatch/batch paths where `ops_context` must be
/// cleared manually BEFORE `finish()` to avoid a race: after `finish()`,
/// another worker already holds the token and may have written
/// `ops_context = Dispatch` on the shared `SimulatorState`. Writing
/// `None` from the old worker would clobber the new worker's value,
/// causing the PMU signal handler to record `ops=none` instead of the
/// true callback context.
pub fn exit_sim_no_clear_ops() {
    // Sync CALLBACK_CTX back (same as exit_sim but without clearing ops_context).
    if let Some(ctx) = get_callback_ctx() {
        if let Some(arc) = clone_sim_arc() {
            crate::preempt::inhibit_preemption();
            let mut guard = arc.lock().unwrap();
            guard.sim.current_cpu = ctx.current_cpu;
            guard.sim.ops_context = ctx.ops_context;
            guard.sim.waker_task_raw = ctx.waker_task_raw;
            drop(guard);
            crate::preempt::allow_preemption();
        }
    }
    clear_callback_ctx();
    crate::preempt::pause_timer();
    clear_sim_arc();
}

/// Read the current CPU's local clock from the thread-local.
///
/// Returns the local clock set by the engine for the current event's CPU.
/// Used by the custom trace formatter to show simulated time.
pub fn sim_clock() -> TimeNs {
    SIM_CONTEXT.with(|c| c.get().clock)
}

/// Read the current CPU ID from the thread-local (if set).
pub fn sim_cpu() -> Option<CpuId> {
    SIM_CONTEXT.with(|c| c.get().cpu)
}

/// Get the current CPU for this worker thread.
///
/// Reads from TLS (`SIM_CONTEXT`), which is safe for concurrent access.
/// In token-ring mode, this returns the same value as `sim.current_cpu`.
/// In native concurrent mode, this is the ONLY safe way to read the
/// current CPU — `sim.current_cpu` is racy.
///
/// # Panics
/// Panics if called outside of an `enter_sim`/`exit_sim` scope (no CPU set).
pub fn current_cpu_from_tls() -> CpuId {
    SIM_CONTEXT.with(|c| {
        c.get()
            .cpu
            .expect("current_cpu_from_tls called outside of simulator context (no CPU set)")
    })
}

/// Read the CPU ID zero-padding width from the thread-local.
pub fn sim_cpu_width() -> u8 {
    SIM_CONTEXT.with(|c| c.get().cpu_width)
}

/// Update the thread-local simulation context. Called by the engine before
/// `info!`/`debug!` calls so the trace formatter has access.
pub fn set_sim_clock(local: TimeNs, cpu: Option<CpuId>) {
    SIM_CONTEXT.with(|c| {
        let mut ctx = c.get();
        ctx.clock = local;
        ctx.cpu = cpu;
        c.set(ctx);
    });
}

/// Set the CPU ID zero-padding width based on total CPU count.
/// Called once at simulation start.
pub fn set_sim_cpu_width(nr_cpus: u32) {
    let width = if nr_cpus == 0 {
        1
    } else {
        // number of digits needed
        ((nr_cpus as f64).log10().floor() as u8) + 1
    };
    SIM_CONTEXT.with(|c| {
        let mut ctx = c.get();
        ctx.cpu_width = width;
        c.set(ctx);
    });
}

/// Hook for clock-window throttling in native concurrent mode.
///
/// Called whenever a CPU's `local_clock` advances. In Phase 2, this
/// will check the window constraint and spin/yield if the calling CPU
/// has raced too far ahead of the slowest CPU.
#[inline(always)]
pub fn clock_window_check(_cpu: CpuId, _local_clock: TimeNs) {
    // Phase 2: enforce window constraint via NativeOrchestrator
}

/// Access the simulator state from within a kfunc.
///
/// Locks `SIM_ARC` to get exclusive access to `SimulatorState`.
///
/// After acquiring the lock, restores `sim.current_cpu` (and related
/// identity fields) from the thread-local `CALLBACK_CTX`. This is
/// necessary because cooperative interleaving (`maybe_yield()`) can
/// transfer the execution token to another worker, which overwrites
/// `sim.current_cpu` via its raw pointer.  Without this restore, kfuncs
/// that read `sim.current_cpu` after a yield (e.g. `dsq_move_to_local`)
/// would operate on the wrong CPU, causing tasks to be dispatched to
/// the wrong local DSQ and producing stalls.
///
/// # Panics
/// Panics if `SIM_ARC` is not installed.
fn with_sim<F, R>(cost_ns: u64, f: F) -> R
where
    F: FnOnce(&mut SimulatorState) -> R,
{
    let arc = SIM_ARC
        .with(|c| c.borrow().clone())
        .expect("kfunc called outside of simulator context (SIM_ARC not installed)");
    // Disable the RBC counter FIRST so that none of the Rust infrastructure
    // below (mutex lock, preemption management, context restore) contributes
    // branches to the PMU count. This is critical for determinism: mutex
    // contention (CAS vs futex slow path) adds a variable number of branches
    // that differ between runs. See sim-70abc8.
    disable_rbc_counter();
    // Inhibit preemption and disable the PMU timer BEFORE acquiring the
    // mutex. A PMU signal may already be queued in the kernel from a
    // counter overflow that occurred before we reach this point. Merely
    // disabling the counter via `pause_timer()` (ioctl) does not prevent
    // delivery of an already-queued signal — it can arrive on return from
    // the ioctl or any subsequent syscall (e.g., the futex inside
    // `arc.lock()`). If the handler runs while the mutex is held, it
    // parks this worker via `yield_token()`, and the woken worker blocks
    // on the same mutex, causing a deadlock.
    //
    // `inhibit_preemption()` sets a thread-local flag that the handler
    // checks; when set, the handler disables the timer and returns
    // without yielding. Combined with `pause_timer()`, this closes both
    // the "future overflow" and "pending signal" windows.
    crate::preempt::inhibit_preemption();
    crate::preempt::pause_timer();
    let result = {
        let mut guard = arc.lock().unwrap();
        let sim = &mut guard.sim;
        // Restore identity fields from thread-local context.
        // After a cooperative yield (`maybe_yield()`), another worker may
        // have overwritten `sim.current_cpu` via its raw pointer.  The
        // thread-local `CALLBACK_CTX` preserves each worker's true CPU
        // across yields.  Restoring here ensures every kfunc sees the
        // correct CPU for the calling worker.
        if let Some(ctx) = get_callback_ctx() {
            sim.current_cpu = ctx.current_cpu;
            sim.ops_context = ctx.ops_context;
            sim.waker_task_raw = ctx.waker_task_raw;
        }
        sim.rbc_kfunc_calls += 1;
        sim.rbc_kfunc_ns += cost_ns;
        // Track the longest unbroken RBC interval between kfuncs.
        // The counter was disabled at the top of with_sim(), so
        // reading it now returns the stopped value — the exact count
        // at the point we left C code.
        if sim.e9_fns.is_some() {
            let current = crate::preempt::e9_read_counter();
            // e9 counter decrements, so interval = last - current.
            let interval = (sim.rbc_e9_last_kfunc - current).max(0) as u64;
            if interval > sim.longest_rbc_interval {
                sim.longest_rbc_interval = interval;
            }
            sim.rbc_e9_last_kfunc = current;
        } else if let Some(ref rbc) = sim.rbc_counter {
            // PMU counter accumulates, so interval = current - last.
            if let Ok(current) = rbc.read() {
                let interval = current.saturating_sub(sim.rbc_pmu_last_kfunc);
                if interval > sim.longest_rbc_interval {
                    sim.longest_rbc_interval = interval;
                }
                sim.rbc_pmu_last_kfunc = current;
            }
        }
        rbc_pause_inner(sim);
        let result = f(sim);
        rbc_resume_inner(sim);
        result
    };
    // Guard dropped: SIM_ARC unlocked. Allow preemption before re-arming
    // the timer so the handler can yield at the next natural point.
    crate::preempt::allow_preemption();
    crate::preempt::resume_timer();
    crate::preempt::maybe_yield_preemptive_post();
    crate::preempt::set_current_kfunc("");
    // Re-enable the RBC counter LAST, right before returning to C code.
    // All Rust infrastructure above runs with the counter disabled.
    enable_rbc_counter();
    result
}

// ---------------------------------------------------------------------------
// C-callable RBC counter pause/resume for C kfunc stubs
// ---------------------------------------------------------------------------

// Nesting depth for RBC pause/resume. Only the outermost pause/resume
// pair actually disables/enables the PMU counter. This allows C kfuncs
// to call other C kfuncs (or Rust kfuncs via `with_sim`) without
// prematurely re-enabling the counter.
thread_local! {
    static RBC_PAUSE_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// Cached raw fd of the main-thread RBC counter (or -1 if none).
    ///
    /// Set by [`set_rbc_counter_fd`] during `start_rbc()`. Enables
    /// [`disable_rbc_counter`] / [`enable_rbc_counter`] to pause/resume
    /// the PMU counter without needing `&SimulatorState` (and hence
    /// without acquiring the `SIM_ARC` mutex). This is critical for
    /// keeping infrastructure branches (mutex lock/unlock, preemption
    /// management) out of the PMU measurement window.
    static RBC_COUNTER_FD: std::cell::Cell<RawFd> = const { std::cell::Cell::new(-1) };
}

/// Cache the main-thread RBC counter fd in TLS for lock-free access.
///
/// Called by `start_rbc()` in engine.rs when the counter is created/reset.
/// Pass -1 to clear (e.g., when the counter is taken out during concurrent
/// batch processing).
pub fn set_rbc_counter_fd(fd: RawFd) {
    RBC_COUNTER_FD.with(|c| c.set(fd));
}

/// Set the RBC pause depth directly.
///
/// Called by `start_rbc()` to initialize the depth to 1 (counter disabled).
/// `sim_callback!`'s `enable_rbc_counter()` will transition depth 1→0
/// to enable the counter right before C code runs.
pub fn set_rbc_pause_depth(depth: u32) {
    RBC_PAUSE_DEPTH.with(|d| d.set(depth));
}

/// Disable the main-thread RBC counter (nesting-safe, lock-free).
///
/// Increments the pause depth. At the outermost level (depth 0 → 1),
/// disables the PMU counter via ioctl and pauses the per-worker
/// measurement counter. Complete no-op if no counter is cached (fd < 0),
/// i.e. when called from `sim_callback!` for ops callbacks that don't
/// call `start_rbc()` (init-time callbacks without RBC measurement).
///
/// This is the primary mechanism for excluding Rust infrastructure
/// branches from the PMU measurement. Call before mutex operations,
/// preemption management, or any other Rust code that should not be
/// counted as scheduler overhead.
pub fn disable_rbc_counter() {
    // Fast path: no counter active — skip depth tracking entirely.
    // This avoids polluting the depth counter when sim_callback! is used
    // without start_rbc() (e.g. update_idle during init).
    if RBC_COUNTER_FD.with(|c| c.get()) < 0 {
        return;
    }
    let depth = RBC_PAUSE_DEPTH.with(|d| {
        let cur = d.get();
        d.set(cur + 1);
        cur
    });
    if depth == 0 {
        let fd = RBC_COUNTER_FD.with(|c| c.get());
        // SAFETY: fd is a valid perf_event fd cached from RbcCounter.
        // PERF_EVENT_IOC_DISABLE is a valid ioctl for perf_event fds.
        unsafe {
            libc::ioctl(fd, scx_perf::PERF_IOC_DISABLE, 0 as libc::c_ulong);
        }
        crate::preempt::pause_measurement();
    }
}

/// Enable the main-thread RBC counter (nesting-safe, lock-free).
///
/// Decrements the pause depth. At the outermost level (depth 1 → 0),
/// enables the PMU counter via ioctl and resumes the per-worker
/// measurement counter. Complete no-op if no counter is cached (fd < 0).
///
/// Call after Rust infrastructure completes, right before returning
/// to C scheduler code.
pub fn enable_rbc_counter() {
    // Fast path: no counter active — skip depth tracking entirely.
    if RBC_COUNTER_FD.with(|c| c.get()) < 0 {
        return;
    }
    let depth = RBC_PAUSE_DEPTH.with(|d| {
        let cur = d.get();
        debug_assert!(
            cur > 0,
            "enable_rbc_counter without matching disable_rbc_counter"
        );
        d.set(cur - 1);
        cur - 1
    });
    if depth == 0 {
        let fd = RBC_COUNTER_FD.with(|c| c.get());
        // SAFETY: fd is a valid perf_event fd cached from RbcCounter.
        // PERF_EVENT_IOC_ENABLE is a valid ioctl for perf_event fds.
        unsafe {
            libc::ioctl(fd, scx_perf::PERF_IOC_ENABLE, 0 as libc::c_ulong);
        }
        crate::preempt::resume_measurement();
    }
}

/// Inner RBC pause logic. Called with `&SimulatorState` already available.
///
/// Delegates to [`disable_rbc_counter`] which uses the cached fd instead
/// of `sim.rbc_counter` — this avoids requiring the lock to pause/resume
/// the counter around infrastructure code.
fn rbc_pause_inner(_sim: &SimulatorState) {
    disable_rbc_counter();
}

/// Inner RBC resume logic. Called with `&SimulatorState` already available.
///
/// Delegates to [`enable_rbc_counter`] which uses the cached fd instead
/// of `sim.rbc_counter`.
fn rbc_resume_inner(_sim: &SimulatorState) {
    enable_rbc_counter();
}

/// Pause the PMU RBC counter from C code.
///
/// Disables the counter immediately (via cached fd) so that the Rust
/// infrastructure below (preemption inhibit, mutex try_lock) is NOT
/// counted. Uses nesting-safe [`disable_rbc_counter`] so nested
/// calls from within `with_sim()` are handled correctly.
///
/// Inhibits preemption around the `try_lock()` to prevent the PMU signal
/// handler from yielding while the mutex is briefly held. Without this, a
/// stale queued signal can park the worker (via `yield_token()`) while the
/// mutex guard is live, causing the woken worker to block on the same mutex.
#[no_mangle]
pub extern "C" fn sim_rbc_pause() {
    // Disable the counter FIRST so infrastructure branches are not counted.
    disable_rbc_counter();
    // Preemption management is now outside the measurement window.
    if SIM_ARC.with(|c| c.borrow().is_some()) {
        crate::preempt::inhibit_preemption();
        crate::preempt::allow_preemption();
    }
}

/// Resume the PMU RBC counter from C code.
///
/// Re-enables the counter (via cached fd) AFTER all Rust infrastructure
/// completes, right before returning to C code. Uses nesting-safe
/// [`enable_rbc_counter`] so nested calls from within `with_sim()` are
/// handled correctly.
///
/// Inhibits preemption around the operation for the same reason as
/// [`sim_rbc_pause`] — prevents signal-handler deadlock.
#[no_mangle]
pub extern "C" fn sim_rbc_resume() {
    // Preemption management runs with the counter still disabled.
    if SIM_ARC.with(|c| c.borrow().is_some()) {
        crate::preempt::inhibit_preemption();
        crate::preempt::allow_preemption();
    }
    // Re-enable the counter LAST, right before returning to C code.
    enable_rbc_counter();
}

// ---------------------------------------------------------------------------
// SCX kfunc implementations
// ---------------------------------------------------------------------------

macro_rules! define_cgroup_bw_yield {
    ($fn_name:ident, $site_name:literal) => {
        #[no_mangle]
        pub extern "C" fn $fn_name() {
            crate::preempt::set_current_kfunc($site_name);
            crate::interleave::maybe_yield();
        }
    };
}

define_cgroup_bw_yield!(scxsim_cgroup_bw_yield_lib_init, "cgroup_bw_lib_init");
define_cgroup_bw_yield!(scxsim_cgroup_bw_yield_init, "cgroup_bw_init");
define_cgroup_bw_yield!(scxsim_cgroup_bw_yield_exit, "cgroup_bw_exit");
define_cgroup_bw_yield!(scxsim_cgroup_bw_yield_set, "cgroup_bw_set");
define_cgroup_bw_yield!(scxsim_cgroup_bw_yield_throttled, "cgroup_bw_throttled");
define_cgroup_bw_yield!(scxsim_cgroup_bw_yield_consume, "cgroup_bw_consume");
define_cgroup_bw_yield!(scxsim_cgroup_bw_yield_put_aside, "cgroup_bw_put_aside");
define_cgroup_bw_yield!(scxsim_cgroup_bw_yield_reenqueue, "cgroup_bw_reenqueue");
define_cgroup_bw_yield!(scxsim_cgroup_bw_yield_cancel, "cgroup_bw_cancel");
define_cgroup_bw_yield!(
    scxsim_cgroup_bw_yield_is_cgroup_throttled,
    "cgroup_bw_is_cgroup_throttled"
);
define_cgroup_bw_yield!(
    scxsim_cgroup_bw_yield_is_task_throttled,
    "cgroup_bw_is_task_throttled"
);
define_cgroup_bw_yield!(scxsim_cgroup_bw_yield_move, "cgroup_bw_move");
define_cgroup_bw_yield!(scxsim_cgroup_bw_yield_dump, "cgroup_bw_dump");

/// V2 observer for `LavdBailOnCgroupThrottle`. Called from
/// wrapper.c's `scx_cgroup_bw_put_aside` macro AFTER the lib call
/// returns 0 (i.e. successful put-aside). Records a trace event so
/// downstream tools can count LAVD-side bail events that V1's
/// engine-side admission gate doesn't observe.
///
/// Safe to call without a SIM_ARC: silently no-ops if there is no
/// active simulation (matches the pattern of the yield hooks above).
///
/// tg `scxsim-eager-throttle-v2-track-lavd-bail-path`.
#[no_mangle]
pub extern "C" fn scxsim_cgroup_bw_observe_put_aside(pid: i32, cgid: u64) {
    if !SIM_ARC.with(|c| c.borrow().is_some()) {
        return;
    }
    use crate::trace::TraceKind;
    use crate::types::{CpuId, Pid};
    let arc = match SIM_ARC.with(|c| c.borrow().clone()) {
        Some(a) => a,
        None => return,
    };
    let cpu = current_cpu_from_tls();
    let mut guard = arc.lock().unwrap();
    let s = &mut *guard;
    let local_t = s.sim.cpus[cpu.0 as usize].local_clock;
    s.sim.trace.record(
        local_t,
        cpu,
        TraceKind::LavdBailOnCgroupThrottle {
            pid: Pid(pid),
            cgid: crate::cgroup::CgroupId(cgid),
        },
    );
    // Maintain bw_blocked tracking for parity with V1's engine-side
    // bw_blocked map. V2 does NOT push redundant TaskWake events on
    // replenish — the lib already drives reenqueue via cbw_drain_btq_batch
    // → scx_cgroup_bw_enqueue_cb → lavd_enqueue_cb → enqueue_cb →
    // scx_bpf_dsq_insert_vtime. Adding an engine-side wake here would
    // duplicate the lib's drain (a fake-approximation violation per
    // scx-sim/CLAUDE.md "Don't Model the Scheduler — Model the Kernel").
    s.sim
        .bw_blocked
        .entry(crate::cgroup::CgroupId(cgid))
        .or_default()
        .push_back(Pid(pid));
    let _ = CpuId(0);
}

/// V2 observer for `LavdReenqueueViaBtqDrain`. Called from wrapper.c's
/// `scx_cgroup_bw_reenqueue` macro AFTER the lib's drain runs.
/// Records a trace event for each cgroup whose BTQ was drained.
///
/// Per-cgroup (not per-task) because the lib's drain pops up to
/// CBW_REENQ_MAX_BATCH tasks in one call and we don't have visibility
/// into per-task pop events without instrumenting deeper.
///
/// tg `scxsim-eager-throttle-v2-track-lavd-bail-path`.
#[no_mangle]
pub extern "C" fn scxsim_cgroup_bw_observe_reenqueue(cgid: u64) {
    if !SIM_ARC.with(|c| c.borrow().is_some()) {
        return;
    }
    use crate::trace::TraceKind;
    let arc = match SIM_ARC.with(|c| c.borrow().clone()) {
        Some(a) => a,
        None => return,
    };
    let cpu = current_cpu_from_tls();
    let mut guard = arc.lock().unwrap();
    let s = &mut *guard;
    let local_t = s.sim.cpus[cpu.0 as usize].local_clock;
    s.sim.trace.record(
        local_t,
        cpu,
        TraceKind::LavdReenqueueViaBtqDrain {
            cgid: crate::cgroup::CgroupId(cgid),
        },
    );
    // Drain the engine-side bw_blocked tracking for this cgroup. The
    // lib's reenqueue is per-cgroup (not per-task), so we drain the
    // entire vec for this cgid. The `bw_blocked` map is now purely an
    // observability counter — no wake injected.
    s.sim.bw_blocked.remove(&crate::cgroup::CgroupId(cgid));
}

/// V4-A observer for `CgroupBwConsumeNs`. Called from wrapper.c's
/// `scx_cgroup_bw_consume(c, n)` macro AFTER the lib call returns,
/// recording the `n` argument the engine charged. The post-stall sum
/// of `ns` per period distinguishes:
///   - sum/period ≈ period_ns → ENGINE BUG (over-charging idle cgroup)
///   - sum/period ≈ 0         → LIB BUG (lib's idealized accounting timer)
///
/// Safe: no-op if no active SIM_ARC.
///
/// tg `scxsim-disambiguate-runtime-overcharge-vs-lib-idealized-accounting`.
#[no_mangle]
pub extern "C" fn scxsim_cgroup_bw_observe_consume(cgid: u64, ns: u64) {
    if !SIM_ARC.with(|c| c.borrow().is_some()) {
        return;
    }
    use crate::trace::TraceKind;
    let arc = match SIM_ARC.with(|c| c.borrow().clone()) {
        Some(a) => a,
        None => return,
    };
    let cpu = current_cpu_from_tls();
    let mut guard = arc.lock().unwrap();
    let s = &mut *guard;
    let local_t = s.sim.cpus[cpu.0 as usize].local_clock;
    s.sim.trace.record(
        local_t,
        cpu,
        TraceKind::CgroupBwConsumeNs {
            cgid: crate::cgroup::CgroupId(cgid),
            ns,
        },
    );
}

const CBW_YIELD_AFTER_MIN_TTT_SAMPLE: u32 = 1;

fn cbw_yield_site_name(site: u32) -> &'static str {
    match site {
        CBW_YIELD_AFTER_MIN_TTT_SAMPLE => "cbw_after_min_ttt_sample",
        2 => "cbw_after_top_half_begin",
        3 => "cbw_after_top_half_end_publish",
        _ => "cbw_unknown_yield_site",
    }
}

#[no_mangle]
pub extern "C" fn scxsim_cbw_yield_site(
    site: u32,
    cgid: u64,
    remaining: i64,
    time_to_throttle: u64,
    min_time_to_throttle: u64,
) {
    let site_name = cbw_yield_site_name(site);
    crate::preempt::set_current_kfunc(site_name);
    if std::env::var_os("SCXSIM_DEBUG_CBW_YIELD").is_some() {
        eprintln!(
            "[SCXSIM-CBW-YIELD] site={site_name} cgid={cgid} remaining={remaining} \
             time_to_throttle={time_to_throttle} min_time_to_throttle={min_time_to_throttle}"
        );
    }
    crate::interleave::maybe_yield();
}

#[no_mangle]
pub extern "C" fn scxsim_cgroup_bw_begin_targeted_timer(
    site: u32,
    cgid: u64,
    remaining: i64,
    time_to_throttle: u64,
    min_time_to_throttle: u64,
    slot_out: *mut u32,
) -> i32 {
    if slot_out.is_null() || site != CBW_YIELD_AFTER_MIN_TTT_SAMPLE {
        return 0;
    }
    if INTERLEAVED_TIMER_SAVED_CTX.with(|c| c.get().is_some()) {
        return 0;
    }
    let Some(saved_ctx) = get_callback_ctx() else {
        return 0;
    };
    let Some(arc) = SIM_ARC.with(|c| c.borrow().clone()) else {
        return 0;
    };

    disable_rbc_counter();
    crate::preempt::inhibit_preemption();
    crate::preempt::pause_timer();
    let selected = {
        let mut guard = arc.lock().unwrap();
        let fields = guard.fields();
        if !fields.sim.targeted_cbw_yield_sites
            || fields.sim.targeted_cbw_yield_count >= fields.sim.targeted_cbw_yield_limit
        {
            None
        } else {
            let cpu = saved_ctx.current_cpu;
            let now = fields.sim.cpus[cpu.0 as usize]
                .local_clock
                .max(fields.sim.clock);
            let horizon = now.saturating_add(fields.sim.targeted_cbw_yield_window_ns);
            let selected = fields.events.pop_targeted_timer_interleave(horizon);
            if selected.is_some() {
                fields.sim.targeted_cbw_yield_count += 1;
            }
            selected
        }
    };

    let Some(event) = selected else {
        crate::preempt::allow_preemption();
        crate::preempt::resume_timer();
        enable_rbc_counter();
        return 0;
    };
    let EventKind::TimerFired { cpu, slot } = event.kind else {
        crate::preempt::allow_preemption();
        crate::preempt::resume_timer();
        enable_rbc_counter();
        return 0;
    };

    if std::env::var_os("SCXSIM_DEBUG_CBW_YIELD").is_some() {
        eprintln!(
            "[SCXSIM-CBW-YIELD] forcing timer slot={slot} cpu={} at site={} \
             cgid={cgid} remaining={remaining} time_to_throttle={time_to_throttle} \
             min_time_to_throttle={min_time_to_throttle}",
            cpu.0,
            cbw_yield_site_name(site)
        );
    }

    {
        let mut guard = arc.lock().unwrap();
        let sim = &mut guard.sim;
        sim.current_cpu = cpu;
        sim.ops_context = OpsContext::FireTimer;
        sim.cpus[cpu.0 as usize].local_clock =
            sim.cpus[cpu.0 as usize].local_clock.max(event.time_ns);
        set_sim_clock(sim.cpus[cpu.0 as usize].local_clock, Some(cpu));
    }

    INTERLEAVED_TIMER_SAVED_CTX.with(|c| c.set(Some(saved_ctx)));
    install_callback_ctx(CallbackContext {
        current_cpu: cpu,
        ops_context: OpsContext::FireTimer,
        waker_task_raw: None,
    });
    unsafe {
        *slot_out = slot as u32;
    }
    crate::preempt::allow_preemption();
    crate::preempt::resume_timer();
    enable_rbc_counter();
    1
}

#[no_mangle]
pub extern "C" fn scxsim_cgroup_bw_begin_interleaved_timer(slot_out: *mut u32) -> i32 {
    if slot_out.is_null() {
        return 0;
    }
    if INTERLEAVED_TIMER_SAVED_CTX.with(|c| c.get().is_some()) {
        return 0;
    }
    let Some(arc) = SIM_ARC.with(|c| c.borrow().clone()) else {
        return 0;
    };
    let Some(saved_ctx) = get_callback_ctx() else {
        return 0;
    };

    disable_rbc_counter();
    crate::preempt::inhibit_preemption();
    crate::preempt::pause_timer();
    let selected = {
        let mut guard = arc.lock().unwrap();
        let fields = guard.fields();
        if !fields.sim.stochastic_timer_interleave {
            None
        } else {
            let cpu = saved_ctx.current_cpu;
            let now = fields.sim.cpus[cpu.0 as usize]
                .local_clock
                .max(fields.sim.clock);
            let horizon = now.saturating_add(fields.sim.stochastic_timer_interleave_window_ns);
            fields.events.pop_stochastic_timer_interleave(
                horizon,
                fields.sim.stochastic_timer_interleave_one_in,
            )
        }
    };

    let Some(event) = selected else {
        crate::preempt::allow_preemption();
        crate::preempt::resume_timer();
        enable_rbc_counter();
        return 0;
    };
    let EventKind::TimerFired { cpu, slot } = event.kind else {
        crate::preempt::allow_preemption();
        crate::preempt::resume_timer();
        enable_rbc_counter();
        return 0;
    };

    {
        let mut guard = arc.lock().unwrap();
        let sim = &mut guard.sim;
        sim.current_cpu = cpu;
        sim.ops_context = OpsContext::FireTimer;
        sim.cpus[cpu.0 as usize].local_clock =
            sim.cpus[cpu.0 as usize].local_clock.max(event.time_ns);
        set_sim_clock(sim.cpus[cpu.0 as usize].local_clock, Some(cpu));
    }

    INTERLEAVED_TIMER_SAVED_CTX.with(|c| c.set(Some(saved_ctx)));
    install_callback_ctx(CallbackContext {
        current_cpu: cpu,
        ops_context: OpsContext::FireTimer,
        waker_task_raw: None,
    });
    unsafe {
        *slot_out = slot as u32;
    }
    crate::preempt::allow_preemption();
    crate::preempt::resume_timer();
    enable_rbc_counter();
    1
}

#[no_mangle]
pub extern "C" fn scxsim_cgroup_bw_end_interleaved_timer() {
    let saved_ctx = INTERLEAVED_TIMER_SAVED_CTX.with(|c| {
        let saved = c.get();
        c.set(None);
        saved
    });
    let Some(saved_ctx) = saved_ctx else {
        return;
    };
    let Some(arc) = SIM_ARC.with(|c| c.borrow().clone()) else {
        install_callback_ctx(saved_ctx);
        return;
    };

    disable_rbc_counter();
    crate::preempt::inhibit_preemption();
    crate::preempt::pause_timer();
    {
        let mut guard = arc.lock().unwrap();
        let fields = guard.fields();
        let mut drained: [Option<(TimeNs, CpuId)>; MAX_BPF_TIMERS] = [None; MAX_BPF_TIMERS];
        for (slot, entry) in fields.sim.pending_timers.iter_mut().enumerate() {
            drained[slot] = entry.take();
        }
        for (slot, entry) in drained.iter().enumerate() {
            if let Some((fire_at, cpu)) = *entry {
                fields.events.push(
                    fire_at,
                    EventKind::TimerFired {
                        cpu,
                        slot: slot as u8,
                    },
                );
            }
        }
        flush_staged_events(fields.sim, fields.events);
        fields.sim.current_cpu = saved_ctx.current_cpu;
        fields.sim.ops_context = saved_ctx.ops_context;
        fields.sim.waker_task_raw = saved_ctx.waker_task_raw;
        let local = fields.sim.cpus[saved_ctx.current_cpu.0 as usize].local_clock;
        set_sim_clock(local, Some(saved_ctx.current_cpu));
    }

    install_callback_ctx(saved_ctx);
    crate::preempt::allow_preemption();
    crate::preempt::resume_timer();
    enable_rbc_counter();
}

/// Create a dispatch queue.
#[no_mangle]
pub extern "C" fn scx_bpf_create_dsq(dsq_id: u64, node: i32) -> i32 {
    with_sim(kfunc_cost::SIMPLE, |sim| {
        let result = if sim.dsqs.create(DsqId(dsq_id)) {
            0
        } else {
            -1
        };
        debug!(dsq_id, result, "enter:kfunc create_dsq");
        // TOP-9 of secondary TraceKind easy-win bundle (tg
        // `bundle-implement-secondary-tracekind-easy-wins`): emit
        // helper/create_dsq entry+exit so the live-vs-sim diff
        // harness can confirm sim and live agree on the per-cgroup
        // DSQ topology that LAVD's cgroup_init builds.
        let cpu = sim.current_cpu;
        let local_t = sim.cpus[cpu.0 as usize].local_clock;
        sim.trace.record(
            local_t,
            cpu,
            TraceKind::CreateDsq {
                dsq_id: DsqId(dsq_id),
                node,
                rc: result,
            },
        );
        result
    })
}

/// Default CPU selection: find an idle CPU, preferring prev_cpu.
///
/// Respects the task's cpumask (`p->cpus_ptr`): only CPUs allowed by the
/// task's affinity mask are considered, matching the kernel's
/// `scx_select_cpu_dfl` behavior.
#[no_mangle]
pub extern "C" fn scx_bpf_select_cpu_dfl(
    p: *mut c_void,
    prev_cpu: i32,
    _wake_flags: u64,
    is_idle: *mut bool,
) -> i32 {
    crate::preempt::set_current_kfunc("select_cpu_dfl");
    crate::interleave::maybe_yield();
    with_sim(kfunc_cost::COMPLEX, |sim| {
        let cpus_ptr = unsafe { ffi::sim_task_get_cpus_ptr(p) };
        let allowed = |cpu: u32| -> bool {
            cpus_ptr.is_null() || unsafe { ffi::bpf_cpumask_test_cpu(cpu, cpus_ptr) }
        };

        let prev = CpuId(prev_cpu as u32);
        // Prefer prev_cpu if it's idle and allowed
        if (prev.0 as usize) < sim.cpus.len() && sim.cpu_is_idle(prev) && allowed(prev.0) {
            unsafe { *is_idle = true };
            debug!(
                prev_cpu,
                cpu = prev_cpu,
                idle = true,
                "enter:kfunc select_cpu_dfl"
            );
            return prev_cpu;
        }
        // Find any idle CPU that is allowed
        if let Some(cpu) = sim
            .cpus
            .iter()
            .find(|c| c.is_idle() && c.local_dsq.is_empty() && allowed(c.id.0))
        {
            let cpu_id = cpu.id;
            unsafe { *is_idle = true };
            debug!(
                prev_cpu,
                cpu = cpu_id.0,
                idle = true,
                "enter:kfunc select_cpu_dfl"
            );
            return cpu_id.0 as i32;
        }
        unsafe { *is_idle = false };
        // Fall back to prev_cpu if allowed, otherwise first allowed CPU
        let fallback = if allowed(prev.0) {
            prev_cpu
        } else {
            (0..sim.cpus.len() as u32)
                .find(|&c| allowed(c))
                .map_or(prev_cpu, |c| c as i32)
        };
        debug!(
            prev_cpu,
            cpu = fallback,
            idle = false,
            "enter:kfunc select_cpu_dfl"
        );
        fallback
    })
}

/// Extended CPU selection with cpumask constraint and SCX_PICK_IDLE_CORE.
///
/// Returns an idle CPU (>= 0) matching both the task's cpus_ptr and
/// `cpus_allowed`, or negative on failure.
#[no_mangle]
pub extern "C" fn scx_bpf_select_cpu_and(
    p: *mut c_void,
    prev_cpu: i32,
    _wake_flags: u64,
    cpus_allowed: *const c_void,
    flags: u64,
) -> i32 {
    const SCX_PICK_IDLE_CORE: u64 = 1;

    crate::preempt::set_current_kfunc("select_cpu_and");
    crate::interleave::maybe_yield();
    with_sim(kfunc_cost::COMPLEX, |sim| {
        let nr_cpus = sim.cpus.len();
        let prev = CpuId(prev_cpu as u32);
        let want_idle_core = flags & SCX_PICK_IDLE_CORE != 0;
        let cpus_ptr = unsafe { ffi::sim_task_get_cpus_ptr(p) };

        let in_mask = |cpu: u32| -> bool {
            let task_ok = cpus_ptr.is_null() || unsafe { ffi::bpf_cpumask_test_cpu(cpu, cpus_ptr) };
            let caller_ok =
                cpus_allowed.is_null() || unsafe { ffi::bpf_cpumask_test_cpu(cpu, cpus_allowed) };
            task_ok && caller_ok
        };

        let has_idle_core = |cpu: CpuId| -> bool {
            let siblings = &sim.cpus[cpu.0 as usize].siblings;
            if siblings.is_empty() {
                return true; // no SMT — single-thread core
            }
            siblings.iter().all(|&sib| sim.cpu_is_idle(sib))
        };

        // Prefer prev_cpu if it meets all constraints
        if (prev.0 as usize) < nr_cpus
            && sim.cpu_is_idle(prev)
            && in_mask(prev.0)
            && (!want_idle_core || has_idle_core(prev))
        {
            debug!(
                prev_cpu,
                cpu = prev_cpu,
                "enter:kfunc select_cpu_and (prev idle)"
            );
            return prev_cpu;
        }

        // Scan for any idle CPU matching constraints
        for i in 0..nr_cpus {
            let cpu = CpuId(i as u32);
            if cpu == prev {
                continue;
            }
            if sim.cpu_is_idle(cpu) && in_mask(i as u32) && (!want_idle_core || has_idle_core(cpu))
            {
                debug!(prev_cpu, cpu = i, "enter:kfunc select_cpu_and (found idle)");
                return i as i32;
            }
        }

        debug!(prev_cpu, "enter:kfunc select_cpu_and (no idle)");
        -1 // EBUSY
    })
}

/// compat.bpf.h falls back to this weak symbol when CO-RE type detection
/// is unavailable. Export the compat alias so scheduler wrappers resolve it
/// to the simulator host binary instead of a NULL weak symbol.
#[no_mangle]
pub extern "C" fn scx_bpf_select_cpu_and___compat(
    p: *mut c_void,
    prev_cpu: i32,
    wake_flags: u64,
    cpus_allowed: *const c_void,
    flags: u64,
) -> i32 {
    scx_bpf_select_cpu_and(p, prev_cpu, wake_flags, cpus_allowed, flags)
}

/// Insert a task into a DSQ (FIFO ordering).
///
/// Like the kernel, this records intent but does NOT insert immediately.
/// The engine resolves the pending dispatch after the callback returns,
/// at which point `SCX_DSQ_LOCAL` is mapped to the correct CPU.
#[no_mangle]
pub extern "C" fn scx_bpf_dsq_insert(p: *mut c_void, dsq_id: u64, slice: u64, enq_flags: u64) {
    crate::preempt::set_current_kfunc("dsq_insert");
    crate::interleave::maybe_yield();
    with_sim(kfunc_cost::MODERATE, |sim| {
        let pid = sim.task_pid_from_raw(p);
        unsafe { ffi::sim_task_set_slice(p, slice) };
        debug!(pid = pid.0, dsq_id, slice = %FmtN(slice), "enter:kfunc dsq_insert");

        sim.pending_dispatches.push_back(PendingDispatch {
            pid,
            dsq_id: DsqId(dsq_id),
            enq_flags,
            vtime: None,
        });

        let cpu = sim.current_cpu;
        let local_t = sim.cpus[cpu.0 as usize].local_clock;
        sim.trace.record(
            local_t,
            cpu,
            TraceKind::DsqInsert {
                pid,
                dsq_id: DsqId(dsq_id),
                slice,
            },
        );
    })
}

/// compat.bpf.h may inline through legacy/new symbol names before landing on
/// the simulator's direct `scx_bpf_dsq_insert()` export. Provide the compat
/// ABI entry points too so userspace schedulers never end up calling a NULL
/// weak symbol when the header fallback path is compiled in.
#[no_mangle]
pub extern "C" fn scx_bpf_dsq_insert___v2___compat(
    p: *mut c_void,
    dsq_id: u64,
    slice: u64,
    enq_flags: u64,
) -> bool {
    scx_bpf_dsq_insert(p, dsq_id, slice, enq_flags);
    true
}

#[no_mangle]
pub extern "C" fn scx_bpf_dsq_insert___v1(p: *mut c_void, dsq_id: u64, slice: u64, enq_flags: u64) {
    scx_bpf_dsq_insert(p, dsq_id, slice, enq_flags);
}

#[no_mangle]
pub extern "C" fn scx_bpf_dispatch___compat(
    p: *mut c_void,
    dsq_id: u64,
    slice: u64,
    enq_flags: u64,
) {
    scx_bpf_dsq_insert(p, dsq_id, slice, enq_flags);
}

#[repr(C)]
pub struct ScxBpfDsqInsertVtimeArgs {
    dsq_id: u64,
    slice: u64,
    vtime: u64,
    enq_flags: u64,
}
/// Insert a task into a DSQ with vtime ordering.
///
/// Deferred like `scx_bpf_dsq_insert` — see its doc comment.
///
/// # Panics
/// Panics if `dsq_id` is a built-in DSQ (LOCAL, GLOBAL), matching the
/// kernel's `dispatch_enqueue` which calls `scx_error()` for this case.
#[no_mangle]
pub extern "C" fn scx_bpf_dsq_insert_vtime(
    p: *mut c_void,
    dsq_id: u64,
    slice: u64,
    vtime: u64,
    enq_flags: u64,
) {
    crate::preempt::set_current_kfunc("dsq_insert_vtime");
    crate::interleave::maybe_yield();
    let dsq = DsqId(dsq_id);
    assert!(
        !dsq.is_builtin(),
        "cannot use vtime ordering for built-in DSQ {:#x}",
        dsq_id
    );
    with_sim(kfunc_cost::MODERATE, |sim| {
        let pid = sim.task_pid_from_raw(p);
        unsafe { ffi::sim_task_set_slice(p, slice) };
        debug!(pid = pid.0, dsq_id, slice = %FmtN(slice), vtime = %Vtime(vtime), "enter:kfunc dsq_insert_vtime");

        sim.pending_dispatches.push_back(PendingDispatch {
            pid,
            dsq_id: DsqId(dsq_id),
            enq_flags,
            vtime: Some(Vtime(vtime)),
        });

        let cpu = sim.current_cpu;
        let local_t = sim.cpus[cpu.0 as usize].local_clock;
        sim.trace.record(
            local_t,
            cpu,
            TraceKind::DsqInsertVtime {
                pid,
                dsq_id: DsqId(dsq_id),
                slice,
                vtime: Vtime(vtime),
            },
        );
    })
}

#[no_mangle]
pub extern "C" fn __scx_bpf_dsq_insert_vtime(
    p: *mut c_void,
    args: *const ScxBpfDsqInsertVtimeArgs,
) -> bool {
    let args = unsafe { args.as_ref() }.expect("__scx_bpf_dsq_insert_vtime called with null args");
    scx_bpf_dsq_insert_vtime(p, args.dsq_id, args.slice, args.vtime, args.enq_flags);
    true
}

#[no_mangle]
pub extern "C" fn scx_bpf_dsq_insert_vtime___compat(
    p: *mut c_void,
    dsq_id: u64,
    slice: u64,
    vtime: u64,
    enq_flags: u64,
) {
    scx_bpf_dsq_insert_vtime(p, dsq_id, slice, vtime, enq_flags);
}

#[no_mangle]
pub extern "C" fn scx_bpf_dispatch_vtime___compat(
    p: *mut c_void,
    dsq_id: u64,
    slice: u64,
    vtime: u64,
    enq_flags: u64,
) {
    scx_bpf_dsq_insert_vtime(p, dsq_id, slice, vtime, enq_flags);
}

/// Move the first eligible task from a DSQ to the current CPU's local DSQ.
///
/// Mirrors the kernel's `consume_dispatch_q()` which iterates the DSQ and
/// skips tasks whose cpumask does not include the consuming CPU.
#[no_mangle]
pub extern "C" fn scx_bpf_dsq_move_to_local(dsq_id: u64) -> bool {
    crate::preempt::set_current_kfunc("dsq_move_to_local");
    crate::interleave::maybe_yield();
    with_sim(kfunc_cost::COMPLEX, |sim| {
        let cpu_idx = sim.current_cpu.0 as usize;
        let current_cpu = sim.current_cpu;
        // Split borrow: extract cpu mutably, build cpumask predicate from
        // task_pid_to_raw, then pass dsqs mutably.
        let cpus_ptr = sim.cpus.as_mut_ptr();
        let cpu = unsafe { &mut *cpus_ptr.add(cpu_idx) };
        let task_map = &sim.task_pid_to_raw;
        let result = sim.dsqs.move_to_local_filtered(DsqId(dsq_id), cpu, |pid| {
            let Some(&raw) = task_map.get(&pid) else {
                return true;
            };
            let task_ptr = raw as *mut c_void;
            let task_cpus = unsafe { ffi::sim_task_get_cpus_ptr(task_ptr) };
            task_cpus.is_null() || unsafe { ffi::bpf_cpumask_test_cpu(current_cpu.0, task_cpus) }
        });
        debug!(dsq_id, result, "enter:kfunc dsq_move_to_local");

        let local_t = sim.cpus[cpu_idx].local_clock;
        sim.trace.record(
            local_t,
            sim.current_cpu,
            TraceKind::DsqMoveToLocal {
                dsq_id: DsqId(dsq_id),
                success: result,
            },
        );

        // Sample DSQ length after consume (if successful and non-builtin)
        let dsq = DsqId(dsq_id);
        if result && !dsq.is_builtin() {
            sim.trace
                .sample_dsq_lengths(local_t, &sim.dsqs, DsqSampleTrigger::Consume, Some(dsq));
        }

        result
    })
}

/// C-side alias: sim_wrapper.h redefines `scx_bpf_dsq_move_to_local` as a
/// variadic macro that strips the v2 `enq_flags` arg and calls this symbol.
#[no_mangle]
pub extern "C" fn __sim_dsq_move_to_local(dsq_id: u64) -> bool {
    scx_bpf_dsq_move_to_local(dsq_id)
}

/// Query the number of tasks queued in a DSQ.
#[no_mangle]
pub extern "C" fn scx_bpf_dsq_nr_queued(dsq_id: u64) -> i32 {
    crate::preempt::set_current_kfunc("dsq_nr_queued");
    crate::interleave::maybe_yield();
    with_sim(kfunc_cost::SIMPLE, |sim| {
        let dsq = DsqId(dsq_id);
        let n = if dsq.is_local() {
            let cpu = sim.current_cpu.0 as usize;
            let n = sim.cpus[cpu].local_dsq.len() as i32;
            debug!(dsq_id, n, "enter:kfunc dsq_nr_queued LOCAL");
            n
        } else if dsq.is_local_on() {
            let cpu = dsq.local_on_cpu();
            if (cpu.0 as usize) < sim.cpus.len() {
                let n = sim.cpus[cpu.0 as usize].local_dsq.len() as i32;
                debug!(dsq_id, cpu = cpu.0, n, "enter:kfunc dsq_nr_queued LOCAL_ON");
                n
            } else {
                0
            }
        } else {
            let n = sim.dsqs.nr_queued(dsq) as i32;
            debug!(dsq_id, n, "enter:kfunc dsq_nr_queued");
            n
        };
        // TOP-9 of secondary TraceKind easy-win bundle: emit
        // helper/dsq_nr_queued entry+exit. **Med** relevance:
        // LAVD polls queue depth on dispatch decisions.
        let cpu = sim.current_cpu;
        let local_t = sim.cpus[cpu.0 as usize].local_clock;
        sim.trace.record(
            local_t,
            cpu,
            TraceKind::DsqNrQueued {
                dsq_id: dsq,
                ret: n,
            },
        );
        n
    })
}

/// Get the current simulated time (per-CPU local clock).
#[no_mangle]
pub extern "C" fn scx_bpf_now() -> u64 {
    crate::preempt::set_current_kfunc("now");
    crate::interleave::maybe_yield();
    with_sim(kfunc_cost::TRIVIAL, |sim| {
        let cpu_idx = sim.current_cpu.0 as usize;
        let now_ns = sim.cpus[cpu_idx].local_clock;
        // TOP-8 of secondary TraceKind easy-win bundle (tg
        // `bundle-implement-secondary-tracekind-easy-wins`): emit
        // helper/now entry+exit so the live-vs-sim diff harness can
        // validate the time-source agrees — the cpu-bw-stall-bug's
        // smoking gun is computed against `bpf_ktime_get_ns()`
        // snapshots and a divergence on this single helper hides the
        // whole bug.
        let cpu = sim.current_cpu;
        sim.trace
            .record(now_ns, cpu, TraceKind::HelperNow { ret_ns: now_ns });
        now_ns
    })
}

/// Simulated rq->clock_task: wall clock minus cumulative IRQ time.
///
/// In the kernel, rq->clock_task = rq->clock - cumulative IRQ/steal time.
/// LAVD uses this to detect steal utilization on IRQ-heavy CPUs.
#[no_mangle]
pub extern "C" fn sim_scx_clock_task(cpu: u32) -> u64 {
    crate::preempt::set_current_kfunc("scx_clock_task");
    crate::interleave::maybe_yield();
    with_sim(kfunc_cost::TRIVIAL, |sim| {
        let idx = cpu as usize;
        if idx < sim.cpus.len() {
            sim.cpus[idx]
                .local_clock
                .saturating_sub(sim.cpus[idx].irq_cumulative_ns)
        } else {
            0
        }
    })
}

/// Get the current CPU ID.
#[no_mangle]
pub extern "C" fn bpf_get_smp_processor_id() -> u32 {
    crate::preempt::set_current_kfunc("get_smp_processor_id");
    crate::interleave::maybe_yield();
    with_sim(kfunc_cost::TRIVIAL, |sim| sim.current_cpu.0)
}

/// Alias for sim_wrapper.h macro override (avoids conflict with the
/// static function pointer in bpf_helper_defs.h).
#[no_mangle]
pub extern "C" fn sim_bpf_get_smp_processor_id() -> u32 {
    crate::preempt::set_current_kfunc("get_smp_processor_id");
    crate::interleave::maybe_yield();
    with_sim(kfunc_cost::TRIVIAL, |sim| sim.current_cpu.0)
}

/// Returns 1 if the current CPU is in hardirq context, 0 otherwise.
#[no_mangle]
pub extern "C" fn sim_bpf_in_hardirq() -> u32 {
    with_sim(kfunc_cost::TRIVIAL, |sim| {
        let cpu = sim.current_cpu;
        u32::from(sim.cpus[cpu.0 as usize].irq_context == crate::cpu::IrqContext::HardIrq)
    })
}

/// Returns 0 — the simulator does not model NMIs.
#[no_mangle]
pub extern "C" fn sim_bpf_in_nmi() -> u32 {
    0
}

/// Returns 1 if the current CPU is serving a softirq, 0 otherwise.
#[no_mangle]
pub extern "C" fn sim_bpf_in_serving_softirq() -> u32 {
    with_sim(kfunc_cost::TRIVIAL, |sim| {
        let cpu = sim.current_cpu;
        u32::from(sim.cpus[cpu.0 as usize].irq_context == crate::cpu::IrqContext::ServingSoftIrq)
    })
}

/// Returns 1 if the current CPU is in any interrupt context
/// (hardirq or softirq), 0 otherwise.
#[no_mangle]
pub extern "C" fn sim_bpf_in_interrupt() -> u32 {
    with_sim(kfunc_cost::TRIVIAL, |sim| {
        let cpu = sim.current_cpu;
        u32::from(sim.cpus[cpu.0 as usize].irq_context != crate::cpu::IrqContext::None)
    })
}

/// Deterministic PRNG replacement for bpf_get_prandom_u32.
#[no_mangle]
pub extern "C" fn sim_bpf_get_prandom_u32() -> u32 {
    with_sim(kfunc_cost::TRIVIAL, |sim| sim.next_prng())
}

/// Get the CPU a task is assigned to.
///
/// Returns the CPU the task was last scheduled on, matching the kernel's
/// `task_cpu(p)` semantics. Falls back to CPU 0 for tasks that haven't
/// run yet.
#[no_mangle]
pub extern "C" fn scx_bpf_task_cpu(p: *const c_void) -> i32 {
    crate::preempt::set_current_kfunc("task_cpu");
    crate::interleave::maybe_yield();
    with_sim(kfunc_cost::SIMPLE, |sim| {
        let pid = sim.task_pid_from_raw(p as *mut c_void);
        let cpu = sim.task_last_cpu.get(&pid).copied().unwrap_or(CpuId(0));
        // TOP-8 of secondary TraceKind easy-win bundle: emit
        // helper/task_cpu entry+exit so the live-vs-sim diff harness
        // can confirm sim and live agree on each scheduler's view of
        // task → CPU placement.
        let current = sim.current_cpu;
        let local_t = sim.cpus[current.0 as usize].local_clock;
        sim.trace.record(
            local_t,
            current,
            TraceKind::HelperTaskCpu { pid, ret_cpu: cpu },
        );
        cpu.0 as i32
    })
}

/// Report a scheduler error.
///
/// Sets a flag in SimulatorState that causes the engine to stop the
/// simulation with `ExitKind::ErrorBpf`. Only the first error is recorded.
#[no_mangle]
pub extern "C" fn scx_bpf_error_bstr(fmt: *const i8, _data: *const u64, _data_sz: u32) {
    let msg = if fmt.is_null() {
        "scheduler error (null fmt)".to_string()
    } else {
        let cstr = unsafe { std::ffi::CStr::from_ptr(fmt) };
        cstr.to_string_lossy().into_owned()
    };
    with_sim(kfunc_cost::TRIVIAL, |sim| {
        if sim.bpf_error.is_none() {
            debug!(msg = %msg, "scx_bpf_error");
            sim.bpf_error = Some(msg);
        }
    });
}

/// Request re-enqueue of all tasks on the current CPU's local DSQ.
///
/// Called by `cpu_release` handlers (e.g. COSMOS). Sets a flag that
/// the engine checks after the callback returns. The engine then
/// drains the local DSQ and calls `enqueue(p, SCX_ENQ_REENQ)` for
/// each task.
#[no_mangle]
pub extern "C" fn scx_bpf_reenqueue_local() -> u32 {
    with_sim(kfunc_cost::TRIVIAL, |sim| {
        debug!("enter:kfunc reenqueue_local");
        sim.reenqueue_local_requested = true;
        0
    })
}

/// compat.bpf.h may route `scx_bpf_reenqueue_local()` through legacy/new weak
/// symbol names. Provide both compat entrypoints so scheduler `.so` files never
/// end up calling an unresolved NULL stub during `cpu_release`.
#[no_mangle]
pub extern "C" fn scx_bpf_reenqueue_local___v1() -> u32 {
    scx_bpf_reenqueue_local()
}

#[no_mangle]
pub extern "C" fn scx_bpf_reenqueue_local___v2___compat() {
    let _ = scx_bpf_reenqueue_local();
}

/// No-op stubs for cpumask ref counting in simulator.
#[no_mangle]
pub extern "C" fn scx_bpf_put_cpumask(_cpumask: *const c_void) {}

#[no_mangle]
pub extern "C" fn scx_bpf_put_idle_cpumask(_cpumask: *const c_void) {}

/// Destroy a DSQ. No-op in simulator (the engine never frees DSQs
/// mid-run), but we record the request so the live-vs-sim diff
/// harness still notices it (TOP-9 of the secondary TraceKind
/// easy-win bundle).
#[no_mangle]
pub extern "C" fn scx_bpf_destroy_dsq(dsq_id: u64) {
    with_sim(kfunc_cost::TRIVIAL, |sim| {
        let cpu = sim.current_cpu;
        let local_t = sim.cpus[cpu.0 as usize].local_clock;
        sim.trace.record(
            local_t,
            cpu,
            TraceKind::DestroyDsq {
                dsq_id: DsqId(dsq_id),
            },
        );
    });
}

/// No-op for bpf_ktime_get_ns -- use per-CPU local clock.
#[no_mangle]
pub extern "C" fn bpf_ktime_get_ns() -> u64 {
    crate::preempt::set_current_kfunc("ktime_get_ns");
    crate::interleave::maybe_yield();
    with_sim(kfunc_cost::TRIVIAL, |sim| {
        let cpu = sim.current_cpu.0 as usize;
        sim.cpus[cpu].local_clock
    })
}

/// Alias for C macro override (`sim_wrapper.h` redirects `bpf_ktime_get_ns()`
/// to this to avoid the static function pointer in `bpf_helper_defs.h`).
#[no_mangle]
pub extern "C" fn sim_bpf_ktime_get_ns() -> u64 {
    bpf_ktime_get_ns()
}

/// Simulated `CONFIG_HZ`. Must agree with `engine::TICK_INTERVAL_NS`
/// (4ms → 250Hz) — the scheduler sees one tick per jiffy, as in the kernel.
pub const CONFIG_HZ: u64 = 1_000_000_000 / crate::engine::TICK_INTERVAL_NS;

/// Convert a simulated nanosecond timestamp to jiffies.
///
/// Single source of truth for the ns↔jiffies relationship, shared by the
/// engine (which stamps `p->scx.runnable_at` in jiffies, like the kernel's
/// `scx_runnable()`) and by scheduler wrappers overriding `bpf_jiffies64()`.
pub const fn ns_to_jiffies(ns: u64) -> u64 {
    ns / crate::engine::TICK_INTERVAL_NS
}

/// `bpf_jiffies64()` — the kernel helper returning the current jiffies count.
///
/// `bpf_helper_defs.h` declares this as a static function pointer initialised
/// to the raw helper number, so calling it unoverridden jumps to a bogus
/// address. Wrappers redirect it here. Derived from the same per-CPU local
/// clock as `bpf_ktime_get_ns()` so the two never disagree.
#[no_mangle]
pub extern "C" fn sim_bpf_jiffies64() -> u64 {
    ns_to_jiffies(bpf_ktime_get_ns())
}

// RCU stubs -- no-op in simulator
#[no_mangle]
pub extern "C" fn bpf_rcu_read_lock() {}

#[no_mangle]
pub extern "C" fn bpf_rcu_read_unlock() {}

// Task reference stubs
#[no_mangle]
pub extern "C" fn bpf_task_release(_p: *mut c_void) {}

/// `bpf_task_acquire(p)` — take a reference on a task and return it.
///
/// The kernel bumps `p->rcu_users` and returns NULL if the task is already
/// dying. The simulator owns every `task_struct` for the whole run (they are
/// allocated by `sim_task_alloc` at fixture load and freed at teardown), so
/// there is no refcount to bump and no window in which a live task pointer
/// can become invalid mid-callback. Returning `p` unchanged is therefore the
/// faithful answer, not a stub: the paired `bpf_task_release` is likewise a
/// no-op, so acquire/release stay balanced.
///
/// Used by scx_layered's `tp_cgroup_attach_task` hook to pin the thread-group
/// leader while it walks the group.
#[no_mangle]
pub extern "C" fn bpf_task_acquire(p: *mut c_void) -> *mut c_void {
    p
}

/// Get the current task's task_struct pointer (for the CPU we're running on).
///
/// During a wake path with a known waker (Phase::Wake), returns the waker's
/// task_struct so that `is_wake_affine()` can compare address spaces.
/// Otherwise returns the task running on `current_cpu`, or a synthetic idle
/// task if no task is running (matching kernel behavior where the idle task
/// is always "current" on idle CPUs).
#[no_mangle]
pub extern "C" fn bpf_get_current_task_btf() -> *mut c_void {
    crate::preempt::set_current_kfunc("get_current_task_btf");
    crate::interleave::maybe_yield();
    with_sim(kfunc_cost::SIMPLE, |sim| {
        // Waker override: during select_cpu for a waker-induced wake,
        // return the waker's task_struct (kernel semantics).
        if let Some(raw) = sim.waker_task_raw {
            return raw as *mut c_void;
        }
        let cpu = sim.current_cpu;
        if let Some(pid) = sim.cpus[cpu.0 as usize].current_task {
            if let Some(&raw) = sim.task_pid_to_raw.get(&pid) {
                return raw as *mut c_void;
            }
        }
        // Idle task fallback — never return NULL (kernel never does).
        sim.idle_task_raw
    })
}

/// Alias for sim_wrapper.h macro override (avoids conflict with the
/// static function pointer in bpf_helper_defs.h).
#[no_mangle]
pub extern "C" fn sim_bpf_get_current_task_btf() -> *mut c_void {
    // No separate maybe_yield — bpf_get_current_task_btf already yields.
    bpf_get_current_task_btf()
}

/// Another alias used by the updated sim_wrapper.h macro.
#[no_mangle]
pub extern "C" fn bpf_get_current_task_btf_kfunc() -> *mut c_void {
    bpf_get_current_task_btf()
}

/// Get the task running on a given CPU.
///
/// Needed as a linkable symbol for compat paths, even though
/// `__COMPAT_scx_bpf_cpu_curr` is overridden to a macro returning NULL.
#[no_mangle]
pub extern "C" fn scx_bpf_cpu_curr(cpu: i32) -> *mut c_void {
    with_sim(kfunc_cost::SIMPLE, |sim| {
        let idx = cpu as usize;
        if idx >= sim.cpus.len() {
            return ptr::null_mut();
        }
        if let Some(pid) = sim.cpus[idx].current_task {
            if let Some(&raw) = sim.task_pid_to_raw.get(&pid) {
                return raw as *mut c_void;
            }
        }
        ptr::null_mut()
    })
}

// ---------------------------------------------------------------------------
// DSQ peek
// ---------------------------------------------------------------------------

/// Peek at the first task in a DSQ without consuming it.
///
/// Returns a pointer to the first `task_struct` in the DSQ (by priority
/// order for priq, FIFO order for fifo), or NULL if the DSQ is empty or
/// does not exist.  This resolves the `__COMPAT_scx_bpf_dsq_peek` runtime
/// check (`bpf_ksym_exists(scx_bpf_dsq_peek)`) so the compat function
/// calls this directly instead of falling through to `bpf_iter_scx_dsq_*`
/// which are not implemented in the simulator.
#[no_mangle]
pub extern "C" fn scx_bpf_dsq_peek(dsq_id: u64) -> *mut c_void {
    with_sim(kfunc_cost::SIMPLE, |sim| {
        let pids = sim.dsqs.ordered_pids(DsqId(dsq_id));
        if pids.is_empty() {
            return ptr::null_mut();
        }
        sim.task_pid_to_raw
            .get(&pids[0])
            .map_or(ptr::null_mut(), |&raw| raw as *mut c_void)
    })
}

// ---------------------------------------------------------------------------
// DSQ iterator kfuncs for bpf_for_each(scx_dsq, ...)
// ---------------------------------------------------------------------------

/// Begin iterating a DSQ. Returns the first task_struct* or NULL.
///
/// Called by the `bpf_for_each(scx_dsq, p, dsq_id, flags)` macro override.
/// Snapshots the DSQ contents in priority order and stores the iterator
/// state in SimulatorState.
#[no_mangle]
pub extern "C" fn sim_dsq_iter_begin(dsq_id: u64, _flags: u64) -> *mut c_void {
    crate::preempt::set_current_kfunc("dsq_iter_begin");
    crate::interleave::maybe_yield();
    with_sim(kfunc_cost::COMPLEX, |sim| {
        let pids = sim.dsqs.ordered_pids(DsqId(dsq_id));
        debug!(dsq_id, count = pids.len(), "dsq_iter_begin");

        if pids.is_empty() {
            sim.dsq_iter = None;
            return ptr::null_mut();
        }

        let first_pid = pids[0];
        sim.dsq_iter = Some(DsqIterState {
            dsq_id: DsqId(dsq_id),
            pids,
            pos: 0,
        });

        sim.task_pid_to_raw
            .get(&first_pid)
            .map_or(ptr::null_mut(), |&raw| raw as *mut c_void)
    })
}

/// Advance the DSQ iterator. Returns the next task_struct* or NULL.
#[no_mangle]
pub extern "C" fn sim_dsq_iter_next() -> *mut c_void {
    crate::preempt::set_current_kfunc("dsq_iter_next");
    crate::interleave::maybe_yield();
    with_sim(kfunc_cost::COMPLEX, |sim| {
        let iter = match sim.dsq_iter.as_mut() {
            Some(it) => it,
            None => return ptr::null_mut(),
        };

        iter.pos += 1;

        // Skip PIDs that were removed from the DSQ (by dsq_move)
        while iter.pos < iter.pids.len() {
            let pid = iter.pids[iter.pos];
            // Check if this PID is still in the DSQ
            if sim.dsqs.ordered_pids(iter.dsq_id).contains(&pid) {
                return sim
                    .task_pid_to_raw
                    .get(&pid)
                    .map_or(ptr::null_mut(), |&raw| raw as *mut c_void);
            }
            iter.pos += 1;
        }

        sim.dsq_iter = None;
        ptr::null_mut()
    })
}

/// Move a task from the currently-iterated DSQ to a destination DSQ.
///
/// Called by `__COMPAT_scx_bpf_dsq_move` macro override. The source DSQ
/// is determined from the active DSQ iterator state.
#[no_mangle]
pub extern "C" fn sim_scx_bpf_dsq_move(p: *mut c_void, dst_dsq_id: u64, _enq_flags: u64) -> bool {
    crate::preempt::set_current_kfunc("dsq_move");
    crate::interleave::maybe_yield();
    with_sim(kfunc_cost::COMPLEX, |sim| {
        let pid = sim.task_pid_from_raw(p);
        let src_dsq_id = match &sim.dsq_iter {
            Some(iter) => iter.dsq_id,
            None => {
                debug!(pid = pid.0, "dsq_move: no active iterator");
                return false;
            }
        };

        // Remove from source DSQ
        if !sim.dsqs.remove_pid(src_dsq_id, pid) {
            debug!(
                pid = pid.0,
                src = src_dsq_id.0,
                "dsq_move: pid not in source DSQ"
            );
            return false;
        }

        let dst = DsqId(dst_dsq_id);
        if dst.is_local() {
            let cpu_idx = sim.current_cpu.0 as usize;
            sim.cpus[cpu_idx].local_dsq.push_back(pid);
            debug!(pid = pid.0, "dsq_move → LOCAL");
        } else if dst.is_local_on() {
            let cpu = dst.local_on_cpu();
            if (cpu.0 as usize) < sim.cpus.len() {
                sim.cpus[cpu.0 as usize].local_dsq.push_back(pid);
            }
            debug!(pid = pid.0, cpu = cpu.0, "dsq_move → LOCAL_ON");
        } else {
            // Move to another named DSQ (FIFO)
            sim.dsqs.insert_fifo(dst, pid);
            debug!(pid = pid.0, dst = dst.0, "dsq_move → DSQ");
        }

        true
    })
}

// ---------------------------------------------------------------------------
// Updated bpf_task_from_pid — return real task_struct*
// ---------------------------------------------------------------------------

/// Reserved PID for the simulator's synthetic "loader" task.
///
/// Upstream cgroup_bw (sched-ext/scx `a52f85e3` "lib/cgroup_bw: resolve root
/// cgroup through the loader task") resolves the root cgroup by capturing the
/// loader process's tgid at `scx_cgroup_bw_lib_init()` and later doing
/// `bpf_task_from_pid(cbw_loader_tgid)->cgroups->dfl_cgrp`. The simulator has
/// no separate loader process, so the LAVD wrapper publishes this reserved
/// tgid via `bpf_get_current_pid_tgid()` and we resolve it to the always-
/// present idle task (which lives in the root cgroup) here.
///
/// Must match `SIM_CBW_LOADER_TGID` in `schedulers/lavd/wrapper.c`. The value
/// is in the reserved high PID range so it never collides with a workload
/// task's PID.
pub const SIM_CBW_LOADER_PID: i32 = 0x7FFF_FF00;

/// Look up a task by PID and return its task_struct pointer.
///
/// In BPF this is a verifier workaround; in the simulator we use it
/// to get the raw pointer from our PID→pointer map.
#[no_mangle]
pub extern "C" fn bpf_task_from_pid(pid: i32) -> *mut c_void {
    with_sim(kfunc_cost::SIMPLE, |sim| {
        // cgroup_bw's loader-task root-cgroup resolution: map the reserved
        // loader PID to the synthetic idle task, which sim_task_alloc() places
        // in the root cgroup (cgroups->dfl_cgrp == sim root). See
        // SIM_CBW_LOADER_PID above.
        if pid == SIM_CBW_LOADER_PID {
            return sim.idle_task_raw;
        }
        sim.task_pid_to_raw
            .get(&Pid(pid))
            .map_or(ptr::null_mut(), |&raw| raw as *mut c_void)
    })
}

// ---------------------------------------------------------------------------
// scx_bpf_kick_cpu — stage a KickDelivered event for the engine
// ---------------------------------------------------------------------------

/// Kick a CPU (send scheduling IPI).
///
/// Stages a `KickDelivered` event in `staged_events`. The engine flushes
/// these into the event queue after the current callback returns, delivering
/// the kick at `local_clock + ipi_delivery_ns`. Multiple kicks to the same
/// CPU accumulate as separate staged events; the engine merges flags when
/// processing them.
#[no_mangle]
pub extern "C" fn scx_bpf_kick_cpu(cpu: i32, flags: u64) {
    crate::preempt::set_current_kfunc("kick_cpu");
    crate::interleave::maybe_yield();
    with_sim(kfunc_cost::SIMPLE, |sim| {
        let cpu_id = CpuId(cpu as u32);
        if (cpu_id.0 as usize) < sim.cpus.len() {
            let new_flags = KickFlags::from_raw(flags);
            let current = sim.current_cpu;
            let local_t = sim.cpus[current.0 as usize].local_clock;
            let delivery_t = local_t + sim.overhead.effective_ipi_delivery_ns();
            sim.staged_events.push((
                delivery_t,
                StagedEvent::KickDelivered {
                    cpu: cpu_id,
                    flags: new_flags,
                },
            ));
            debug!(cpu, flags, "kick_cpu");

            sim.trace
                .record(local_t, current, TraceKind::KickCpu { target_cpu: cpu_id });
        }
    })
}

// ---------------------------------------------------------------------------
// Dump kfuncs — scheduler debug output capture
// ---------------------------------------------------------------------------

/// Format scheduler debug text into the simulator's per-run dump buffer.
#[no_mangle]
pub extern "C" fn scx_bpf_dump_bstr(fmt: *const i8, data: *const u64, data_sz: u32) {
    if fmt.is_null() {
        return;
    }
    // SAFETY: `fmt` is a NUL-terminated literal emitted by the scx_bpf_dump()
    // macro, and `data`/`data_sz` describe the matching u64 argument array.
    let text = unsafe { format_bstr(fmt, data, data_sz) };
    DUMP_BUF.with(|b| b.borrow_mut().push_str(&text));
}

thread_local! {
    /// Text captured from `scx_bpf_dump_bstr`.
    ///
    /// The kernel routes `ops.dump` output into the exit dump buffer that
    /// userspace prints on scheduler unload. Discarding it here would make
    /// `ops.dump` untestable for every scheduler — a dump that faults and a
    /// dump that is a no-op would look identical.
    static DUMP_BUF: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

/// Clear the captured `ops.dump` text. Called by the engine at run start.
pub fn dump_buffer_reset() {
    DUMP_BUF.with(|b| b.borrow_mut().clear());
}

/// Read back everything `ops.dump` emitted during the run.
pub fn dump_buffer_take() -> String {
    DUMP_BUF.with(|b| std::mem::take(&mut *b.borrow_mut()))
}

/// Format a BPF `bstr`-style call: a printf format string plus an array of
/// u64 arguments (the kernel's `bpf_bprintf` ABI).
///
/// Supports the `%[-+ #0][width][l|ll]{d,i,u,x,s,c,%}` subset the scx
/// schedulers use. An unsupported conversion is copied through literally
/// rather than silently misformatted — `tests/layered.rs` asserts no such
/// leftover reaches the dump, which is how the missing `+` flag was caught.
///
/// # Safety
/// `fmt` must be NUL-terminated; `data` must point to at least
/// `data_sz / 8` u64 values, and any `%s` argument must be a valid
/// NUL-terminated string pointer.
unsafe fn format_bstr(fmt: *const i8, data: *const u64, data_sz: u32) -> String {
    let fmt_s = std::ffi::CStr::from_ptr(fmt).to_string_lossy().into_owned();
    let nr_args = if data.is_null() {
        0
    } else {
        (data_sz as usize) / std::mem::size_of::<u64>()
    };
    let arg = |i: usize| -> Option<u64> {
        if i < nr_args {
            Some(*data.add(i))
        } else {
            None
        }
    };

    let mut out = String::with_capacity(fmt_s.len() + 32);
    let bytes: Vec<char> = fmt_s.chars().collect();
    let mut i = 0usize;
    let mut argi = 0usize;
    while i < bytes.len() {
        if bytes[i] != '%' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        let spec_start = i;
        i += 1;
        if i < bytes.len() && bytes[i] == '%' {
            out.push('%');
            i += 1;
            continue;
        }
        // Flags: '-' (left-justify), '+' (always sign), ' ', '#', '0'.
        let mut zero_pad = false;
        let mut left_justify = false;
        let mut plus_sign = false;
        loop {
            match bytes.get(i) {
                Some('0') => zero_pad = true,
                Some('-') => left_justify = true,
                Some('+') => plus_sign = true,
                Some(' ') | Some('#') => {}
                _ => break,
            }
            i += 1;
        }
        let mut width = 0usize;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            width = width * 10 + bytes[i].to_digit(10).unwrap() as usize;
            i += 1;
        }
        while i < bytes.len() && bytes[i] == 'l' {
            i += 1;
        }
        let conv = if i < bytes.len() { bytes[i] } else { '\0' };
        let v = match arg(argi) {
            Some(v) => v,
            None => {
                // Not enough arguments — emit the spec literally.
                out.extend(&bytes[spec_start..bytes.len().min(i + 1)]);
                i += 1;
                continue;
            }
        };
        let rendered = match conv {
            'd' | 'i' => {
                let sv = v as i64;
                if plus_sign && sv >= 0 {
                    format!("+{sv}")
                } else {
                    format!("{sv}")
                }
            }
            'u' => format!("{v}"),
            'x' => format!("{v:x}"),
            'c' => char::from_u32(v as u32).map_or_else(String::new, |c| c.to_string()),
            's' => {
                if v == 0 {
                    "(null)".to_string()
                } else {
                    // SAFETY: the scheduler passed a NUL-terminated string
                    // pointer for a %s conversion, per the bstr ABI.
                    std::ffi::CStr::from_ptr(v as *const i8)
                        .to_string_lossy()
                        .into_owned()
                }
            }
            _ => {
                // Unsupported conversion — copy it through, consume nothing.
                out.extend(&bytes[spec_start..bytes.len().min(i + 1)]);
                i += 1;
                continue;
            }
        };
        argi += 1;
        let pad_n = width.saturating_sub(rendered.len());
        if left_justify {
            out.push_str(&rendered);
            for _ in 0..pad_n {
                out.push(' ');
            }
        } else {
            let pad = if zero_pad { '0' } else { ' ' };
            for _ in 0..pad_n {
                out.push(pad);
            }
            out.push_str(&rendered);
        }
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// Cgroup kfuncs
// ---------------------------------------------------------------------------

/// Get a task's cgroup. Returns the cgroup the task belongs to, falling back
/// to the root cgroup if no cgroup has been assigned.
#[no_mangle]
pub extern "C" fn scx_bpf_task_cgroup(p: *mut c_void, _subsys_id: i32) -> *mut c_void {
    if p.is_null() {
        return ptr::null_mut();
    }
    // Get the task's cgroup from the C-side task_struct
    let cgrp = unsafe { ffi::sim_task_get_cgroup(p) };
    let resolved = if !cgrp.is_null() {
        cgrp
    } else {
        // Fallback: return root cgroup (task not assigned to any cgroup)
        unsafe { ffi::sim_get_root_cgroup() }
    };
    // TOP-8 of secondary TraceKind easy-win bundle (tg
    // `bundle-implement-secondary-tracekind-easy-wins`): emit
    // helper/task_cgroup entry+exit. **High** cpu-bw-stall-bug
    // relevance: LAVD calls this on every enqueue to look up cgroup
    // state; a divergence here means sim and live see different
    // cgroups for the same task and the bug is invisible.
    // SAFETY: `resolved` is either a real cgroup the engine allocated
    // via `sim_cgroup_alloc` (which sets up `cgrp->kn->id`) or
    // `sim_get_root_cgroup()` (which sets `kn->id = 1`); both are
    // safe to pass to `sim_cgroup_get_kn_id`.
    let cgid_u64 = unsafe { ffi::sim_cgroup_get_kn_id(resolved) };
    with_sim(kfunc_cost::TRIVIAL, |sim| {
        let pid = sim.task_pid_from_raw(p);
        let cpu = sim.current_cpu;
        let local_t = sim.cpus[cpu.0 as usize].local_clock;
        sim.trace.record(
            local_t,
            cpu,
            TraceKind::HelperTaskCgroup {
                pid,
                cgid: crate::cgroup::CgroupId(cgid_u64),
            },
        );
    });
    resolved
}

/// C-side alias: sim_wrapper.h redefines `scx_bpf_task_cgroup` as a macro
/// that calls this symbol with a default subsys_id=0.
#[no_mangle]
pub extern "C" fn __sim_task_cgroup(p: *mut c_void, subsys_id: i32) -> *mut c_void {
    scx_bpf_task_cgroup(p, subsys_id)
}

/// Look up a cgroup by its kernfs ID. Returns the cgroup pointer if found,
/// or the root cgroup if no registry is installed.
#[no_mangle]
pub extern "C" fn bpf_cgroup_from_id(id: u64) -> *mut c_void {
    crate::cgroup_ffi::sim_cgroup_lookup_by_id(id)
}

/// Get the ancestor cgroup at a given hierarchy level. Returns NULL if the
/// cgroup pointer is null, the level is negative, or no ancestor exists at
/// the requested level.
#[no_mangle]
pub extern "C" fn bpf_cgroup_ancestor(cgrp: *mut c_void, level: i32) -> *mut c_void {
    if cgrp.is_null() || level < 0 {
        return ptr::null_mut();
    }
    crate::cgroup_ffi::sim_cgroup_lookup_ancestor(cgrp, level as u32)
}

/// Acquire a reference on a cgroup. No-op in the simulator.
#[no_mangle]
pub extern "C" fn bpf_cgroup_acquire(_cgrp: *mut c_void) -> *mut c_void {
    _cgrp
}

/// Release a cgroup reference. No-op in the simulator.
#[no_mangle]
pub extern "C" fn bpf_cgroup_release(_cgrp: *mut c_void) {}

// ---------------------------------------------------------------------------
// Per-cgroup / per-task local storage + per-CPU array shims.
//
// These three kfuncs were previously NULL-returning stubs (a clear
// no-stub-policy violation). Phase 1 BPF infra scale-up items 5 + 6
// (tg `scxsim-bpf-infra-scale-up-phase1`) retire them by delegating to
// the existing scxtest map machinery at `lib/scxtest/scx_test_map.c`,
// which knows how to look up the registered map's value-size and
// allocate / fetch / delete a slot keyed by the pointer the caller
// passed in.
//
// The C entry points -- `scx_test_cgrp_storage_get`,
// `scx_test_task_storage_get`, `scx_test_map_lookup_percpu_elem`,
// `scx_test_cgrp_storage_delete`, `scx_test_map_delete_elem` -- are
// the same ones that scheduler wrapper.c files install via macro
// `#define`; the kfuncs.rs versions below provide the strong symbols
// resolved at .so load time when a wrapper has NOT installed an
// override. After Phase 2 wires `cgroup_bw.bpf.c` into the LAVD
// wrapper, the cgroup-storage call sites inside the library will
// resolve to these kfuncs (because the library is `#include`d AFTER
// the macro establishment, but the macro is a compile-time
// substitution that DOES apply to the included source).
//
// Either path runs the same C implementation -- single source of
// truth for the per-cgroup / per-task / per-CPU storage semantics.
// ---------------------------------------------------------------------------

extern "C" {
    fn scx_test_cgrp_storage_get(
        map: *mut c_void,
        cgrp_ptr_loc: *const c_void,
        value: *mut c_void,
        flags: u64,
    ) -> *mut c_void;
    fn scx_test_cgrp_storage_delete(map: *mut c_void, cgrp_ptr_loc: *const c_void) -> i32;
    fn scx_test_task_storage_get(
        map: *mut c_void,
        task_ptr_loc: *const c_void,
        value: *mut c_void,
        flags: u64,
    ) -> *mut c_void;
    fn scx_test_map_delete_elem(map: *mut c_void, key: *const c_void) -> i32;
    fn scx_test_map_lookup_percpu_elem(
        map: *mut c_void,
        key: *const c_void,
        cpu: i32,
    ) -> *mut c_void;
}

/// Get per-cgroup BPF local storage.
///
/// Phase 1 BPF infra scale-up item 5: real implementation backed by
/// `scx_test_cgrp_storage_get` (which itself delegates to the same
/// machinery as task storage -- both are pointer-keyed open-addressing
/// over a registered `scx_test_map`). Honors
/// `BPF_LOCAL_STORAGE_GET_F_CREATE` (flag bit 0); the BPF map must
/// have been registered via `scx_test_map_register` so that the
/// per-cgroup value_size is known.
///
/// `cgrp` is the `struct cgroup *` cast to `*mut c_void`. We pass its
/// ADDRESS as the key (`&cgrp`) so the open-addressing layer
/// memcmps `sizeof(void *)` bytes -- matching how the C-side scxtest
/// task_storage uses the task pointer as a key.
#[no_mangle]
pub extern "C" fn bpf_cgrp_storage_get(
    map: *mut c_void,
    cgrp: *mut c_void,
    value: *mut c_void,
    flags: u64,
) -> *mut c_void {
    if map.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: we're handing C three pointers. `&cgrp` is the address of
    // a local variable holding `cgrp`; the C side memcmps it for
    // `sizeof(void *)` bytes against stored keys. This is the same
    // calling convention `bpf_task_storage_get` uses below.
    unsafe { scx_test_cgrp_storage_get(map, &cgrp as *const _ as *const c_void, value, flags) }
}

/// Drop a per-cgroup BPF local-storage slot.
///
/// Phase 1 BPF infra scale-up item 5: required by Phase 2's compiled-in
/// `scx/lib/cgroup_bw.bpf.c::cbw_del_cgroup_ctx` which calls
/// `bpf_cgrp_storage_delete(&cbw_cgrp_map, cgrp)` to drop a cgroup's
/// bandwidth-control context on cgroup exit. Returns 0 on success,
/// `-ENOENT` (-2) if the slot was not present.
#[no_mangle]
pub extern "C" fn bpf_cgrp_storage_delete(map: *mut c_void, cgrp: *mut c_void) -> i64 {
    if map.is_null() {
        return -2; // -ENOENT
    }
    // SAFETY: same key-by-address convention as bpf_cgrp_storage_get.
    let rc = unsafe { scx_test_cgrp_storage_delete(map, &cgrp as *const _ as *const c_void) };
    if rc == 0 {
        0
    } else {
        -2
    } // -ENOENT
}

/// Get per-task BPF local storage.
///
/// Phase 1 BPF infra scale-up item 5: real implementation backed by
/// `scx_test_task_storage_get`. Same calling convention and storage
/// semantics as `bpf_cgrp_storage_get` above (pointer-by-address as
/// the key, value-size from the registered `scx_test_map`).
#[no_mangle]
pub extern "C" fn bpf_task_storage_get(
    map: *mut c_void,
    task: *mut c_void,
    value: *mut c_void,
    flags: u64,
) -> *mut c_void {
    if map.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: hand C the address of `task` as the key; same convention
    // as bpf_cgrp_storage_get.
    unsafe { scx_test_task_storage_get(map, &task as *const _ as *const c_void, value, flags) }
}

/// Drop a per-task BPF local-storage slot.
///
/// Pairs with `bpf_task_storage_get` for completeness (cgroup_bw and
/// other Phase 2 / Phase 3 libraries call delete on task exit).
#[no_mangle]
pub extern "C" fn bpf_task_storage_delete(map: *mut c_void, task: *mut c_void) -> i64 {
    if map.is_null() {
        return -2; // -ENOENT
    }
    let rc = unsafe { scx_test_map_delete_elem(map, &task as *const _ as *const c_void) };
    if rc == 0 {
        0
    } else {
        -2
    }
}

/// Look up per-CPU array element.
///
/// Phase 1 BPF infra scale-up item 6: real implementation backed by
/// `scx_test_map_lookup_percpu_elem`. Replaces the prior NULL stub.
/// The map must have been registered via `scx_register_percpu_test_map`
/// (typically through `INIT_SCX_PERCPU_TEST_MAP` in a wrapper.c
/// `register_maps()` function); unregistered maps return NULL.
#[no_mangle]
pub extern "C" fn bpf_map_lookup_percpu_elem(
    map: *mut c_void,
    key: *const c_void,
    cpu: u32,
) -> *mut c_void {
    if map.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: forward to the C lookup machinery, which handles the
    // null-map and missing-key cases internally.
    unsafe { scx_test_map_lookup_percpu_elem(map, key, cpu as i32) }
}

/// Check if a task is currently running on any CPU.
#[no_mangle]
pub extern "C" fn scx_bpf_task_running(p: *const c_void) -> bool {
    crate::preempt::set_current_kfunc("task_running");
    crate::interleave::maybe_yield();
    with_sim(kfunc_cost::MODERATE, |sim| {
        let pid = sim.task_pid_from_raw(p as *mut c_void);
        let running = sim.cpus.iter().any(|cpu| cpu.current_task == Some(pid));
        debug!(pid = pid.0, running, "enter:kfunc task_running");
        running
    })
}

/// Return the number of possible CPUs.
#[no_mangle]
pub extern "C" fn scx_bpf_nr_cpu_ids() -> u32 {
    with_sim(kfunc_cost::TRIVIAL, |sim| {
        let n = sim.cpus.len() as u32;
        debug!(n, "enter:kfunc nr_cpu_ids");
        n
    })
}

/// Set CPU performance level. Stores in SimCpu for observability.
#[no_mangle]
pub extern "C" fn scx_bpf_cpuperf_set(cpu: i32, perf: u32) {
    with_sim(kfunc_cost::TRIVIAL, |sim| {
        if let Some(c) = sim.cpus.get_mut(cpu as usize) {
            c.perf_lvl = perf;
        }
    });
}

/// Get current CPU performance level. Returns the level set by cpuperf_set,
/// or SCX_CPUPERF_ONE (1024) if never set.
#[no_mangle]
pub extern "C" fn scx_bpf_cpuperf_cur(cpu: i32) -> u32 {
    with_sim(kfunc_cost::TRIVIAL, |sim| {
        sim.cpus
            .get(cpu as usize)
            .map_or(0, |c| if c.perf_lvl > 0 { c.perf_lvl } else { 1024 })
    })
}

/// Get CPU performance capacity. Returns SCX_CPUPERF_ONE (1024) for all
/// CPUs in the simulator (all CPUs have equal max capacity).
#[no_mangle]
pub extern "C" fn scx_bpf_cpuperf_cap(_cpu: i32) -> u32 {
    1024
}

/// Schedule a BPF timer to fire after `nsecs` nanoseconds (slot 0 --
/// backward-compat wrapper for single-timer schedulers).
///
/// Called from C scheduler code when `bpf_timer_start(timer, nsecs, flags)`
/// is invoked WITHOUT explicit slot tracking (i.e. legacy single-timer
/// wrappers in mitosis / cosmos / pre-Phase-1 LAVD). Pinned to slot 0.
///
/// In the kernel, BPF timers fire in softirq on the CPU that armed them
/// (with `BPF_F_TIMER_CPU_PIN`). We capture `current_cpu` here so the
/// resulting `TimerFired` event is associated with the correct CPU.
#[no_mangle]
pub extern "C" fn sim_timer_start(nsecs: u64) {
    sim_timer_start_slot(0, nsecs);
}

/// Schedule a BPF timer in slot `slot` to fire after `nsecs` nanoseconds.
///
/// Phase 1 BPF infra scale-up items 1+2 (tg
/// `scxsim-bpf-infra-scale-up-phase1`). Replaces the prior single
/// `pending_timer_*` pair with a per-slot array so multi-timer
/// wrappers (Phase 1 LAVD; Phase 2 compiled-in cgroup_bw library) can
/// arm distinct timers without the engine collapsing them.
///
/// Slot allocation policy is owned C-side (each scheduler's
/// wrapper.c keeps its own `bpf_timer *` -> slot table; see
/// `schedulers/lavd/wrapper.c::lavd_timer_*` for the worked example).
/// Slot 0 is the legacy single-timer slot.
///
/// The engine drains ALL slots after each callback and inserts one
/// `EventKind::TimerFired { cpu, slot }` per non-empty slot. A timer
/// re-arming itself overwrites its own slot (matches kernel
/// `bpf_timer_start` semantics: re-arming an already-armed timer
/// reschedules it).
#[no_mangle]
pub extern "C" fn sim_timer_start_slot(slot: u32, nsecs: u64) {
    with_sim(kfunc_cost::TRIVIAL, |sim| {
        let idx = slot as usize;
        if idx >= MAX_BPF_TIMERS {
            // Out-of-range slot: silently drop. Schedulers that need
            // > MAX_BPF_TIMERS timers must bump the constant. The drop
            // is logged below for diagnosability.
            debug!(
                slot,
                MAX_BPF_TIMERS, "timer_start_slot dropped: slot >= MAX_BPF_TIMERS"
            );
            return;
        }
        sim.pending_timers[idx] = Some((sim.clock + nsecs, sim.current_cpu));
        debug!(
            slot,
            nsecs,
            fire_at = sim.clock + nsecs,
            cpu = sim.current_cpu.0,
            "timer_start_slot"
        );
    });
}

// ---------------------------------------------------------------------------
// sim_atq.c (Phase 1 BPF infra scale-up item 7) -- userspace impl of
// the scx_atq_* arena-task-queue API.
//
// Phase 2's compiled-in scx/lib/cgroup_bw.bpf.c will resolve scx_atq_*
// references at .so dlopen time against the strong symbols defined in
// csrc/sim_atq.c. To keep that path linked into the main scxsim
// binary (and into integration / lib test binaries) we extern-declare
// every entry point below and re-export them as #[no_mangle] passthrough
// wrappers. Without these references, --gc-sections would discard
// libsim_atq.a from binaries that don't directly reference scx_atq_*
// from Rust (e.g. integration test executables), and scheduler .so
// files would fail to resolve scx_atq_* at dlopen.
//
// The wrappers also give Rust callers a sanctioned, type-safe entry
// point in case future engine-side glue needs to inspect / drive the
// atq directly (Phase 3 concurrent-execution mode might).
// ---------------------------------------------------------------------------

// dead_code: these declarations exist purely so the compiled-in
// scheduler `.so` files (and the `--undefined=scx_atq_create_internal`
// linker keep-alive on the main binary) resolve the symbols at
// dlopen time. Rust callers do not invoke them directly today; the
// references are intentional and must NOT be removed (see comment block
// above this `extern` declaration).
#[allow(dead_code)]
extern "C" {
    fn scx_atq_init() -> i32;
    fn scx_atq_create_internal(fifo: i32, capacity: u64) -> u64;
    fn scx_atq_destroy(atq: u64) -> i32;
    fn scx_atq_insert_vtime(atq: u64, taskc: *mut c_void, vtime: u64) -> i32;
    fn scx_atq_insert_vtime_unlocked(atq: u64, taskc: *mut c_void, vtime: u64) -> i32;
    fn scx_atq_insert(atq: u64, taskc: *mut c_void) -> i32;
    fn scx_atq_insert_unlocked(atq: u64, taskc: *mut c_void) -> i32;
    fn scx_atq_pop(atq: u64) -> u64;
    fn scx_atq_peek(atq: u64) -> u64;
    fn scx_atq_nr_queued(atq: u64) -> i32;
    fn scx_atq_cancel(taskc: *mut c_void) -> i32;
    fn scx_atq_remove(atq: u64, taskc: *mut c_void) -> i32;
    fn scx_atq_remove_unlocked(atq: u64, taskc: *mut c_void) -> i32;
    fn sim_atq_set_taskc_atq_offset(off: u64);
}

/// Linker keep-alive: a `#[used]` static holding a single function
/// pointer into `csrc/sim_atq.c` so `--gc-sections` cannot discard
/// `libsim_atq.a` from binaries that don't otherwise reference
/// `scx_atq_*` from Rust (integration tests; the main scxsim binary
/// pulls them in via build.rs `--undefined=scx_atq_create_internal`).
///
/// One pointer is enough: the C side compiles `sim_atq.c` into a
/// single translation unit (`sim_atq.o`); referencing any one symbol
/// from that .o forces the linker to include the whole object, and
/// every other `scx_atq_*` symbol comes along for free.
///
/// `#[used]` is required because `--gc-sections` is enabled and would
/// otherwise drop this static (it's not read by any Rust code path).
/// The cost is one function pointer (8 B) of read-only data per binary.
#[used]
static SCX_ATQ_KEEPALIVE: unsafe extern "C" fn() -> i32 = scx_atq_init;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cgroup::CgroupRegistry;
    use crate::cpu::SimCpu;
    use crate::dsq::DsqManager;
    use crate::engine::EventQueue;
    use crate::scenario::{NoiseConfig, OverheadConfig};
    use crate::trace::Trace;
    use crate::types::{CpuId, DsqId, KickFlags, Pid};
    use crate::SIM_LOCK;
    use rand::SeedableRng;

    /// Create a minimal SimulatorState for unit testing.
    fn test_state(nr_cpus: u32) -> SimulatorState {
        SimulatorState {
            cpus: (0..nr_cpus).map(|i| SimCpu::new(CpuId(i))).collect(),
            dsqs: DsqManager::new(),
            current_cpu: CpuId(0),
            trace: Trace::new(nr_cpus, &[]),
            clock: 0,
            task_raw_to_pid: HashMap::new(),
            task_pid_to_raw: HashMap::new(),
            task_last_cpu: HashMap::new(),
            task_ops_state: BTreeMap::new(),
            rng: SmallRng::seed_from_u64(0xDEAD_BEEF),
            ops_context: OpsContext::None,
            pending_dispatches: VecDeque::new(),
            dsq_iter: None,
            staged_events: Vec::new(),
            reenqueue_local_requested: false,
            pending_timers: [None; MAX_BPF_TIMERS],
            waker_task_raw: None,
            idle_task_raw: ptr::null_mut(),
            noise: NoiseConfig {
                enabled: false,
                ..Default::default()
            },
            overhead: OverheadConfig {
                enabled: false,
                ..Default::default()
            },
            rbc_counter: None,
            sched_overhead_rbc_ns: None,
            rbc_kfunc_calls: 0,
            rbc_kfunc_ns: 0,
            rbc_e9_snapshot: 0,
            rbc_e9_last_kfunc: 0,
            rbc_pmu_last_kfunc: 0,
            longest_structop_rbc: 0,
            longest_rbc_interval: 0,
            bpf_error: None,
            interleave: false,
            preemptive: None,
            replay_trace: None,
            replay_backend: None,
            e9_replay_backend: None,
            e9_fns: None,
            structop_accum: vec![crate::preempt::StructopInfo::default(); nr_cpus as usize],
            native_concurrent: None,
            stochastic_timer_interleave: false,
            stochastic_timer_interleave_window_ns: 0,
            stochastic_timer_interleave_one_in: 0,
            bw_blocked: BTreeMap::new(),
            targeted_cbw_yield_sites: false,
            targeted_cbw_yield_window_ns: 0,
            targeted_cbw_yield_limit: 0,
            targeted_cbw_yield_count: 0,
        }
    }

    /// Wrap a `SimulatorState` in `Arc<Mutex<SimState>>` for unit tests.
    fn test_sim_arc(state: SimulatorState) -> SimArc {
        let nr_cpus = state.cpus.len() as u32;
        Arc::new(Mutex::new(SimState {
            sim: state,
            tasks: HashMap::new(),
            events: EventQueue::new(0, false),
            cgroup_registry: CgroupRegistry::new(nr_cpus, 100),
            task_to_cgid: HashMap::new(),
        }))
    }

    /// Install `SIM_ARC` and set up `SIM_CONTEXT` for a test kfunc call.
    fn enter_test_sim(arc: &SimArc, cpu: CpuId) {
        {
            let mut guard = arc.lock().unwrap();
            guard.sim.current_cpu = cpu;
            set_sim_clock(guard.sim.cpus[cpu.0 as usize].local_clock, Some(cpu));
        }
        install_sim_arc(arc);
    }

    /// Clear `SIM_ARC` after test kfunc calls complete.
    fn exit_test_sim() {
        clear_sim_arc();
    }

    fn assert_pending_dispatch_at(
        arc: &SimArc,
        idx: usize,
        pid: Pid,
        dsq_id: DsqId,
        vtime: Option<Vtime>,
    ) {
        let guard = arc.lock().unwrap();
        let pd = guard.sim.pending_dispatches.get(idx).unwrap();
        assert_eq!(pd.pid, pid);
        assert_eq!(pd.dsq_id, dsq_id);
        assert_eq!(pd.vtime, vtime);
    }

    fn assert_single_pending_dispatch(arc: &SimArc, pid: Pid, dsq_id: DsqId, vtime: Option<Vtime>) {
        assert_eq!(arc.lock().unwrap().sim.pending_dispatches.len(), 1);
        assert_pending_dispatch_at(arc, 0, pid, dsq_id, vtime);
    }

    fn clear_pending_dispatch(arc: &SimArc) {
        arc.lock().unwrap().sim.pending_dispatches.clear();
    }

    /// Allocate a C task_struct and register it in the state's pointer maps.
    ///
    /// Returns the raw pointer. Caller must call `ffi::sim_task_free` when done.
    fn register_task(state: &mut SimulatorState, pid: Pid) -> *mut c_void {
        let raw = unsafe { ffi::sim_task_alloc() };
        assert!(!raw.is_null());
        unsafe { ffi::sim_task_set_pid(raw, pid.0) };
        state.task_raw_to_pid.insert(raw as usize, pid);
        state.task_pid_to_raw.insert(pid, raw as usize);
        raw
    }

    /// Free a previously registered task.
    fn free_task(state: &mut SimulatorState, pid: Pid) {
        if let Some(raw) = state.task_pid_to_raw.remove(&pid) {
            state.task_raw_to_pid.remove(&raw);
            unsafe { ffi::sim_task_free(raw as *mut c_void) };
        }
    }

    /// Run a closure with SimulatorState installed via the SIM_ARC path.
    #[allow(dead_code)]
    fn with_state<F, R>(arc: &SimArc, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(arc, cpu);
        let result = f();
        exit_test_sim();
        result
    }

    // -----------------------------------------------------------------------
    // DSQ creation and querying
    // -----------------------------------------------------------------------

    #[test]
    fn test_create_dsq() {
        let _lock = SIM_LOCK.lock().unwrap();
        let state = test_state(1);

        let arc = test_sim_arc(state);

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);
        assert_eq!(scx_bpf_create_dsq(42, -1), 0);
        // Creating the same DSQ again should fail
        assert_eq!(scx_bpf_create_dsq(42, -1), -1);
        exit_test_sim();

        assert_eq!(arc.lock().unwrap().sim.dsqs.nr_queued(DsqId(42)), 0);
    }

    #[test]
    fn test_dsq_nr_queued_empty() {
        let _lock = SIM_LOCK.lock().unwrap();
        let mut state = test_state(1);
        state.dsqs.create(DsqId(100));

        let arc = test_sim_arc(state);

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);
        assert_eq!(scx_bpf_dsq_nr_queued(100), 0);
        exit_test_sim();
    }

    #[test]
    fn test_dsq_nr_queued_with_tasks() {
        let _lock = SIM_LOCK.lock().unwrap();
        let mut state = test_state(1);
        state.dsqs.create(DsqId(100));
        state.dsqs.insert_fifo(DsqId(100), Pid(1));
        state.dsqs.insert_fifo(DsqId(100), Pid(2));

        let arc = test_sim_arc(state);

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);
        assert_eq!(scx_bpf_dsq_nr_queued(100), 2);
        exit_test_sim();
    }

    // -----------------------------------------------------------------------
    // DSQ insert (deferred dispatch)
    // -----------------------------------------------------------------------

    #[test]
    fn test_dsq_insert_deferred() {
        let _lock = SIM_LOCK.lock().unwrap();
        let state = test_state(1);
        let arc = test_sim_arc(state);

        let p = register_task(&mut arc.lock().unwrap().sim, Pid(1));

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);
        scx_bpf_dsq_insert(p, DsqId::GLOBAL.0, 5_000_000, 0);
        exit_test_sim();

        // Insert is deferred, not immediate
        let guard = arc.lock().unwrap();
        assert_eq!(guard.sim.pending_dispatches.len(), 1);
        let pd = guard.sim.pending_dispatches.front().unwrap();
        assert_eq!(pd.pid, Pid(1));
        assert_eq!(pd.dsq_id, DsqId::GLOBAL);
        assert!(pd.vtime.is_none());
        // DSQ should still be empty until resolved
        assert_eq!(guard.sim.dsqs.nr_queued(DsqId::GLOBAL), 0);
        drop(guard);

        free_task(&mut arc.lock().unwrap().sim, Pid(1));
    }

    #[test]
    fn test_dsq_insert_vtime_deferred() {
        let _lock = SIM_LOCK.lock().unwrap();
        let mut state = test_state(1);
        state.dsqs.create(DsqId(50));
        let arc = test_sim_arc(state);

        let p = register_task(&mut arc.lock().unwrap().sim, Pid(3));

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);
        scx_bpf_dsq_insert_vtime(p, 50, 5_000_000, 1000, 0);
        exit_test_sim();

        let guard = arc.lock().unwrap();
        let pd = guard.sim.pending_dispatches.front().unwrap();
        assert_eq!(pd.pid, Pid(3));
        assert_eq!(pd.dsq_id, DsqId(50));
        assert_eq!(pd.vtime, Some(Vtime(1000)));

        drop(guard);
        free_task(&mut arc.lock().unwrap().sim, Pid(3));
    }

    #[test]
    fn test_dsq_insert_compat_aliases_deferred() {
        let _lock = SIM_LOCK.lock().unwrap();
        let state = test_state(1);
        let arc = test_sim_arc(state);

        let p = register_task(&mut arc.lock().unwrap().sim, Pid(7));

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);

        assert!(scx_bpf_dsq_insert___v2___compat(
            p,
            DsqId::GLOBAL.0,
            5_000_000,
            0
        ));
        assert_single_pending_dispatch(&arc, Pid(7), DsqId::GLOBAL, None);
        clear_pending_dispatch(&arc);

        scx_bpf_dsq_insert___v1(p, DsqId::GLOBAL.0, 5_000_000, 0);
        assert_single_pending_dispatch(&arc, Pid(7), DsqId::GLOBAL, None);
        clear_pending_dispatch(&arc);

        scx_bpf_dispatch___compat(p, DsqId::LOCAL.0, 5_000_000, 0);
        assert_single_pending_dispatch(&arc, Pid(7), DsqId::LOCAL, None);

        exit_test_sim();
        free_task(&mut arc.lock().unwrap().sim, Pid(7));
    }

    #[test]
    fn test_dsq_insert_vtime_compat_aliases_deferred() {
        let _lock = SIM_LOCK.lock().unwrap();
        let mut state = test_state(1);
        state.dsqs.create(DsqId(50));
        let arc = test_sim_arc(state);

        let p = register_task(&mut arc.lock().unwrap().sim, Pid(8));
        let args = ScxBpfDsqInsertVtimeArgs {
            dsq_id: 50,
            slice: 5_000_000,
            vtime: 1234,
            enq_flags: 0,
        };

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);

        assert!(__scx_bpf_dsq_insert_vtime(p, &args));
        assert_single_pending_dispatch(&arc, Pid(8), DsqId(50), Some(Vtime(1234)));
        clear_pending_dispatch(&arc);

        scx_bpf_dsq_insert_vtime___compat(p, 50, 5_000_000, 5678, 0);
        assert_single_pending_dispatch(&arc, Pid(8), DsqId(50), Some(Vtime(5678)));
        clear_pending_dispatch(&arc);

        scx_bpf_dispatch_vtime___compat(p, 50, 5_000_000, 9012, 0);
        assert_single_pending_dispatch(&arc, Pid(8), DsqId(50), Some(Vtime(9012)));

        exit_test_sim();
        free_task(&mut arc.lock().unwrap().sim, Pid(8));
    }

    #[test]
    fn test_resolve_pending_dispatch_multiple_in_order() {
        let _lock = SIM_LOCK.lock().unwrap();
        let mut state = test_state(2);
        state.dsqs.create(DsqId(50));
        let arc = test_sim_arc(state);

        let p1 = register_task(&mut arc.lock().unwrap().sim, Pid(1));
        let p2 = register_task(&mut arc.lock().unwrap().sim, Pid(2));
        let p3 = register_task(&mut arc.lock().unwrap().sim, Pid(3));

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);
        scx_bpf_dsq_insert(p1, DsqId::GLOBAL.0, 5_000_000, 0);
        scx_bpf_dsq_insert(p2, DsqId::LOCAL.0, 5_000_000, 0);
        scx_bpf_dsq_insert_vtime(p3, 50, 5_000_000, 1234, 0);
        exit_test_sim();

        assert_pending_dispatch_at(&arc, 0, Pid(1), DsqId::GLOBAL, None);
        assert_pending_dispatch_at(&arc, 1, Pid(2), DsqId::LOCAL, None);
        assert_pending_dispatch_at(&arc, 2, Pid(3), DsqId(50), Some(Vtime(1234)));

        let mut guard = arc.lock().unwrap();
        let result = guard.sim.resolve_pending_dispatch(CpuId(1));
        assert_eq!(result, Some(CpuId(1)));
        assert!(guard.sim.pending_dispatches.is_empty());
        assert_eq!(guard.sim.dsqs.ordered_pids(DsqId::GLOBAL), vec![Pid(1)]);
        assert_eq!(
            guard.sim.cpus[1]
                .local_dsq
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![Pid(2)]
        );
        assert_eq!(guard.sim.dsqs.ordered_pids(DsqId(50)), vec![Pid(3)]);

        drop(guard);
        for pid in [Pid(1), Pid(2), Pid(3)] {
            free_task(&mut arc.lock().unwrap().sim, pid);
        }
    }

    #[test]
    fn test_reenqueue_local_compat_aliases_set_flag() {
        let _lock = SIM_LOCK.lock().unwrap();
        let arc = test_sim_arc(test_state(1));

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);

        assert_eq!(scx_bpf_reenqueue_local___v1(), 0);
        assert!(arc.lock().unwrap().sim.reenqueue_local_requested);
        arc.lock().unwrap().sim.reenqueue_local_requested = false;

        scx_bpf_reenqueue_local___v2___compat();
        assert!(arc.lock().unwrap().sim.reenqueue_local_requested);

        exit_test_sim();
    }

    #[test]
    fn test_select_cpu_and_compat_alias_prefers_idle_prev() {
        let _lock = SIM_LOCK.lock().unwrap();
        let arc = test_sim_arc(test_state(2));

        let p = register_task(&mut arc.lock().unwrap().sim, Pid(9));
        let prev_cpu = arc.lock().unwrap().sim.current_cpu.0 as i32;

        enter_test_sim(&arc, CpuId(prev_cpu as u32));
        assert_eq!(
            scx_bpf_select_cpu_and___compat(p, prev_cpu, 0, std::ptr::null(), 0),
            prev_cpu
        );
        exit_test_sim();

        free_task(&mut arc.lock().unwrap().sim, Pid(9));
    }

    #[test]
    fn test_resolve_pending_dispatch_global() {
        let _lock = SIM_LOCK.lock().unwrap();
        let state = test_state(2);
        let arc = test_sim_arc(state);

        let p = register_task(&mut arc.lock().unwrap().sim, Pid(1));

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);
        scx_bpf_dsq_insert(p, DsqId::GLOBAL.0, 5_000_000, 0);
        exit_test_sim();

        let mut guard = arc.lock().unwrap();
        let result = guard.sim.resolve_pending_dispatch(CpuId(0));
        // Global DSQ dispatch returns None (not a local dispatch)
        assert!(result.is_none());
        assert_eq!(guard.sim.dsqs.nr_queued(DsqId::GLOBAL), 1);

        drop(guard);
        free_task(&mut arc.lock().unwrap().sim, Pid(1));
    }

    #[test]
    fn test_resolve_pending_dispatch_local() {
        let _lock = SIM_LOCK.lock().unwrap();
        let state = test_state(2);
        let arc = test_sim_arc(state);

        let p = register_task(&mut arc.lock().unwrap().sim, Pid(1));

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);
        scx_bpf_dsq_insert(p, DsqId::LOCAL.0, 5_000_000, 0);
        exit_test_sim();

        let mut guard = arc.lock().unwrap();
        let result = guard.sim.resolve_pending_dispatch(CpuId(1));
        // Local dispatch resolves to the specified CPU
        assert_eq!(result, Some(CpuId(1)));
        assert_eq!(guard.sim.cpus[1].local_dsq.len(), 1);
        assert_eq!(guard.sim.cpus[1].local_dsq[0], Pid(1));

        drop(guard);
        free_task(&mut arc.lock().unwrap().sim, Pid(1));
    }

    /// Test that SCX_DSQ_LOCAL_ON dispatch to valid CPU succeeds.
    #[test]
    fn test_resolve_pending_dispatch_local_on_valid() {
        let _lock = SIM_LOCK.lock().unwrap();
        let state = test_state(4);
        let arc = test_sim_arc(state);

        let p = register_task(&mut arc.lock().unwrap().sim, Pid(1));

        // Create LOCAL_ON DSQ for CPU 2
        let local_on_dsq = DsqId::LOCAL_ON_MASK | 2;

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);
        scx_bpf_dsq_insert(p, local_on_dsq, 5_000_000, 0);
        exit_test_sim();

        // Task has no cpumask restriction (cpus_ptr is null), so all CPUs allowed
        let mut guard = arc.lock().unwrap();
        let result = guard.sim.resolve_pending_dispatch(CpuId(0));
        // LOCAL_ON dispatch resolves to the specified target CPU
        assert_eq!(result, Some(CpuId(2)));
        assert_eq!(guard.sim.cpus[2].local_dsq.len(), 1);
        assert_eq!(guard.sim.cpus[2].local_dsq[0], Pid(1));
        assert!(guard.sim.bpf_error.is_none());

        drop(guard);
        free_task(&mut arc.lock().unwrap().sim, Pid(1));
    }

    /// Test that SCX_DSQ_LOCAL_ON dispatch to a CPU outside cpumask fails.
    #[test]
    fn test_resolve_pending_dispatch_local_on_cpumask_violation() {
        let _lock = SIM_LOCK.lock().unwrap();
        let state = test_state(4);
        let arc = test_sim_arc(state);

        let p = register_task(&mut arc.lock().unwrap().sim, Pid(1));

        // Set up cpumask: task can only run on CPUs 0 and 1
        unsafe {
            ffi::sim_task_clear_cpumask(p);
            ffi::sim_task_set_cpumask_cpu(p, 0);
            ffi::sim_task_set_cpumask_cpu(p, 1);
        }

        // Try to dispatch to CPU 3 (not in cpumask)
        let local_on_dsq = DsqId::LOCAL_ON_MASK | 3;

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);
        scx_bpf_dsq_insert(p, local_on_dsq, 5_000_000, 0);
        exit_test_sim();

        let mut guard = arc.lock().unwrap();
        let result = guard.sim.resolve_pending_dispatch(CpuId(0));

        // Dispatch should be rejected
        assert_eq!(result, None);
        // CPU 3's local DSQ should be empty
        assert!(guard.sim.cpus[3].local_dsq.is_empty());
        // BPF error should be set
        assert!(guard.sim.bpf_error.is_some());
        let error_msg = guard.sim.bpf_error.as_ref().unwrap();
        assert!(
            error_msg.contains("SCX_DSQ_LOCAL_ON"),
            "error should mention SCX_DSQ_LOCAL_ON: {}",
            error_msg
        );
        assert!(
            error_msg.contains("cpumask"),
            "error should mention cpumask: {}",
            error_msg
        );

        // Verify trace event was recorded
        let events = guard.sim.trace.events();
        let dispatch_reject = events.iter().find(|e| {
            matches!(
                e.kind,
                TraceKind::DispatchRejected {
                    pid,
                    target_cpu,
                    reason: DispatchRejectReason::CpumaskViolation
                } if pid == Pid(1) && target_cpu == CpuId(3)
            )
        });
        assert!(
            dispatch_reject.is_some(),
            "DispatchRejected trace event should be recorded"
        );

        drop(guard);
        free_task(&mut arc.lock().unwrap().sim, Pid(1));
    }

    /// Test that SCX_DSQ_LOCAL_ON dispatch to a CPU in cpumask succeeds.
    #[test]
    fn test_resolve_pending_dispatch_local_on_cpumask_valid() {
        let _lock = SIM_LOCK.lock().unwrap();
        let state = test_state(4);
        let arc = test_sim_arc(state);

        let p = register_task(&mut arc.lock().unwrap().sim, Pid(1));

        // Set up cpumask: task can only run on CPUs 0 and 2
        unsafe {
            ffi::sim_task_clear_cpumask(p);
            ffi::sim_task_set_cpumask_cpu(p, 0);
            ffi::sim_task_set_cpumask_cpu(p, 2);
        }

        // Dispatch to CPU 2 (in cpumask)
        let local_on_dsq = DsqId::LOCAL_ON_MASK | 2;

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);
        scx_bpf_dsq_insert(p, local_on_dsq, 5_000_000, 0);
        exit_test_sim();

        let mut guard = arc.lock().unwrap();
        let result = guard.sim.resolve_pending_dispatch(CpuId(0));

        // Dispatch should succeed
        assert_eq!(result, Some(CpuId(2)));
        assert_eq!(guard.sim.cpus[2].local_dsq.len(), 1);
        assert_eq!(guard.sim.cpus[2].local_dsq[0], Pid(1));
        assert!(guard.sim.bpf_error.is_none());

        drop(guard);
        free_task(&mut arc.lock().unwrap().sim, Pid(1));
    }

    /// Test SCX_DSQ_LOCAL_ON dispatch to different CPU for migration-disabled task fails.
    ///
    /// This reproduces the production LAVD bug:
    /// ```text
    /// kworker/8:1[3065910] triggered exit kind 1024:
    ///   runtime error (SCX_DSQ_LOCAL[_ON] cannot move migration disabled
    ///   kworker/u208:2[4181193] from CPU 8 to 31)
    /// ```
    ///
    /// A migration-disabled task (migration_disabled > 0) cannot be dispatched
    /// to a different CPU than its last known CPU, even if that CPU is in the
    /// task's cpumask.
    #[test]
    fn test_resolve_pending_dispatch_local_on_migration_disabled_violation() {
        let _lock = SIM_LOCK.lock().unwrap();
        let state = test_state(32);
        let arc = test_sim_arc(state);

        let p = register_task(&mut arc.lock().unwrap().sim, Pid(1));

        // Task was last running on CPU 8
        arc.lock()
            .unwrap()
            .sim
            .task_last_cpu
            .insert(Pid(1), CpuId(8));

        // Task has migration_disabled > 0 (like a kworker in BPF code)
        // In production, migration_disabled > 1 means pre-existing disable
        // (BPF prolog adds 1, so >1 means it was already disabled before BPF)
        unsafe {
            ffi::sim_task_set_migration_disabled(p, 2);
        }

        // Try to dispatch to CPU 31 (different from last CPU 8)
        let local_on_dsq = DsqId::LOCAL_ON_MASK | 31;

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);
        scx_bpf_dsq_insert(p, local_on_dsq, 5_000_000, 0);
        exit_test_sim();

        let mut guard = arc.lock().unwrap();
        let result = guard.sim.resolve_pending_dispatch(CpuId(0));

        // Dispatch should be rejected
        assert_eq!(result, None);
        // CPU 31's local DSQ should be empty
        assert!(guard.sim.cpus[31].local_dsq.is_empty());
        // BPF error should be set
        assert!(guard.sim.bpf_error.is_some());
        let error_msg = guard.sim.bpf_error.as_ref().unwrap();
        assert!(
            error_msg.contains("SCX_DSQ_LOCAL_ON"),
            "error should mention SCX_DSQ_LOCAL_ON: {}",
            error_msg
        );
        assert!(
            error_msg.contains("migration disabled"),
            "error should mention migration disabled: {}",
            error_msg
        );

        // Verify trace event was recorded with correct reason
        let events = guard.sim.trace.events();
        let dispatch_reject = events.iter().find(|e| {
            matches!(
                e.kind,
                TraceKind::DispatchRejected {
                    pid,
                    target_cpu,
                    reason: DispatchRejectReason::MigrationDisabled
                } if pid == Pid(1) && target_cpu == CpuId(31)
            )
        });
        assert!(
            dispatch_reject.is_some(),
            "DispatchRejected with MigrationDisabled reason should be recorded"
        );

        drop(guard);
        free_task(&mut arc.lock().unwrap().sim, Pid(1));
    }

    /// Test SCX_DSQ_LOCAL_ON dispatch to same CPU for migration-disabled task succeeds.
    ///
    /// A migration-disabled task CAN be dispatched to its last known CPU.
    /// Only cross-CPU dispatch is rejected.
    #[test]
    fn test_resolve_pending_dispatch_local_on_migration_disabled_same_cpu_ok() {
        let _lock = SIM_LOCK.lock().unwrap();
        let state = test_state(16);
        let arc = test_sim_arc(state);

        let p = register_task(&mut arc.lock().unwrap().sim, Pid(1));

        // Task was last running on CPU 8
        arc.lock()
            .unwrap()
            .sim
            .task_last_cpu
            .insert(Pid(1), CpuId(8));

        // Task has migration_disabled > 0
        unsafe {
            ffi::sim_task_set_migration_disabled(p, 2);
        }

        // Dispatch to CPU 8 (same as last CPU) - should succeed
        let local_on_dsq = DsqId::LOCAL_ON_MASK | 8;

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);
        scx_bpf_dsq_insert(p, local_on_dsq, 5_000_000, 0);
        exit_test_sim();

        let mut guard = arc.lock().unwrap();
        let result = guard.sim.resolve_pending_dispatch(CpuId(0));

        // Dispatch should succeed
        assert_eq!(result, Some(CpuId(8)));
        assert_eq!(guard.sim.cpus[8].local_dsq.len(), 1);
        assert_eq!(guard.sim.cpus[8].local_dsq[0], Pid(1));
        assert!(guard.sim.bpf_error.is_none());

        drop(guard);
        free_task(&mut arc.lock().unwrap().sim, Pid(1));
    }

    /// Test that migration_disabled=0 tasks can move to different CPUs.
    ///
    /// Only migration_disabled > 0 restricts cross-CPU dispatch.
    #[test]
    fn test_resolve_pending_dispatch_local_on_migration_enabled_can_move() {
        let _lock = SIM_LOCK.lock().unwrap();
        let state = test_state(16);
        let arc = test_sim_arc(state);

        let p = register_task(&mut arc.lock().unwrap().sim, Pid(1));

        // Task was last running on CPU 8
        arc.lock()
            .unwrap()
            .sim
            .task_last_cpu
            .insert(Pid(1), CpuId(8));

        // Task has migration_disabled = 0 (migration enabled)
        unsafe {
            ffi::sim_task_set_migration_disabled(p, 0);
        }

        // Dispatch to CPU 12 (different from last CPU 8) - should succeed
        let local_on_dsq = DsqId::LOCAL_ON_MASK | 12;

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);
        scx_bpf_dsq_insert(p, local_on_dsq, 5_000_000, 0);
        exit_test_sim();

        let mut guard = arc.lock().unwrap();
        let result = guard.sim.resolve_pending_dispatch(CpuId(0));

        // Dispatch should succeed (migration is enabled)
        assert_eq!(result, Some(CpuId(12)));
        assert_eq!(guard.sim.cpus[12].local_dsq.len(), 1);
        assert_eq!(guard.sim.cpus[12].local_dsq[0], Pid(1));
        assert!(guard.sim.bpf_error.is_none());

        drop(guard);
        free_task(&mut arc.lock().unwrap().sim, Pid(1));
    }

    // -----------------------------------------------------------------------
    // DSQ move_to_local
    // -----------------------------------------------------------------------

    #[test]
    fn test_dsq_move_to_local() {
        let _lock = SIM_LOCK.lock().unwrap();
        let mut state = test_state(2);
        state.dsqs.create(DsqId(77));
        state.dsqs.insert_fifo(DsqId(77), Pid(10));
        state.dsqs.insert_fifo(DsqId(77), Pid(11));
        state.current_cpu = CpuId(1);

        let arc = test_sim_arc(state);

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);
        let moved = scx_bpf_dsq_move_to_local(77);
        exit_test_sim();

        assert!(moved);
        assert_eq!(arc.lock().unwrap().sim.cpus[1].local_dsq.len(), 1);
        assert_eq!(arc.lock().unwrap().sim.cpus[1].local_dsq[0], Pid(10));
        assert_eq!(arc.lock().unwrap().sim.dsqs.nr_queued(DsqId(77)), 1);
    }

    #[test]
    fn test_dsq_move_to_local_empty() {
        let _lock = SIM_LOCK.lock().unwrap();
        let mut state = test_state(1);
        state.dsqs.create(DsqId(77));

        let arc = test_sim_arc(state);

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);
        let moved = scx_bpf_dsq_move_to_local(77);
        exit_test_sim();

        assert!(!moved);
        assert!(arc.lock().unwrap().sim.cpus[0].local_dsq.is_empty());
    }

    // -----------------------------------------------------------------------
    // CPU selection
    // -----------------------------------------------------------------------

    #[test]
    fn test_select_cpu_dfl_prefers_prev_if_idle() {
        let _lock = SIM_LOCK.lock().unwrap();
        let state = test_state(4);
        // All CPUs idle, prev_cpu=2 should be returned
        let arc = test_sim_arc(state);

        let p = register_task(&mut arc.lock().unwrap().sim, Pid(1));

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);
        let mut is_idle = false;
        let cpu = scx_bpf_select_cpu_dfl(p, 2, 0, &mut is_idle);
        exit_test_sim();

        assert_eq!(cpu, 2);
        assert!(is_idle);

        free_task(&mut arc.lock().unwrap().sim, Pid(1));
    }

    #[test]
    fn test_select_cpu_dfl_finds_other_idle() {
        let _lock = SIM_LOCK.lock().unwrap();
        let mut state = test_state(3);
        // Make prev_cpu busy, leave others idle
        state.cpus[1].current_task = Some(Pid(99));
        let arc = test_sim_arc(state);

        let p = register_task(&mut arc.lock().unwrap().sim, Pid(1));

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);
        let mut is_idle = false;
        let cpu = scx_bpf_select_cpu_dfl(p, 1, 0, &mut is_idle);
        exit_test_sim();

        assert!(is_idle);
        assert_ne!(cpu, 1); // Should pick a different idle CPU

        free_task(&mut arc.lock().unwrap().sim, Pid(1));
    }

    #[test]
    fn test_select_cpu_dfl_no_idle() {
        let _lock = SIM_LOCK.lock().unwrap();
        let mut state = test_state(2);
        // All CPUs busy
        state.cpus[0].current_task = Some(Pid(10));
        state.cpus[1].current_task = Some(Pid(11));
        let arc = test_sim_arc(state);

        let p = register_task(&mut arc.lock().unwrap().sim, Pid(1));

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);
        let mut is_idle = false;
        let cpu = scx_bpf_select_cpu_dfl(p, 0, 0, &mut is_idle);
        exit_test_sim();

        assert!(!is_idle);
        assert_eq!(cpu, 0); // Falls back to prev_cpu

        free_task(&mut arc.lock().unwrap().sim, Pid(1));
    }

    // -----------------------------------------------------------------------
    // Clock and timing
    // -----------------------------------------------------------------------

    #[test]
    fn test_scx_bpf_now() {
        let _lock = SIM_LOCK.lock().unwrap();
        let mut state = test_state(2);
        state.current_cpu = CpuId(1);
        state.cpus[1].local_clock = 42_000_000;

        let arc = test_sim_arc(state);

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);
        let now = scx_bpf_now();
        exit_test_sim();

        assert_eq!(now, 42_000_000);
    }

    #[test]
    fn test_bpf_ktime_get_ns() {
        let _lock = SIM_LOCK.lock().unwrap();
        let mut state = test_state(1);
        state.cpus[0].local_clock = 99_000;

        let arc = test_sim_arc(state);

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);
        let t = bpf_ktime_get_ns();
        exit_test_sim();

        assert_eq!(t, 99_000);
    }

    // -----------------------------------------------------------------------
    // CPU ID helpers
    // -----------------------------------------------------------------------

    #[test]
    fn test_get_smp_processor_id() {
        let _lock = SIM_LOCK.lock().unwrap();
        let mut state = test_state(4);
        state.current_cpu = CpuId(3);

        let arc = test_sim_arc(state);

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);
        assert_eq!(bpf_get_smp_processor_id(), 3);
        assert_eq!(sim_bpf_get_smp_processor_id(), 3);
        exit_test_sim();
    }

    #[test]
    fn test_nr_cpu_ids() {
        let _lock = SIM_LOCK.lock().unwrap();
        let state = test_state(8);

        let arc = test_sim_arc(state);

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);
        assert_eq!(scx_bpf_nr_cpu_ids(), 8);
        exit_test_sim();
    }

    // -----------------------------------------------------------------------
    // PRNG
    // -----------------------------------------------------------------------

    #[test]
    fn test_prng_deterministic() {
        let _lock = SIM_LOCK.lock().unwrap();
        let mut state = test_state(1);
        state.rng = SmallRng::seed_from_u64(12345);

        let arc = test_sim_arc(state);

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);
        let a = sim_bpf_get_prandom_u32();
        let b = sim_bpf_get_prandom_u32();
        exit_test_sim();

        // Replay with same seed
        {
            let mut guard = arc.lock().unwrap();
            guard.sim.rng = SmallRng::seed_from_u64(12345);
        }

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);
        let a2 = sim_bpf_get_prandom_u32();
        let b2 = sim_bpf_get_prandom_u32();
        exit_test_sim();

        assert_eq!(a, a2);
        assert_eq!(b, b2);
        assert_ne!(a, b); // Successive values should differ
    }

    // -----------------------------------------------------------------------
    // Task lookup
    // -----------------------------------------------------------------------

    #[test]
    fn test_bpf_task_from_pid() {
        let _lock = SIM_LOCK.lock().unwrap();
        let state = test_state(1);
        let arc = test_sim_arc(state);

        let raw = register_task(&mut arc.lock().unwrap().sim, Pid(42));

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);
        let found = bpf_task_from_pid(42);
        let not_found = bpf_task_from_pid(999);
        exit_test_sim();

        assert_eq!(found, raw);
        assert!(not_found.is_null());

        free_task(&mut arc.lock().unwrap().sim, Pid(42));
    }

    #[test]
    fn test_nr_cpus_allowed_roundtrip() {
        let _lock = SIM_LOCK.lock().unwrap();
        let raw = unsafe { ffi::sim_task_alloc() };
        assert!(!raw.is_null());

        // Default after calloc should be 0
        let initial = unsafe { ffi::sim_task_get_nr_cpus_allowed(raw) };
        assert_eq!(
            initial, 0,
            "calloc-zeroed task should have nr_cpus_allowed=0"
        );

        // Set to 4 (like SimTask::new does)
        unsafe { ffi::sim_task_set_nr_cpus_allowed(raw, 4) };
        assert_eq!(unsafe { ffi::sim_task_get_nr_cpus_allowed(raw) }, 4);

        // Override to 1 (like engine does for pinned tasks)
        unsafe { ffi::sim_task_set_nr_cpus_allowed(raw, 1) };
        assert_eq!(unsafe { ffi::sim_task_get_nr_cpus_allowed(raw) }, 1);

        unsafe { ffi::sim_task_free(raw) };
    }

    #[test]
    fn test_get_current_task_btf() {
        let _lock = SIM_LOCK.lock().unwrap();
        let state = test_state(2);
        let arc = test_sim_arc(state);

        let raw = register_task(&mut arc.lock().unwrap().sim, Pid(7));
        arc.lock().unwrap().sim.cpus[0].current_task = Some(Pid(7));
        arc.lock().unwrap().sim.current_cpu = CpuId(0);

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);
        let current = bpf_get_current_task_btf();
        exit_test_sim();

        assert_eq!(current, raw);

        free_task(&mut arc.lock().unwrap().sim, Pid(7));
    }

    #[test]
    fn test_get_current_task_btf_idle() {
        let _lock = SIM_LOCK.lock().unwrap();
        let state = test_state(1);
        // No task running on CPU 0

        let arc = test_sim_arc(state);

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);
        let current = bpf_get_current_task_btf();
        exit_test_sim();

        assert!(current.is_null());
    }

    // -----------------------------------------------------------------------
    // scx_bpf_cpu_curr
    // -----------------------------------------------------------------------

    #[test]
    fn test_cpu_curr() {
        let _lock = SIM_LOCK.lock().unwrap();
        let state = test_state(2);
        let arc = test_sim_arc(state);

        let raw = register_task(&mut arc.lock().unwrap().sim, Pid(5));
        arc.lock().unwrap().sim.cpus[1].current_task = Some(Pid(5));

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);
        let p = scx_bpf_cpu_curr(1);
        let idle = scx_bpf_cpu_curr(0);
        let oob = scx_bpf_cpu_curr(99);
        exit_test_sim();

        assert_eq!(p, raw);
        assert!(idle.is_null());
        assert!(oob.is_null());

        free_task(&mut arc.lock().unwrap().sim, Pid(5));
    }

    // -----------------------------------------------------------------------
    // Kick CPU
    // -----------------------------------------------------------------------

    #[test]
    fn test_kick_cpu() {
        let _lock = SIM_LOCK.lock().unwrap();
        let state = test_state(4);

        let arc = test_sim_arc(state);

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);
        scx_bpf_kick_cpu(1, 0);
        scx_bpf_kick_cpu(3, 2); // SCX_KICK_PREEMPT
        scx_bpf_kick_cpu(1, 2); // Second kick to same CPU
        exit_test_sim();

        // Kicks are now staged events, not in kicked_cpus
        let guard = arc.lock().unwrap();
        assert_eq!(guard.sim.staged_events.len(), 3);
        // All staged events target the correct CPUs with correct flags
        let events: Vec<_> = guard
            .sim
            .staged_events
            .iter()
            .map(|(_, ev)| match ev {
                StagedEvent::KickDelivered { cpu, flags } => (*cpu, *flags),
            })
            .collect();
        assert_eq!(events[0], (CpuId(1), KickFlags::from_raw(0)));
        assert_eq!(events[1], (CpuId(3), KickFlags::PREEMPT));
        assert_eq!(events[2], (CpuId(1), KickFlags::PREEMPT));
    }

    #[test]
    fn test_kick_cpu_out_of_range() {
        let _lock = SIM_LOCK.lock().unwrap();
        let state = test_state(2);

        let arc = test_sim_arc(state);

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);
        scx_bpf_kick_cpu(99, 0);
        exit_test_sim();

        assert!(arc.lock().unwrap().sim.staged_events.is_empty());
    }

    // -----------------------------------------------------------------------
    // DSQ iterator
    // -----------------------------------------------------------------------

    #[test]
    fn test_dsq_iter_empty() {
        let _lock = SIM_LOCK.lock().unwrap();
        let mut state = test_state(1);
        state.dsqs.create(DsqId(200));

        let arc = test_sim_arc(state);

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);
        let first = sim_dsq_iter_begin(200, 0);
        exit_test_sim();

        assert!(first.is_null());
        assert!(arc.lock().unwrap().sim.dsq_iter.is_none());
    }

    #[test]
    fn test_dsq_iter_traversal() {
        let _lock = SIM_LOCK.lock().unwrap();
        let mut state = test_state(1);
        state.dsqs.create(DsqId(200));
        state.dsqs.insert_fifo(DsqId(200), Pid(1));
        state.dsqs.insert_fifo(DsqId(200), Pid(2));
        state.dsqs.insert_fifo(DsqId(200), Pid(3));

        let arc = test_sim_arc(state);

        let raw1 = register_task(&mut arc.lock().unwrap().sim, Pid(1));
        let raw2 = register_task(&mut arc.lock().unwrap().sim, Pid(2));
        let raw3 = register_task(&mut arc.lock().unwrap().sim, Pid(3));

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);

        let p1 = sim_dsq_iter_begin(200, 0);
        assert_eq!(p1, raw1);

        let p2 = sim_dsq_iter_next();
        assert_eq!(p2, raw2);

        let p3 = sim_dsq_iter_next();
        assert_eq!(p3, raw3);

        let end = sim_dsq_iter_next();
        assert!(end.is_null());

        exit_test_sim();

        free_task(&mut arc.lock().unwrap().sim, Pid(1));
        free_task(&mut arc.lock().unwrap().sim, Pid(2));
        free_task(&mut arc.lock().unwrap().sim, Pid(3));
    }

    // -----------------------------------------------------------------------
    // DSQ move during iteration
    // -----------------------------------------------------------------------

    #[test]
    fn test_dsq_move() {
        let _lock = SIM_LOCK.lock().unwrap();
        let mut state = test_state(1);
        state.dsqs.create(DsqId(300));
        state.dsqs.create(DsqId(301));
        state.dsqs.insert_fifo(DsqId(300), Pid(1));
        state.dsqs.insert_fifo(DsqId(300), Pid(2));

        let arc = test_sim_arc(state);

        let raw1 = register_task(&mut arc.lock().unwrap().sim, Pid(1));
        let _raw2 = register_task(&mut arc.lock().unwrap().sim, Pid(2));

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);

        // Start iterating DSQ 300
        let p = sim_dsq_iter_begin(300, 0);
        assert_eq!(p, raw1);

        // Move pid 1 from DSQ 300 to DSQ 301
        let moved = sim_scx_bpf_dsq_move(raw1, 301, 0);
        assert!(moved);

        exit_test_sim();

        assert_eq!(arc.lock().unwrap().sim.dsqs.nr_queued(DsqId(300)), 1); // pid 2 remains
        assert_eq!(arc.lock().unwrap().sim.dsqs.nr_queued(DsqId(301)), 1); // pid 1 moved here

        free_task(&mut arc.lock().unwrap().sim, Pid(1));
        free_task(&mut arc.lock().unwrap().sim, Pid(2));
    }

    // -----------------------------------------------------------------------
    // scx_bpf_task_running
    // -----------------------------------------------------------------------

    #[test]
    fn test_task_running() {
        let _lock = SIM_LOCK.lock().unwrap();
        let state = test_state(2);
        let arc = test_sim_arc(state);

        let raw_running = register_task(&mut arc.lock().unwrap().sim, Pid(1));
        let raw_idle = register_task(&mut arc.lock().unwrap().sim, Pid(2));
        arc.lock().unwrap().sim.cpus[0].current_task = Some(Pid(1));

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);
        assert!(scx_bpf_task_running(raw_running));
        assert!(!scx_bpf_task_running(raw_idle));
        exit_test_sim();

        free_task(&mut arc.lock().unwrap().sim, Pid(1));
        free_task(&mut arc.lock().unwrap().sim, Pid(2));
    }

    // -----------------------------------------------------------------------
    // Advance CPU clock
    // -----------------------------------------------------------------------

    #[test]
    fn test_advance_cpu_clock() {
        let _lock = SIM_LOCK.lock().unwrap();
        let mut state = test_state(2);
        state.clock = 100_000;
        state.cpus[0].local_clock = 50_000;
        state.cpus[1].local_clock = 200_000;

        state.advance_cpu_clock(CpuId(0));
        state.advance_cpu_clock(CpuId(1));

        // CPU 0 should advance to event clock (100k > 50k)
        assert_eq!(state.cpus[0].local_clock, 100_000);
        // CPU 1 stays at its higher local clock (200k > 100k)
        assert_eq!(state.cpus[1].local_clock, 200_000);
    }

    // -----------------------------------------------------------------------
    // RCU / ref-counting stubs (just verify they don't panic)
    // -----------------------------------------------------------------------

    #[test]
    fn test_rcu_stubs_no_panic() {
        bpf_rcu_read_lock();
        bpf_rcu_read_unlock();
    }

    #[test]
    fn test_task_release_no_panic() {
        bpf_task_release(ptr::null_mut());
    }

    #[test]
    fn test_put_cpumask_no_panic() {
        scx_bpf_put_cpumask(ptr::null());
        scx_bpf_put_idle_cpumask(ptr::null());
    }

    // -----------------------------------------------------------------------
    // Cgroup kfuncs
    // -----------------------------------------------------------------------

    #[test]
    fn test_cgroup_kfuncs() {
        let _lock = SIM_LOCK.lock().unwrap();
        let state = test_state(1);

        let arc = test_sim_arc(state);

        let cpu = arc.lock().unwrap().sim.current_cpu;
        enter_test_sim(&arc, cpu);

        // Null task pointer returns null
        assert!(scx_bpf_task_cgroup(ptr::null_mut(), 0).is_null());

        // bpf_cgroup_from_id with root cgroup ID returns non-null
        assert!(!bpf_cgroup_from_id(1).is_null());

        // Non-existent cgroup ID returns null
        assert!(bpf_cgroup_from_id(999).is_null());

        // Null cgroup pointer returns null
        assert!(bpf_cgroup_ancestor(ptr::null_mut(), 0).is_null());
        // Negative level returns null
        assert!(bpf_cgroup_ancestor(0x1234usize as *mut c_void, -1).is_null());

        assert!(
            bpf_cgrp_storage_get(ptr::null_mut(), ptr::null_mut(), ptr::null_mut(), 0).is_null()
        );
        assert!(
            bpf_task_storage_get(ptr::null_mut(), ptr::null_mut(), ptr::null_mut(), 0).is_null()
        );
        assert!(bpf_map_lookup_percpu_elem(ptr::null_mut(), ptr::null(), 0).is_null());

        exit_test_sim();
    }

    #[test]
    fn test_cgroup_acquire_release_passthrough() {
        let sentinel = 0x1234usize as *mut c_void;
        let acquired = bpf_cgroup_acquire(sentinel);
        assert_eq!(acquired, sentinel);
        bpf_cgroup_release(sentinel); // should not panic
    }

    // -----------------------------------------------------------------------
    // SDT / arena per-task storage
    // -----------------------------------------------------------------------

    #[test]
    fn test_sdt_task_alloc_data_free() {
        let _lock = SIM_LOCK.lock().unwrap();
        let state = test_state(1);
        let arc = test_sim_arc(state);

        let p = register_task(&mut arc.lock().unwrap().sim, Pid(100));

        // Initialize the allocator with 256 bytes per task
        let ret = unsafe { ffi::scx_task_init(256) };
        assert_eq!(ret, 0);

        // Allocate per-task data
        let data = unsafe { ffi::scx_task_alloc(p) };
        assert!(!data.is_null(), "scx_task_alloc must return non-null");

        // Retrieve the same data
        let data2 = unsafe { ffi::scx_task_data(p) };
        assert_eq!(data, data2, "scx_task_data must return same pointer");

        // Write to the allocated memory to verify it's valid
        unsafe { *(data as *mut u64) = 0xDEADBEEF };
        let read_back = unsafe { *(data2 as *mut u64) };
        assert_eq!(read_back, 0xDEADBEEF);

        // Free the data
        unsafe { ffi::scx_task_free(p) };

        // After free, data should be gone
        let data3 = unsafe { ffi::scx_task_data(p) };
        assert!(data3.is_null(), "scx_task_data must return null after free");

        free_task(&mut arc.lock().unwrap().sim, Pid(100));
    }

    #[test]
    fn test_sdt_task_multiple_tasks() {
        let _lock = SIM_LOCK.lock().unwrap();
        let state = test_state(2);
        let arc = test_sim_arc(state);

        let p1 = register_task(&mut arc.lock().unwrap().sim, Pid(10));
        let p2 = register_task(&mut arc.lock().unwrap().sim, Pid(20));

        let ret = unsafe { ffi::scx_task_init(64) };
        assert_eq!(ret, 0);

        let d1 = unsafe { ffi::scx_task_alloc(p1) };
        let d2 = unsafe { ffi::scx_task_alloc(p2) };
        assert!(!d1.is_null());
        assert!(!d2.is_null());
        assert_ne!(d1, d2, "different tasks must get different allocations");

        // Each task retrieves its own data
        assert_eq!(unsafe { ffi::scx_task_data(p1) }, d1);
        assert_eq!(unsafe { ffi::scx_task_data(p2) }, d2);

        // Free one, other remains
        unsafe { ffi::scx_task_free(p1) };
        assert!(unsafe { ffi::scx_task_data(p1) }.is_null());
        assert_eq!(unsafe { ffi::scx_task_data(p2) }, d2);

        unsafe { ffi::scx_task_free(p2) };
        free_task(&mut arc.lock().unwrap().sim, Pid(10));
        free_task(&mut arc.lock().unwrap().sim, Pid(20));
    }

    #[test]
    fn test_sdt_task_data_null_before_alloc() {
        let _lock = SIM_LOCK.lock().unwrap();
        let state = test_state(1);
        let arc = test_sim_arc(state);

        let p = register_task(&mut arc.lock().unwrap().sim, Pid(50));

        let ret = unsafe { ffi::scx_task_init(32) };
        assert_eq!(ret, 0);

        // Before alloc, data should be null
        let data = unsafe { ffi::scx_task_data(p) };
        assert!(data.is_null(), "scx_task_data before alloc must be null");

        free_task(&mut arc.lock().unwrap().sim, Pid(50));
    }

    // -----------------------------------------------------------------------
    // sim_atq.c (Phase 1 BPF infra scale-up item 7)
    //
    // Exercise the scx_atq_* userspace impl that Phase 2's compiled-in
    // cgroup_bw.bpf.c will rely on. We mirror the production
    // `struct scx_task_common` layout (rbnode prefix at offset 0,
    // `scx_atq_t *atq` back-pointer at offset 56) by allocating a 80-byte
    // buffer per "task" -- same offset that sim_atq.c writes through.
    // -----------------------------------------------------------------------

    const SCX_ATQ_INF_CAPACITY: u64 = u64::MAX;
    const TASKC_SIZE: usize = 80;
    const ATQ_OFFSET: usize = 56;

    fn alloc_taskc() -> Box<[u8]> {
        vec![0u8; TASKC_SIZE].into_boxed_slice()
    }
    fn taskc_ptr(b: &mut [u8]) -> *mut c_void {
        b.as_mut_ptr() as *mut c_void
    }
    fn taskc_get_atq(b: &[u8]) -> u64 {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&b[ATQ_OFFSET..ATQ_OFFSET + 8]);
        u64::from_ne_bytes(bytes)
    }

    #[test]
    fn test_sim_atq_create_destroy_roundtrip() {
        unsafe {
            sim_atq_set_taskc_atq_offset(ATQ_OFFSET as u64);
            let atq = scx_atq_create_internal(0, SCX_ATQ_INF_CAPACITY);
            assert!(atq != 0, "create_internal returned NULL");
            assert_eq!(scx_atq_nr_queued(atq), 0);
            assert_eq!(scx_atq_destroy(atq), 0);
        }
    }

    #[test]
    fn test_sim_atq_insert_vtime_pop_in_ascending_order() {
        unsafe {
            sim_atq_set_taskc_atq_offset(ATQ_OFFSET as u64);
            let atq = scx_atq_create_internal(0, SCX_ATQ_INF_CAPACITY);
            let mut t1 = alloc_taskc();
            let mut t2 = alloc_taskc();
            let mut t3 = alloc_taskc();
            // Insert non-monotonic: 30, 10, 20.
            assert_eq!(scx_atq_insert_vtime(atq, taskc_ptr(&mut t1), 30), 0);
            assert_eq!(scx_atq_insert_vtime(atq, taskc_ptr(&mut t2), 10), 0);
            assert_eq!(scx_atq_insert_vtime(atq, taskc_ptr(&mut t3), 20), 0);
            assert_eq!(scx_atq_nr_queued(atq), 3);
            // Back-pointers set.
            assert_eq!(taskc_get_atq(&t1), atq);
            assert_eq!(taskc_get_atq(&t2), atq);
            assert_eq!(taskc_get_atq(&t3), atq);
            // peek does NOT remove and returns smallest-vtime taskc (t2).
            assert_eq!(scx_atq_peek(atq), taskc_ptr(&mut t2) as u64);
            assert_eq!(scx_atq_nr_queued(atq), 3);
            // Pop in ascending vtime order: t2 (10), t3 (20), t1 (30).
            assert_eq!(scx_atq_pop(atq), taskc_ptr(&mut t2) as u64);
            assert_eq!(scx_atq_pop(atq), taskc_ptr(&mut t3) as u64);
            assert_eq!(scx_atq_pop(atq), taskc_ptr(&mut t1) as u64);
            assert_eq!(scx_atq_nr_queued(atq), 0);
            assert_eq!(scx_atq_pop(atq), 0); // empty -> NULL
                                             // Back-pointers cleared.
            assert_eq!(taskc_get_atq(&t1), 0);
            assert_eq!(taskc_get_atq(&t2), 0);
            assert_eq!(taskc_get_atq(&t3), 0);
            scx_atq_destroy(atq);
        }
    }

    #[test]
    fn test_sim_atq_fifo_pop_in_insertion_order() {
        unsafe {
            sim_atq_set_taskc_atq_offset(ATQ_OFFSET as u64);
            let atq = scx_atq_create_internal(1, SCX_ATQ_INF_CAPACITY);
            let mut a = alloc_taskc();
            let mut b = alloc_taskc();
            let mut c = alloc_taskc();
            assert_eq!(scx_atq_insert(atq, taskc_ptr(&mut a)), 0);
            assert_eq!(scx_atq_insert(atq, taskc_ptr(&mut b)), 0);
            assert_eq!(scx_atq_insert(atq, taskc_ptr(&mut c)), 0);
            assert_eq!(scx_atq_pop(atq), taskc_ptr(&mut a) as u64);
            assert_eq!(scx_atq_pop(atq), taskc_ptr(&mut b) as u64);
            assert_eq!(scx_atq_pop(atq), taskc_ptr(&mut c) as u64);
            scx_atq_destroy(atq);
        }
    }

    #[test]
    fn test_sim_atq_cancel_removes_from_atq() {
        unsafe {
            sim_atq_set_taskc_atq_offset(ATQ_OFFSET as u64);
            let atq = scx_atq_create_internal(0, SCX_ATQ_INF_CAPACITY);
            let mut a = alloc_taskc();
            let mut b = alloc_taskc();
            let mut c = alloc_taskc();
            scx_atq_insert_vtime(atq, taskc_ptr(&mut a), 100);
            scx_atq_insert_vtime(atq, taskc_ptr(&mut b), 200);
            scx_atq_insert_vtime(atq, taskc_ptr(&mut c), 300);
            assert_eq!(scx_atq_cancel(taskc_ptr(&mut b)), 0);
            assert_eq!(taskc_get_atq(&b), 0);
            assert_eq!(scx_atq_nr_queued(atq), 2);
            assert_eq!(scx_atq_pop(atq), taskc_ptr(&mut a) as u64);
            assert_eq!(scx_atq_pop(atq), taskc_ptr(&mut c) as u64);
            // Cancelling a taskc not in any atq is a no-op.
            let mut orphan = alloc_taskc();
            assert_eq!(scx_atq_cancel(taskc_ptr(&mut orphan)), 0);
            scx_atq_destroy(atq);
        }
    }

    #[test]
    fn test_sim_atq_capacity_enforcement() {
        unsafe {
            sim_atq_set_taskc_atq_offset(ATQ_OFFSET as u64);
            // 2-slot atq: third insert must fail -ENOSPC (-28).
            let atq = scx_atq_create_internal(0, 2);
            let mut a = alloc_taskc();
            let mut b = alloc_taskc();
            let mut c = alloc_taskc();
            assert_eq!(scx_atq_insert_vtime(atq, taskc_ptr(&mut a), 1), 0);
            assert_eq!(scx_atq_insert_vtime(atq, taskc_ptr(&mut b), 2), 0);
            assert_eq!(scx_atq_insert_vtime(atq, taskc_ptr(&mut c), 3), -28);
            assert_eq!(scx_atq_nr_queued(atq), 2);
            assert_eq!(taskc_get_atq(&c), 0);
            scx_atq_destroy(atq);
        }
    }

    #[test]
    fn test_sim_atq_destroy_clears_back_pointers() {
        unsafe {
            sim_atq_set_taskc_atq_offset(ATQ_OFFSET as u64);
            let atq = scx_atq_create_internal(0, SCX_ATQ_INF_CAPACITY);
            let mut a = alloc_taskc();
            let mut b = alloc_taskc();
            scx_atq_insert_vtime(atq, taskc_ptr(&mut a), 1);
            scx_atq_insert_vtime(atq, taskc_ptr(&mut b), 2);
            assert_eq!(taskc_get_atq(&a), atq);
            assert_eq!(taskc_get_atq(&b), atq);
            scx_atq_destroy(atq);
            assert_eq!(taskc_get_atq(&a), 0);
            assert_eq!(taskc_get_atq(&b), 0);
        }
    }

    #[test]
    fn test_sim_atq_null_safety() {
        unsafe {
            assert_eq!(scx_atq_pop(0), 0);
            assert_eq!(scx_atq_peek(0), 0);
            assert_eq!(scx_atq_nr_queued(0), 0);
            assert_eq!(scx_atq_destroy(0), 0);
            assert_eq!(scx_atq_cancel(ptr::null_mut()), 0);
        }
    }
}
