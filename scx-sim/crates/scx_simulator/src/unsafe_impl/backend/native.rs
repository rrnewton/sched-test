//! Native concurrent backend — no PMU, no token ring.
//!
//! Provides [`NullBackend`] (a [`PreemptionBackend`] with no instrumentation)
//! and [`NativeOrchestrator`] (a [`ThreadOrchestrator`] that lets all workers
//! run freely in parallel). Together they implement the `--native-concurrent`
//! dispatch path where threads run with true OS-level concurrency and no
//! serialisation.

use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::Barrier;

use tracing::{debug, info};

use super::{PreemptionBackend, StructopDelta};
use crate::engine_ring::EngineRing;
use crate::interleave::WorkerId;
use crate::preempt::PreemptRing;

// ---------------------------------------------------------------------------
// NullBackend — PreemptionBackend with no instrumentation
// ---------------------------------------------------------------------------

/// A preemption backend that installs no PMU timer, no signal handler, and
/// produces no preemption events. Used with `--native-concurrent` so that
/// the generic `run_dispatch_with_orchestrator` / `run_batch_with_orchestrator`
/// drivers can be reused without any hardware instrumentation.
#[allow(dead_code)] // PreemptionBackend impl; callers temporarily removed
pub(crate) struct NullBackend;

/// Per-worker context for NullBackend — intentionally empty.
#[allow(dead_code)] // PreemptionBackend impl; callers temporarily removed
pub(crate) struct NullWorkerCtx;

impl PreemptionBackend for NullBackend {
    type WorkerCtx = NullWorkerCtx;

    fn worker_setup(
        &self,
        _ring: &PreemptRing,
        _engine: &EngineRing,
        worker_id: WorkerId,
    ) -> NullWorkerCtx {
        // Do NOT install preempt TLS here. Workers in native-concurrent mode
        // run freely with no preemptive yield points. Installing PREEMPT_CTX
        // would cause maybe_yield_preemptive() to call PreemptRing::yield_token(),
        // but the NativeOrchestrator is the orchestrator (not PreemptRing), so
        // the worker would deadlock waiting for a token that never comes.
        //
        // All preempt TLS consumers (maybe_yield_preemptive, pause_timer,
        // resume_timer, pause_measurement, resume_measurement) gracefully
        // no-op when PREEMPT_CTX is None.
        debug!(
            worker = worker_id.0,
            "native-concurrent: worker setup (null backend)"
        );
        NullWorkerCtx
    }

    fn build_target(
        &self,
        _ctx: &NullWorkerCtx,
        _ring: &PreemptRing,
    ) -> Option<super::PreemptTarget> {
        // No preemption target — workers run uninterrupted.
        None
    }

    fn arm(&self, _ctx: &mut NullWorkerCtx, _target: super::PreemptTarget) {
        unreachable!("NullBackend::arm() should never be called (build_target returns None)");
    }

    fn disarm(&self, _ctx: &mut NullWorkerCtx) -> StructopDelta {
        StructopDelta::default()
    }

    fn worker_teardown(&self, _ctx: NullWorkerCtx) {
        // No preempt TLS to uninstall — we never installed it.
    }

    fn log_completion(&self, _ring: &PreemptRing) {
        info!("native-concurrent dispatch: complete (null backend, no preemption)");
    }
}

// ---------------------------------------------------------------------------
// NativeOrchestrator — ThreadOrchestrator with no serialisation
// ---------------------------------------------------------------------------

/// A thread orchestrator that lets all workers run freely in parallel.
///
/// Phase 1: no clock-window throttling — all workers start simultaneously
/// (via a barrier) and finish independently. An atomic counter tracks
/// completions so `wait_all_done` can detect when every worker has finished.
#[allow(dead_code)] // Callers temporarily removed during dispatch refactor
pub(crate) struct NativeOrchestrator {
    /// Barrier that workers wait on before starting work.
    barrier: Barrier,
    /// Number of workers that have called `finish`.
    finished: AtomicUsize,
    /// Total number of workers.
    total: usize,
}

impl NativeOrchestrator {
    #[allow(dead_code)] // Callers temporarily removed during dispatch refactor
    pub fn new(num_workers: usize) -> Self {
        // +1 for the orchestrator thread itself (which calls `start`).
        NativeOrchestrator {
            barrier: Barrier::new(num_workers + 1),
            finished: AtomicUsize::new(0),
            total: num_workers,
        }
    }

    /// Orchestrator: release all workers by participating in the barrier.
    #[allow(dead_code)]
    pub fn start(&self) {
        self.barrier.wait();
    }

    /// Orchestrator: spin-wait until all workers have called `finish`.
    #[allow(dead_code)]
    pub fn wait_all_done(&self) {
        while self.finished.load(Relaxed) < self.total {
            std::hint::spin_loop();
        }
    }

    /// Worker: wait on the barrier, then run concurrently.
    #[allow(dead_code)]
    pub fn wait_for_token(&self, _worker_id: WorkerId) {
        self.barrier.wait();
    }

    /// No token to yield — all workers run freely.
    #[allow(dead_code)]
    pub fn yield_token(&self, _worker_id: WorkerId) -> bool {
        false
    }

    /// Worker: mark as finished.
    #[allow(dead_code)]
    pub fn finish(&self, _worker_id: WorkerId) {
        self.finished.fetch_add(1, Relaxed);
    }
}
