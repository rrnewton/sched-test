//! Preemption backend trait, thread orchestration, and generic interleaving
//! drivers.
//!
//! Provides a trait-based abstraction for different preemption backends
//! (PMU timer, hardware breakpoint replay, e9patch). Each backend
//! implements [`PreemptionBackend`] to define how workers are instrumented;
//! the generic [`run_preemptive_dispatch`] and [`run_preemptive_batch`]
//! drivers handle the common worker lifecycle.
//!
//! Thread synchronization is decoupled from preemption instrumentation via
//! the [`ThreadOrchestrator`] trait, which captures the wait/yield/finish
//! protocol. Both [`PreemptRing`] (futex-based) and
//! [`TokenRing`](crate::interleave::TokenRing) (Mutex/Condvar-based)
//! implement this trait, enabling future backends (e.g. native concurrency)
//! to provide alternative synchronization strategies.

pub mod e9patch;
pub mod native;
pub mod pmu;
pub mod replay;

use std::collections::HashMap;

use tracing::debug;

use crate::engine::{batch_worker_body, dispatch_worker_body, EventQueue, Simulator};
use crate::ffi::Scheduler;
use crate::interleave::WorkerId;
use crate::kfuncs::{self, OpsContext, SimulatorState};
use crate::preempt::PreemptRing;
use crate::types::CpuId;

// ---------------------------------------------------------------------------
// ThreadOrchestrator — synchronization strategy abstraction
// ---------------------------------------------------------------------------

/// Trait capturing the thread synchronization protocol for concurrent
/// worker execution.
///
/// Decouples the wait/yield/finish protocol from preemption instrumentation.
/// Both [`PreemptRing`] (futex-based, signal-safe) and
/// [`TokenRing`](crate::interleave::TokenRing) (Mutex/Condvar-based)
/// implement this trait. Future backends (e.g. native concurrency with no
/// serialization) can provide alternative implementations.
///
/// # Protocol
///
/// **Orchestrator side:**
/// 1. [`start`](ThreadOrchestrator::start) — signal all workers to begin
/// 2. [`wait_all_done`](ThreadOrchestrator::wait_all_done) — block until
///    all workers have called `finish`
///
/// **Worker side:**
/// 1. [`wait_for_token`](ThreadOrchestrator::wait_for_token) — block until
///    allowed to execute
/// 2. (worker body runs)
/// 3. [`finish`](ThreadOrchestrator::finish) — signal completion, wake next
///
/// Workers may also call [`yield_token`](ThreadOrchestrator::yield_token)
/// at cooperative yield points to release and re-acquire the execution token.
///
/// The [`yield_to_engine`](ThreadOrchestrator::yield_to_engine) method is
/// the simulator-in-the-loop variant of `yield_token`: instead of picking
/// the next worker directly (PRNG), it returns control to the simulator
/// engine, which updates CPU clocks, inspects the event queue, and decides
/// what to run next. In Phase 1 (sim-a730ac) the default implementation
/// delegates to `yield_token` for backward compatibility.
pub(crate) trait ThreadOrchestrator: Sync {
    /// Orchestrator: select the first worker and wake it.
    fn start(&self);

    /// Orchestrator: block until all workers have finished.
    fn wait_all_done(&self);

    /// Worker: block until this worker is selected to execute.
    fn wait_for_token(&self, worker_id: WorkerId);

    /// Worker: release token, select next worker via PRNG, block until
    /// re-selected.
    ///
    /// Returns `true` if a different worker was selected (actual context
    /// switch), `false` if the same worker was re-selected (no-op yield).
    ///
    /// Currently called through concrete types (`PreemptRing::yield_token`,
    /// `TokenRing::yield_token`) rather than through the trait, but is part
    /// of the orchestrator protocol surface for future backends.
    #[allow(dead_code)]
    fn yield_token(&self, worker_id: WorkerId) -> bool;

    /// Worker: yield to the simulator engine for a scheduling decision.
    ///
    /// Unlike [`yield_token`](ThreadOrchestrator::yield_token) (which picks
    /// the next worker via PRNG), this returns control to the engine thread.
    /// The engine inspects state (CPU clocks, event queue) and decides which
    /// worker to resume.
    ///
    /// Default implementation delegates to `yield_token` for backward
    /// compatibility. Later phases (sim-a730ac Phases 2-3) override this
    /// with simulator-in-the-loop logic.
    #[allow(dead_code)] // Phase 1 surface; callers come in Phases 2-3
    fn yield_to_engine(&self, worker_id: WorkerId) -> bool {
        self.yield_token(worker_id)
    }

    /// Worker: mark as finished and wake the next worker (or signal
    /// all-done to the orchestrator).
    fn finish(&self, worker_id: WorkerId);
}

impl ThreadOrchestrator for PreemptRing {
    fn start(&self) {
        PreemptRing::start(self);
    }

    fn wait_all_done(&self) {
        PreemptRing::wait_all_done(self);
    }

    fn wait_for_token(&self, worker_id: WorkerId) {
        PreemptRing::wait_for_token(self, worker_id);
    }

    fn yield_token(&self, worker_id: WorkerId) -> bool {
        PreemptRing::yield_token(self, worker_id)
    }

    fn finish(&self, worker_id: WorkerId) {
        PreemptRing::finish(self, worker_id);
    }
}

/// Wrapper to send raw pointers across thread boundaries.
///
/// # Safety
///
/// Callers must ensure only one thread accesses the pointed-to data at a time
/// (enforced by PreemptRing / TokenRing token passing).
pub(crate) struct SendPtr<T>(pub *mut T);
unsafe impl<T> Send for SendPtr<T> {}
unsafe impl<T> Sync for SendPtr<T> {}

/// Per-worker accounting delta merged into `structop_accum` after the worker
/// body runs. Backends populate this in [`PreemptionBackend::disarm`].
#[derive(Default)]
pub(crate) struct StructopDelta {
    /// Cumulative C-code-only retired branch conditional count (from a
    /// measurement counter that pauses during kfuncs).
    pub rbc_total: u64,
    /// Number of cooperative kfunc-boundary yields + preemptive signal yields.
    pub interleave_count: u64,
}

/// Relative RBC count -- branches to execute from the current counter position.
///
/// Used by [`PmuBackend`](pmu::PmuBackend) for random timeslices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelativeRbc(pub u64);

/// Absolute RBC count -- cumulative branches from the start of the current structop.
///
/// Used by [`ReplayBackend`](replay::ReplayBackend) for precise targeting.
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
/// # Lifecycle (per worker)
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
/// [`worker_setup`]: PreemptionBackend::worker_setup
/// [`build_target`]: PreemptionBackend::build_target
/// [`arm`]: PreemptionBackend::arm
/// [`disarm`]: PreemptionBackend::disarm
/// [`worker_teardown`]: PreemptionBackend::worker_teardown
pub(crate) trait PreemptionBackend: Sync {
    /// Per-worker context created during setup, carried through arm/disarm.
    type WorkerCtx: Send;

    /// One-time global setup before spawning workers (e.g., install signal
    /// handlers). Default: no-op.
    fn global_setup(&self) {}

    /// One-time global teardown after all workers finish. Default: no-op.
    fn global_teardown(&self) {}

    /// Create per-worker instrumentation state and install preemption TLS.
    fn worker_setup(&self, ring: &PreemptRing, worker_id: WorkerId) -> Self::WorkerCtx;

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
    fn worker_teardown(&self, ctx: Self::WorkerCtx);

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

/// Build a target and arm instrumentation for the current worker.
///
/// Shared by [`run_preemptive_dispatch`] and [`run_preemptive_batch`] to
/// avoid duplicating the `build_target` + `arm` sequence.
fn build_and_arm<B: PreemptionBackend>(backend: &B, ctx: &mut B::WorkerCtx, ring: &PreemptRing) {
    if let Some(target) = backend.build_target(ctx, ring) {
        backend.arm(ctx, target);
    }
}

/// Drain per-worker structop deltas into the accumulator.
///
/// # Safety
/// Caller must hold the execution token (single-writer access to `sp`).
pub(crate) unsafe fn drain_structop_accum(
    sp: *mut SimulatorState,
    cpu: CpuId,
    delta: &StructopDelta,
) {
    let idx = cpu.0 as usize;
    if idx < (*sp).structop_accum.len() {
        let accum = &mut (&mut (*sp).structop_accum)[idx];
        accum.rbc_total += delta.rbc_total;
        accum.interleave_count += delta.interleave_count;
    }
}

/// Clear ops_context and release the token via the orchestrator.
///
/// Must be called AFTER disabling instrumentation (so pending signals
/// still see the true callback context) and BEFORE releasing the token
/// (so the new token holder's ops_context isn't clobbered by our
/// exit_sim).
///
/// # Safety
/// Caller must hold the execution token (single-writer access to `sp`).
pub(crate) unsafe fn clear_ops_and_finish<O: ThreadOrchestrator>(
    sp: *mut SimulatorState,
    orchestrator: &O,
    worker_id: WorkerId,
) {
    (*sp).ops_context = OpsContext::None;
    crate::preempt::set_current_ops_context(OpsContext::None);
    orchestrator.finish(worker_id);
    kfuncs::exit_sim_no_clear_ops();
}

/// Run concurrent dispatch using a [`PreemptionBackend`].
///
/// Spawns one worker per CPU, each executing `dispatch_worker_body` inside
/// the token-passing protocol with backend-specific instrumentation. The
/// common lifecycle (structop drain, ops_context clear, synchronization
/// protocol) is handled here.
///
/// Thread synchronization (wait/finish/start/wait_all_done) is routed
/// through the [`ThreadOrchestrator`] trait. For preemptive backends the
/// orchestrator is the `PreemptRing` itself; the `PreemptRing` is still
/// needed for PRNG/recording/timeslice functionality.
pub(crate) fn run_preemptive_dispatch<S, B>(
    dispatch_cpus: &[CpuId],
    state_send: &SendPtr<SimulatorState>,
    sched_send: &SendPtr<S>,
    seed: u32,
    backend: &B,
) where
    S: Scheduler,
    B: PreemptionBackend,
{
    let ring = PreemptRing::new(dispatch_cpus.len(), seed);
    run_dispatch_with_orchestrator(dispatch_cpus, state_send, sched_send, &ring, &ring, backend);
}

/// Inner dispatch driver parameterised over [`ThreadOrchestrator`].
///
/// Separated from [`run_preemptive_dispatch`] so that future backends can
/// supply a different orchestrator while reusing the same worker lifecycle.
pub(crate) fn run_dispatch_with_orchestrator<S, B, O>(
    dispatch_cpus: &[CpuId],
    state_send: &SendPtr<SimulatorState>,
    sched_send: &SendPtr<S>,
    ring: &PreemptRing,
    orchestrator: &O,
    backend: &B,
) where
    S: Scheduler,
    B: PreemptionBackend,
    O: ThreadOrchestrator,
{
    backend.global_setup();

    std::thread::scope(|s| {
        let ring_ref = ring;
        let orch_ref = orchestrator;
        let state_ref = state_send;
        let sched_ref = sched_send;

        for (i, &cpu) in dispatch_cpus.iter().enumerate() {
            let worker_id = WorkerId(i);

            s.spawn(move || {
                let sp = state_ref.0;
                let schp = sched_ref.0 as *const S;

                let mut ctx = backend.worker_setup(ring_ref, worker_id);
                orch_ref.wait_for_token(worker_id);

                // Enter sim AFTER acquiring the token to avoid racing on
                // SimulatorState.current_cpu with other workers.
                unsafe { kfuncs::enter_sim(&mut *sp, cpu) };

                build_and_arm(backend, &mut ctx, ring_ref);

                unsafe {
                    debug!(cpu = cpu.0, "enter:structop dispatch (preemptive)");
                    dispatch_worker_body(sp, schp, cpu);
                }

                let delta = backend.disarm(&mut ctx);
                unsafe { drain_structop_accum(sp, cpu, &delta) };
                unsafe { clear_ops_and_finish(sp, orch_ref, worker_id) };

                backend.worker_teardown(ctx);
            });
        }

        orchestrator.start();
        orchestrator.wait_all_done();
    });

    backend.log_completion(ring);
    backend.global_teardown();
}

/// Run concurrent batch event processing using a [`PreemptionBackend`].
///
/// Like [`run_preemptive_dispatch`] but each worker processes a batch of
/// events for its CPU via `batch_worker_body`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_preemptive_batch<S, B>(
    per_cpu: &HashMap<CpuId, Vec<crate::engine::Event>>,
    cpu_ids: &[CpuId],
    sim_send: &SendPtr<Simulator<S>>,
    state_send: &SendPtr<SimulatorState>,
    tasks_send: &SendPtr<HashMap<crate::types::Pid, crate::task::SimTask>>,
    events_send: &SendPtr<EventQueue>,
    cgroup_send: &SendPtr<crate::cgroup::CgroupRegistry>,
    seed: u32,
    watchdog_timeout: Option<crate::types::TimeNs>,
    duration_ns: crate::types::TimeNs,
    max_cgroups: u32,
    backend: &B,
) where
    S: Scheduler,
    B: PreemptionBackend,
{
    let ring = PreemptRing::new(cpu_ids.len(), seed);
    run_batch_with_orchestrator(
        per_cpu,
        cpu_ids,
        sim_send,
        state_send,
        tasks_send,
        events_send,
        cgroup_send,
        &ring,
        &ring,
        watchdog_timeout,
        duration_ns,
        max_cgroups,
        backend,
    );
}

/// Inner batch driver parameterised over [`ThreadOrchestrator`].
///
/// Separated from [`run_preemptive_batch`] so that future backends can
/// supply a different orchestrator while reusing the same worker lifecycle.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_batch_with_orchestrator<S, B, O>(
    per_cpu: &HashMap<CpuId, Vec<crate::engine::Event>>,
    cpu_ids: &[CpuId],
    sim_send: &SendPtr<Simulator<S>>,
    state_send: &SendPtr<SimulatorState>,
    tasks_send: &SendPtr<HashMap<crate::types::Pid, crate::task::SimTask>>,
    events_send: &SendPtr<EventQueue>,
    cgroup_send: &SendPtr<crate::cgroup::CgroupRegistry>,
    ring: &PreemptRing,
    orchestrator: &O,
    watchdog_timeout: Option<crate::types::TimeNs>,
    duration_ns: crate::types::TimeNs,
    max_cgroups: u32,
    backend: &B,
) where
    S: Scheduler,
    B: PreemptionBackend,
    O: ThreadOrchestrator,
{
    backend.global_setup();

    std::thread::scope(|s| {
        let ring_ref = ring;
        let orch_ref = orchestrator;
        let sim_ref = sim_send;
        let state_ref = state_send;
        let tasks_ref = tasks_send;
        let events_ref = events_send;
        let cgroup_ref = cgroup_send;

        for (i, &cpu) in cpu_ids.iter().enumerate() {
            let worker_id = WorkerId(i);
            let cpu_events = per_cpu.get(&cpu).cloned().unwrap_or_default();

            s.spawn(move || {
                let simp = sim_ref.0 as *const Simulator<S>;
                let sp = state_ref.0;

                let mut ctx = backend.worker_setup(ring_ref, worker_id);
                orch_ref.wait_for_token(worker_id);

                unsafe { kfuncs::enter_sim(&mut *sp, cpu) };

                build_and_arm(backend, &mut ctx, ring_ref);

                unsafe {
                    batch_worker_body(
                        simp,
                        sp,
                        tasks_ref.0,
                        events_ref.0,
                        cgroup_ref.0,
                        cpu_events,
                        watchdog_timeout,
                        duration_ns,
                        max_cgroups,
                    );
                }

                let delta = backend.disarm(&mut ctx);
                unsafe { drain_structop_accum(sp, cpu, &delta) };
                unsafe { clear_ops_and_finish(sp, orch_ref, worker_id) };

                backend.worker_teardown(ctx);
            });
        }

        orchestrator.start();
        orchestrator.wait_all_done();
    });

    backend.log_completion(ring);
    backend.global_teardown();
}
