//! ktstr's declarative surface, as plain data.
//!
//! This is the *input* to the lowering. It mirrors the shape of ktstr's
//! `WorkType` / `WorkSpec` / `CgroupDef` / `Op` / `Step` rather than importing
//! them, for one reason: ktstr depends on scx-sim as a library, so a Cargo edge
//! from here to ktstr would invert the dependency the placement decision exists
//! to fix.
//!
//! Mirroring costs a synchronisation obligation, and the mitigation is that
//! ktstr's `WorkType` and `WorkSpec` already derive serde with
//! `#[serde(rename_all = "snake_case")]`. These types use the same
//! representation, so ktstr's own serialised output deserialises here. The
//! round-trip is the contract; [`crate::lower`]'s tests pin the variant set so a
//! new ktstr work type shows up as a compile or test failure rather than as a
//! silently-ignored field.
//!
//! Two variants deliberately have no data behind them —
//! [`SourceWorkType::Schbench`], [`SourceWorkType::Taobench`] — and one carries
//! only a name, [`SourceWorkType::Custom`]. They are here so the lowering can
//! *recognise and refuse* them. See [`crate::LoweringError::Unsupported`].

use serde::{Deserialize, Serialize};

use crate::units::DurationNs;

/// One step of ktstr's `WorkType::Sequence`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceWorkPhase {
    Spin(DurationNs),
    Sleep(DurationNs),
    Yield(DurationNs),
    Io(DurationNs),
    AluHot(DurationNs),
}

/// How a worker is woken in `WorkType::WakeChain`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WakeMechanism {
    Futex,
    Pipe,
    Eventfd,
    CondVar,
}

/// Scheduling class a source worker requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceSchedClass {
    Normal,
    Batch,
    Idle,
    Fifo,
    RoundRobin,
}

/// ktstr's `WorkType`, as data.
///
/// All 45 variants are represented. Where ktstr's variant carries tuning knobs
/// the simulator has no model for (cache footprints, strides, byte counts), the
/// field is kept rather than dropped at this layer — the lowering needs it to
/// say *what* it discarded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceWorkType {
    // --- pure CPU / timing ---
    SpinWait,
    YieldHeavy,
    Mixed,
    Bursty {
        burst_duration: DurationNs,
        sleep_duration: DurationNs,
    },
    IdleChurn {
        burst_duration: DurationNs,
        sleep_duration: DurationNs,
        precise_timing: bool,
    },
    Sequence {
        first: SourceWorkPhase,
        rest: Vec<SourceWorkPhase>,
    },
    AluHot {
        width: String,
    },
    SmtSiblingSpin,
    IpcVariance {
        hot_iters: u64,
        cold_iters: u64,
        period_iters: u64,
    },

    // --- I/O ---
    IoSyncWrite,
    IoRandRead,
    IoConvoy,
    PipeIo {
        burst_iters: u64,
    },

    // --- cache / memory ---
    CachePressure {
        size_kib: usize,
        stride: usize,
    },
    CacheYield {
        size_kib: usize,
        stride: usize,
    },
    CachePipe {
        size_kib: usize,
        burst_iters: u64,
    },
    PageFaultChurn {
        region_kib: usize,
        touches_per_cycle: usize,
        spin_iters: u64,
    },
    NumaWorkingSetSweep {
        region_kib: usize,
        sweep_period_ms: u64,
        target_nodes: Vec<usize>,
    },
    NumaMigrationChurn {
        period_ms: u64,
    },

    // --- multi-task structures ---
    FutexPingPong {
        spin_iters: u64,
    },
    FutexFanOut {
        fan_out: usize,
        spin_iters: u64,
    },
    WakeChain {
        depth: usize,
        wake: WakeMechanism,
        work_per_hop: DurationNs,
    },
    MutexContention {
        contenders: usize,
        hold_iters: u64,
        work_iters: u64,
    },
    ThunderingHerd {
        waiters: usize,
        batches: u64,
        inter_batch_ms: u64,
    },
    ProducerConsumerImbalance {
        producers: usize,
        consumers: usize,
        produce_rate_hz: u64,
        consume_iters: u64,
    },
    PriorityInversion {
        high_count: usize,
        medium_count: usize,
        low_count: usize,
        hold_iters: u64,
        work_iters: u64,
    },
    RtStarvation {
        rt_workers: usize,
        cfs_workers: usize,
        rt_priority: i32,
        burst_iters: u64,
    },
    PreemptStorm {
        cfs_workers: usize,
        rt_burst_iters: u64,
        rt_sleep_us: u64,
    },
    EpollStorm {
        producers: usize,
        consumers: usize,
        events_per_burst: u64,
    },
    AsymmetricWaker {
        waker_class: SourceSchedClass,
        wakee_class: SourceSchedClass,
        burst_iters: u64,
    },
    FanOutCompute {
        fan_out: usize,
        cache_footprint_kib: usize,
        operations: usize,
        sleep_usec: u64,
    },

    // --- lifecycle / sched-attribute churn ---
    ForkExit,
    NiceSweep,
    AffinityChurn {
        spin_iters: u64,
    },
    CrossAffinityChurn {
        spin_iters: u64,
    },
    PolicyChurn {
        spin_iters: u64,
    },
    SignalStorm {
        signals_per_iter: u64,
        work_iters: u64,
    },

    // --- cgroup structures ---
    CgroupChurn {
        groups: usize,
        cycle_ms: u64,
    },
    CgroupAttachStorm {
        dest: String,
        reap: String,
    },

    // --- periodic external stimulus ---
    TimerLatency {
        interval_us: u64,
    },
    NetTraffic {
        interval_us: u64,
        frame_bytes: u16,
    },
    IrqWake {
        interval_us: u64,
        frame_bytes: u16,
    },

    // --- refused: see LoweringError::Unsupported ---
    /// A raw Rust fn pointer in ktstr. Carries only the name here because there
    /// is nothing else to carry — and `#[serde(skip)]` on ktstr's side means it
    /// cannot round-trip at all.
    Custom {
        name: String,
    },
    /// A real benchmark binary.
    Schbench,
    /// A real benchmark binary.
    Taobench,
}

impl SourceWorkType {
    /// The variant name as ktstr spells it, for diagnostics and fidelity records.
    pub fn variant_name(&self) -> &'static str {
        use SourceWorkType::*;
        match self {
            SpinWait => "SpinWait",
            YieldHeavy => "YieldHeavy",
            Mixed => "Mixed",
            Bursty { .. } => "Bursty",
            IdleChurn { .. } => "IdleChurn",
            Sequence { .. } => "Sequence",
            AluHot { .. } => "AluHot",
            SmtSiblingSpin => "SmtSiblingSpin",
            IpcVariance { .. } => "IpcVariance",
            IoSyncWrite => "IoSyncWrite",
            IoRandRead => "IoRandRead",
            IoConvoy => "IoConvoy",
            PipeIo { .. } => "PipeIo",
            CachePressure { .. } => "CachePressure",
            CacheYield { .. } => "CacheYield",
            CachePipe { .. } => "CachePipe",
            PageFaultChurn { .. } => "PageFaultChurn",
            NumaWorkingSetSweep { .. } => "NumaWorkingSetSweep",
            NumaMigrationChurn { .. } => "NumaMigrationChurn",
            FutexPingPong { .. } => "FutexPingPong",
            FutexFanOut { .. } => "FutexFanOut",
            WakeChain { .. } => "WakeChain",
            MutexContention { .. } => "MutexContention",
            ThunderingHerd { .. } => "ThunderingHerd",
            ProducerConsumerImbalance { .. } => "ProducerConsumerImbalance",
            PriorityInversion { .. } => "PriorityInversion",
            RtStarvation { .. } => "RtStarvation",
            PreemptStorm { .. } => "PreemptStorm",
            EpollStorm { .. } => "EpollStorm",
            AsymmetricWaker { .. } => "AsymmetricWaker",
            FanOutCompute { .. } => "FanOutCompute",
            ForkExit => "ForkExit",
            NiceSweep => "NiceSweep",
            AffinityChurn { .. } => "AffinityChurn",
            CrossAffinityChurn { .. } => "CrossAffinityChurn",
            PolicyChurn { .. } => "PolicyChurn",
            SignalStorm { .. } => "SignalStorm",
            CgroupChurn { .. } => "CgroupChurn",
            CgroupAttachStorm { .. } => "CgroupAttachStorm",
            TimerLatency { .. } => "TimerLatency",
            NetTraffic { .. } => "NetTraffic",
            IrqWake { .. } => "IrqWake",
            Custom { .. } => "Custom",
            Schbench => "Schbench",
            Taobench => "Taobench",
        }
    }

    /// Qualified name as it appears in ktstr source, e.g. `WorkType::SpinWait`.
    pub fn qualified_name(&self) -> String {
        format!("WorkType::{}", self.variant_name())
    }
}

/// ktstr's `CpusetSpec`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceCpuset {
    Llc(u32),
    Numa(u32),
    Range { start_frac: f64, end_frac: f64 },
    Disjoint { index: u32, of: u32 },
    Overlap { index: u32, of: u32, frac: f64 },
    Exact(Vec<u32>),
}

// NOTE: SourceCpuset, and everything containing it, deliberately derive only
// PartialEq. Range/Overlap carry f64 fractions, and Eq promises reflexivity —
// which NaN breaks. A hand-written `impl Eq` would paper over that and let a
// NaN-carrying cpuset silently compare unequal to itself inside a HashMap key.

/// ktstr's `WorkSpec`: what one group of workers does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceWorkSpec {
    /// `None` = inherit the harness default, resolved by the backend rather than
    /// baked in here. Keeping it symbolic is what lets the simulator bind its
    /// own default instead of ktstr's.
    pub workers: Option<u32>,
    pub work_type: SourceWorkType,
    pub nice: Option<i8>,
}

impl SourceWorkSpec {
    pub fn new(work_type: SourceWorkType) -> Self {
        SourceWorkSpec {
            workers: None,
            work_type,
            nice: None,
        }
    }

    pub fn workers(mut self, n: u32) -> Self {
        self.workers = Some(n);
        self
    }
}

/// ktstr's `CgroupDef`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceCgroupDef {
    pub name: String,
    pub cpuset: Option<SourceCpuset>,
    pub works: Vec<SourceWorkSpec>,
    /// `cpu.max` as (quota, period).
    pub cpu_quota: Option<(DurationNs, DurationNs)>,
    pub cpu_weight: Option<u32>,
}

impl SourceCgroupDef {
    pub fn named(name: impl Into<String>) -> Self {
        SourceCgroupDef {
            name: name.into(),
            cpuset: None,
            works: Vec::new(),
            cpu_quota: None,
            cpu_weight: None,
        }
    }

    pub fn work(mut self, w: SourceWorkSpec) -> Self {
        self.works.push(w);
        self
    }

    pub fn cpuset(mut self, c: SourceCpuset) -> Self {
        self.cpuset = Some(c);
        self
    }
}

/// ktstr's `Op` — a mid-run mutation.
///
/// Only the subset with a counterpart in the simulator's world is modelled.
/// ktstr's VM-introspection ops (`read_kernel_cold`, `capture_snapshot`,
/// `watch_snapshot`) map onto [`SourceOp::Observe`]; the rest of ktstr's op set
/// — payload spawning, scheduler attach/detach, IRQ steering, BPF map pinning —
/// has no simulator analogue and is refused by the lowering rather than
/// approximated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceOp {
    AddCgroup {
        name: String,
    },
    AddCgroupDef {
        def: SourceCgroupDef,
    },
    RemoveCgroup {
        cgroup: String,
    },
    SetCpuset {
        cgroup: String,
        cpus: SourceCpuset,
    },
    ClearCpuset {
        cgroup: String,
    },
    SwapCpusets {
        a: String,
        b: String,
    },
    MoveAllTasks {
        from: String,
        to: String,
    },
    /// A simulator system call: ask for state, append it to the run's record.
    Observe {
        label: String,
        what: String,
    },
    /// Anything else ktstr can express. Named so the lowering can refuse it with
    /// the op's own name in the message instead of a generic failure.
    Unmodelled {
        op: String,
    },
}

/// ktstr's `HoldSpec`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceHold {
    /// Fraction of the scenario duration.
    Frac(f64),
    Fixed(DurationNs),
    /// Repeat the step's ops at this interval until time runs out.
    Loop {
        interval: DurationNs,
    },
}

impl SourceHold {
    /// Hold for the whole scenario duration — ktstr's `HoldSpec::FULL`, which is
    /// defined there as `Frac(1.0)`. Named so a lowered scenario reads the same
    /// as the ktstr source it came from.
    pub const FULL: SourceHold = SourceHold::Frac(1.0);
}

/// ktstr's `Step`: setup, ops, then hold.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceStep {
    pub setup: Vec<SourceCgroupDef>,
    pub ops: Vec<SourceOp>,
    pub hold: SourceHold,
}

impl SourceStep {
    pub fn new(setup: Vec<SourceCgroupDef>, hold: SourceHold) -> Self {
        SourceStep {
            setup,
            ops: Vec::new(),
            hold,
        }
    }
}

/// Topology as ktstr's `#[ktstr_test]` attributes declare it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceTopology {
    pub numa_nodes: u32,
    pub llcs: u32,
    pub cores: u32,
    pub threads: u32,
}

impl Default for SourceTopology {
    /// ktstr's macro defaults.
    fn default() -> Self {
        SourceTopology {
            numa_nodes: 1,
            llcs: 1,
            cores: 2,
            threads: 1,
        }
    }
}

/// A whole ktstr scenario, as data: the lowering's input.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceScenario {
    pub name: String,
    pub topology: SourceTopology,
    /// ktstr's `duration_s`, defaulting to its 12s.
    pub duration: DurationNs,
    pub steps: Vec<SourceStep>,
    /// Workers per cgroup when a `SourceWorkSpec` does not say. Resolved here
    /// rather than in ktstr so the value is explicit in the record.
    pub default_workers_per_cgroup: u32,
}

impl SourceScenario {
    pub fn new(name: impl Into<String>) -> Self {
        SourceScenario {
            name: name.into(),
            topology: SourceTopology::default(),
            duration: DurationNs::from_secs(12),
            steps: Vec::new(),
            default_workers_per_cgroup: 2,
        }
    }

    pub fn step(mut self, s: SourceStep) -> Self {
        self.steps.push(s);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The source vocabulary is a wire format shared with ktstr; a serde break
    /// here desynchronises the two repos silently.
    #[test]
    fn source_scenario_round_trips_through_json() {
        let s = SourceScenario::new("demo").step(SourceStep::new(
            vec![SourceCgroupDef::named("cg_0")
                .work(SourceWorkSpec::new(SourceWorkType::SpinWait).workers(2))
                .cpuset(SourceCpuset::Llc(0))],
            SourceHold::Frac(1.0),
        ));
        let json = serde_json::to_string(&s).expect("serialize");
        let back: SourceScenario = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(s, back);
    }

    /// ktstr uses snake_case for its serde representation; matching it is what
    /// lets ktstr's own output deserialise here without a translation shim.
    #[test]
    fn work_type_serialises_snake_case() {
        let j = serde_json::to_string(&SourceWorkType::SpinWait).unwrap();
        assert_eq!(j, "\"spin_wait\"");
        let j = serde_json::to_string(&SourceWorkType::Bursty {
            burst_duration: DurationNs::from_millis(1),
            sleep_duration: DurationNs::ZERO,
        })
        .unwrap();
        assert!(j.contains("bursty"), "got {j}");
    }

    #[test]
    fn qualified_name_reads_like_ktstr_source() {
        assert_eq!(
            SourceWorkType::SpinWait.qualified_name(),
            "WorkType::SpinWait"
        );
        assert_eq!(
            SourceWorkType::CachePressure {
                size_kib: 256,
                stride: 64
            }
            .qualified_name(),
            "WorkType::CachePressure"
        );
    }
}
