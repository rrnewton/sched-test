//! Scheduler-specific probe accessors and LAVD monitor.
//!
//! [`LavdProbes`] resolves function pointers from the loaded LAVD `.so`
//! and wraps them for safe(r) access. [`LavdMonitor`] implements the
//! [`Monitor`](crate::monitor::Monitor) trait to sample LAVD state at
//! each scheduling event.
//!
//! # Safety
//!
//! This module resolves C function pointers via `dlsym` at runtime and
//! calls them through `unsafe extern "C" fn` types. The resolved pointers
//! are valid only for the lifetime of the loaded scheduler `.so`. Callers
//! must ensure the scheduler remains loaded while [`LavdProbes`] is in use.

use std::ffi::c_void;

use crate::ffi::DynamicScheduler;
use crate::monitor::{Monitor, ProbeContext, ProbePoint};
use crate::types::{CpuId, Pid, TimeNs};

// C function pointer types for LAVD probe functions.
type TaskProbeU16 = unsafe extern "C" fn(*mut c_void) -> u16;
type TaskProbeU64 = unsafe extern "C" fn(*mut c_void) -> u64;
type SysProbeU32 = unsafe extern "C" fn() -> u32;
type SysProbeU64 = unsafe extern "C" fn() -> u64;

/// Resolved LAVD probe function pointers.
///
/// Created from a loaded [`DynamicScheduler`] via [`LavdProbes::new`].
/// All function pointers are valid for the lifetime of the scheduler.
// Additional C function pointer types for slice boost probes.
type SysProbeU8 = unsafe extern "C" fn() -> u8;

pub struct LavdProbes {
    lat_cri_fn: TaskProbeU16,
    wait_freq_fn: TaskProbeU64,
    wake_freq_fn: TaskProbeU64,
    avg_runtime_fn: TaskProbeU64,
    lat_cri_waker_fn: TaskProbeU16,
    lat_cri_wakee_fn: TaskProbeU16,
    sys_avg_lat_cri_fn: SysProbeU32,
    sys_thr_lat_cri_fn: SysProbeU32,
    sys_nr_sched_fn: SysProbeU64,
    sys_nr_lat_cri_fn: SysProbeU64,
    // Slice boost debug probes
    sys_slice_wall_fn: SysProbeU64,
    sys_nr_queued_task_fn: SysProbeU32,
    can_boost_slice_fn: SysProbeU8,
    task_slice_wall_fn: TaskProbeU64,
}

impl LavdProbes {
    /// Resolve all LAVD probe symbols from the loaded scheduler.
    ///
    /// # Panics
    /// Panics if any required probe symbol is missing (indicates the
    /// scheduler was built without probe exports).
    pub fn new(sched: &DynamicScheduler) -> Self {
        // SAFETY: Symbols are resolved from a loaded scheduler `.so` built
        // by our build system. The function pointer types match the C
        // signatures exported by the LAVD scheduler probe functions.
        unsafe {
            macro_rules! resolve {
                ($name:expr, $ty:ty) => {
                    *sched.get_symbol::<$ty>($name).unwrap_or_else(|| {
                        panic!(
                            "probe symbol {:?} not found",
                            std::str::from_utf8($name).unwrap_or("<invalid>")
                        )
                    })
                };
            }

            LavdProbes {
                lat_cri_fn: resolve!(b"lavd_probe_lat_cri", TaskProbeU16),
                wait_freq_fn: resolve!(b"lavd_probe_wait_freq", TaskProbeU64),
                wake_freq_fn: resolve!(b"lavd_probe_wake_freq", TaskProbeU64),
                avg_runtime_fn: resolve!(b"lavd_probe_avg_runtime", TaskProbeU64),
                lat_cri_waker_fn: resolve!(b"lavd_probe_lat_cri_waker", TaskProbeU16),
                lat_cri_wakee_fn: resolve!(b"lavd_probe_lat_cri_wakee", TaskProbeU16),
                sys_avg_lat_cri_fn: resolve!(b"lavd_probe_sys_avg_lat_cri", SysProbeU32),
                sys_thr_lat_cri_fn: resolve!(b"lavd_probe_sys_thr_lat_cri", SysProbeU32),
                sys_nr_sched_fn: resolve!(b"lavd_probe_sys_nr_sched", SysProbeU64),
                sys_nr_lat_cri_fn: resolve!(b"lavd_probe_sys_nr_lat_cri", SysProbeU64),
                // Slice boost debug probes
                sys_slice_wall_fn: resolve!(b"lavd_probe_sys_slice_wall", SysProbeU64),
                sys_nr_queued_task_fn: resolve!(b"lavd_probe_sys_nr_queued_task", SysProbeU32),
                can_boost_slice_fn: resolve!(b"lavd_probe_can_boost_slice", SysProbeU8),
                task_slice_wall_fn: resolve!(b"lavd_probe_task_slice_wall", TaskProbeU64),
            }
        }
    }

    // -- Per-task probes --

    /// Read `task_ctx.lat_cri` (final latency criticality score).
    /// # Safety
    /// `task_raw` must be a valid `task_struct` pointer with allocated
    /// per-task storage (via `scx_task_alloc`).
    pub unsafe fn lat_cri(&self, task_raw: *mut c_void) -> u16 {
        (self.lat_cri_fn)(task_raw)
    }

    /// Read `task_ctx.wait_freq` (sleep frequency EWMA).
    /// # Safety
    /// `task_raw` must be a valid `task_struct` pointer.
    pub unsafe fn wait_freq(&self, task_raw: *mut c_void) -> u64 {
        (self.wait_freq_fn)(task_raw)
    }

    /// Read `task_ctx.wake_freq` (wakeup frequency EWMA).
    /// # Safety
    /// `task_raw` must be a valid `task_struct` pointer.
    pub unsafe fn wake_freq(&self, task_raw: *mut c_void) -> u64 {
        (self.wake_freq_fn)(task_raw)
    }

    /// Read `task_ctx.avg_runtime` (average runtime per schedule).
    /// # Safety
    /// `task_raw` must be a valid `task_struct` pointer.
    pub unsafe fn avg_runtime(&self, task_raw: *mut c_void) -> u64 {
        (self.avg_runtime_fn)(task_raw)
    }

    /// Read `task_ctx.lat_cri_waker` (inherited waker latency criticality).
    /// # Safety
    /// `task_raw` must be a valid `task_struct` pointer.
    pub unsafe fn lat_cri_waker(&self, task_raw: *mut c_void) -> u16 {
        (self.lat_cri_waker_fn)(task_raw)
    }

    /// Read `task_ctx.lat_cri_wakee` (inherited wakee latency criticality).
    /// # Safety
    /// `task_raw` must be a valid `task_struct` pointer.
    pub unsafe fn lat_cri_wakee(&self, task_raw: *mut c_void) -> u16 {
        (self.lat_cri_wakee_fn)(task_raw)
    }

    // -- System-wide probes --

    /// Read `sys_stat.avg_lat_cri` (system average latency criticality).
    pub fn sys_avg_lat_cri(&self) -> u32 {
        // SAFETY: `sys_avg_lat_cri_fn` is a valid function pointer resolved
        // from the loaded scheduler `.so`. No pointer arguments.
        unsafe { (self.sys_avg_lat_cri_fn)() }
    }

    /// Read `sys_stat.thr_lat_cri` (latency criticality kick threshold).
    pub fn sys_thr_lat_cri(&self) -> u32 {
        // SAFETY: Valid function pointer; no pointer arguments.
        unsafe { (self.sys_thr_lat_cri_fn)() }
    }

    /// Read `sys_stat.nr_sched` (total scheduling decisions).
    pub fn sys_nr_sched(&self) -> u64 {
        // SAFETY: Valid function pointer; no pointer arguments.
        unsafe { (self.sys_nr_sched_fn)() }
    }

    /// Read `sys_stat.nr_lat_cri` (number of latency-critical scheduling decisions).
    pub fn sys_nr_lat_cri(&self) -> u64 {
        // SAFETY: Valid function pointer; no pointer arguments.
        unsafe { (self.sys_nr_lat_cri_fn)() }
    }

    // -- Slice boost debug probes --

    /// Read `sys_stat.slice_wall` (current target slice for the system).
    pub fn sys_slice_wall(&self) -> u64 {
        // SAFETY: Valid function pointer; no pointer arguments.
        unsafe { (self.sys_slice_wall_fn)() }
    }

    /// Read `sys_stat.nr_queued_task` (number of queued tasks).
    pub fn sys_nr_queued_task(&self) -> u32 {
        // SAFETY: Valid function pointer; no pointer arguments.
        unsafe { (self.sys_nr_queued_task_fn)() }
    }

    /// Check if `can_boost_slice()` returns true.
    pub fn can_boost_slice(&self) -> bool {
        // SAFETY: Valid function pointer; no pointer arguments.
        unsafe { (self.can_boost_slice_fn)() != 0 }
    }

    /// Read `task_ctx.slice_wall` (task's assigned slice).
    /// # Safety
    /// `task_raw` must be a valid `task_struct` pointer.
    pub unsafe fn task_slice_wall(&self, task_raw: *mut c_void) -> u64 {
        (self.task_slice_wall_fn)(task_raw)
    }
}

/// A snapshot of LAVD state at a single probe point.
#[derive(Debug, Clone)]
pub struct LavdSnapshot {
    pub time_ns: TimeNs,
    pub pid: Pid,
    pub point: ProbePoint,
    pub lat_cri: u16,
    pub wait_freq: u64,
    pub wake_freq: u64,
    pub avg_runtime: u64,
    pub lat_cri_waker: u16,
    pub lat_cri_wakee: u16,
    pub sys_avg_lat_cri: u32,
    pub sys_thr_lat_cri: u32,
    // Slice boost debug fields
    pub sys_slice_wall: u64,
    pub sys_nr_queued_task: u32,
    pub can_boost_slice: bool,
    pub task_slice_wall: u64,
}

/// Accumulates per-task LAVD probe snapshots at each scheduling event.
///
/// After simulation, use [`final_snapshot`](LavdMonitor::final_snapshot)
/// and [`task_history`](LavdMonitor::task_history) to inspect the
/// trajectory of LAVD state.
pub struct LavdMonitor {
    probes: LavdProbes,
    /// Time series of snapshots across all tasks and events.
    pub snapshots: Vec<LavdSnapshot>,
}

impl LavdMonitor {
    /// Create a new LAVD monitor with the given probe accessors.
    pub fn new(probes: LavdProbes) -> Self {
        LavdMonitor {
            probes,
            snapshots: Vec::new(),
        }
    }

    /// Get the final (most recent) snapshot for a task.
    pub fn final_snapshot(&self, pid: Pid) -> Option<&LavdSnapshot> {
        self.snapshots.iter().rev().find(|s| s.pid == pid)
    }

    /// Get all snapshots for a task, ordered by time.
    pub fn task_history(&self, pid: Pid) -> Vec<&LavdSnapshot> {
        self.snapshots.iter().filter(|s| s.pid == pid).collect()
    }
}

impl Monitor for LavdMonitor {
    fn sample(&mut self, ctx: &ProbeContext) {
        // SAFETY: task_raw is a valid task_struct pointer from the engine,
        // with per-task storage allocated by scx_task_alloc during init_task.
        unsafe {
            self.snapshots.push(LavdSnapshot {
                time_ns: ctx.time_ns,
                pid: ctx.pid,
                point: ctx.point,
                lat_cri: self.probes.lat_cri(ctx.task_raw),
                wait_freq: self.probes.wait_freq(ctx.task_raw),
                wake_freq: self.probes.wake_freq(ctx.task_raw),
                avg_runtime: self.probes.avg_runtime(ctx.task_raw),
                lat_cri_waker: self.probes.lat_cri_waker(ctx.task_raw),
                lat_cri_wakee: self.probes.lat_cri_wakee(ctx.task_raw),
                sys_avg_lat_cri: self.probes.sys_avg_lat_cri(),
                sys_thr_lat_cri: self.probes.sys_thr_lat_cri(),
                // Slice boost debug probes
                sys_slice_wall: self.probes.sys_slice_wall(),
                sys_nr_queued_task: self.probes.sys_nr_queued_task(),
                can_boost_slice: self.probes.can_boost_slice(),
                task_slice_wall: self.probes.task_slice_wall(ctx.task_raw),
            });
        }
    }
}

// ---------------------------------------------------------------------------
// scx_layered probes
// ---------------------------------------------------------------------------

/// Read-only accessors for scx_layered's own state.
///
/// Every value below is read straight out of the scheduler's `task_ctx` /
/// `cpu_ctx` / `layers[]`, so tests assert on what scx_layered actually
/// decided rather than on a re-derivation of what it should have decided.
///
/// Created from a loaded [`DynamicScheduler`] via [`LayeredProbes::new`]; the
/// resolved pointers live as long as that scheduler.
pub struct LayeredProbes {
    enum_fn: unsafe extern "C" fn(i32) -> i32,
    task_layer_fn: unsafe extern "C" fn(i32) -> u32,
    task_dsq_fn: unsafe extern "C" fn(i32) -> u64,
    nr_layers_fn: unsafe extern "C" fn() -> u32,
    layer_nr_cpus_fn: unsafe extern "C" fn(u32) -> u32,
    layer_has_cpu_fn: unsafe extern "C" fn(u32, u32) -> i32,
    layer_bpf_has_cpu_fn: unsafe extern "C" fn(u32, u32) -> i32,
    layer_nr_tasks_fn: unsafe extern "C" fn(u32) -> u64,
    layer_stat_fn: unsafe extern "C" fn(u32, u32) -> u64,
    global_stat_fn: unsafe extern "C" fn(u32) -> u64,
    cpu_llc_fn: unsafe extern "C" fn(u32) -> u32,
    cpu_node_fn: unsafe extern "C" fn(u32) -> u32,
    nr_llcs_fn: unsafe extern "C" fn() -> u32,
    nr_nodes_fn: unsafe extern "C" fn() -> u32,
    sibling_cpu_fn: unsafe extern "C" fn(u32) -> i32,
    timer_fires_fn: unsafe extern "C" fn() -> u64,
}

/// scx_layered's "task belongs to no layer" sentinel (`MAX_LAYERS`).
pub const LAYERED_NO_LAYER: u32 = 16;

impl LayeredProbes {
    /// Resolve all scx_layered probe symbols from the loaded scheduler.
    ///
    /// # Panics
    /// Panics if a probe symbol is missing, which means the loaded `.so` is
    /// not `libscx_layered.so`.
    pub fn new(sched: &DynamicScheduler) -> Self {
        // SAFETY: Symbols are resolved from a loaded scheduler `.so` built by
        // our build system; the fn-pointer types match schedulers/layered/
        // wrapper.c's exported signatures.
        unsafe {
            macro_rules! resolve {
                ($name:expr, $ty:ty) => {
                    *sched.get_symbol::<$ty>($name).unwrap_or_else(|| {
                        panic!(
                            "probe symbol {:?} not found (is this libscx_layered.so?)",
                            std::str::from_utf8($name).unwrap_or("<invalid>")
                        )
                    })
                };
            }
            LayeredProbes {
                enum_fn: resolve!(b"layered_probe_enum", unsafe extern "C" fn(i32) -> i32),
                task_layer_fn: resolve!(
                    b"layered_probe_task_layer",
                    unsafe extern "C" fn(i32) -> u32
                ),
                task_dsq_fn: resolve!(b"layered_probe_task_dsq", unsafe extern "C" fn(i32) -> u64),
                nr_layers_fn: resolve!(b"layered_probe_nr_layers", unsafe extern "C" fn() -> u32),
                layer_nr_cpus_fn: resolve!(
                    b"layered_probe_layer_nr_cpus",
                    unsafe extern "C" fn(u32) -> u32
                ),
                layer_has_cpu_fn: resolve!(
                    b"layered_probe_layer_has_cpu",
                    unsafe extern "C" fn(u32, u32) -> i32
                ),
                layer_bpf_has_cpu_fn: resolve!(
                    b"layered_probe_layer_bpf_has_cpu",
                    unsafe extern "C" fn(u32, u32) -> i32
                ),
                layer_nr_tasks_fn: resolve!(
                    b"layered_probe_layer_nr_tasks",
                    unsafe extern "C" fn(u32) -> u64
                ),
                layer_stat_fn: resolve!(
                    b"layered_probe_layer_stat",
                    unsafe extern "C" fn(u32, u32) -> u64
                ),
                global_stat_fn: resolve!(
                    b"layered_probe_global_stat",
                    unsafe extern "C" fn(u32) -> u64
                ),
                cpu_llc_fn: resolve!(b"layered_probe_cpu_llc", unsafe extern "C" fn(u32) -> u32),
                cpu_node_fn: resolve!(b"layered_probe_cpu_node", unsafe extern "C" fn(u32) -> u32),
                nr_llcs_fn: resolve!(b"layered_probe_nr_llcs", unsafe extern "C" fn() -> u32),
                nr_nodes_fn: resolve!(b"layered_probe_nr_nodes", unsafe extern "C" fn() -> u32),
                sibling_cpu_fn: resolve!(
                    b"layered_probe_sibling_cpu",
                    unsafe extern "C" fn(u32) -> i32
                ),
                timer_fires_fn: resolve!(
                    b"layered_probe_timer_fires",
                    unsafe extern "C" fn() -> u64
                ),
            }
        }
    }

    /// Read one intf.h enum value by [`LayeredEnumProbe`] selector.
    pub fn enum_value(&self, which: LayeredEnumProbe) -> i32 {
        // SAFETY: pure switch over an integer selector, no pointers.
        unsafe { (self.enum_fn)(which as i32) }
    }

    /// The layer scx_layered assigned to `pid`, or [`LAYERED_NO_LAYER`].
    pub fn task_layer(&self, pid: Pid) -> u32 {
        // SAFETY: pid is bounds-checked C-side; no pointers cross the call.
        unsafe { (self.task_layer_fn)(pid.0) }
    }

    /// The DSQ scx_layered last enqueued `pid` to.
    pub fn task_dsq(&self, pid: Pid) -> u64 {
        // SAFETY: pid is bounds-checked C-side.
        unsafe { (self.task_dsq_fn)(pid.0) }
    }

    /// Number of configured layers.
    pub fn nr_layers(&self) -> u32 {
        // SAFETY: no arguments.
        unsafe { (self.nr_layers_fn)() }
    }

    /// `layer->nr_cpus` for `layer_id`.
    pub fn layer_nr_cpus(&self, layer_id: u32) -> u32 {
        // SAFETY: layer_id is bounds-checked C-side.
        unsafe { (self.layer_nr_cpus_fn)(layer_id) }
    }

    /// Whether `cpu` is in `layer_id`'s published cpumask.
    pub fn layer_has_cpu(&self, layer_id: u32, cpu: CpuId) -> bool {
        // SAFETY: both indices are bounds-checked C-side.
        unsafe { (self.layer_has_cpu_fn)(layer_id, cpu.0) != 0 }
    }

    /// Whether the real BPF kptr cpumask contains `cpu` after refresh.
    pub fn layer_bpf_has_cpu(&self, layer_id: u32, cpu: CpuId) -> bool {
        // SAFETY: both indices are bounds-checked C-side.
        unsafe { (self.layer_bpf_has_cpu_fn)(layer_id, cpu.0) != 0 }
    }

    /// `layer->nr_tasks` — how many tasks scx_layered currently places in it.
    pub fn layer_nr_tasks(&self, layer_id: u32) -> u64 {
        // SAFETY: layer_id is bounds-checked C-side.
        unsafe { (self.layer_nr_tasks_fn)(layer_id) }
    }

    /// A per-layer stat summed across CPUs (see `enum layer_stat_id`).
    pub fn layer_stat(&self, layer_id: u32, stat: LayerStat) -> u64 {
        // SAFETY: both indices are bounds-checked C-side.
        unsafe { (self.layer_stat_fn)(layer_id, stat as u32) }
    }

    /// A global stat summed across CPUs (see `enum global_stat_id`).
    pub fn global_stat(&self, stat: GlobalStat) -> u64 {
        // SAFETY: the index is bounds-checked C-side.
        unsafe { (self.global_stat_fn)(stat as u32) }
    }

    /// The LLC id scx_layered recorded for `cpu`.
    pub fn cpu_llc(&self, cpu: CpuId) -> u32 {
        // SAFETY: cpu is bounds-checked C-side.
        unsafe { (self.cpu_llc_fn)(cpu.0) }
    }

    /// The NUMA node id scx_layered recorded for `cpu`.
    pub fn cpu_node(&self, cpu: CpuId) -> u32 {
        // SAFETY: cpu is bounds-checked C-side.
        unsafe { (self.cpu_node_fn)(cpu.0) }
    }

    /// Number of LLCs the scheduler was told about.
    pub fn nr_llcs(&self) -> u32 {
        // SAFETY: no arguments.
        unsafe { (self.nr_llcs_fn)() }
    }

    /// Number of NUMA nodes the scheduler was told about.
    pub fn nr_nodes(&self) -> u32 {
        // SAFETY: no arguments.
        unsafe { (self.nr_nodes_fn)() }
    }

    /// `__sibling_cpu[cpu]`, or `-1` when SMT is off.
    pub fn sibling_cpu(&self, cpu: CpuId) -> i32 {
        // SAFETY: cpu is bounds-checked C-side.
        unsafe { (self.sibling_cpu_fn)(cpu.0) }
    }

    /// How many times the antistall timer callback has run.
    pub fn timer_fires(&self) -> u64 {
        // SAFETY: no arguments.
        unsafe { (self.timer_fires_fn)() }
    }
}

/// Selectors for [`LayeredProbes::enum_value`], mirroring
/// `enum layered_enum_probe_id` in `schedulers/layered/wrapper.c`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum LayeredEnumProbe {
    /// `LAYER_KIND_OPEN`
    KindOpen = 0,
    /// `LAYER_KIND_GROUPED`
    KindGrouped = 1,
    /// `LAYER_KIND_CONFINED`
    KindConfined = 2,
    /// `MATCH_CGROUP_PREFIX`
    MatchCgroupPrefix = 3,
    /// `MATCH_COMM_PREFIX`
    MatchCommPrefix = 4,
    /// `MATCH_PCOMM_PREFIX`
    MatchPcommPrefix = 5,
    /// `MATCH_NICE_ABOVE`
    MatchNiceAbove = 6,
    /// `MATCH_NICE_BELOW`
    MatchNiceBelow = 7,
    /// `MATCH_NICE_EQUALS`
    MatchNiceEquals = 8,
    /// `MATCH_USER_ID_EQUALS`
    MatchUserIdEquals = 9,
    /// `MATCH_GROUP_ID_EQUALS`
    MatchGroupIdEquals = 10,
    /// `MATCH_PID_EQUALS`
    MatchPidEquals = 11,
    /// `MATCH_PPID_EQUALS`
    MatchPpidEquals = 12,
    /// `MATCH_TGID_EQUALS`
    MatchTgidEquals = 13,
    /// `MATCH_IS_GROUP_LEADER`
    MatchIsGroupLeader = 14,
    /// `MATCH_IS_KTHREAD`
    MatchIsKthread = 15,
    /// `MATCH_CGROUP_SUFFIX`
    MatchCgroupSuffix = 16,
    /// `MATCH_CGROUP_CONTAINS`
    MatchCgroupContains = 17,
    /// `MATCH_NUMA_NODE`
    MatchNumaNode = 18,
    /// `GROWTH_ALGO_STICKY`
    GrowthSticky = 19,
    /// `GROWTH_ALGO_LINEAR`
    GrowthLinear = 20,
    /// `GROWTH_ALGO_REVERSE`
    GrowthReverse = 21,
    /// `GROWTH_ALGO_TOPO`
    GrowthTopo = 22,
    /// `GROWTH_ALGO_ROUND_ROBIN`
    GrowthRoundRobin = 23,
    /// `MAX_LAYERS`
    MaxLayers = 24,
    /// `DEFAULT_LAYER_WEIGHT`
    DefaultLayerWeight = 25,
}

/// The subset of `enum layer_stat_id` the tests assert on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum LayerStat {
    /// `LSTAT_SEL_LOCAL` — select_cpu found an idle CPU and went local.
    SelLocal = 0,
    /// `LSTAT_ENQ_LOCAL` — enqueue went straight to the local DSQ.
    EnqLocal = 1,
    /// `LSTAT_ENQ_WAKEUP` — enqueued on a wakeup.
    EnqWakeup = 2,
    /// `LSTAT_ENQ_EXPIRE` — enqueued after the slice expired.
    EnqExpire = 3,
    /// `LSTAT_ENQ_REENQ` — re-enqueued.
    EnqReenq = 4,
    /// `LSTAT_ENQ_DSQ` — enqueued to a layer DSQ.
    EnqDsq = 5,
    /// `LSTAT_KEEP` — kept running on the same CPU.
    Keep = 6,
    /// `LSTAT_YIELD` — ops.yield handled a `sched_yield()`.
    Yield = 21,
    /// `LSTAT_YIELD_IGNORE` — ops.yield declined.
    YieldIgnore = 22,
    /// `LSTAT_MIGRATION` — task migrated between CPUs.
    Migration = 23,
}

/// The subset of `enum global_stat_id` the tests assert on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum GlobalStat {
    /// `GSTAT_EXCL_IDLE`
    ExclIdle = 0,
    /// `GSTAT_HI_FB_EVENTS` — hi fallback DSQ was used.
    HiFbEvents = 2,
    /// `GSTAT_LO_FB_EVENTS` — lo fallback DSQ was used.
    LoFbEvents = 4,
    /// `GSTAT_ANTISTALL` — antistall consumed a delayed DSQ.
    Antistall = 7,
}
