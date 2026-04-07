//! Persistent thread pool for dispatch and batch rounds.
//!
//! Wraps [`WorkerPool`] with type-erased round context to eliminate
//! per-dispatch-round `std::thread::scope` overhead. Worker threads are
//! created once and reused across rounds within a simulation run.
//!
//! # Architecture
//!
//! Each round, the engine:
//! 1. Creates a stack-local round context (e.g. [`CoopDispatchCtx`])
//! 2. Writes `WorkDesc` entries with a type-erased pointer to the context
//! 3. Wakes workers via [`WorkerPool::wake_workers`]
//! 4. Runs the [`EngineRing::engine_loop`] on the main thread
//! 5. Collects workers via [`WorkerPool::wait_workers_complete`]
//!
//! Workers wake up, cast the context pointer back, execute the round body
//! (TLS install, EngineRing protocol, dispatch/batch work, cleanup), then
//! signal completion and re-park.
//!
//! # Safety
//!
//! Round context structs contain raw pointers to data that lives on the
//! engine thread's stack. The engine must not return from the dispatch
//! function until all workers have completed, which is guaranteed by
//! `wait_workers_complete`. The EngineRing protocol ensures only one
//! worker accesses shared state at a time.

use std::collections::HashMap;

use tracing::debug;

use crate::backend::{
    build_and_arm, clear_ops_and_finish, drain_structop_accum, engine_pick_next,
    pick_first_by_min_clock, PreemptionBackend, SendPtr, StructopDelta,
};
use crate::engine::{batch_worker_body, dispatch_worker_body, Simulator};
use crate::engine_ring::{EngineRing, YieldReason};
use crate::ffi::Scheduler;
use crate::interleave::{self, WorkerId};
use crate::kfuncs::{self, SimArc, SimulatorState};
use crate::preempt::PreemptRing;
use crate::scheduler_wrapper::SchedulerWrapper;
use crate::types::{CpuId, TimeNs};
use crate::worker_pool::{WorkDesc, WorkerCommand, WorkerPool};

// ---------------------------------------------------------------------------
// DispatchPool — persistent thread pool for dispatch/batch rounds
// ---------------------------------------------------------------------------

/// Persistent thread pool for dispatch and batch rounds.
///
/// Created once per simulation run. Reuses threads across all dispatch
/// and batch rounds, eliminating `clone3` + `perf_event_open` overhead
/// from per-round `std::thread::scope`.
///
/// Owns a persistent [`EngineRing`] and [`PreemptRing`] that are
/// [`reset()`](EngineRing::reset) between rounds instead of reallocated.
/// This avoids per-round allocation of the 426 KB `PreemptionRecordStore`
/// and per-round futex-word arrays.
pub(crate) struct DispatchPool {
    pool: WorkerPool,
    /// Persistent engine ring, allocated once for `max_workers` CPUs.
    engine_ring: EngineRing,
    /// Persistent preempt ring, allocated once for `max_workers` workers.
    preempt_ring: PreemptRing,
}

impl DispatchPool {
    /// Create a pool with `max_workers` persistent threads.
    ///
    /// Workers install `SIM_ARC` once at creation and persist until the
    /// pool is dropped. The `cpu_ids` slice maps worker index to CPU ID
    /// and is used to initialize the persistent `EngineRing`.
    pub fn new(max_workers: usize, sim_arc: &SimArc, cpu_ids: &[CpuId], seed: u32) -> Self {
        assert!(
            cpu_ids.len() == max_workers,
            "cpu_ids.len() ({}) must equal max_workers ({max_workers})",
            cpu_ids.len()
        );
        let sim_arc_clone = sim_arc.clone();
        let pool = WorkerPool::new(
            max_workers,
            move |_worker_id| {
                // One-time TLS install on each worker thread.
                kfuncs::install_sim_arc(&sim_arc_clone);
            },
            |_worker_id, _cmd, _desc| {
                // Default work_fn unused; all rounds use WorkDesc.round_fn.
            },
        );
        let engine_ring = EngineRing::new(cpu_ids);
        let preempt_ring = PreemptRing::new(max_workers, seed);
        DispatchPool {
            pool,
            engine_ring,
            preempt_ring,
        }
    }

    /// Number of worker threads in the pool.
    pub fn max_workers(&self) -> usize {
        self.pool.total()
    }

    /// Access the persistent engine ring.
    pub fn engine_ring(&self) -> &EngineRing {
        &self.engine_ring
    }

    /// Access the persistent preempt ring.
    pub fn preempt_ring(&self) -> &PreemptRing {
        &self.preempt_ring
    }

    /// Reset both rings for a new round.
    ///
    /// Must be called before each dispatch/batch round to clear per-round
    /// state (finished mask, yield info, PRNG, preemption records).
    pub fn reset_rings(&self, seed: u32) {
        self.engine_ring.reset();
        self.preempt_ring.reset(seed);
    }
}

impl Drop for DispatchPool {
    fn drop(&mut self) {
        // WorkerPool::drop handles shutdown.
    }
}

// ---------------------------------------------------------------------------
// Cooperative dispatch via DispatchPool
// ---------------------------------------------------------------------------

/// Per-round context for cooperative dispatch.
///
/// Lives on the engine thread's stack. Workers access it via raw pointer
/// from `WorkDesc::round_ctx`.
struct CoopDispatchCtx {
    ring: *const EngineRing,
    sp: *mut SimulatorState,
    /// Type-erased scheduler pointer (*const SchedulerWrapper<S>).
    sched: *const (),
    sim_arc: *const SimArc,
    /// Pointer to the dispatch_cpus slice data.
    cpus: *const CpuId,
}

// SAFETY: All fields are raw pointers to data owned by the engine thread.
// Access is synchronized by the EngineRing token-passing protocol.
unsafe impl Send for CoopDispatchCtx {}
unsafe impl Sync for CoopDispatchCtx {}

/// Type-erased cooperative dispatch worker body.
///
/// Monomorphized per `S: Scheduler` so the function pointer type is
/// uniform `RoundFn = unsafe fn(WorkerId, *const ())`.
///
/// # Safety
///
/// `ctx_ptr` must point to a valid `CoopDispatchCtx` with a scheduler
/// pointer that is actually `*const SchedulerWrapper<S>`. The EngineRing
/// and SimulatorState must be valid for the duration of the call.
unsafe fn coop_dispatch_worker<S: Scheduler>(worker_id: WorkerId, ctx_ptr: *const ()) {
    let ctx = &*(ctx_ptr as *const CoopDispatchCtx);
    let ring = &*ctx.ring;
    let sp = ctx.sp;
    let schp = ctx.sched as *const SchedulerWrapper<S>;
    let sim_arc = &*ctx.sim_arc;
    let cpu = *ctx.cpus.add(worker_id.0);

    kfuncs::install_sim_arc(sim_arc);
    interleave::install_engine_ring(ring, worker_id);
    ring.wait_for_token(worker_id);

    kfuncs::enter_sim(&mut *sp, cpu);
    debug!(cpu = cpu.0, "enter:structop dispatch (concurrent/pooled)");
    dispatch_worker_body(&mut *sp, &*schp, cpu);

    let delta = StructopDelta {
        rbc_total: 0,
        interleave_count: crate::preempt::structop_info().interleave_count,
    };
    drain_structop_accum(sp, cpu, &delta);
    clear_ops_and_finish(sp, ring, worker_id);
    interleave::uninstall();
}

/// Run cooperative dispatch using a persistent [`DispatchPool`].
///
/// Replaces [`run_cooperative_dispatch`](crate::backend::run_cooperative_dispatch)
/// when a pool is available, eliminating per-round thread creation.
/// Uses the pool's persistent [`EngineRing`] (reset between rounds).
pub(crate) fn run_cooperative_dispatch_pooled<S: Scheduler>(
    pool: &DispatchPool,
    dispatch_cpus: &[CpuId],
    state_send: &SendPtr<SimulatorState>,
    sched_send: &SendPtr<SchedulerWrapper<S>>,
    sim_arc: &SimArc,
    seed: u32,
) {
    let nr = dispatch_cpus.len();
    pool.reset_rings(seed);
    let ring = pool.engine_ring();

    let ctx = CoopDispatchCtx {
        ring,
        sp: state_send.0,
        sched: sched_send.0 as *const (),
        sim_arc,
        cpus: dispatch_cpus.as_ptr(),
    };

    // Set work desc for each worker with the type-erased round function.
    for i in 0..nr {
        pool.pool.set_work_desc(
            WorkerId(i),
            WorkDesc {
                cpu: WorkerId(i),
                payload: 0,
                round_fn: Some(coop_dispatch_worker::<S>),
                round_ctx: &ctx as *const CoopDispatchCtx as *const (),
            },
        );
    }

    // Wake workers, run engine loop, collect.
    pool.pool.wake_workers(nr, WorkerCommand::Dispatch);

    let first = pick_first_by_min_clock(dispatch_cpus, state_send);
    ring.start_first_worker(first);
    ring.engine_loop(|_yielded, reason| {
        if reason == YieldReason::Finished {
            return engine_pick_next(dispatch_cpus, state_send, ring);
        }
        engine_pick_next(dispatch_cpus, state_send, ring)
    });

    pool.pool.wait_workers_complete(nr);
}

// ---------------------------------------------------------------------------
// Cooperative batch via DispatchPool
// ---------------------------------------------------------------------------

/// Per-round context for cooperative batch.
struct CoopBatchCtx {
    ring: *const EngineRing,
    sp: *mut SimulatorState,
    /// Type-erased simulator pointer (*const Simulator<S>).
    sim: *const (),
    sim_arc: *const SimArc,
    cpus: *const CpuId,
    /// Per-CPU event lists. Workers index by CPU to get their events.
    per_cpu: *const HashMap<CpuId, Vec<crate::engine::Event>>,
    watchdog_timeout: Option<TimeNs>,
    duration_ns: TimeNs,
    max_cgroups: u32,
}

unsafe impl Send for CoopBatchCtx {}
unsafe impl Sync for CoopBatchCtx {}

/// Type-erased cooperative batch worker body.
///
/// # Safety
///
/// Same invariants as [`coop_dispatch_worker`].
unsafe fn coop_batch_worker<S: Scheduler>(worker_id: WorkerId, ctx_ptr: *const ()) {
    let ctx = &*(ctx_ptr as *const CoopBatchCtx);
    let ring = &*ctx.ring;
    let sp = ctx.sp;
    let simp = ctx.sim as *const Simulator<S>;
    let sim_arc = &*ctx.sim_arc;
    let cpu = *ctx.cpus.add(worker_id.0);
    let cpu_events = (*ctx.per_cpu).get(&cpu).cloned().unwrap_or_default();

    kfuncs::install_sim_arc(sim_arc);
    interleave::install_engine_ring(ring, worker_id);
    ring.wait_for_token(worker_id);

    kfuncs::enter_sim(&mut *sp, cpu);
    batch_worker_body(
        &*simp,
        sim_arc,
        cpu_events,
        ctx.watchdog_timeout,
        ctx.duration_ns,
        ctx.max_cgroups,
    );

    let delta = StructopDelta {
        rbc_total: 0,
        interleave_count: crate::preempt::structop_info().interleave_count,
    };
    drain_structop_accum(sp, cpu, &delta);
    clear_ops_and_finish(sp, ring, worker_id);
    interleave::uninstall();
}

/// Run cooperative batch using a persistent [`DispatchPool`].
/// Uses the pool's persistent [`EngineRing`] (reset between rounds).
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_cooperative_batch_pooled<S: Scheduler>(
    pool: &DispatchPool,
    per_cpu: &HashMap<CpuId, Vec<crate::engine::Event>>,
    cpu_ids: &[CpuId],
    sim_send: &SendPtr<Simulator<S>>,
    state_send: &SendPtr<SimulatorState>,
    sim_arc: &SimArc,
    seed: u32,
    watchdog_timeout: Option<TimeNs>,
    duration_ns: TimeNs,
    max_cgroups: u32,
) {
    let nr = cpu_ids.len();
    pool.reset_rings(seed);
    let ring = pool.engine_ring();

    let ctx = CoopBatchCtx {
        ring,
        sp: state_send.0,
        sim: sim_send.0 as *const (),
        sim_arc,
        cpus: cpu_ids.as_ptr(),
        per_cpu,
        watchdog_timeout,
        duration_ns,
        max_cgroups,
    };

    for i in 0..nr {
        pool.pool.set_work_desc(
            WorkerId(i),
            WorkDesc {
                cpu: WorkerId(i),
                payload: 0,
                round_fn: Some(coop_batch_worker::<S>),
                round_ctx: &ctx as *const CoopBatchCtx as *const (),
            },
        );
    }

    pool.pool.wake_workers(nr, WorkerCommand::Batch);

    let first = pick_first_by_min_clock(cpu_ids, state_send);
    ring.start_first_worker(first);
    ring.engine_loop(|_yielded, reason| {
        if reason == YieldReason::Finished {
            return engine_pick_next(cpu_ids, state_send, ring);
        }
        engine_pick_next(cpu_ids, state_send, ring)
    });

    pool.pool.wait_workers_complete(nr);
}

// ---------------------------------------------------------------------------
// Preemptive dispatch via DispatchPool
// ---------------------------------------------------------------------------

/// Per-round context for preemptive dispatch.
#[allow(dead_code)]
struct PreemptDispatchCtx {
    ring: *const PreemptRing,
    engine: *const EngineRing,
    sp: *mut SimulatorState,
    /// Type-erased scheduler pointer (*const SchedulerWrapper<S>).
    sched: *const (),
    sim_arc: *const SimArc,
    cpus: *const CpuId,
    /// Type-erased backend pointer (*const B where B: PreemptionBackend).
    backend: *const (),
}

unsafe impl Send for PreemptDispatchCtx {}
unsafe impl Sync for PreemptDispatchCtx {}

/// Type-erased preemptive dispatch worker body.
///
/// # Safety
///
/// `ctx_ptr` must point to a valid `PreemptDispatchCtx` with correct
/// type-erased pointers. The PreemptRing, EngineRing, and SimulatorState
/// must be valid for the duration of the call.
#[allow(dead_code)]
unsafe fn preempt_dispatch_worker<S: Scheduler, B: PreemptionBackend>(
    worker_id: WorkerId,
    ctx_ptr: *const (),
) {
    let ctx = &*(ctx_ptr as *const PreemptDispatchCtx);
    let ring = &*ctx.ring;
    let engine = &*ctx.engine;
    let sp = ctx.sp;
    let schp = ctx.sched as *const SchedulerWrapper<S>;
    let sim_arc = &*ctx.sim_arc;
    let cpu = *ctx.cpus.add(worker_id.0);
    let backend = &*(ctx.backend as *const B);

    kfuncs::install_sim_arc(sim_arc);

    let mut bctx = backend.worker_setup(ring, engine, worker_id);
    engine.wait_for_token(worker_id);

    kfuncs::enter_sim(&mut *sp, cpu);
    build_and_arm(backend, &mut bctx, ring);

    debug!(cpu = cpu.0, "enter:structop dispatch (preemptive/pooled)");
    dispatch_worker_body(&mut *sp, &*schp, cpu);

    let delta = backend.disarm(&mut bctx);
    drain_structop_accum(sp, cpu, &delta);
    clear_ops_and_finish(sp, engine, worker_id);
    backend.worker_teardown(bctx);
}

/// Run preemptive dispatch using a persistent [`DispatchPool`].
/// Uses the pool's persistent [`EngineRing`] and [`PreemptRing`]
/// (reset between rounds).
#[allow(dead_code)]
pub(crate) fn run_preemptive_dispatch_pooled<S: Scheduler, B: PreemptionBackend>(
    pool: &DispatchPool,
    dispatch_cpus: &[CpuId],
    state_send: &SendPtr<SimulatorState>,
    sched_send: &SendPtr<SchedulerWrapper<S>>,
    sim_arc: &SimArc,
    seed: u32,
    backend: &B,
) {
    let nr = dispatch_cpus.len();
    pool.reset_rings(seed);
    let ring = pool.preempt_ring();
    let engine = pool.engine_ring();

    backend.global_setup();

    let ctx = PreemptDispatchCtx {
        ring,
        engine,
        sp: state_send.0,
        sched: sched_send.0 as *const (),
        sim_arc,
        cpus: dispatch_cpus.as_ptr(),
        backend: backend as *const B as *const (),
    };

    for i in 0..nr {
        pool.pool.set_work_desc(
            WorkerId(i),
            WorkDesc {
                cpu: WorkerId(i),
                payload: 0,
                round_fn: Some(preempt_dispatch_worker::<S, B>),
                round_ctx: &ctx as *const PreemptDispatchCtx as *const (),
            },
        );
    }

    pool.pool.wake_workers(nr, WorkerCommand::Dispatch);

    let first = pick_first_by_min_clock(dispatch_cpus, state_send);
    engine.start_first_worker(first);
    engine.engine_loop(|_yielded, reason| {
        if reason == YieldReason::Finished {
            return engine_pick_next(dispatch_cpus, state_send, engine);
        }
        engine_pick_next(dispatch_cpus, state_send, engine)
    });

    pool.pool.wait_workers_complete(nr);

    backend.log_completion(ring);
    backend.global_teardown();
}

// ---------------------------------------------------------------------------
// Preemptive batch via DispatchPool
// ---------------------------------------------------------------------------

/// Per-round context for preemptive batch.
#[allow(dead_code)]
struct PreemptBatchCtx {
    ring: *const PreemptRing,
    engine: *const EngineRing,
    sp: *mut SimulatorState,
    /// Type-erased simulator pointer (*const Simulator<S>).
    sim: *const (),
    sim_arc: *const SimArc,
    cpus: *const CpuId,
    per_cpu: *const HashMap<CpuId, Vec<crate::engine::Event>>,
    backend: *const (),
    watchdog_timeout: Option<TimeNs>,
    duration_ns: TimeNs,
    max_cgroups: u32,
}

unsafe impl Send for PreemptBatchCtx {}
unsafe impl Sync for PreemptBatchCtx {}

/// Type-erased preemptive batch worker body.
///
/// # Safety
///
/// Same invariants as [`preempt_dispatch_worker`].
#[allow(dead_code)]
unsafe fn preempt_batch_worker<S: Scheduler, B: PreemptionBackend>(
    worker_id: WorkerId,
    ctx_ptr: *const (),
) {
    let ctx = &*(ctx_ptr as *const PreemptBatchCtx);
    let ring = &*ctx.ring;
    let engine = &*ctx.engine;
    let sp = ctx.sp;
    let simp = ctx.sim as *const Simulator<S>;
    let sim_arc = &*ctx.sim_arc;
    let cpu = *ctx.cpus.add(worker_id.0);
    let backend = &*(ctx.backend as *const B);
    let cpu_events = (*ctx.per_cpu).get(&cpu).cloned().unwrap_or_default();

    kfuncs::install_sim_arc(sim_arc);

    let mut bctx = backend.worker_setup(ring, engine, worker_id);
    engine.wait_for_token(worker_id);

    kfuncs::enter_sim(&mut *sp, cpu);
    build_and_arm(backend, &mut bctx, ring);

    batch_worker_body(
        &*simp,
        sim_arc,
        cpu_events,
        ctx.watchdog_timeout,
        ctx.duration_ns,
        ctx.max_cgroups,
    );

    let delta = backend.disarm(&mut bctx);
    drain_structop_accum(sp, cpu, &delta);
    clear_ops_and_finish(sp, engine, worker_id);
    backend.worker_teardown(bctx);
}

/// Run preemptive batch using a persistent [`DispatchPool`].
/// Uses the pool's persistent [`EngineRing`] and [`PreemptRing`]
/// (reset between rounds).
#[allow(clippy::too_many_arguments)]
#[allow(dead_code)]
pub(crate) fn run_preemptive_batch_pooled<S: Scheduler, B: PreemptionBackend>(
    pool: &DispatchPool,
    per_cpu: &HashMap<CpuId, Vec<crate::engine::Event>>,
    cpu_ids: &[CpuId],
    sim_send: &SendPtr<Simulator<S>>,
    state_send: &SendPtr<SimulatorState>,
    sim_arc: &SimArc,
    seed: u32,
    watchdog_timeout: Option<TimeNs>,
    duration_ns: TimeNs,
    max_cgroups: u32,
    backend: &B,
) {
    let nr = cpu_ids.len();
    pool.reset_rings(seed);
    let ring = pool.preempt_ring();
    let engine = pool.engine_ring();

    backend.global_setup();

    let ctx = PreemptBatchCtx {
        ring,
        engine,
        sp: state_send.0,
        sim: sim_send.0 as *const (),
        sim_arc,
        cpus: cpu_ids.as_ptr(),
        per_cpu,
        backend: backend as *const B as *const (),
        watchdog_timeout,
        duration_ns,
        max_cgroups,
    };

    for i in 0..nr {
        pool.pool.set_work_desc(
            WorkerId(i),
            WorkDesc {
                cpu: WorkerId(i),
                payload: 0,
                round_fn: Some(preempt_batch_worker::<S, B>),
                round_ctx: &ctx as *const PreemptBatchCtx as *const (),
            },
        );
    }

    pool.pool.wake_workers(nr, WorkerCommand::Batch);

    let first = pick_first_by_min_clock(cpu_ids, state_send);
    engine.start_first_worker(first);
    engine.engine_loop(|_yielded, reason| {
        if reason == YieldReason::Finished {
            return engine_pick_next(cpu_ids, state_send, engine);
        }
        engine_pick_next(cpu_ids, state_send, engine)
    });

    pool.pool.wait_workers_complete(nr);

    backend.log_completion(ring);
    backend.global_teardown();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use std::sync::Arc;

    /// Build a minimal SimArc for testing (no real SimulatorState needed).
    fn test_sim_arc() -> SimArc {
        // Reuse the existing test infrastructure from kfuncs.
        use crate::cpu::SimCpu;
        use crate::dsq::DsqManager;
        use crate::kfuncs::{OpsContext, SimState, SimulatorState};
        use crate::scenario::{NoiseConfig, OverheadConfig};
        use crate::trace::Trace;
        use rand::rngs::SmallRng;
        use rand::SeedableRng;
        use std::collections::{BTreeMap, HashMap};
        use std::ptr;

        let state = SimulatorState {
            cpus: (0..2).map(|i| SimCpu::new(CpuId(i))).collect(),
            dsqs: DsqManager::new(),
            current_cpu: CpuId(0),
            trace: Trace::new(2, &[]),
            clock: 0,
            task_raw_to_pid: HashMap::new(),
            task_pid_to_raw: HashMap::new(),
            task_last_cpu: HashMap::new(),
            task_ops_state: BTreeMap::new(),
            rng: SmallRng::seed_from_u64(0xDEAD_BEEF),
            ops_context: OpsContext::None,
            pending_dispatch: None,
            dsq_iter: None,
            staged_events: Vec::new(),
            reenqueue_local_requested: false,
            pending_timer_ns: None,
            pending_timer_cpu: None,
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
            structop_accum: vec![crate::preempt::StructopInfo::default(); 2],
            native_concurrent: None,
        };
        Arc::new(std::sync::Mutex::new(SimState {
            sim: state,
            tasks: HashMap::new(),
            events: crate::engine::EventQueue::new(0, false),
            cgroup_registry: crate::CgroupRegistry::new(2, 100),
        }))
    }

    #[test]
    fn test_dispatch_pool_creation_and_drop() {
        let sim_arc = test_sim_arc();
        let cpu_ids = [CpuId(0), CpuId(1), CpuId(2), CpuId(3)];
        let pool = DispatchPool::new(4, &sim_arc, &cpu_ids, 42);
        assert_eq!(pool.max_workers(), 4);
        // Drop calls shutdown via WorkerPool::drop.
    }

    #[test]
    fn test_dispatch_pool_round_fn_called() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = Arc::clone(&counter);
        let sim_arc = test_sim_arc();

        let counter_ptr = Arc::into_raw(counter_clone);

        unsafe fn test_round_fn(_worker_id: WorkerId, ctx: *const ()) {
            let counter = &*(ctx as *const AtomicUsize);
            counter.fetch_add(1, SeqCst);
        }

        let pool = DispatchPool::new(3, &sim_arc, &[CpuId(0), CpuId(1), CpuId(2)], 42);

        for i in 0..2 {
            pool.pool.set_work_desc(
                WorkerId(i),
                WorkDesc {
                    cpu: WorkerId(i),
                    payload: 0,
                    round_fn: Some(test_round_fn),
                    round_ctx: counter_ptr as *const (),
                },
            );
        }
        pool.pool.wake_workers(2, WorkerCommand::Dispatch);
        pool.pool.wait_workers_complete(2);

        let counter_arc = unsafe { Arc::from_raw(counter_ptr) };
        assert_eq!(counter_arc.load(SeqCst), 2, "round_fn called for 2 workers");

        drop(pool);
    }

    #[test]
    fn test_dispatch_pool_multiple_rounds() {
        let counter = Arc::new(AtomicUsize::new(0));
        let sim_arc = test_sim_arc();

        unsafe fn increment_fn(_worker_id: WorkerId, ctx: *const ()) {
            let counter = &*(ctx as *const AtomicUsize);
            counter.fetch_add(1, SeqCst);
        }

        let pool = DispatchPool::new(3, &sim_arc, &[CpuId(0), CpuId(1), CpuId(2)], 42);
        let ctx_ptr = &*counter as *const AtomicUsize as *const ();

        // Run 5 rounds with 2 workers each.
        for _ in 0..5 {
            for i in 0..2 {
                pool.pool.set_work_desc(
                    WorkerId(i),
                    WorkDesc {
                        cpu: WorkerId(i),
                        payload: 0,
                        round_fn: Some(increment_fn),
                        round_ctx: ctx_ptr,
                    },
                );
            }
            pool.pool.wake_workers(2, WorkerCommand::Dispatch);
            pool.pool.wait_workers_complete(2);
        }

        assert_eq!(counter.load(SeqCst), 10, "2 workers * 5 rounds = 10");
        drop(pool);
    }
}
