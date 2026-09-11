//! Pure type definitions for the task model.
//!
//! This module contains the safe, pure-Rust types that describe task
//! behavior, state, and configuration. No `unsafe` code lives here;
//! the FFI-backed runtime type `SimTask` is in `unsafe_impl::sim_task`.

use crate::types::{CpuId, Gid, MmId, Pid, TimeNs, Uid};

/// Kernel sched_prio_to_weight table from kernel/sched/core.c.
/// Maps nice levels -20..19 (indices 0..39) to scheduler weights.
const SCHED_PRIO_TO_WEIGHT: [u32; 40] = [
    /* -20 */ 88761, 71755, 56483, 46273, 36291, /* -15 */ 29154, 23254, 18705, 14949,
    11916, /* -10 */ 9548, 7620, 6100, 4904, 3906, /*  -5 */ 3121, 2501, 1991, 1586,
    1277, /*   0 */ 1024, 820, 655, 526, 423, /*   5 */ 335, 272, 215, 172, 137,
    /*  10 */ 110, 87, 70, 56, 45, /*  15 */ 36, 29, 23, 18, 15,
];

/// Convert a nice value (-20..=19) to a kernel scheduler weight.
pub fn nice_to_weight(nice: i8) -> u32 {
    assert!(
        (-20..=19).contains(&nice),
        "nice value {nice} out of range -20..=19"
    );
    SCHED_PRIO_TO_WEIGHT[(nice + 20) as usize]
}

/// Cgroup weight constants from include/linux/cgroup.h.
const CGROUP_WEIGHT_MIN: u32 = 1;
const CGROUP_WEIGHT_DFL: u32 = 100;
const CGROUP_WEIGHT_MAX: u32 = 10000;

/// Convert a raw kernel scheduler weight to cgroup-weight space [1..10000].
///
/// Mirrors the kernel's `sched_weight_to_cgroup()` from kernel/sched/sched.h:
///   clamp(weight * CGROUP_WEIGHT_DFL / 1024, 1, 10000)
///
/// The kernel stores this converted value in `p->scx.weight`, not the raw
/// sched_prio_to_weight value. BPF schedulers (e.g. scx_simple's stopping
/// callback) assume cgroup-weight space when they divide by `p->scx.weight`.
pub fn sched_weight_to_cgroup(weight: u32) -> u32 {
    let cg = ((weight as u64 * CGROUP_WEIGHT_DFL as u64) + 512) / 1024;
    (cg as u32).clamp(CGROUP_WEIGHT_MIN, CGROUP_WEIGHT_MAX)
}

/// Per-task SCX ops_state, modeling the kernel's `SCX_OPSS_*` state machine.
///
/// In the kernel, `ops.dequeue()` is only called when a task is in
/// `SCX_OPSS_QUEUED` — i.e., it was handed to the BPF scheduler via
/// `enqueue()` but has not yet been dispatched to a DSQ. Once dispatched
/// (or picked to run), the task transitions to `NONE` and `dequeue()` is
/// no longer valid.
///
/// We only need two states: the kernel's `DISPATCHING` is transient and
/// resolved immediately by `resolve_pending_dispatch()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OpsTaskState {
    /// Task is not queued in the BPF scheduler (default / after dispatch).
    #[default]
    None,
    /// Task has been enqueued to the BPF scheduler but not yet dispatched.
    Queued,
}

/// The state a simulated task can be in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    /// Task is sleeping (not runnable).
    Sleeping,
    /// Task is runnable but not currently executing on any CPU.
    Runnable,
    /// Task is currently executing on the given CPU.
    Running { cpu: CpuId },
    /// Task has completed all its phases and exited.
    Exited,
}

/// A phase in a task's scripted behavior.
#[derive(Debug, Clone)]
pub enum Phase {
    /// Run (consume CPU) for the given number of nanoseconds.
    Run(TimeNs),
    /// Calibrated task-context system CPU for the given number of nanoseconds.
    ///
    /// This takes the ordinary scheduled-task path, but unlike [`Phase::Run`]
    /// its already-resolved duration is not perturbed by generic compute
    /// jitter.  Doing so would silently replace a calibrated estimand with a
    /// simulator noise sample.
    SystemCpu(TimeNs),
    /// Sleep (block) for the given number of nanoseconds.
    Sleep(TimeNs),
    /// Remain alive but sleeping until global scenario teardown. Unlike a very
    /// long timed sleep, this has no wake event and cannot complete the task.
    Park,
    /// Wake another task by PID (instantaneous).
    Wake(Pid),
    /// Call `sched_yield()` (instantaneous). The task stays runnable and
    /// continues with the next phase, but the kernel first runs
    /// `yield_task_scx()` → `ops.yield`, giving the scheduler a chance to
    /// deprioritise it. When the scheduler has no `ops.yield` — or it returns
    /// `false` — the kernel zeroes `p->scx.slice`, which the engine mirrors.
    ///
    /// Only the plain one-argument `sched_yield()` form is modelled;
    /// `sched_yield_to()` (the `to` argument of `ops.yield`) is not.
    Yield,
}

/// How a task's phase sequence repeats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepeatMode {
    /// Run the phase sequence exactly once and exit.
    Once,
    /// Repeat the phase sequence a fixed number of times, then exit.
    Count(u32),
    /// Repeat the phase sequence indefinitely (until simulation ends).
    Forever,
}

/// The scripted behavior for a task: a sequence of phases with a repeat mode.
#[derive(Debug, Clone)]
pub struct TaskBehavior {
    pub phases: Vec<Phase>,
    pub repeat: RepeatMode,
}

/// Definition of a task for scenario creation.
#[derive(Debug, Clone)]
pub struct TaskDef {
    pub name: String,
    pub pid: Pid,
    pub nice: i8,
    pub behavior: TaskBehavior,
    /// When the task first becomes runnable (simulated ns).
    pub start_time_ns: TimeNs,
    /// Address-space group. Tasks with the same `MmId` share an address
    /// space (like threads) and are eligible for wake-affine scheduling.
    pub mm_id: Option<MmId>,
    /// CPU affinity mask. `None` means all CPUs are allowed.
    /// When `Some(cpus)`, the task may only run on the listed CPUs.
    pub allowed_cpus: Option<Vec<CpuId>>,
    /// Parent task PID. When set, `real_parent` on the C `task_struct`
    /// will point to the specified parent's struct, enabling scheduler
    /// features that track parent-child relationships (e.g. LAVD's
    /// waker-wakee latency criticality propagation).
    pub parent_pid: Option<Pid>,
    /// Cgroup name. When set, the task belongs to the named cgroup.
    /// The cgroup must be defined in the scenario via `.cgroup()`.
    /// If `None`, the task belongs to the root cgroup.
    pub cgroup_name: Option<String>,
    /// Kernel task_struct `flags` (PF_*). Defaults to 0 (normal user task).
    /// Common values: `PF_KTHREAD` (0x200000), `PF_WQ_WORKER` (0x20),
    /// `PF_IO_WORKER` (0x10). Kernel tasks have `mm = NULL` automatically.
    pub task_flags: u32,
    /// Migration disabled counter. When > 0, the task cannot migrate to a
    /// different CPU even if `nr_cpus_allowed > 1`. This models the kernel's
    /// `migration_disabled` field which is incremented when tasks enter BPF
    /// code or explicitly disable migration (e.g., kworkers bound to a CPU).
    /// Defaults to 0 (migration enabled).
    pub migration_disabled: u16,
    /// Thread-group membership: the pid of this task's thread-group leader.
    ///
    /// `None` — the default — means the task is a single-threaded process and
    /// is therefore its own leader, which is what `sim_task_alloc()` already
    /// sets up. `Some(pid)` makes this task a *thread* of the process led by
    /// `pid`, and the engine points `task_struct->group_leader` at that
    /// leader's struct. That is the field scx_layered's `MATCH_PCOMM_PREFIX`
    /// reads (`p->group_leader->comm`), so it is what lets a worker thread be
    /// matched by its *process* name rather than its own.
    ///
    /// The named leader must itself be a leader: the kernel has no nested
    /// thread groups, and `group_leader` always points at one.
    ///
    /// NOTE this does **not** set `task_struct->tgid`, which is a separate and
    /// currently blocked piece of state — see the `DANGER TODO(sim-6mheb)` in
    /// `csrc/sim_task.c`. `MATCH_TGID_EQUALS` and `MATCH_IS_GROUP_LEADER`
    /// therefore remain wrong for every task, grouped or not.
    pub thread_group_leader: Option<Pid>,
    /// Effective user id, published into `real_cred->{uid,euid}`.
    ///
    /// Read by scx_layered's `MATCH_USER_ID_EQUALS`. Defaults to 0 (root),
    /// which is the identity a simulated workload runs under unless it says
    /// otherwise.
    pub uid: Uid,
    /// Effective group id, published into `real_cred->{gid,egid}`.
    ///
    /// Read by scx_layered's `MATCH_GROUP_ID_EQUALS`. Defaults to 0.
    pub gid: Gid,
}

impl TaskDef {
    /// Initial CPU for this task, matching kernel semantics.
    ///
    /// In the kernel, a new task's `cpu` field is set to the CPU where it was
    /// forked, which is always within its cpumask. We model this by returning
    /// the first allowed CPU when a cpumask is specified, or `CpuId(0)` when
    /// the task is unrestricted.
    pub fn initial_cpu(&self) -> CpuId {
        self.allowed_cpus
            .as_ref()
            .and_then(|cpus| cpus.first().copied())
            .unwrap_or(CpuId(0))
    }
}
