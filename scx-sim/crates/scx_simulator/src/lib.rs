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
//! The standalone quick-start loads a bundled scheduler with a convenience
//! constructor (`DynamicScheduler::simple` and friends). These exist only with
//! the default `standalone` feature, which bakes in a compile-time
//! `SCHEDULER_SO_DIR`:
//!
//! ```rust,no_run
//! use scx_simulator::prelude::*;
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
//!
//! # Embedding
//!
//! An embedder builds with `default-features = false` (which drops the
//! `standalone` convenience constructors above) and loads a scheduler it built
//! itself: pass an explicit `.so` path plus a `SchedulerDefinition` to
//! [`DynamicScheduler::load_with_definition`], or the fallible
//! [`DynamicScheduler::try_load_with_definition`]. The `embed_harness` crate is
//! a worked downstream example, and `ai_docs/ktstr_scxsim_embed_contract.md`
//! documents the full link/build contract.

// === Safe modules (zero unsafe) — grouped under safe/ ===
// `pub(crate)`: the public surface is the curated re-exports below + the
// `prelude`, NOT `scx_simulator::safe::*` (no external consumer uses that path).
pub(crate) mod safe;

// === Unsafe-heavy modules — grouped under unsafe_impl/ ===
// `pub(crate)`: same as `safe` -- internals are reached via the curated
// re-exports / `prelude`, not `scx_simulator::unsafe_impl::*`.
pub(crate) mod unsafe_impl;

// Re-export the safe modules at the crate root. With `safe` now `pub(crate)`,
// these aliases + the curated items below + the `prelude` ARE the public surface.
// Aliases with no external consumer are `pub(crate)` (internal-only); a few stay
// `pub` (det_hashmap/rtapp/scenario/structops_jsonl/task/trace/workloads) because
// an external test, the bin, or a doctest still reaches them by module path,
// pending migration onto a curated re-export.
pub(crate) use safe::atomic_types;
pub(crate) use safe::cgroup;
pub(crate) use safe::cpu;
pub use safe::det_hashmap;
pub(crate) use safe::dsq;
pub(crate) use safe::engine;
pub(crate) use safe::fmt;
pub(crate) use safe::monitor;
pub(crate) use safe::perf;
pub(crate) use safe::perfetto;
pub(crate) use safe::perfetto_pb;
pub use safe::rtapp;
pub use safe::scenario;
pub(crate) use safe::stats;
pub use safe::structops_jsonl;
pub use safe::task;
pub use safe::trace;
pub(crate) use safe::types;
pub use safe::workloads;

// Re-export the unsafe_impl sub-modules at the crate root so internal
// `crate::ffi`, `crate::kfuncs`, etc. paths resolve. Most are `pub(crate)` --
// the public surface is the curated re-exports below + the `prelude`. The few
// left `pub` (backend/ffi/kfuncs/preempt/probes) still have external
// module-path consumers pending migration / a surface decision.
pub use unsafe_impl::backend;
pub(crate) use unsafe_impl::cgroup_bw_replenish;
pub(crate) use unsafe_impl::cgroup_ffi;
pub(crate) use unsafe_impl::cgroup_wrapper;
pub(crate) use unsafe_impl::engine_ring;
pub use unsafe_impl::ffi;
pub(crate) use unsafe_impl::interleave;
pub use unsafe_impl::kfuncs;
pub use unsafe_impl::preempt;
pub use unsafe_impl::probes;
pub(crate) use unsafe_impl::scheduler_wrapper;
pub(crate) use unsafe_impl::sim_task;
pub(crate) use unsafe_impl::task_wrapper;

// Re-export the main public types for convenience.
pub use cgroup::{CgroupId, CgroupInfo, CgroupRegistry, DEFAULT_MAX_CGROUPS};
pub use engine::{ExitKind, SimulationResult, Simulator};
pub use ffi::{
    discover_schedulers, DebuggerInfo, DynamicScheduler, LavdPowerMode, LoadError, Scheduler,
    SchedulerInfo,
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
pub use safe::stats::{
    percentile, CpuStats, DistributionStats, TaskStats, TraceComparison, TraceStats,
};
pub use safe::trace::{
    DsqLengthSample, DsqSampleTrigger, Trace, TraceEvent, TraceKind, TraceSummary,
};
pub use safe::types::{CpuId, DsqId, KickFlags, MmId, Pid, TimeNs, Vtime};
pub use task::{nice_to_weight, sched_weight_to_cgroup, Phase, RepeatMode, TaskBehavior, TaskDef};

/// Curated public surface for embedding scx-sim as a library.
///
/// `use scx_simulator::prelude::*` brings in just the embed flow — load a
/// scheduler ([`DynamicScheduler`] / [`LoadError`]), build a [`Scenario`], run it
/// with a [`Simulator`], inspect the [`Trace`] / [`ExitKind`] — without the
/// crate-root glob, which also re-exports internal modules. The runtime load
/// entry takes a [`SchedulerDefinition`](scxsim_build::SchedulerDefinition), so
/// the prelude re-exports the `scxsim_build` config types too: an embedder needs
/// one import at runtime.
pub mod prelude {
    pub use crate::engine::{ExitKind, SimulationResult, Simulator};
    pub use crate::ffi::{DynamicScheduler, LoadError};
    pub use crate::scenario::{CgroupDef, Scenario};
    pub use crate::task::{Phase, RepeatMode, TaskBehavior, TaskDef};
    pub use crate::trace::{Trace, TraceEvent, TraceSummary};
    pub use crate::types::{CpuId, DsqId, MmId, Pid, TimeNs, Vtime};
    pub use scxsim_build::{ConfigValue, KernelConfig, SchedulerDefinition};
}

use std::sync::Mutex;

/// Global lock for serializing simulator tests.
///
/// The compiled C scheduler has global mutable state, so only one
/// simulation can run at a time within a process. Tests that use
/// `cargo test` (threads in a single process) must hold this lock.
/// `cargo nextest` (separate processes) works without it, but holding
/// the lock is harmless.
pub static SIM_LOCK: Mutex<()> = Mutex::new(());
