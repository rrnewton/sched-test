//! Engine-mediated thread orchestrator using futex + atomics.
//!
//! Replaces PRNG-based worker selection with engine-mediated decisions:
//! the engine thread picks the next worker (e.g. by min-local-clock),
//! ensuring that simulated time advances correctly.
//!
//! # Protocol
//!
//! ```text
//! Worker                              Engine
//! ------                              ------
//! store PARKED for self
//! store yielded_worker, yield_reason
//! store ENGINE_WOKEN to engine_wake
//! futex_wake(engine_wake)
//! futex_wait(workers[self], PARKED)
//!                                     futex_wait(engine_wake, ENGINE_SLEEPING)
//!                                     read yielded_worker, yield_reason
//!                                     call on_yield(worker, reason) -> next
//!                                     store RUNNING for next
//!                                     futex_wake(workers[next])
//!                                     loop back to futex_wait
//! ```
//!
//! When a worker finishes, it sets its bit in `finished_mask`, publishes
//! yield info with [`YieldReason::Finished`], wakes the engine, and returns
//! immediately (no blocking). The engine detects all-done via the mask and
//! exits its loop.
//!
//! # Signal safety
//!
//! All worker-side methods are 100% async-signal-safe: only atomic stores
//! and raw `futex()` syscalls. This allows the PMU signal handler to call
//! [`EngineRing::yield_to_engine`] directly.
//!
//! # Type layout
//!
//! The safe enum/wrapper types (`WorkerState`, `AtomicWorkerState`,
//! `EngineWakeState`, `AtomicEngineWake`, `YieldReason`, etc.) live in
//! [`crate::atomic_types`]. This module adds the futex-using extension
//! methods and contains `EngineRing` itself.

use std::sync::atomic::AtomicU32;

use crate::atomic_types::{
    AtomicEngineWake, AtomicFinishedMask, AtomicWorkerState, AtomicYieldReason,
    AtomicYieldedWorker, EngineWakeState, WorkerState,
};
pub use crate::atomic_types::{TimeslicePrng, YieldReason};
use crate::interleave::WorkerId;
use crate::types::CpuId;

// ---------------------------------------------------------------------------
// Futex wrappers (async-signal-safe)
// ---------------------------------------------------------------------------
// Local copies identical to `crate::preempt::{futex_wait, futex_wake}`.
// These will be deduplicated into a shared crate-level module in Phase E.

/// Atomically check `*futex == expected` and sleep until woken.
///
/// Returns immediately (spurious wakeup) if the value has changed.
fn futex_wait(futex: &AtomicU32, expected: u32) {
    // SAFETY: `SYS_futex` with FUTEX_WAIT is async-signal-safe.
    // `futex` is a valid pointer to an AtomicU32.
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            futex as *const AtomicU32,
            libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG,
            expected,
            std::ptr::null::<libc::timespec>(),
            std::ptr::null::<u32>(),
            0u32,
        );
    }
    // Return value intentionally ignored -- spurious wakeups handled by caller.
}

/// Wake up to `count` threads blocked on `futex`.
fn futex_wake(futex: &AtomicU32, count: i32) {
    // SAFETY: `SYS_futex` with FUTEX_WAKE is async-signal-safe.
    // `futex` is a valid pointer to an AtomicU32.
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            futex as *const AtomicU32,
            libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG,
            count,
            std::ptr::null::<libc::timespec>(),
            std::ptr::null::<u32>(),
            0u32,
        );
    }
}

// ---------------------------------------------------------------------------
// Futex extension methods for AtomicWorkerState
// ---------------------------------------------------------------------------
// These methods require futex syscalls and therefore cannot live in the safe
// partition. They access the inner AtomicU32 via the `inner()` accessor.

impl AtomicWorkerState {
    /// Block until this worker transitions to [`WorkerState::Running`].
    ///
    /// **Async-signal-safe**: only atomic loads and futex syscalls.
    pub(crate) fn wait_until_running(&self) {
        loop {
            if self.is_running() {
                break;
            }
            futex_wait(self.inner(), WorkerState::Parked as u32);
        }
    }

    /// Wake one thread blocked on this worker's futex.
    pub(crate) fn futex_wake_one(&self) {
        futex_wake(self.inner(), 1);
    }
}

// ---------------------------------------------------------------------------
// Futex extension methods for AtomicEngineWake
// ---------------------------------------------------------------------------

impl AtomicEngineWake {
    /// Signal the engine that a worker has yielded.
    pub(crate) fn wake(&self) {
        use std::sync::atomic::Ordering::SeqCst;
        self.inner().store(EngineWakeState::Woken as u32, SeqCst);
        futex_wake(self.inner(), 1);
    }

    /// Block until the engine_wake word changes from Sleeping.
    /// Only call after `swap_sleeping()` returned `Sleeping`.
    pub(crate) fn futex_wait_sleeping(&self) {
        futex_wait(self.inner(), EngineWakeState::Sleeping as u32);
    }
}

// ---------------------------------------------------------------------------
// EngineRing
// ---------------------------------------------------------------------------

/// Engine-mediated thread orchestrator.
///
/// Workers yield to the engine thread via futex. The engine makes scheduling
/// decisions (e.g. pick the worker with the smallest local clock) and wakes
/// the chosen worker. All worker-side operations are async-signal-safe.
pub struct EngineRing {
    /// Per-worker state: parked or running.
    workers: Box<[AtomicWorkerState]>,
    /// Engine wake futex word. Workers signal this to hand control to the engine.
    engine_wake: AtomicEngineWake,
    /// Which worker just yielded (written by worker, read by engine).
    yielded_worker: AtomicYieldedWorker,
    /// Why the worker yielded (written by worker, read by engine).
    yield_reason: AtomicYieldReason,
    /// Bitmask of finished workers (up to 64).
    finished_mask: AtomicFinishedMask,
    /// Total number of workers.
    total: usize,
    /// `WorkerId` -> `CpuId` mapping for tie-breaking in min-clock selection.
    worker_cpu_map: Box<[CpuId]>,
}

impl EngineRing {
    /// Create a new engine ring for the given worker-to-CPU mapping.
    ///
    /// # Panics
    ///
    /// Panics if `worker_cpu_map` is empty or has more than 64 entries.
    pub fn new(worker_cpu_map: &[CpuId]) -> Self {
        let total = worker_cpu_map.len();
        assert!(
            total > 0 && total <= 64,
            "EngineRing supports 1-64 workers, got {total}"
        );
        let workers: Box<[AtomicWorkerState]> = (0..total)
            .map(|_| AtomicWorkerState::new_parked())
            .collect();
        EngineRing {
            workers,
            engine_wake: AtomicEngineWake::new_sleeping(),
            yielded_worker: AtomicYieldedWorker::new(),
            yield_reason: AtomicYieldReason::new(),
            finished_mask: AtomicFinishedMask::new(),
            total,
            worker_cpu_map: worker_cpu_map.into(),
        }
    }

    /// Total number of workers in the ring.
    pub fn total(&self) -> usize {
        self.total
    }

    /// Reset the ring for reuse in a new round.
    ///
    /// Resets all per-worker state, engine wake, yield info, and finished
    /// mask to their initial values. The ring can then be used for another
    /// dispatch/batch round without reallocation.
    ///
    /// # Safety contract
    ///
    /// All workers must be parked (not executing) when this is called.
    /// The engine thread calls this between rounds.
    pub fn reset(&self) {
        for w in self.workers.iter() {
            w.set_parked();
        }
        self.engine_wake.reset_sleeping();
        self.yielded_worker.reset();
        self.yield_reason.reset();
        self.finished_mask.reset();
    }

    /// Current finished bitmask (one bit per worker).
    pub fn finished_mask(&self) -> u64 {
        self.finished_mask.load()
    }

    /// Look up the `CpuId` for a given worker.
    pub fn cpu_for_worker(&self, worker: WorkerId) -> CpuId {
        self.worker_cpu_map[worker.0]
    }

    /// Whether all workers have finished.
    fn all_done(&self) -> bool {
        self.finished_mask.is_all_done(self.total)
    }

    // -----------------------------------------------------------------------
    // Worker side (all async-signal-safe)
    // -----------------------------------------------------------------------

    /// Worker: block until this worker is selected to execute.
    ///
    /// **Async-signal-safe**: only atomic loads and futex syscalls.
    pub fn wait_for_token(&self, worker_id: WorkerId) {
        self.workers[worker_id.0].wait_until_running();
    }

    /// Worker: yield control to the engine with the given reason.
    ///
    /// Parks the worker, notifies the engine, and blocks until the engine
    /// wakes this worker again. Returns `true` always (the engine always
    /// makes a scheduling decision, so a "context switch" conceptually
    /// occurred even if the engine picks the same worker).
    ///
    /// **Async-signal-safe**: only atomic stores and futex syscalls.
    pub fn yield_to_engine(&self, worker_id: WorkerId, reason: YieldReason) -> bool {
        // 1. Park ourselves to prevent wake-before-wait races.
        self.workers[worker_id.0].park();

        // 2. Publish yield info for the engine to read.
        self.yielded_worker.store(worker_id);
        self.yield_reason.store(reason);

        // 3. Wake the engine.
        self.engine_wake.wake();

        // 4. Block until the engine wakes us.
        self.workers[worker_id.0].wait_until_running();

        true
    }

    /// Worker: mark as finished and notify the engine.
    ///
    /// Sets this worker's bit in the finished mask, publishes the yield
    /// info, and wakes the engine. Unlike [`yield_to_engine`], this does
    /// NOT block -- the worker thread returns immediately and should exit.
    ///
    /// **Async-signal-safe**: only atomic stores and futex_wake.
    pub fn finish_worker(&self, worker_id: WorkerId) {
        self.finished_mask.mark_finished(worker_id);

        // Publish yield info so the engine knows who finished.
        self.yielded_worker.store(worker_id);
        self.yield_reason.store(YieldReason::Finished);

        // Wake the engine (non-blocking).
        self.engine_wake.wake();
    }

    // -----------------------------------------------------------------------
    // Engine side
    // -----------------------------------------------------------------------

    /// Engine: wake the first worker to begin execution.
    ///
    /// Typically called before entering [`engine_loop`](Self::engine_loop).
    pub fn start_first_worker(&self, first: WorkerId) {
        self.workers[first.0].set_running();
        self.workers[first.0].futex_wake_one();
    }

    /// Engine: main scheduling loop.
    ///
    /// Blocks on `engine_wake` until a worker yields. Reads the yielded
    /// worker and reason, calls `on_yield` to decide the next worker,
    /// wakes it, and re-parks.
    ///
    /// The loop exits when `on_yield` returns `None` (all workers done)
    /// or when the finished mask indicates all workers have completed.
    ///
    /// # Arguments
    ///
    /// * `on_yield` - Decision callback. Receives the yielded worker and
    ///   reason, returns `Some(next)` to wake a specific worker or `None`
    ///   to signal all-done.
    pub fn engine_loop<F>(&self, mut on_yield: F)
    where
        F: FnMut(WorkerId, YieldReason) -> Option<WorkerId>,
    {
        loop {
            // Atomically swap to Sleeping and check old value.
            // If old was Woken, a worker already signaled -- skip the wait.
            let old = self.engine_wake.swap_sleeping();
            if old == EngineWakeState::Sleeping {
                // No pending signal. Wait for a worker to wake us.
                self.engine_wake.futex_wait_sleeping();
                // Spurious wakeup possible -- re-check at top of loop.
                continue;
            }

            // old == Woken: a worker yielded. Read the yield info.
            let yielded = self.yielded_worker.load();
            let reason = self.yield_reason.load();

            if self.all_done() {
                break;
            }

            match on_yield(yielded, reason) {
                Some(next) => {
                    self.workers[next.0].set_running();
                    self.workers[next.0].futex_wake_one();
                }
                None => break,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: pick worker by minimum clock
// ---------------------------------------------------------------------------

/// Pick the non-finished worker with the smallest local clock.
///
/// Ties are broken by [`CpuId`] (lowest CPU wins) for determinism.
/// Returns `None` if all workers are finished.
///
/// This is a pure helper that does not access `EngineRing` state directly,
/// making it easy to test in isolation.
// Exercised only by this module's tests; the sequential engine never picks
// the next worker by clock (that drives the dormant interleaving path).
#[allow(dead_code)]
pub fn pick_by_min_clock(
    clocks: impl Iterator<Item = (WorkerId, CpuId, u64)>,
    finished_mask: u64,
) -> Option<WorkerId> {
    let mut best: Option<(WorkerId, CpuId, u64)> = None;

    for (worker, cpu, clock) in clocks {
        // Skip finished workers.
        if finished_mask & (1u64 << worker.0) != 0 {
            continue;
        }
        let is_better = match best {
            None => true,
            Some((_, best_cpu, best_clock)) => {
                clock < best_clock || (clock == best_clock && cpu.0 < best_cpu.0)
            }
        };
        if is_better {
            best = Some((worker, cpu, clock));
        }
    }

    best.map(|(worker, _, _)| worker)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    #[test]
    fn test_single_worker() {
        let ring = EngineRing::new(&[CpuId(0)]);

        std::thread::scope(|s| {
            let ring_ref = &ring;

            // Worker thread: wait for token, then finish.
            s.spawn(move || {
                ring_ref.wait_for_token(WorkerId(0));
                ring_ref.finish_worker(WorkerId(0));
            });

            // Engine: wake worker 0, then run the engine loop.
            ring.start_first_worker(WorkerId(0));
            ring.engine_loop(|_yielded, reason| {
                assert_eq!(reason, YieldReason::Finished);
                // All done.
                None
            });
        });

        assert!(ring.all_done());
    }

    #[test]
    fn test_three_workers_engine_decides() {
        let ring = EngineRing::new(&[CpuId(0), CpuId(1), CpuId(2)]);
        let activation_order = [
            AtomicUsize::new(usize::MAX),
            AtomicUsize::new(usize::MAX),
            AtomicUsize::new(usize::MAX),
        ];
        let counter = AtomicUsize::new(0);

        std::thread::scope(|s| {
            let ring_ref = &ring;
            let order_ref = &activation_order;
            let counter_ref = &counter;

            // Spawn 3 worker threads.
            for (i, order_slot) in order_ref.iter().enumerate() {
                s.spawn(move || {
                    ring_ref.wait_for_token(WorkerId(i));
                    let seq = counter_ref.fetch_add(1, SeqCst);
                    order_slot.store(seq, SeqCst);
                    // Yield once, then finish.
                    ring_ref.yield_to_engine(WorkerId(i), YieldReason::Cooperative);
                    ring_ref.finish_worker(WorkerId(i));
                });
            }

            // Engine: start worker 0, then round-robin on yields.
            ring.start_first_worker(WorkerId(0));
            let mut next_rr = 1usize; // round-robin pointer
            ring.engine_loop(|_yielded, reason| {
                if reason == YieldReason::Finished {
                    // Find any non-finished worker.
                    let mask = ring.finished_mask();
                    for i in 0..ring.total() {
                        if mask & (1u64 << i) == 0 {
                            return Some(WorkerId(i));
                        }
                    }
                    return None;
                }
                // Round-robin among non-finished workers.
                let mask = ring.finished_mask();
                for _ in 0..ring.total() {
                    let candidate = next_rr % ring.total();
                    next_rr += 1;
                    if mask & (1u64 << candidate) == 0 {
                        return Some(WorkerId(candidate));
                    }
                }
                None
            });
        });

        assert!(ring.all_done());

        // Verify all 3 workers recorded an activation.
        for (i, slot) in activation_order.iter().enumerate() {
            let seq = slot.load(SeqCst);
            assert_ne!(seq, usize::MAX, "worker {i} was never activated");
        }
    }

    #[test]
    fn test_min_clock_picker() {
        // 3 workers with different clocks.
        let data = [
            (WorkerId(0), CpuId(0), 100u64),
            (WorkerId(1), CpuId(1), 50u64),
            (WorkerId(2), CpuId(2), 200u64),
        ];
        let result = pick_by_min_clock(data.iter().copied(), 0);
        assert_eq!(result, Some(WorkerId(1)), "worker 1 has the smallest clock");
    }

    #[test]
    fn test_min_clock_picker_tie_broken_by_cpu() {
        // Two workers with equal clocks; lower CpuId wins.
        let data = [
            (WorkerId(0), CpuId(5), 100u64),
            (WorkerId(1), CpuId(2), 100u64),
        ];
        let result = pick_by_min_clock(data.iter().copied(), 0);
        assert_eq!(
            result,
            Some(WorkerId(1)),
            "tie broken by CpuId: CpuId(2) < CpuId(5)"
        );
    }

    #[test]
    fn test_min_clock_picker_skips_finished() {
        let data = [
            (WorkerId(0), CpuId(0), 10u64),
            (WorkerId(1), CpuId(1), 50u64),
            (WorkerId(2), CpuId(2), 30u64),
        ];
        // Worker 0 is finished (bit 0 set).
        let result = pick_by_min_clock(data.iter().copied(), 0b001);
        assert_eq!(
            result,
            Some(WorkerId(2)),
            "worker 0 is finished, worker 2 has the next smallest clock"
        );
    }

    #[test]
    fn test_min_clock_picker_all_finished() {
        let data = [
            (WorkerId(0), CpuId(0), 10u64),
            (WorkerId(1), CpuId(1), 20u64),
        ];
        let result = pick_by_min_clock(data.iter().copied(), 0b11);
        assert_eq!(result, None, "all workers finished");
    }

    #[test]
    fn test_reset_allows_reuse() {
        let ring = EngineRing::new(&[CpuId(0), CpuId(1)]);

        // Round 1: run two workers to completion.
        std::thread::scope(|s| {
            let ring_ref = &ring;
            for i in 0..2 {
                s.spawn(move || {
                    ring_ref.wait_for_token(WorkerId(i));
                    ring_ref.finish_worker(WorkerId(i));
                });
            }
            ring.start_first_worker(WorkerId(0));
            ring.engine_loop(|_yielded, reason| {
                if reason == YieldReason::Finished {
                    let mask = ring.finished_mask();
                    for i in 0..ring.total() {
                        if mask & (1u64 << i) == 0 {
                            return Some(WorkerId(i));
                        }
                    }
                    return None;
                }
                None
            });
        });
        assert!(ring.all_done());

        // Reset and run Round 2.
        ring.reset();
        assert!(!ring.all_done());
        assert_eq!(ring.finished_mask(), 0);

        std::thread::scope(|s| {
            let ring_ref = &ring;
            for i in 0..2 {
                s.spawn(move || {
                    ring_ref.wait_for_token(WorkerId(i));
                    ring_ref.finish_worker(WorkerId(i));
                });
            }
            ring.start_first_worker(WorkerId(1));
            ring.engine_loop(|_yielded, reason| {
                if reason == YieldReason::Finished {
                    let mask = ring.finished_mask();
                    for i in 0..ring.total() {
                        if mask & (1u64 << i) == 0 {
                            return Some(WorkerId(i));
                        }
                    }
                    return None;
                }
                None
            });
        });
        assert!(ring.all_done());
    }
}
