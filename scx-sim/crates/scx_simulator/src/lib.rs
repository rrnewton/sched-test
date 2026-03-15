//! scx_simulator - Deterministic event-driven simulator for sched_ext schedulers.
//!
//! This crate compiles sched_ext BPF scheduler code as regular userspace C and
//! runs it in a deterministic simulation with scripted task behaviors.
//!
//! # Architecture
//!
//! - **Engine**: Event-driven simulation loop that drives scheduling decisions
//! - **Tasks**: Scripted behaviors (run/sleep/wake phases)
//! - **DSQs**: Simulated dispatch queues (FIFO and vtime-ordered)
//! - **Kfuncs**: Rust implementations of BPF helper functions the scheduler calls
//! - **FFI**: Scheduler ops trait and C interop
//!
//! # Usage
//!
//! ```rust,no_run
//! use scx_simulator::*;
//!
//! let scenario = Scenario::builder()
//!     .cpus(2)
//!     .add_task("worker", 0, TaskBehavior {
//!         phases: vec![Phase::Run(10_000_000)],
//!         repeat: RepeatMode::Forever,
//!     })
//!     .duration_ms(100)
//!     .build();
//!
//! let trace = Simulator::new(DynamicScheduler::simple()).run(scenario);
//! trace.dump();
//! ```

// === Modules not yet fully partitioned ===
pub mod cgroup;

// === Safe modules (zero unsafe) — grouped under safe/ ===
pub mod safe;

// === Unsafe-heavy modules — grouped under unsafe_impl/ ===
pub mod unsafe_impl;

// Re-export safe modules at crate root so `crate::types`, `crate::dsq`, etc.
// continue to resolve for all internal `use crate::xxx` paths.
pub use safe::bpf_trace;
pub use safe::cpu;
pub use safe::det_hashmap;
pub use safe::dsq;
pub use safe::engine;
pub use safe::fmt;
pub use safe::monitor;
pub use safe::perf;
pub(crate) use safe::perfetto;
pub use safe::rtapp;
pub use safe::scenario;
pub use safe::stats;
pub use safe::task;
pub use safe::trace;
pub use safe::types;
pub use safe::workloads;

// Re-export unsafe_impl sub-modules at crate root for backward compatibility.
// All internal `crate::ffi`, `crate::kfuncs`, etc. paths continue to resolve.
pub use unsafe_impl::backend;
pub use unsafe_impl::cgroup_wrapper;
pub use unsafe_impl::ffi;
pub use unsafe_impl::interleave;
pub use unsafe_impl::kfuncs;
pub use unsafe_impl::preempt;
pub use unsafe_impl::probes;
pub use unsafe_impl::scheduler_wrapper;
pub use unsafe_impl::sim_task;
pub use unsafe_impl::task_wrapper;

// Re-export the main public types for convenience.
pub use cgroup::{CgroupId, CgroupInfo, CgroupRegistry, DEFAULT_MAX_CGROUPS};
pub use cgroup_wrapper::{free_cgroup_raw, CgroupAlloc, CgroupPtr, CssIterGuard, SimCgroupHandle};
pub use engine::{ExitKind, SimulationResult, Simulator};
pub use ffi::{
    discover_schedulers, DebuggerInfo, DynamicScheduler, LavdPowerMode, Scheduler, SchedulerInfo,
};
pub use kfuncs::sim_clock;
pub use preempt::trace::PreemptionTrace;
pub use preempt::trace::TraceMetadata;
pub use preempt::{
    compare_checkpoints, compute_so_hash, compute_so_hash_from_path, drain_determinism_checkpoints,
    drain_preemption_records, enable_determinism_mode, enable_preemption_collection, fnv1a_combine,
    fnv1a_hash_bytes, fnv1a_hash_u64, is_determinism_mode_enabled, record_checkpoint,
    reset_preemption_sequence, scheduler_so_base, scheduler_so_path, CheckpointDivergence,
    CheckpointEvent, DeterminismCheckpoint, DivergenceType, PreemptionRecord, StructopInfo,
    INSN_BYTES_LEN,
};
pub use safe::bpf_trace::{
    BpfEventKind, BpfTrace, BpfTraceEvent, TraceComparisonResult, TraceDifferences,
};
pub use safe::fmt::{FmtN, FmtTs, SimFormat};
pub use safe::monitor::{Monitor, ProbeContext, ProbePoint};
pub use safe::perf::PmuEvent;
pub use safe::perf::RbcCounter;
pub use safe::rtapp::load_rtapp;
pub use safe::scenario::{
    CgroupBandwidth, CgroupCpusetChangeEvent, CgroupCreateEvent, CgroupDef, CgroupDestroyEvent,
    CgroupMigrateEvent, CpuPreemptEvent, HotplugEvent, IrqEvent, IrqType, NativeConcurrentConfig,
    NoiseConfig, OverheadConfig, PreemptMode, PreemptiveConfig, Scenario,
};
pub use safe::stats::{CpuStats, DistributionStats, TaskStats, TraceComparison, TraceStats};
pub use safe::trace::{
    DsqLengthSample, DsqSampleTrigger, Trace, TraceEvent, TraceKind, TraceSummary,
};
pub use safe::types::{CpuId, DsqId, KickFlags, MmId, Pid, TimeNs, Vtime};
pub use scheduler_wrapper::{OptionalPtr, SchedulerWrapper, TaskPtr};
pub use sim_task::SimTask;
pub use task::{nice_to_weight, sched_weight_to_cgroup, Phase, RepeatMode, TaskBehavior, TaskDef};
pub use task_wrapper::SimTaskHandle;

use std::sync::Mutex;

/// Global lock for serializing simulator tests.
///
/// The compiled C scheduler has global mutable state, so only one
/// simulation can run at a time within a process. Tests that use
/// `cargo test` (threads in a single process) must hold this lock.
/// `cargo nextest` (separate processes) works without it, but holding
/// the lock is harmless.
pub static SIM_LOCK: Mutex<()> = Mutex::new(());
