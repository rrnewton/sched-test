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
//! (woken, resumes work)
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

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::SeqCst};

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
// Constants
// ---------------------------------------------------------------------------

/// Worker is parked (blocked on futex). Matches `PreemptRing::PARKED`.
const PARKED: u32 = 0;
/// Worker is running (holds the execution token). Matches `PreemptRing::RUNNING`.
const RUNNING: u32 = 1;

/// Engine futex word: engine is sleeping, waiting for a worker to yield.
const ENGINE_SLEEPING: u32 = 0;
/// Engine futex word: a worker has yielded and the engine should wake.
const ENGINE_WOKEN: u32 = 1;

/// Sentinel value for `yielded_worker` indicating no worker has yielded yet.
const NO_WORKER: u32 = u32::MAX;

// ---------------------------------------------------------------------------
// YieldReason
// ---------------------------------------------------------------------------

/// Why a worker yielded control to the engine.
///
/// Encoded as `u32` for async-signal-safe atomic storage.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YieldReason {
    /// Cooperative yield at a kfunc boundary.
    Cooperative = 1,
    /// PMU-driven preemption (signal handler).
    Preemption = 2,
    /// Worker has finished its dispatch round.
    Finished = 3,
}

impl YieldReason {
    /// Convert from raw `u32`. Returns `None` for invalid values.
    fn from_u32(v: u32) -> Option<Self> {
        match v {
            1 => Some(Self::Cooperative),
            2 => Some(Self::Preemption),
            3 => Some(Self::Finished),
            _ => None,
        }
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
    /// Per-worker state: `PARKED` (0) or `RUNNING` (1).
    workers: Box<[AtomicU32]>,
    /// Engine wake futex word. Workers store `ENGINE_WOKEN` and wake this
    /// to hand control to the engine.
    engine_wake: AtomicU32,
    /// Which worker just yielded (written by worker, read by engine).
    yielded_worker: AtomicU32,
    /// Why the worker yielded (written by worker, read by engine).
    yield_reason: AtomicU32,
    /// Bitmask of finished workers (up to 64).
    finished_mask: AtomicU64,
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
        let workers: Box<[AtomicU32]> = (0..total).map(|_| AtomicU32::new(PARKED)).collect();
        EngineRing {
            workers,
            engine_wake: AtomicU32::new(ENGINE_SLEEPING),
            yielded_worker: AtomicU32::new(NO_WORKER),
            yield_reason: AtomicU32::new(0),
            finished_mask: AtomicU64::new(0),
            total,
            worker_cpu_map: worker_cpu_map.into(),
        }
    }

    /// Total number of workers in the ring.
    pub fn total(&self) -> usize {
        self.total
    }

    /// Current finished bitmask (one bit per worker).
    pub fn finished_mask(&self) -> u64 {
        self.finished_mask.load(SeqCst)
    }

    /// Look up the `CpuId` for a given worker.
    pub fn cpu_for_worker(&self, worker: WorkerId) -> CpuId {
        self.worker_cpu_map[worker.0]
    }

    /// Whether all workers have finished.
    fn all_done(&self) -> bool {
        let mask = self.finished_mask.load(SeqCst);
        mask.count_ones() as usize == self.total
    }

    /// Build the full finished bitmask for `self.total` workers.
    fn full_mask(&self) -> u64 {
        if self.total == 64 {
            u64::MAX
        } else {
            (1u64 << self.total) - 1
        }
    }

    // -----------------------------------------------------------------------
    // Worker side (all async-signal-safe)
    // -----------------------------------------------------------------------

    /// Worker: block until this worker is selected to execute.
    ///
    /// **Async-signal-safe**: only atomic loads and futex syscalls.
    pub fn wait_for_token(&self, worker_id: WorkerId) {
        loop {
            if self.workers[worker_id.0].load(SeqCst) == RUNNING {
                break;
            }
            futex_wait(&self.workers[worker_id.0], PARKED);
        }
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
        self.workers[worker_id.0].store(PARKED, SeqCst);

        // 2. Publish yield info for the engine to read.
        self.yielded_worker.store(worker_id.0 as u32, SeqCst);
        self.yield_reason.store(reason as u32, SeqCst);

        // 3. Wake the engine.
        self.engine_wake.store(ENGINE_WOKEN, SeqCst);
        futex_wake(&self.engine_wake, 1);

        // 4. Block until the engine wakes us.
        loop {
            if self.workers[worker_id.0].load(SeqCst) == RUNNING {
                break;
            }
            futex_wait(&self.workers[worker_id.0], PARKED);
        }

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
        self.finished_mask.fetch_or(1u64 << worker_id.0, SeqCst);

        // Publish yield info so the engine knows who finished.
        self.yielded_worker.store(worker_id.0 as u32, SeqCst);
        self.yield_reason.store(YieldReason::Finished as u32, SeqCst);

        // Wake the engine (non-blocking).
        self.engine_wake.store(ENGINE_WOKEN, SeqCst);
        futex_wake(&self.engine_wake, 1);
    }

    // -----------------------------------------------------------------------
    // Engine side
    // -----------------------------------------------------------------------

    /// Engine: wake the first worker to begin execution.
    ///
    /// Typically called before entering [`engine_loop`](Self::engine_loop).
    pub fn start_first_worker(&self, first: WorkerId) {
        self.workers[first.0].store(RUNNING, SeqCst);
        futex_wake(&self.workers[first.0], 1);
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
            // Park the engine until a worker yields.
            self.engine_wake.store(ENGINE_SLEEPING, SeqCst);
            loop {
                if self.engine_wake.load(SeqCst) == ENGINE_WOKEN {
                    break;
                }
                futex_wait(&self.engine_wake, ENGINE_SLEEPING);
            }

            // Read yield info published by the worker.
            let raw_worker = self.yielded_worker.load(SeqCst);
            let raw_reason = self.yield_reason.load(SeqCst);

            let yielded = WorkerId(raw_worker as usize);
            // SAFETY-ish: invalid reason values should never appear because
            // only `yield_to_engine` writes the reason field with valid enum
            // discriminants. Panic loudly on corruption.
            let reason = YieldReason::from_u32(raw_reason).unwrap_or_else(|| {
                panic!(
                    "EngineRing: invalid yield_reason {raw_reason} from worker {raw_worker}"
                )
            });

            // Check if everyone is done.
            if self.all_done() {
                break;
            }

            // Ask the engine for the next worker to run.
            match on_yield(yielded, reason) {
                Some(next) => {
                    self.workers[next.0].store(RUNNING, SeqCst);
                    futex_wake(&self.workers[next.0], 1);
                }
                None => {
                    break;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ThreadOrchestrator implementation
// ---------------------------------------------------------------------------

impl crate::backend::ThreadOrchestrator for EngineRing {
    fn start(&self) {
        // Start worker 0 by default. The real usage goes through
        // `start_first_worker` + `engine_loop` with a decision callback.
        self.start_first_worker(WorkerId(0));
    }

    fn wait_all_done(&self) {
        // Spin on the finished mask until all workers are done.
        // Uses futex on engine_wake to avoid busy-waiting: each worker
        // finish wakes the engine, which gives us a convenient wakeup.
        loop {
            if self.finished_mask.load(SeqCst) == self.full_mask() {
                break;
            }
            // Wait on engine_wake as a proxy -- workers wake this on yield.
            self.engine_wake.store(ENGINE_SLEEPING, SeqCst);
            if self.finished_mask.load(SeqCst) == self.full_mask() {
                break;
            }
            futex_wait(&self.engine_wake, ENGINE_SLEEPING);
        }
    }

    fn wait_for_token(&self, worker_id: WorkerId) {
        EngineRing::wait_for_token(self, worker_id);
    }

    fn yield_token(&self, worker_id: WorkerId) -> bool {
        self.yield_to_engine(worker_id, YieldReason::Cooperative)
    }

    fn yield_to_engine(&self, worker_id: WorkerId) -> bool {
        self.yield_to_engine(worker_id, YieldReason::Cooperative)
    }

    fn finish(&self, worker_id: WorkerId) {
        self.finish_worker(worker_id);
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
    use std::sync::atomic::AtomicUsize;

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
            for i in 0..3 {
                s.spawn(move || {
                    ring_ref.wait_for_token(WorkerId(i));
                    let seq = counter_ref.fetch_add(1, SeqCst);
                    order_ref[i].store(seq, SeqCst);
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
                    let mask = ring.finished_mask.load(SeqCst);
                    for i in 0..ring.total() {
                        if mask & (1u64 << i) == 0 {
                            return Some(WorkerId(i));
                        }
                    }
                    return None;
                }
                // Round-robin among non-finished workers.
                let mask = ring.finished_mask.load(SeqCst);
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
            assert_ne!(
                seq,
                usize::MAX,
                "worker {i} was never activated"
            );
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
    fn test_yield_reason_roundtrip() {
        for reason in [
            YieldReason::Cooperative,
            YieldReason::Preemption,
            YieldReason::Finished,
        ] {
            let raw = reason as u32;
            let recovered = YieldReason::from_u32(raw);
            assert_eq!(recovered, Some(reason));
        }
        assert_eq!(YieldReason::from_u32(0), None);
        assert_eq!(YieldReason::from_u32(99), None);
    }

    #[test]
    fn test_thread_orchestrator_trait_single_worker() {
        use crate::backend::ThreadOrchestrator;

        let ring = EngineRing::new(&[CpuId(0)]);

        std::thread::scope(|s| {
            let ring_ref = &ring;

            s.spawn(move || {
                ring_ref.wait_for_token(WorkerId(0));
                // Use trait method.
                <EngineRing as ThreadOrchestrator>::finish(ring_ref, WorkerId(0));
            });

            // `start` from the trait wakes worker 0.
            <EngineRing as ThreadOrchestrator>::start(&ring);

            // Run engine loop to service the yield from finish_worker.
            ring.engine_loop(|_yielded, _reason| None);
        });

        assert!(ring.all_done());
    }
}
