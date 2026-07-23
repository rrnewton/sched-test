//! Scenario definition and builder API.

use tracing::warn;

use crate::cgroup::DEFAULT_MAX_CGROUPS;
use crate::perf::PmuEvent;
use crate::task::{TaskBehavior, TaskDef};
use crate::types::{CpuId, MmId, Pid, TimeNs};

/// A CPU hotplug event: take a CPU offline or bring it online at a given time.
#[derive(Debug, Clone)]
pub struct HotplugEvent {
    /// Simulated time (ns) when the hotplug event fires.
    pub time_ns: TimeNs,
    /// Which CPU transitions.
    pub cpu: CpuId,
    /// `true` = CPU comes online, `false` = CPU goes offline.
    pub online: bool,
}

/// A higher-priority scheduler class preemption event.
///
/// Simulates a CPU being temporarily taken by a higher-priority scheduler
/// class (e.g., RT or DL). The engine calls `cpu_release` at `release_at_ns`
/// and `cpu_acquire` at `acquire_at_ns`.
#[derive(Debug, Clone)]
pub struct CpuPreemptEvent {
    /// Which CPU is preempted by the higher-priority class.
    pub cpu: CpuId,
    /// When the higher-priority class takes the CPU (calls cpu_release).
    pub release_at_ns: TimeNs,
    /// When sched_ext regains the CPU (calls cpu_acquire).
    pub acquire_at_ns: TimeNs,
}

/// Cgroup migration event: move a task between cgroups at a given time.
///
/// Simulates a task being moved between cgroups (e.g., via cgroup.procs write).
/// The engine calls `cgroup_move` at `at_ns`.
#[derive(Debug, Clone)]
pub struct CgroupMigrateEvent {
    /// PID of the task to migrate.
    pub pid: Pid,
    /// Name of the source cgroup.
    pub from_cgroup: String,
    /// Name of the destination cgroup.
    pub to_cgroup: String,
    /// Simulation time at which the migration occurs.
    pub at_ns: TimeNs,
}

/// Cgroup creation event: create a new cgroup at runtime.
///
/// Simulates a cgroup being created (e.g., via mkdir in cgroup filesystem).
/// The engine calls `cgroup_init` at `at_ns`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CgroupCreateEvent {
    /// Name of the new cgroup.
    pub name: String,
    /// Parent cgroup name. If `None`, the parent is the root cgroup.
    pub parent_name: Option<String>,
    /// Optional cpuset configuration: list of allowed CPU IDs.
    pub cpuset: Option<Vec<CpuId>>,
    /// Simulation time at which the cgroup is created.
    pub at_ns: TimeNs,
}

/// Cgroup destruction event: destroy a cgroup at runtime.
///
/// Simulates a cgroup being removed (e.g., via rmdir in cgroup filesystem).
/// The engine calls `cgroup_exit` at `at_ns`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CgroupDestroyEvent {
    /// Name of the cgroup to destroy.
    pub name: String,
    /// Simulation time at which the cgroup is destroyed.
    pub at_ns: TimeNs,
}

/// Cgroup cpuset change event: update a cgroup's allowed CPUs at runtime.
///
/// Simulates writing to cpuset.cpus for a cgroup (e.g., `echo "0-3" > cpuset.cpus`).
/// The engine updates the C-side cpuset and calls `cgroup_init` with updated args
/// so the scheduler can detect the change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CgroupCpusetChangeEvent {
    /// Name of the cgroup whose cpuset is being changed.
    pub cgroup_name: String,
    /// New cpuset: list of allowed CPU IDs.
    pub new_cpuset: Vec<CpuId>,
    /// Simulation time at which the cpuset change occurs.
    pub at_ns: TimeNs,
}

/// Type of interrupt to simulate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqType {
    /// Hardware interrupt (top half). Sets `bpf_in_hardirq()=true`.
    HardIrq,
    /// Software interrupt (bottom half, inline). Sets `bpf_in_serving_softirq()=true`.
    SoftIrq,
}

/// An interrupt event to inject during simulation.
#[derive(Debug, Clone)]
pub struct IrqEvent {
    /// Which CPU the interrupt fires on.
    pub cpu: CpuId,
    /// When the interrupt fires (simulated ns).
    pub at_ns: TimeNs,
    /// Duration the interrupt handler runs (ns). Steals time from running task.
    pub duration_ns: TimeNs,
    /// Type of interrupt (hardirq or softirq).
    pub irq_type: IrqType,
    /// Tasks to wake during this interrupt (e.g., I/O completion handlers).
    pub wake_pids: Vec<Pid>,
}

/// A userspace-lock futex transition the scheduler can observe, as delivered
/// by the kernel's futex tracepoint/fexit. This is what LAVD's `lock.bpf.c`
/// keys its lock-holder boosting off of. See `ai_docs/FUTEX_SIM_DESIGN.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FutexOp {
    /// A contended `futex_wait` returned success — the task acquired the lock.
    /// Delivered to the scheduler as `FUTEX_WAIT` with `ret == 0`, which boosts
    /// the lock holder (`LAVD_FLAG_FUTEX_BOOST`).
    WaitAcquired,
    /// A `futex_wake` woke ≥1 waiter — the lock was released. Delivered as
    /// `FUTEX_WAKE` with `ret == 1` (waiters woken), which clears the boost.
    WakeReleased,
}

impl FutexOp {
    /// The `(op, ret)` pair passed to the scheduler's `futex_hook` — the
    /// FUTEX_* command and the syscall return the scheduler observes.
    /// `FUTEX_WAIT = 0`, `FUTEX_WAKE = 1` (see `linux/uapi/linux/futex.h`,
    /// mirrored in `lock.bpf.c`).
    pub fn to_op_ret(self) -> (i32, i64) {
        match self {
            FutexOp::WaitAcquired => (0 /* FUTEX_WAIT */, 0),
            FutexOp::WakeReleased => (1 /* FUTEX_WAKE */, 1),
        }
    }
}

/// A scheduled futex transition to inject during simulation, mirroring
/// [`IrqEvent`]. The engine delivers it to the scheduler's real futex hooks
/// (running actual `lock.bpf.c` code) at `at_ns`, provided `pid` is the task
/// running on a CPU at that instant.
#[derive(Debug, Clone)]
pub struct FutexEvent {
    /// The task performing the futex op (must be running when it fires).
    pub pid: Pid,
    /// When the op fires (simulated ns).
    pub at_ns: TimeNs,
    /// Which transition (wait-acquired boosts, wake-released unboosts).
    pub op: FutexOp,
}

/// CPU bandwidth configuration for a cgroup (cpu.max parameters).
#[derive(Debug, Clone)]
pub struct CgroupBandwidth {
    /// Bandwidth period in microseconds.
    pub period_us: u64,
    /// Quota within the period in microseconds.
    pub quota_us: u64,
    /// Burst allowance in microseconds.
    pub burst_us: u64,
}

/// Definition of a cgroup for scenario creation.
#[derive(Debug, Clone)]
pub struct CgroupDef {
    /// Name of the cgroup (used to reference it from tasks).
    pub name: String,
    /// Parent cgroup name. If `None`, the parent is the root cgroup.
    pub parent_name: Option<String>,
    /// Optional cpuset configuration: list of allowed CPU IDs.
    /// If `None`, the cgroup inherits the parent's cpuset.
    pub cpuset: Option<Vec<CpuId>>,
    /// Optional CPU bandwidth configuration (cpu.max).
    /// If set, `cgroup_set_bandwidth` is called after `cgroup_init`.
    pub bandwidth: Option<CgroupBandwidth>,
}

/// Configuration for simulation timing noise (tick jitter).
///
/// Models hardware interrupt delivery latency: on commodity non-RT kernels,
/// timer interrupts show 1–10μs of jitter due to interrupt latency, cache
/// misses, and pipeline stalls. Modeled as normally-distributed noise added
/// to each tick interval.
#[derive(Debug, Clone)]
pub struct NoiseConfig {
    /// Master switch: false disables all noise (exact deterministic tick timing).
    pub enabled: bool,
    /// Enable tick jitter (normally-distributed variation in tick intervals).
    pub tick_jitter: bool,
    /// Standard deviation for tick jitter (ns). Default: 2000 (2μs).
    pub tick_jitter_stddev_ns: TimeNs,
    /// Enable run-time jitter (normally-distributed variation on Phase::Run duration).
    ///
    /// Models real compute-time variability from cache misses, branch mispredictions,
    /// TLB misses, and memory bandwidth contention. Without this, Phase::Run(250μs)
    /// executes for exactly 250μs every time, producing unrealistically tight e2e
    /// latency distributions (p50≈p99). Default: true.
    pub run_jitter: bool,
    /// Coefficient of variation for run-time jitter (millionths, i.e., ppm).
    ///
    /// Applied as: `duration * (1 + normal(0, cv/1e6))`. A cv_ppm of 200_000
    /// means 20% CV — so Phase::Run(250μs) becomes ~250μs ± 50μs.
    /// Default: 200_000 (20% CV).
    pub run_jitter_cv_ppm: u64,
}

impl Default for NoiseConfig {
    fn default() -> Self {
        NoiseConfig {
            enabled: true,
            tick_jitter: true,
            tick_jitter_stddev_ns: 2_000,
            run_jitter: true,
            run_jitter_cv_ppm: 200_000,
        }
    }
}

impl NoiseConfig {
    /// Create a NoiseConfig with defaults influenced by environment variables.
    ///
    /// Precedence: `SCX_SIM_NOISE` > `SCX_SIM_INSTANT_TIMING` > hardcoded default.
    ///
    /// - `SCX_SIM_NOISE=0` disables noise; `SCX_SIM_NOISE=1` enables it.
    /// - `SCX_SIM_INSTANT_TIMING=1` disables noise (lower priority).
    /// - If neither is set, defaults to enabled.
    pub fn from_env() -> Self {
        let mut config = Self::default();
        if std::env::var("SCX_SIM_INSTANT_TIMING").ok().as_deref() == Some("1") {
            config.enabled = false;
        }
        match std::env::var("SCX_SIM_NOISE").ok().as_deref() {
            Some("0") => config.enabled = false,
            Some("1") => config.enabled = true,
            _ => {}
        }
        if let Ok(v) = std::env::var("SCX_SIM_RUN_JITTER_CV_PPM") {
            if let Ok(ppm) = v.parse::<u64>() {
                config.run_jitter_cv_ppm = ppm;
            }
        }
        config
    }
}

/// Which preemption mechanism to use for mid-C-code preemption.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PreemptMode {
    /// PMU hardware timer (near-zero overhead; signal delivery has skid but counter values are exact).
    #[default]
    Pmu,
    /// e9patch software RBC (deterministic, debugger-compatible, requires
    /// `_e9.so` variant of the scheduler).
    E9patch,
    /// Native concurrency: workers run truly concurrently with real locks
    /// and window-based clock throttling. No token ring.
    NativeConcurrent,
}

/// Configuration for the native concurrency backend.
///
/// Worker threads run truly concurrently with window-based clock throttling
/// instead of being serialized by a token ring. This enables external
/// determinism/chaos tools (`hermit`, `rr`) for replay and fuzz-testing
/// of cross-CPU scheduler interactions.
#[derive(Debug, Clone, Copy)]
pub struct NativeConcurrentConfig {
    /// Maximum logical time (ns) any CPU can be ahead of the slowest.
    /// Default: 10_000_000 (10ms).
    pub window_ns: TimeNs,
}

impl Default for NativeConcurrentConfig {
    fn default() -> Self {
        NativeConcurrentConfig {
            window_ns: 10_000_000,
        }
    }
}

/// Configuration for preemptive interleaving via PMU timer signals.
///
/// When enabled, dispatch callbacks are preempted at random PMU event
/// count intervals, enabling exploration of mid-C-code interleavings
/// beyond the cooperative kfunc-boundary yield points.
///
/// The timeslice (in PMU events) is rolled uniformly in
/// `[timeslice_min, timeslice_max]` from the interleave PRNG.
///
/// The default is min=100, max=500, which provides a good balance between
/// preemption coverage and signal overhead. With PMU skid (~30-100 events),
/// actual preemption happens 130-600 branches after the last kfunc boundary.
///
/// WARNING: min=max=1 causes severe performance degradation with multi-worker
/// workloads (e.g. 8 tasks on 4 CPUs). The PMU fires after every ~30 branches
/// (1 requested + skid), generating ~10,000 signals/ms/worker. With 4 workers
/// that is ~40,000 signals/ms, each costing ~1-5us of real time, making a
/// 200ms simulation take minutes instead of milliseconds.
#[derive(Debug, Clone)]
pub struct PreemptiveConfig {
    /// Minimum timeslice in PMU events.
    pub timeslice_min: u64,
    /// Maximum timeslice in PMU events.
    pub timeslice_max: u64,
    /// If true, disable PMU timers and use only cooperative yields at kfunc
    /// boundaries. This ensures deterministic interleaving regardless of
    /// hardware behavior. Default: false (use PMU when available).
    pub cooperative_only: bool,
    /// Which PMU event to break on. Default: `RetiredBranchConditional`.
    ///
    /// `InstructionsRetired` fires more frequently (every instruction vs.
    /// every conditional branch), so `timeslice_min`/`timeslice_max` may
    /// need to be larger to avoid excessive preemption overhead.
    pub break_on: PmuEvent,
    /// Which preemption mechanism to use. Default: `Pmu`.
    pub preempt_mode: PreemptMode,
}

impl Default for PreemptiveConfig {
    fn default() -> Self {
        PreemptiveConfig {
            timeslice_min: 300,
            timeslice_max: 1500,
            cooperative_only: false,
            break_on: PmuEvent::RetiredBranchConditional,
            preempt_mode: PreemptMode::Pmu,
        }
    }
}

impl PreemptiveConfig {
    /// Create a cooperative-only preemptive config (no PMU timers).
    ///
    /// This mode uses the futex-based `PreemptRing` for token passing but
    /// disables PMU timers, yielding only at kfunc boundaries. The result
    /// is fully deterministic interleaving.
    pub fn cooperative_only() -> Self {
        PreemptiveConfig {
            cooperative_only: true,
            ..Default::default()
        }
    }
}

/// Configuration for context switch overhead.
///
/// Models real CPU time consumed during task transitions. A voluntary yield
/// (sleep, exit) costs ~500ns (~1000 cycles at 2GHz). An involuntary
/// preemption costs ~1000ns due to pipeline flush, TLB shootdown, and cache
/// cold effects. Each has optional per-switch jitter.
#[derive(Debug, Clone)]
pub struct OverheadConfig {
    /// Master switch: false disables all overhead (zero-cost transitions).
    pub enabled: bool,
    /// Enable overhead for voluntary context switches (sleep, exit, yield).
    pub voluntary_csw: bool,
    /// Enable overhead for involuntary context switches (preemption).
    pub involuntary_csw: bool,
    /// Time consumed by a voluntary context switch (ns). Default: 500.
    pub voluntary_csw_ns: TimeNs,
    /// Time consumed by an involuntary context switch (ns). Default: 1000.
    pub involuntary_csw_ns: TimeNs,
    /// Enable per-switch jitter on CSW overhead.
    pub csw_jitter: bool,
    /// Standard deviation for CSW overhead jitter (ns). Default: 100.
    pub csw_jitter_stddev_ns: TimeNs,
    /// Nanosecond cost for global DSQ consume operation.
    /// Models the cache-line transfer and dequeue overhead. Default: 100.
    pub dsq_consume_ns: TimeNs,
    /// Overhead for ops.running() callback dispatch.
    /// Models the kernel context setup before calling the callback. Default: 50.
    pub running_overhead_ns: TimeNs,
    /// IPI delivery latency for scx_bpf_kick_cpu.
    /// Models the inter-processor interrupt latency. Default: 200.
    pub ipi_delivery_ns: TimeNs,
    /// Overhead for ops.update_idle() callback dispatch. Default: 50.
    pub update_idle_overhead_ns: TimeNs,
    /// Minimum scheduling latency floor (ns) for wake→running transitions.
    ///
    /// Models kernel overhead that exists even with zero queuing: IPI delivery,
    /// context switch setup, cache/TLB warming, and scheduler BPF callback
    /// overhead. Production traces show cache_worker p50 of 2.7-6.1μs even on
    /// lightly loaded machines (252 threads on 315 CPUs).
    ///
    /// Applied as a floor on the per-CPU clock advance between EnqueueTask and
    /// TaskScheduled. Set to 0 to disable. Default: 3000ns (3μs).
    pub wakeup_latency_floor_ns: TimeNs,
    /// Standard deviation for wakeup latency jitter (ns).
    ///
    /// Adds normally-distributed jitter on top of the wakeup latency floor,
    /// modeling variation from cache state, interrupt timing, and scheduler
    /// path length. Production traces show p50→p90 spread of ~2-4x, which
    /// a stddev of ~2μs reproduces well. Set to 0 to disable. Default: 2000ns.
    pub wakeup_jitter_stddev_ns: TimeNs,
    /// Extra latency (ns) when a task migrates to a different CPU.
    ///
    /// Models cache/TLB cold-start penalty when a task runs on a CPU different
    /// from where it last ran. Production traces show significant migration
    /// counts (e.g., 26k migrations for 252 threads in 191ms). Default: 10000ns (10μs).
    pub migration_penalty_ns: TimeNs,
    /// Extra latency (ns) for cross-LLC migrations (on top of migration_penalty_ns).
    ///
    /// When a task migrates to a CPU in a different LLC domain, the entire
    /// working set must be fetched from remote LLC or DRAM instead of the
    /// local LLC. This adds ~20-50μs on AMD Zen3/4 (cross-CCX) or ~10-20μs
    /// on Intel (cross-ring-stop). Default: 25000ns (25μs).
    pub cross_llc_migration_penalty_ns: TimeNs,
}

impl Default for OverheadConfig {
    fn default() -> Self {
        OverheadConfig {
            enabled: true,
            voluntary_csw: true,
            involuntary_csw: true,
            voluntary_csw_ns: 500,
            involuntary_csw_ns: 1_000,
            csw_jitter: true,
            csw_jitter_stddev_ns: 100,
            dsq_consume_ns: 100,
            running_overhead_ns: 50,
            ipi_delivery_ns: 200,
            update_idle_overhead_ns: 50,
            wakeup_latency_floor_ns: 3_000,
            wakeup_jitter_stddev_ns: 2_000,
            migration_penalty_ns: 10_000,
            cross_llc_migration_penalty_ns: 25_000,
        }
    }
}

impl OverheadConfig {
    /// Create an OverheadConfig with defaults influenced by environment variables.
    ///
    /// Precedence: `SCX_SIM_OVERHEAD` > `SCX_SIM_INSTANT_TIMING` > hardcoded default.
    ///
    /// - `SCX_SIM_OVERHEAD=0` disables overhead; `SCX_SIM_OVERHEAD=1` enables it.
    /// - `SCX_SIM_INSTANT_TIMING=1` disables overhead (lower priority).
    /// - If neither is set, defaults to enabled.
    pub fn from_env() -> Self {
        let mut config = Self::default();
        if std::env::var("SCX_SIM_INSTANT_TIMING").ok().as_deref() == Some("1") {
            config.enabled = false;
        }
        match std::env::var("SCX_SIM_OVERHEAD").ok().as_deref() {
            Some("0") => config.enabled = false,
            Some("1") => config.enabled = true,
            _ => {}
        }

        // Per-parameter env var overrides for tuning against production traces.
        fn env_u64(name: &str) -> Option<u64> {
            std::env::var(name).ok()?.parse().ok()
        }
        if let Some(v) = env_u64("SCX_SIM_VOL_CSW_NS") {
            config.voluntary_csw_ns = v;
        }
        if let Some(v) = env_u64("SCX_SIM_INVOL_CSW_NS") {
            config.involuntary_csw_ns = v;
        }
        if let Some(v) = env_u64("SCX_SIM_CSW_JITTER_NS") {
            config.csw_jitter_stddev_ns = v;
        }
        if let Some(v) = env_u64("SCX_SIM_IPI_NS") {
            config.ipi_delivery_ns = v;
        }
        if let Some(v) = env_u64("SCX_SIM_DSQ_CONSUME_NS") {
            config.dsq_consume_ns = v;
        }
        if let Some(v) = env_u64("SCX_SIM_WAKEUP_FLOOR_NS") {
            config.wakeup_latency_floor_ns = v;
        }
        if let Some(v) = env_u64("SCX_SIM_WAKEUP_JITTER_NS") {
            config.wakeup_jitter_stddev_ns = v;
        }
        if let Some(v) = env_u64("SCX_SIM_MIGRATION_PENALTY_NS") {
            config.migration_penalty_ns = v;
        }
        if let Some(v) = env_u64("SCX_SIM_CROSS_LLC_PENALTY_NS") {
            config.cross_llc_migration_penalty_ns = v;
        }

        config
    }

    /// Effective IPI delivery latency: 0 when overhead is disabled.
    pub fn effective_ipi_delivery_ns(&self) -> TimeNs {
        if self.enabled {
            self.ipi_delivery_ns
        } else {
            0
        }
    }

    /// Effective DSQ consume overhead: 0 when overhead is disabled.
    pub fn effective_dsq_consume_ns(&self) -> TimeNs {
        if self.enabled {
            self.dsq_consume_ns
        } else {
            0
        }
    }

    /// Effective running callback overhead: 0 when overhead is disabled.
    pub fn effective_running_overhead_ns(&self) -> TimeNs {
        if self.enabled {
            self.running_overhead_ns
        } else {
            0
        }
    }

    /// Effective update_idle callback overhead: 0 when overhead is disabled.
    pub fn effective_update_idle_overhead_ns(&self) -> TimeNs {
        if self.enabled {
            self.update_idle_overhead_ns
        } else {
            0
        }
    }
}

/// Default PRNG seed used when no seed is specified.
pub const DEFAULT_SEED: u32 = 42;

/// Parse a seed string: a `u32` integer or `"entropy"` for OS randomness.
///
/// Returns `DEFAULT_SEED` (42) for `None` or empty strings.
pub fn parse_seed(s: Option<&str>) -> u32 {
    match s {
        None | Some("") => DEFAULT_SEED,
        Some(s) if s.eq_ignore_ascii_case("entropy") => {
            // Use OS randomness: read 4 bytes from /dev/urandom.
            let seed = {
                use std::io::Read;
                let mut buf = [0u8; 4];
                std::fs::File::open("/dev/urandom")
                    .and_then(|mut f| f.read_exact(&mut buf).map(|_| u32::from_le_bytes(buf)))
                    .unwrap_or_else(|_| {
                        // Fallback: use process ID + rough timestamp.
                        let pid = std::process::id();
                        let ts = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_nanos() as u32)
                            .unwrap_or(0);
                        pid ^ ts
                    })
            };
            // Avoid seed 0 which is a fixed point for xorshift.
            let seed = if seed == 0 { 1 } else { seed };
            warn!(
                seed,
                "seed=entropy: seeding PRNG with OS randomness \
                 (set seed={seed} to reproduce this run)"
            );
            seed
        }
        Some(s) => s.parse::<u32>().unwrap_or_else(|_| {
            panic!("seed={s:?}: expected a u32 integer or \"entropy\"");
        }),
    }
}

/// Resolve the PRNG seed from the `SCX_SIM_SEED` environment variable.
///
/// - Unset or empty: returns `DEFAULT_SEED` (42).
/// - `"entropy"` (case-insensitive): seeds from OS randomness and logs the
///   chosen value so the run can be reproduced later.
/// - Any decimal integer: parsed as a `u32` seed.
pub fn seed_from_env() -> u32 {
    parse_seed(std::env::var("SCX_SIM_SEED").ok().as_deref())
}

/// Parse a duration string with optional unit suffix into nanoseconds.
///
/// Supported formats:
/// - `"1s"`, `"0.5s"` — seconds
/// - `"500ms"` — milliseconds
/// - `"100us"`, `"100μs"` — microseconds
/// - `"1000ns"` — nanoseconds (explicit)
/// - `"1000000"` — bare number, interpreted as nanoseconds
///
/// Returns an error string if the input cannot be parsed.
pub fn parse_duration_ns(s: &str) -> Result<TimeNs, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration string".into());
    }

    // Try suffixes longest-first to avoid ambiguity (e.g. "ms" before "s").
    let (num_str, multiplier) = if let Some(n) = s.strip_suffix("ms") {
        (n, 1_000_000.0)
    } else if let Some(n) = s.strip_suffix("us") {
        (n, 1_000.0)
    } else if let Some(n) = s.strip_suffix("μs") {
        (n, 1_000.0)
    } else if let Some(n) = s.strip_suffix("ns") {
        (n, 1.0)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1_000_000_000.0)
    } else {
        // Bare number — nanoseconds
        (s, 1.0)
    };

    let num: f64 = num_str
        .trim()
        .parse()
        .map_err(|_| format!("invalid duration number: {num_str:?}"))?;

    if num < 0.0 {
        return Err(format!("duration must be non-negative: {s:?}"));
    }

    let ns = num * multiplier;
    if ns > u64::MAX as f64 {
        return Err(format!("duration overflow: {s:?}"));
    }

    Ok(ns as TimeNs)
}

/// Resolve `sched_overhead_rbc_ns` from the `SCX_SIM_RBC_NS` environment variable.
///
/// - Unset or empty: returns `Some(10)` (default: 10ns per RBC).
/// - `"0"`: returns `Some(0)` (disabled).
/// - Any decimal integer: returns `Some(value)`.
pub fn sched_overhead_rbc_ns_from_env() -> Option<u64> {
    match std::env::var("SCX_SIM_RBC_NS").ok().as_deref() {
        None | Some("") => Some(10),
        Some(s) => Some(s.parse::<u64>().unwrap_or_else(|_| {
            panic!("SCX_SIM_RBC_NS={s:?}: expected a u64 integer");
        })),
    }
}

/// Default watchdog timeout: 30 seconds (matches kernel SCX_WATCHDOG_MAX_TIMEOUT).
pub const DEFAULT_WATCHDOG_TIMEOUT_NS: TimeNs = 30_000_000_000;

/// A complete simulation scenario: CPUs, tasks, and duration.
#[derive(Debug, Clone)]
pub struct Scenario {
    pub nr_cpus: u32,
    /// SMT threads per physical core (1 = no SMT, 2 = hyperthreading).
    /// CPUs are grouped sequentially: with 4 CPUs and smt=2, CPUs 0,1
    /// share core 0 and CPUs 2,3 share core 1.
    pub smt_threads_per_core: u32,
    /// CPUs per LLC domain. 0 = single domain. Used to assign `llc_id` to CPUs.
    pub cpus_per_llc: u32,
    pub tasks: Vec<TaskDef>,
    /// Cgroup definitions (excluding root, which always exists).
    pub cgroups: Vec<CgroupDef>,
    pub duration_ns: TimeNs,
    pub noise: NoiseConfig,
    pub overhead: OverheadConfig,
    /// PRNG seed for deterministic simulation. Default: 42.
    pub seed: u32,
    /// Use insertion-order tiebreaking instead of PRNG-randomized tiebreaking
    /// for events at the same timestamp. Default: false (randomized).
    pub fixed_priority: bool,
    /// Nanoseconds per retired conditional branch in scheduler C code.
    /// `None` = disabled (no PMU counter). `Some(10)` = 10ns per RBC.
    pub sched_overhead_rbc_ns: Option<u64>,
    /// Watchdog timeout for detecting stalled runnable tasks.
    ///
    /// - `Some(ns)` — watchdog fires after `ns` simulated nanoseconds of stall.
    /// - `None` — watchdog disabled.
    /// - Default: `Some(30_000_000_000)` (30s, matching kernel default).
    pub watchdog_timeout_ns: Option<TimeNs>,
    /// Whether to ignore BPF errors (scx_bpf_error calls).
    ///
    /// - `false` — BPF errors terminate simulation with `ExitKind::ErrorBpf`.
    /// - `true` — BPF errors are logged but simulation continues.
    /// - Default: `true` (for compatibility with existing tests).
    ///
    /// Set to `false` to enable strict error detection for new tests.
    pub ignore_bpf_errors: bool,
    /// CPU hotplug events to inject during the simulation.
    pub hotplug_events: Vec<HotplugEvent>,
    /// CPU preemption events (higher-priority scheduler class).
    pub cpu_preempt_events: Vec<CpuPreemptEvent>,
    /// Cgroup migration events (task moves between cgroups at runtime).
    pub cgroup_migrate_events: Vec<CgroupMigrateEvent>,
    /// Cgroup creation events (new cgroups created at runtime).
    pub cgroup_create_events: Vec<CgroupCreateEvent>,
    /// Cgroup destruction events (cgroups destroyed at runtime).
    pub cgroup_destroy_events: Vec<CgroupDestroyEvent>,
    /// Cgroup cpuset change events (cpuset.cpus modifications at runtime).
    pub cgroup_cpuset_change_events: Vec<CgroupCpusetChangeEvent>,
    /// Enable concurrent callback interleaving at kfunc yield points.
    ///
    /// When true, dispatch callbacks for multiple idle CPUs run on
    /// separate OS threads with PRNG-driven token passing, enabling
    /// deterministic exploration of different interleavings.
    pub interleave: bool,
    /// Enable stochastic BPF timer interleaving at cgroup_bw yield sites.
    pub stochastic_timer_interleave: bool,
    /// Timer fire-ahead window for stochastic timer interleaving.
    pub stochastic_timer_interleave_window_ns: TimeNs,
    /// Approximate rate: one eligible timer is pulled once per N yield sites.
    pub stochastic_timer_interleave_one_in: u32,
    /// Force deterministic timer interleavings at targeted cgroup_bw race sites.
    pub targeted_cbw_yield_sites: bool,
    /// Fire-ahead window for targeted cgroup_bw race-site timer pulls.
    pub targeted_cbw_yield_window_ns: TimeNs,
    /// Maximum number of targeted cgroup_bw timer pulls per simulation.
    pub targeted_cbw_yield_limit: u32,
    /// Preemptive interleaving configuration.
    ///
    /// When `Some`, dispatch callbacks are additionally preempted at random
    /// retired branch count intervals via PMU timer signals. Implies
    /// `interleave = true`. When the PMU is unavailable (VMs, containers),
    /// falls back to cooperative-only interleaving with the PreemptRing.
    pub preemptive: Option<PreemptiveConfig>,
    /// Optional preemption trace for replay mode.
    ///
    /// When set, preemptive dispatch uses the recorded trace instead of
    /// random PMU timeslices, enabling exact reproduction of preemption
    /// points via hardware breakpoints.
    pub replay_trace: Option<crate::preempt::trace::PreemptionTrace>,
    /// Skip PMU timer in replay mode, using hardware breakpoint stepping only.
    ///
    /// When true, replay arms the hardware breakpoint directly at each
    /// target instruction pointer and checks the RBC count on every hit
    /// to find the right dynamic instance. Slower but deterministic.
    pub no_pmu_signal: bool,
    /// Maximum number of cgroups that can have BPF map entries allocated.
    ///
    /// This simulates BPF hash map capacity limits. In production LAVD,
    /// `CBW_NR_CGRP_MAX = 2048` limits the cgroup_bw_map size.
    /// When this limit is reached, `cgroup_init` fails with ENOMEM.
    ///
    /// - Default: 10000 (high value for normal tests).
    /// - Set to a low value (e.g., 50) to test resource exhaustion.
    pub max_cgroups: u32,
    /// IRQ events to inject during the simulation.
    pub irq_events: Vec<IrqEvent>,
    /// Scheduled futex transitions to inject (LAVD lock-holder boosting).
    pub futex_events: Vec<FutexEvent>,
    /// Native concurrency backend configuration.
    ///
    /// When `Some`, workers run truly concurrently with real locks and
    /// window-based clock throttling. Implies `interleave = true`.
    pub native_concurrent: Option<NativeConcurrentConfig>,
    /// Pause before `ops.init()` so a debugger can attach.
    ///
    /// When true, the engine writes an lldb breakpoint script next to the
    /// scheduler `.so`, prints the PID and a copy-pasteable `lldb` command,
    /// then spin-waits for a debugger to attach. After the debugger
    /// attaches and the user types `continue`, execution proceeds to
    /// `ops.init()` and hits the first breakpoint.
    pub wait_debugger: bool,
    /// Warmup period in nanoseconds.
    ///
    /// When set to a non-zero value, trace statistics (summary, TraceStats)
    /// exclude events that occurred before this simulated time. The simulation
    /// still runs from time 0, but metrics only reflect post-warmup behavior.
    /// This allows scheduler internal state (EWMA, vruntime, etc.) to converge
    /// before measurement begins.
    pub warmup_ns: TimeNs,
}

/// Builder for constructing scenarios.
pub struct ScenarioBuilder {
    nr_cpus: u32,
    smt_threads_per_core: u32,
    /// CPUs per LLC domain. 0 = all CPUs in one domain (default).
    /// E.g., cpus_per_llc=12 with nr_cpus=48 creates 4 LLC domains.
    cpus_per_llc: u32,
    tasks: Vec<TaskDef>,
    cgroups: Vec<CgroupDef>,
    duration_ns: TimeNs,
    next_pid: Pid,
    noise: NoiseConfig,
    overhead: OverheadConfig,
    seed: u32,
    fixed_priority: bool,
    sched_overhead_rbc_ns: Option<u64>,
    watchdog_timeout_ns: Option<TimeNs>,
    ignore_bpf_errors: bool,
    hotplug_events: Vec<HotplugEvent>,
    cpu_preempt_events: Vec<CpuPreemptEvent>,
    cgroup_migrate_events: Vec<CgroupMigrateEvent>,
    cgroup_create_events: Vec<CgroupCreateEvent>,
    cgroup_destroy_events: Vec<CgroupDestroyEvent>,
    cgroup_cpuset_change_events: Vec<CgroupCpusetChangeEvent>,
    interleave: bool,
    stochastic_timer_interleave: bool,
    stochastic_timer_interleave_window_ns: TimeNs,
    stochastic_timer_interleave_one_in: u32,
    targeted_cbw_yield_sites: bool,
    targeted_cbw_yield_window_ns: TimeNs,
    targeted_cbw_yield_limit: u32,
    preemptive: Option<PreemptiveConfig>,
    replay_trace: Option<crate::preempt::trace::PreemptionTrace>,
    no_pmu_signal: bool,
    max_cgroups: u32,
    irq_events: Vec<IrqEvent>,
    futex_events: Vec<FutexEvent>,
    native_concurrent: Option<NativeConcurrentConfig>,
    wait_debugger: bool,
    warmup_ns: TimeNs,
}

impl Scenario {
    pub fn builder() -> ScenarioBuilder {
        ScenarioBuilder {
            nr_cpus: 1,
            smt_threads_per_core: 1,
            cpus_per_llc: 0,
            tasks: Vec::new(),
            cgroups: Vec::new(),
            duration_ns: 100_000_000, // 100ms default
            next_pid: Pid(1),
            noise: NoiseConfig::from_env(),
            overhead: OverheadConfig::from_env(),
            seed: seed_from_env(),
            fixed_priority: false,
            sched_overhead_rbc_ns: None,
            watchdog_timeout_ns: Some(DEFAULT_WATCHDOG_TIMEOUT_NS),
            ignore_bpf_errors: true, // Default true for compatibility
            hotplug_events: Vec::new(),
            cpu_preempt_events: Vec::new(),
            cgroup_migrate_events: Vec::new(),
            cgroup_create_events: Vec::new(),
            cgroup_destroy_events: Vec::new(),
            cgroup_cpuset_change_events: Vec::new(),
            interleave: false,
            stochastic_timer_interleave: false,
            stochastic_timer_interleave_window_ns: 20_000_000,
            stochastic_timer_interleave_one_in: 4,
            targeted_cbw_yield_sites: false,
            targeted_cbw_yield_window_ns: 100_000_000,
            targeted_cbw_yield_limit: 1,
            preemptive: None,
            replay_trace: None,
            no_pmu_signal: false,
            max_cgroups: DEFAULT_MAX_CGROUPS,
            irq_events: Vec::new(),
            futex_events: Vec::new(),
            native_concurrent: None,
            wait_debugger: false,
            warmup_ns: 0,
        }
    }
}

impl ScenarioBuilder {
    /// Set the number of simulated CPUs.
    pub fn cpus(mut self, n: u32) -> Self {
        self.nr_cpus = n;
        self
    }

    /// Set SMT threads per core (default 1 = no SMT).
    ///
    /// `nr_cpus` must be divisible by this value.
    pub fn smt(mut self, threads_per_core: u32) -> Self {
        self.smt_threads_per_core = threads_per_core;
        self
    }

    /// Set the number of CPUs per LLC domain (CCX).
    ///
    /// E.g., `cpus_per_llc(12)` with 48 CPUs creates 4 LLC domains (CCXs),
    /// each containing CPUs 0-11, 12-23, 24-35, 36-47. LAVD uses LLC domains
    /// for DSQ routing and migration decisions.
    ///
    /// Default: 0 (all CPUs in a single LLC domain).
    pub fn cpus_per_llc(mut self, cpus: u32) -> Self {
        self.cpus_per_llc = cpus;
        self
    }

    /// Add a task with a full TaskDef.
    pub fn task(mut self, def: TaskDef) -> Self {
        // Advance next_pid past this task's PID to avoid collisions
        // with subsequent add_task() calls.
        if def.pid.0 >= self.next_pid.0 {
            self.next_pid = Pid(def.pid.0 + 1);
        }
        self.tasks.push(def);
        self
    }

    /// Convenience: add a task with auto-assigned PID.
    pub fn add_task(mut self, name: &str, nice: i8, behavior: TaskBehavior) -> Self {
        let pid = self.next_pid;
        self.next_pid = Pid(pid.0 + 1);
        self.tasks.push(TaskDef {
            name: name.to_string(),
            pid,
            nice,
            behavior,
            start_time_ns: 0,
            mm_id: None,
            allowed_cpus: None,
            parent_pid: None,
            cgroup_name: None,
            task_flags: 0,
            migration_disabled: 0,
        });
        self
    }

    /// Convenience: add a task with auto-assigned PID and a shared address space.
    ///
    /// Tasks with the same `MmId` are treated as threads sharing an address
    /// space, enabling wake-affine scheduling in COSMOS.
    pub fn add_task_with_mm(
        mut self,
        name: &str,
        nice: i8,
        behavior: TaskBehavior,
        mm_id: MmId,
    ) -> Self {
        let pid = self.next_pid;
        self.next_pid = Pid(pid.0 + 1);
        self.tasks.push(TaskDef {
            name: name.to_string(),
            pid,
            nice,
            behavior,
            start_time_ns: 0,
            mm_id: Some(mm_id),
            allowed_cpus: None,
            parent_pid: None,
            cgroup_name: None,
            task_flags: 0,
            migration_disabled: 0,
        });
        self
    }

    /// Set the simulation duration in nanoseconds.
    pub fn duration_ns(mut self, ns: TimeNs) -> Self {
        self.duration_ns = ns;
        self
    }

    /// Set the simulation duration in milliseconds.
    pub fn duration_ms(mut self, ms: u64) -> Self {
        self.duration_ns = ms * 1_000_000;
        self
    }

    /// Set the warmup period in nanoseconds.
    pub fn warmup_ns(mut self, ns: TimeNs) -> Self {
        self.warmup_ns = ns;
        self
    }

    /// Set the warmup period in milliseconds.
    pub fn warmup_ms(mut self, ms: u64) -> Self {
        self.warmup_ns = ms * 1_000_000;
        self
    }

    /// Enable or disable all simulation noise (tick jitter).
    pub fn noise(mut self, enabled: bool) -> Self {
        self.noise.enabled = enabled;
        self
    }

    /// Set a custom noise configuration.
    pub fn noise_config(mut self, config: NoiseConfig) -> Self {
        self.noise = config;
        self
    }

    /// Enable or disable all context switch overhead.
    pub fn overhead(mut self, enabled: bool) -> Self {
        self.overhead.enabled = enabled;
        self
    }

    /// Set a custom overhead configuration.
    pub fn overhead_config(mut self, config: OverheadConfig) -> Self {
        self.overhead = config;
        self
    }

    /// Disable all noise and overhead for instant timing.
    ///
    /// Context switches and tick interrupts happen instantaneously with zero
    /// cost. Shorthand for `.noise(false).overhead(false)`.
    pub fn instant_timing(self) -> Self {
        self.noise(false).overhead(false)
    }

    /// Set the PRNG seed for deterministic simulation.
    pub fn seed(mut self, seed: u32) -> Self {
        self.seed = seed;
        self
    }

    /// Use insertion-order tiebreaking (disable randomized event ordering).
    ///
    /// By default, events at the same timestamp are processed in a
    /// PRNG-randomized order to detect ordering-dependent bugs. With
    /// `fixed_priority(true)`, events are processed in insertion order
    /// (lower `seq` wins), matching the pre-randomization behavior.
    pub fn fixed_priority(mut self, fixed: bool) -> Self {
        self.fixed_priority = fixed;
        self
    }

    /// Set nanoseconds per retired conditional branch for PMU-based
    /// scheduler overhead measurement. `None` disables RBC counting.
    pub fn sched_overhead_rbc_ns(mut self, ns: Option<u64>) -> Self {
        self.sched_overhead_rbc_ns = ns;
        self
    }

    /// Define a cgroup with a cpuset under the root cgroup.
    ///
    /// Tasks can be assigned to this cgroup via `TaskDef::cgroup_name`.
    /// The cpuset determines which CPUs tasks in this cgroup may run on.
    pub fn cgroup(mut self, name: &str, cpuset: &[CpuId]) -> Self {
        self.cgroups.push(CgroupDef {
            name: name.to_string(),
            parent_name: None, // Under root
            cpuset: Some(cpuset.to_vec()),
            bandwidth: None,
        });
        self
    }

    /// Define a nested cgroup under an existing parent cgroup.
    ///
    /// The parent cgroup must have been defined previously via `.cgroup()`.
    pub fn cgroup_nested(mut self, name: &str, parent: &str, cpuset: Option<&[CpuId]>) -> Self {
        self.cgroups.push(CgroupDef {
            name: name.to_string(),
            parent_name: Some(parent.to_string()),
            cpuset: cpuset.map(|c| c.to_vec()),
            bandwidth: None,
        });
        self
    }

    /// Define a cgroup with bandwidth limits (cpu.max) under the root cgroup.
    ///
    /// After `cgroup_init`, `cgroup_set_bandwidth` is called with the given parameters.
    pub fn cgroup_with_bandwidth(
        mut self,
        name: &str,
        cpuset: &[CpuId],
        period_us: u64,
        quota_us: u64,
        burst_us: u64,
    ) -> Self {
        self.cgroups.push(CgroupDef {
            name: name.to_string(),
            parent_name: None,
            cpuset: Some(cpuset.to_vec()),
            bandwidth: Some(CgroupBandwidth {
                period_us,
                quota_us,
                burst_us,
            }),
        });
        self
    }

    /// Define a NESTED cgroup (under an existing parent) that also carries
    /// bandwidth limits (cpu.max).
    ///
    /// Combines [`Self::cgroup_nested`] (parenting) with
    /// [`Self::cgroup_with_bandwidth`] (cpu.max): the parent must have been
    /// defined previously, the child inherits the parent's cpuset (`None`),
    /// and after `cgroup_init` the engine calls `cgroup_set_bandwidth` with
    /// the given parameters. Use this to exercise bandwidth enforcement on a
    /// cgroup that lives below the root of the hierarchy.
    pub fn cgroup_nested_bw(
        mut self,
        name: &str,
        parent: &str,
        period_us: u64,
        quota_us: u64,
        burst_us: u64,
    ) -> Self {
        self.cgroups.push(CgroupDef {
            name: name.to_string(),
            parent_name: Some(parent.to_string()),
            cpuset: None,
            bandwidth: Some(CgroupBandwidth {
                period_us,
                quota_us,
                burst_us,
            }),
        });
        self
    }

    /// Add a task to a specific cgroup.
    ///
    /// This is a convenience wrapper for building a TaskDef with a cgroup assignment.
    pub fn add_task_in_cgroup(
        mut self,
        name: &str,
        nice: i8,
        behavior: TaskBehavior,
        cgroup: &str,
    ) -> Self {
        let pid = self.next_pid;
        self.next_pid = Pid(pid.0 + 1);
        self.tasks.push(TaskDef {
            name: name.to_string(),
            pid,
            nice,
            behavior,
            start_time_ns: 0,
            mm_id: None,
            allowed_cpus: None,
            parent_pid: None,
            cgroup_name: Some(cgroup.to_string()),
            task_flags: 0,
            migration_disabled: 0,
        });
        self
    }

    /// Set the watchdog timeout for detecting stalled runnable tasks.
    ///
    /// - `Some(ns)` — watchdog fires after `ns` simulated nanoseconds of stall.
    /// - `None` — watchdog disabled.
    ///
    /// Default: 30 seconds (matching kernel SCX_WATCHDOG_MAX_TIMEOUT).
    pub fn watchdog_timeout_ns(mut self, timeout: Option<TimeNs>) -> Self {
        self.watchdog_timeout_ns = timeout;
        self
    }

    /// Disable the watchdog (stall detection).
    ///
    /// Shorthand for `.watchdog_timeout_ns(None)`.
    pub fn no_watchdog(self) -> Self {
        self.watchdog_timeout_ns(None)
    }

    /// Enable or disable BPF error detection.
    ///
    /// - `false` — BPF errors (scx_bpf_error calls) terminate simulation
    ///   with `ExitKind::ErrorBpf`.
    /// - `true` — BPF errors are logged to stderr but simulation continues.
    ///
    /// Default: `true` (for compatibility with existing tests).
    pub fn ignore_bpf_errors(mut self, ignore: bool) -> Self {
        self.ignore_bpf_errors = ignore;
        self
    }

    /// Enable strict BPF error detection.
    ///
    /// Shorthand for `.ignore_bpf_errors(false)`.
    pub fn detect_bpf_errors(self) -> Self {
        self.ignore_bpf_errors(false)
    }

    /// Schedule a CPU to go offline at the given simulated time.
    pub fn cpu_offline_at(mut self, cpu: CpuId, time_ns: TimeNs) -> Self {
        self.hotplug_events.push(HotplugEvent {
            time_ns,
            cpu,
            online: false,
        });
        self
    }

    /// Schedule a CPU to come online at the given simulated time.
    pub fn cpu_online_at(mut self, cpu: CpuId, time_ns: TimeNs) -> Self {
        self.hotplug_events.push(HotplugEvent {
            time_ns,
            cpu,
            online: true,
        });
        self
    }

    /// Schedule a higher-priority scheduler class preemption on a CPU.
    ///
    /// At `release_at_ns`, the engine calls `cpu_release` on the CPU
    /// (simulating a higher-priority class taking over). At `acquire_at_ns`,
    /// `cpu_acquire` is called (sched_ext regains control).
    pub fn cpu_preempt(mut self, cpu: CpuId, release_at_ns: TimeNs, acquire_at_ns: TimeNs) -> Self {
        assert!(
            acquire_at_ns > release_at_ns,
            "cpu_acquire must come after cpu_release"
        );
        self.cpu_preempt_events.push(CpuPreemptEvent {
            cpu,
            release_at_ns,
            acquire_at_ns,
        });
        self
    }

    /// Schedule a task cgroup migration at a specific simulation time.
    ///
    /// At `at_ns`, the engine calls `cgroup_move` for the task, moving it
    /// from `from_cgroup` to `to_cgroup`.
    pub fn cgroup_migrate(
        mut self,
        pid: Pid,
        from_cgroup: &str,
        to_cgroup: &str,
        at_ns: TimeNs,
    ) -> Self {
        self.cgroup_migrate_events.push(CgroupMigrateEvent {
            pid,
            from_cgroup: from_cgroup.to_string(),
            to_cgroup: to_cgroup.to_string(),
            at_ns,
        });
        self
    }

    /// Schedule a cgroup to be created at a specific simulation time.
    ///
    /// At `at_ns`, the engine creates the cgroup in the registry and calls
    /// `cgroup_init`. If `max_cgroups` is set and the limit would be exceeded,
    /// `cgroup_init` returns `-ENOMEM`.
    pub fn cgroup_create_at(
        mut self,
        name: &str,
        parent: Option<&str>,
        cpuset: Option<&[CpuId]>,
        at_ns: TimeNs,
    ) -> Self {
        self.cgroup_create_events.push(CgroupCreateEvent {
            name: name.to_string(),
            parent_name: parent.map(|s| s.to_string()),
            cpuset: cpuset.map(|c| c.to_vec()),
            at_ns,
        });
        self
    }

    /// Schedule a cgroup to be destroyed at a specific simulation time.
    ///
    /// At `at_ns`, the engine calls `cgroup_exit` and removes the cgroup
    /// from the registry. All tasks in this cgroup should have been moved
    /// out before destruction.
    pub fn cgroup_destroy_at(mut self, name: &str, at_ns: TimeNs) -> Self {
        self.cgroup_destroy_events.push(CgroupDestroyEvent {
            name: name.to_string(),
            at_ns,
        });
        self
    }

    /// Add a cpuset change event (modify a cgroup's allowed CPUs at runtime).
    pub fn cgroup_cpuset_change(mut self, event: CgroupCpusetChangeEvent) -> Self {
        self.cgroup_cpuset_change_events.push(event);
        self
    }

    /// Enable concurrent callback interleaving at kfunc yield points.
    pub fn interleave(mut self, enabled: bool) -> Self {
        self.interleave = enabled;
        self
    }

    /// Enable stochastic BPF timer interleaving at cgroup_bw yield sites.
    pub fn stochastic_timer_interleave(
        mut self,
        enabled: bool,
        window_ns: TimeNs,
        one_in: u32,
    ) -> Self {
        self.stochastic_timer_interleave = enabled;
        self.stochastic_timer_interleave_window_ns = window_ns;
        self.stochastic_timer_interleave_one_in = one_in.max(1);
        self
    }

    /// Force deterministic timer interleavings at targeted cgroup_bw race sites.
    pub fn targeted_cbw_yield_sites(mut self, enabled: bool) -> Self {
        self.targeted_cbw_yield_sites = enabled;
        self
    }

    /// Set the fire-ahead window for targeted cgroup_bw race-site timer pulls.
    pub fn targeted_cbw_yield_window_ns(mut self, window_ns: TimeNs) -> Self {
        self.targeted_cbw_yield_window_ns = window_ns;
        self
    }

    /// Set the maximum number of targeted cgroup_bw timer pulls per simulation.
    pub fn targeted_cbw_yield_limit(mut self, limit: u32) -> Self {
        self.targeted_cbw_yield_limit = limit;
        self
    }

    /// Enable preemptive interleaving with the given configuration.
    ///
    /// Implies `interleave(true)`. Each dispatch callback will be
    /// preempted at random retired branch count intervals via PMU timer
    /// signals, in addition to cooperative yields at kfunc boundaries.
    pub fn preemptive(mut self, config: PreemptiveConfig) -> Self {
        self.preemptive = Some(config);
        self.interleave = true;
        self
    }

    /// Enable native concurrency backend with the given configuration.
    ///
    /// Implies `interleave(true)`. Workers run truly concurrently with
    /// real locks and window-based clock throttling instead of being
    /// serialized by a token ring.
    pub fn native_concurrent(mut self, config: NativeConcurrentConfig) -> Self {
        self.native_concurrent = Some(config);
        self.interleave = true;
        self
    }

    /// Pause before `ops.init()` so a debugger can attach.
    ///
    /// When enabled, the engine writes an lldb breakpoint script next to
    /// the scheduler `.so`, prints the PID and a copy-pasteable `lldb`
    /// command, then spin-waits for a debugger. A single `continue` from
    /// the attach stop hits the first ops breakpoint.
    pub fn wait_debugger(mut self, enabled: bool) -> Self {
        self.wait_debugger = enabled;
        self
    }

    /// Set a preemption trace for replay mode.
    ///
    /// When set, preemptive dispatch uses the recorded trace instead of
    /// random PMU timeslices, enabling exact reproduction of preemption
    /// points via hardware breakpoints.
    pub fn replay_trace(mut self, trace: crate::preempt::trace::PreemptionTrace) -> Self {
        self.replay_trace = Some(trace);
        self
    }

    /// Set the maximum number of cgroups that can have BPF map entries.
    ///
    /// This simulates BPF hash map capacity limits. In production LAVD,
    /// `CBW_NR_CGRP_MAX = 2048` limits the cgroup_bw_map size.
    /// When this limit is reached, `cgroup_init` fails with ENOMEM.
    ///
    /// - Default: 10000 (high value for normal tests).
    /// - Set to a low value (e.g., 50) to test resource exhaustion.
    pub fn max_cgroups(mut self, max: u32) -> Self {
        self.max_cgroups = max;
        self
    }

    /// Schedule a hardware interrupt on a CPU.
    pub fn hardirq(
        mut self,
        cpu: CpuId,
        at_ns: TimeNs,
        duration_ns: TimeNs,
        wake_pids: &[Pid],
    ) -> Self {
        self.irq_events.push(IrqEvent {
            cpu,
            at_ns,
            duration_ns,
            irq_type: IrqType::HardIrq,
            wake_pids: wake_pids.to_vec(),
        });
        self
    }

    /// Schedule a software interrupt (inline softirq) on a CPU.
    pub fn softirq(
        mut self,
        cpu: CpuId,
        at_ns: TimeNs,
        duration_ns: TimeNs,
        wake_pids: &[Pid],
    ) -> Self {
        self.irq_events.push(IrqEvent {
            cpu,
            at_ns,
            duration_ns,
            irq_type: IrqType::SoftIrq,
            wake_pids: wake_pids.to_vec(),
        });
        self
    }

    /// Schedule repeating IRQs (for high-IRQ-load simulation).
    ///
    /// Generates IRQ events starting at `start_ns` and repeating every
    /// `interval_ns` until the scenario duration. Each instance has the
    /// specified `duration_ns` and `irq_type`.
    pub fn periodic_irq(
        mut self,
        cpu: CpuId,
        irq_type: IrqType,
        start_ns: TimeNs,
        interval_ns: TimeNs,
        duration_ns: TimeNs,
        wake_pids: &[Pid],
    ) -> Self {
        assert!(interval_ns > 0, "periodic_irq interval must be > 0");
        let mut t = start_ns;
        // Generate events up to a reasonable bound. The engine will clip
        // events past duration_ns at run time, but we cap here to avoid
        // generating billions of events for tiny intervals.
        let limit = self.duration_ns;
        while t <= limit {
            self.irq_events.push(IrqEvent {
                cpu,
                at_ns: t,
                duration_ns,
                irq_type,
                wake_pids: wake_pids.to_vec(),
            });
            t += interval_ns;
        }
        self
    }

    /// Schedule a futex transition for `pid` at `at_ns` (LAVD lock-holder
    /// boosting). `pid` must be the task running on some CPU when the event
    /// fires (otherwise the engine warns and skips it — use a workload where
    /// the task is on-CPU at `at_ns`, e.g. a running task's mid-run window).
    ///
    /// `FutexOp::WaitAcquired` boosts the holder; `FutexOp::WakeReleased`
    /// clears the boost. See `ai_docs/FUTEX_SIM_DESIGN.md`.
    pub fn futex_event(mut self, pid: Pid, at_ns: TimeNs, op: FutexOp) -> Self {
        self.futex_events.push(FutexEvent { pid, at_ns, op });
        self
    }

    /// Build the scenario.
    pub fn build(self) -> Scenario {
        assert!(
            !self.tasks.is_empty(),
            "scenario must have at least one task"
        );
        assert!(self.nr_cpus > 0, "scenario must have at least one CPU");
        assert!(
            self.smt_threads_per_core > 0,
            "smt_threads_per_core must be at least 1"
        );
        assert!(
            self.nr_cpus.is_multiple_of(self.smt_threads_per_core),
            "nr_cpus ({}) must be divisible by smt_threads_per_core ({})",
            self.nr_cpus,
            self.smt_threads_per_core
        );
        assert!(
            !(self.native_concurrent.is_some() && self.preemptive.is_some()),
            "--native-concurrent and --preemptive are mutually exclusive: \
             native concurrent mode runs workers freely without PMU or token ring"
        );
        if self.cpus_per_llc > 0 {
            assert!(
                self.nr_cpus.is_multiple_of(self.cpus_per_llc),
                "nr_cpus ({}) must be divisible by cpus_per_llc ({})",
                self.nr_cpus,
                self.cpus_per_llc
            );
        }
        Scenario {
            nr_cpus: self.nr_cpus,
            smt_threads_per_core: self.smt_threads_per_core,
            cpus_per_llc: self.cpus_per_llc,
            tasks: self.tasks,
            cgroups: self.cgroups,
            duration_ns: self.duration_ns,
            noise: self.noise,
            overhead: self.overhead,
            seed: self.seed,
            fixed_priority: self.fixed_priority,
            sched_overhead_rbc_ns: self.sched_overhead_rbc_ns,
            watchdog_timeout_ns: self.watchdog_timeout_ns,
            ignore_bpf_errors: self.ignore_bpf_errors,
            hotplug_events: self.hotplug_events,
            cpu_preempt_events: self.cpu_preempt_events,
            cgroup_migrate_events: self.cgroup_migrate_events,
            cgroup_create_events: self.cgroup_create_events,
            cgroup_destroy_events: self.cgroup_destroy_events,
            cgroup_cpuset_change_events: self.cgroup_cpuset_change_events,
            interleave: self.interleave,
            stochastic_timer_interleave: self.stochastic_timer_interleave,
            stochastic_timer_interleave_window_ns: self.stochastic_timer_interleave_window_ns,
            stochastic_timer_interleave_one_in: self.stochastic_timer_interleave_one_in,
            targeted_cbw_yield_sites: self.targeted_cbw_yield_sites,
            targeted_cbw_yield_window_ns: self.targeted_cbw_yield_window_ns,
            targeted_cbw_yield_limit: self.targeted_cbw_yield_limit,
            preemptive: self.preemptive,
            replay_trace: self.replay_trace,
            no_pmu_signal: self.no_pmu_signal,
            max_cgroups: self.max_cgroups,
            irq_events: self.irq_events,
            futex_events: self.futex_events,
            native_concurrent: self.native_concurrent,
            wait_debugger: self.wait_debugger,
            warmup_ns: self.warmup_ns,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_duration_seconds() {
        assert_eq!(parse_duration_ns("1s").unwrap(), 1_000_000_000);
        assert_eq!(parse_duration_ns("0.5s").unwrap(), 500_000_000);
        assert_eq!(parse_duration_ns("2.5s").unwrap(), 2_500_000_000);
    }

    #[test]
    fn test_parse_duration_milliseconds() {
        assert_eq!(parse_duration_ns("500ms").unwrap(), 500_000_000);
        assert_eq!(parse_duration_ns("1ms").unwrap(), 1_000_000);
        assert_eq!(parse_duration_ns("0.5ms").unwrap(), 500_000);
    }

    #[test]
    fn test_parse_duration_microseconds() {
        assert_eq!(parse_duration_ns("100us").unwrap(), 100_000);
        assert_eq!(parse_duration_ns("100μs").unwrap(), 100_000);
        assert_eq!(parse_duration_ns("1.5us").unwrap(), 1_500);
    }

    #[test]
    fn test_parse_duration_nanoseconds() {
        assert_eq!(parse_duration_ns("1000ns").unwrap(), 1_000);
        assert_eq!(parse_duration_ns("1ns").unwrap(), 1);
    }

    #[test]
    fn test_parse_duration_bare_number() {
        assert_eq!(parse_duration_ns("1000000").unwrap(), 1_000_000);
        assert_eq!(parse_duration_ns("0").unwrap(), 0);
    }

    #[test]
    fn test_parse_duration_whitespace() {
        assert_eq!(parse_duration_ns("  500ms  ").unwrap(), 500_000_000);
        assert_eq!(parse_duration_ns(" 1 s").unwrap(), 1_000_000_000);
    }

    #[test]
    fn test_parse_duration_errors() {
        assert!(parse_duration_ns("").is_err());
        assert!(parse_duration_ns("abc").is_err());
        assert!(parse_duration_ns("-1s").is_err());
        assert!(parse_duration_ns("xs").is_err());
    }
}
