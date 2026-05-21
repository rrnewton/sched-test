//! Preemption backend trait and generic interleaving drivers.
//!
//! Provides a trait-based abstraction for different preemption backends
//! (PMU timer, hardware breakpoint replay, e9patch). Each backend
//! implements `PreemptionBackend` to define how workers are instrumented.
//!
//! # Safety
//!
//! Sub-modules perform `unsafe` operations including: `perf_event_open` and
//! `ioctl` syscalls for PMU timer setup (`pmu.rs`), `/proc/self/mem` writes
//! for hardware breakpoint replay (`replay.rs`), binary patching of loaded
//! `.so` files via e9patch (`e9patch.rs`), and raw `mmap` of shared memory
//! regions. All backends manipulate raw pointers and file descriptors that
//! must remain valid for the duration of the interleaving session.

pub mod e9patch;
pub mod native;
pub mod pmu;
pub mod replay;

use crate::engine_ring::EngineRing;
use crate::interleave::WorkerId;
use crate::preempt::PreemptRing;

/// Per-worker accounting delta merged into `structop_accum` after the worker
/// body runs. Backends populate this in [`PreemptionBackend::disarm`].
#[derive(Default)]
#[allow(dead_code)] // Used by PreemptionBackend impls; callers temporarily removed
pub(crate) struct StructopDelta {
    /// Cumulative C-code-only retired branch conditional count (from a
    /// measurement counter that pauses during kfuncs).
    pub rbc_total: u64,
    /// Number of cooperative kfunc-boundary yields + preemptive signal yields.
    pub interleave_count: u64,
}

/// Relative RBC count -- branches to execute from the current counter position.
///
/// Used by `PmuBackend` (in `pmu`) for random timeslices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelativeRbc(pub u64);

/// Absolute RBC count -- cumulative branches from the start of the current structop.
///
/// Used by `ReplayBackend` (in `replay`) for precise targeting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AbsoluteRbc(pub u64);

/// RBC target: either relative (from current position) or absolute (from structop start).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RbcTarget {
    /// Relative count from the current counter position.
    Relative(RelativeRbc),
    /// Absolute count from the start of the current structop.
    Absolute(AbsoluteRbc),
}

/// Target for preemption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreemptTarget {
    /// RBC count to preempt at (relative or absolute).
    pub count_rbc: RbcTarget,
    /// For precise backends with a known target RIP (replay).
    /// `None` for first-time recording where we don't know where we'll stop.
    pub target_rip: Option<u64>,
}

/// Trait for preemption interleaving backends.
///
/// A backend determines how worker threads are preempted during concurrent
/// scheduler execution. The generic drivers ([`run_preemptive_dispatch`],
/// [`run_preemptive_batch`]) call these methods at the appropriate lifecycle
/// points, ensuring consistent structop accounting, ops_context clearing,
/// and token ring protocol across all backends.
///
/// # Lifecycle (per worker, per-round — legacy `thread::scope` path)
///
/// 1. [`worker_setup`] — create instrumentation state, install TLS
/// 2. `ring.wait_for_token(worker_id)` — acquire execution token
/// 3. `enter_sim(state, cpu)` — enter simulation context
/// 4. [`build_target`] + [`arm`] — construct and apply preemption target
/// 5. Worker body runs (dispatch or batch event processing)
/// 6. [`disarm`] — disable instrumentation, return accounting deltas
/// 7. Common: drain structop, clear ops_context, finish, exit_sim
/// 8. [`worker_teardown`] — uninstall TLS, close fds
///
/// # Persistent worker lifecycle (split TLS, `DispatchPool` path)
///
/// At simulation start:
/// 1. [`global_setup`] — install signal handler (once)
/// 2. [`worker_initial_setup`] — open PMU fds, install TLS (once per thread)
///
/// Per round:
/// 3. [`round_reconfigure`] — cheap ioctl-only reconfiguration
/// 4. [`build_target`] + [`arm`] — construct and apply preemption target
/// 5. Worker body runs
/// 6. [`disarm`] — disable instrumentation, return accounting deltas
///
/// At simulation end:
/// 7. [`worker_final_teardown`] — uninstall TLS, close fds (once per thread)
/// 8. [`global_teardown`] — uninstall signal handler (once)
///
/// [`worker_setup`]: PreemptionBackend::worker_setup
/// [`build_target`]: PreemptionBackend::build_target
/// [`arm`]: PreemptionBackend::arm
/// [`disarm`]: PreemptionBackend::disarm
/// [`worker_teardown`]: PreemptionBackend::worker_teardown
/// [`worker_initial_setup`]: PreemptionBackend::worker_initial_setup
/// [`round_reconfigure`]: PreemptionBackend::round_reconfigure
/// [`worker_final_teardown`]: PreemptionBackend::worker_final_teardown
#[allow(dead_code)] // Trait infrastructure; callers temporarily removed during dispatch refactor
pub(crate) trait PreemptionBackend: Sync {
    /// Per-worker context created during setup, carried through arm/disarm.
    type WorkerCtx: Send;

    /// One-time global setup before spawning workers (e.g., install signal
    /// handlers). Default: no-op.
    fn global_setup(&self) {}

    /// One-time global teardown after all workers finish. Default: no-op.
    fn global_teardown(&self) {}

    /// Create per-worker instrumentation state and install preemption TLS.
    ///
    /// Legacy per-round path: opens fds, installs TLS, everything at once.
    fn worker_setup(
        &self,
        ring: &PreemptRing,
        engine: &EngineRing,
        worker_id: WorkerId,
    ) -> Self::WorkerCtx;

    /// One-time per-worker setup for persistent threads.
    ///
    /// Opens PMU fds, installs signal handler TLS (PREEMPT_CTX),
    /// installs interleave TLS (INTERLEAVE_CTX). Called once at pool
    /// creation, not per round.
    ///
    /// Default: delegates to [`worker_setup`](Self::worker_setup).
    #[allow(dead_code)] // Wired up in persistent-worker path (sim-e34b08)
    fn worker_initial_setup(
        &self,
        ring: &PreemptRing,
        engine: &EngineRing,
        worker_id: WorkerId,
    ) -> Self::WorkerCtx {
        self.worker_setup(ring, engine, worker_id)
    }

    /// Per-round reconfiguration for persistent workers (cheap, no syscalls).
    ///
    /// Resets/re-arms PMU counters via ioctl, updates context pointers.
    /// Called at the start of each round after `ring.reset()`.
    ///
    /// Default: no-op (for backends that don't need per-round reconfiguration).
    #[allow(dead_code)] // Wired up in persistent-worker path (sim-e34b08)
    fn round_reconfigure(&self, _ctx: &mut Self::WorkerCtx, _ring: &PreemptRing) {}

    /// Build the preemption target for this worker, consuming PRNG state
    /// from the ring to maintain deterministic sequencing.
    ///
    /// Returns `None` if no preemption should be armed (e.g. replay cursor
    /// exhausted). The generic drivers call this before [`arm`].
    fn build_target(&self, ctx: &Self::WorkerCtx, ring: &PreemptRing) -> Option<PreemptTarget>;

    /// Arm instrumentation with the given preemption target.
    ///
    /// Called after the worker acquires the token and enters sim. The
    /// target is constructed by [`build_target`], which handles PRNG
    /// consumption and cursor advancement.
    fn arm(&self, ctx: &mut Self::WorkerCtx, target: PreemptTarget);

    /// Disarm instrumentation after scheduler code returns.
    /// Returns structop accounting deltas to merge into `structop_accum`.
    fn disarm(&self, ctx: &mut Self::WorkerCtx) -> StructopDelta;

    /// Per-worker cleanup: uninstall preemption TLS, close fds.
    /// Called after the worker has released the token and exited sim.
    ///
    /// Legacy per-round path: full teardown every round.
    fn worker_teardown(&self, ctx: Self::WorkerCtx);

    /// One-time per-worker teardown for persistent threads.
    ///
    /// Uninstalls TLS and closes PMU fds. Called once at pool shutdown,
    /// not per round.
    ///
    /// Default: delegates to [`worker_teardown`](Self::worker_teardown).
    #[allow(dead_code)] // Wired up in persistent-worker path (sim-e34b08)
    fn worker_final_teardown(&self, ctx: Self::WorkerCtx) {
        self.worker_teardown(ctx);
    }

    /// Log the completion summary after all workers finish.
    fn log_completion(&self, ring: &PreemptRing);

    /// Whether this backend supports precise (exact RBC) targeting.
    ///
    /// PMU backends return `false` (skid). Breakpoint/Frida backends return
    /// `true`.
    fn is_precise(&self) -> bool {
        false
    }

    /// Read the current RBC count from the backend's counter.
    ///
    /// For PMU: reads from the measurement perf fd.
    /// For replay: reads from the timer perf fd.
    ///
    /// Currently unused but part of the Phase 1 trait surface for future
    /// per-structop RBC accounting. The current architecture uses
    /// cumulative counting (via `rearm_timer` in recording mode and
    /// continuous counters in replay mode).
    #[allow(dead_code)] // Phase 1 surface; callers TBD in Phase 3
    fn read_count(&self, _ctx: &Self::WorkerCtx) -> u64 {
        0
    }

    /// Reset the RBC counter to zero.
    ///
    /// Intended for per-structop RBC accounting at structop boundaries.
    /// Currently unused: recording uses per-kfunc resets via `rearm_timer`,
    /// and replay uses cumulative counting without resets. If per-structop
    /// RBC tracking is added, this should be called from `begin_structop()`
    /// in the generic dispatch/batch drivers.
    #[allow(dead_code)] // Phase 1 surface; callers TBD in Phase 3
    fn reset_count(&self, _ctx: &mut Self::WorkerCtx) {}
}
