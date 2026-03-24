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
// TimeslicePrng — extracted from PreemptRing for centralized PRNG
// ---------------------------------------------------------------------------

/// Atomic xorshift32 PRNG for timeslice generation.
///
/// All methods are async-signal-safe (atomic CAS only).
pub struct TimeslicePrng {
    state: AtomicU32,
}

impl TimeslicePrng {
    /// Create a new PRNG with the given seed (0 is promoted to 1).
    pub fn new(seed: u32) -> Self {
        Self {
            state: AtomicU32::new(if seed == 0 { 1 } else { seed }),
        }
    }

    /// Atomically advance the xorshift32 PRNG and return the new value.
    pub fn next(&self) -> u32 {
        loop {
            let old = self.state.load(SeqCst);
            let mut x = old;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            if self.state.compare_exchange(old, x, SeqCst, SeqCst).is_ok() {
                return x;
            }
        }
    }

    /// Roll a random timeslice in `[min, max]`.
    pub fn roll_timeslice(&self, min: u64, max: u64) -> u64 {
        debug_assert!(max >= min);
        let range = max - min;
        if range == 0 {
            return min;
        }
        min + (self.next() as u64) % (range + 1)
    }
}

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
// Typed atomic wrappers (zero-cost, repr(transparent))
// ---------------------------------------------------------------------------

/// Per-worker state: parked (blocked on futex) or running (holds token).
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerState {
    Parked = 0,
    Running = 1,
}

/// Atomic wrapper for [`WorkerState`]. Enforces that only valid states
/// are stored. All operations use SeqCst and are async-signal-safe.
#[repr(transparent)]
struct AtomicWorkerState(AtomicU32);

impl AtomicWorkerState {
    fn new_parked() -> Self {
        Self(AtomicU32::new(WorkerState::Parked as u32))
    }

    fn park(&self) {
        self.0.store(WorkerState::Parked as u32, SeqCst);
    }

    fn set_running(&self) {
        self.0.store(WorkerState::Running as u32, SeqCst);
    }

    fn is_running(&self) -> bool {
        self.0.load(SeqCst) == WorkerState::Running as u32
    }

    /// Block until this worker transitions to [`WorkerState::Running`].
    ///
    /// **Async-signal-safe**: only atomic loads and futex syscalls.
    fn wait_until_running(&self) {
        loop {
            if self.is_running() {
                break;
            }
            futex_wait(&self.0, WorkerState::Parked as u32);
        }
    }

    /// Wake one thread blocked on this worker's futex.
    fn futex_wake_one(&self) {
        futex_wake(&self.0, 1);
    }
}

/// Engine wake state: sleeping (waiting for work) or woken (worker yielded).
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EngineWakeState {
    Sleeping = 0,
    Woken = 1,
}

/// Atomic wrapper for [`EngineWakeState`]. Controls the engine's futex-based
/// sleep/wake protocol. All operations use SeqCst and are async-signal-safe.
#[repr(transparent)]
struct AtomicEngineWake(AtomicU32);

impl AtomicEngineWake {
    fn new_sleeping() -> Self {
        Self(AtomicU32::new(EngineWakeState::Sleeping as u32))
    }

    fn set_sleeping(&self) {
        self.0.store(EngineWakeState::Sleeping as u32, SeqCst);
    }

    fn is_woken(&self) -> bool {
        self.0.load(SeqCst) == EngineWakeState::Woken as u32
    }

    /// Signal the engine that a worker has yielded.
    fn wake(&self) {
        self.0.store(EngineWakeState::Woken as u32, SeqCst);
        futex_wake(&self.0, 1);
    }

    /// Block until the engine is woken by a worker.
    fn wait_until_woken(&self) {
        loop {
            if self.is_woken() {
                break;
            }
            futex_wait(&self.0, EngineWakeState::Sleeping as u32);
        }
    }

    /// Set sleeping and futex-wait if still sleeping. Used by `wait_all_done`
    /// to block until the next worker event.
    fn sleep_and_wait(&self) {
        self.set_sleeping();
    }

    /// Futex-wait on the underlying word (used after `sleep_and_wait` with
    /// a re-check between the store and the wait).
    fn futex_wait_sleeping(&self) {
        futex_wait(&self.0, EngineWakeState::Sleeping as u32);
    }
}

/// Atomic storage for [`YieldReason`]. Wraps an `AtomicU32` and enforces
/// that only valid `YieldReason` discriminants are stored.
/// All operations use SeqCst and are async-signal-safe.
#[repr(transparent)]
struct AtomicYieldReason(AtomicU32);

impl AtomicYieldReason {
    fn new() -> Self {
        Self(AtomicU32::new(0))
    }

    fn store(&self, reason: YieldReason) {
        self.0.store(reason as u32, SeqCst);
    }

    /// Load the stored reason. Panics on invalid discriminant (data corruption).
    fn load(&self) -> YieldReason {
        let raw = self.0.load(SeqCst);
        YieldReason::from_u32(raw)
            .unwrap_or_else(|| panic!("AtomicYieldReason: invalid discriminant {raw}"))
    }
}

/// Sentinel: no worker has yielded yet.
const NO_WORKER: u32 = u32::MAX;

/// Atomic storage for the yielded worker index. Stores a `WorkerId` index
/// or [`NO_WORKER`] sentinel. All operations use SeqCst and are
/// async-signal-safe.
#[repr(transparent)]
struct AtomicYieldedWorker(AtomicU32);

impl AtomicYieldedWorker {
    fn new() -> Self {
        Self(AtomicU32::new(NO_WORKER))
    }

    fn store(&self, worker: WorkerId) {
        self.0.store(worker.0 as u32, SeqCst);
    }

    fn load(&self) -> WorkerId {
        WorkerId(self.0.load(SeqCst) as usize)
    }
}

/// Atomic bitmask of finished workers (up to 64). Each bit corresponds
/// to a `WorkerId`. All operations use SeqCst and are async-signal-safe.
#[repr(transparent)]
struct AtomicFinishedMask(AtomicU64);

impl AtomicFinishedMask {
    fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    /// Atomically set the bit for the given worker.
    fn mark_finished(&self, worker: WorkerId) {
        self.0.fetch_or(1u64 << worker.0, SeqCst);
    }

    /// Load the raw bitmask.
    fn load(&self) -> u64 {
        self.0.load(SeqCst)
    }

    /// Whether all `total` workers have their bits set.
    fn is_all_done(&self, total: usize) -> bool {
        self.load().count_ones() as usize == total
    }

    /// Build the full bitmask for `total` workers (all bits set).
    fn full_mask(total: usize) -> u64 {
        if total == 64 {
            u64::MAX
        } else {
            (1u64 << total) - 1
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

    /// Build the full finished bitmask for `self.total` workers.
    fn full_mask(&self) -> u64 {
        AtomicFinishedMask::full_mask(self.total)
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
            // Park the engine until a worker yields.
            self.engine_wake.set_sleeping();
            self.engine_wake.wait_until_woken();

            // Read yield info published by the worker.
            let yielded = self.yielded_worker.load();
            let reason = self.yield_reason.load();

            // Check if everyone is done.
            if self.all_done() {
                break;
            }

            // Ask the engine for the next worker to run.
            match on_yield(yielded, reason) {
                Some(next) => {
                    self.workers[next.0].set_running();
                    self.workers[next.0].futex_wake_one();
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
            if self.finished_mask() == self.full_mask() {
                break;
            }
            // Wait on engine_wake as a proxy -- workers wake this on yield.
            self.engine_wake.sleep_and_wait();
            if self.finished_mask() == self.full_mask() {
                break;
            }
            self.engine_wake.futex_wait_sleeping();
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

    // -----------------------------------------------------------------------
    // Wrapper type unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_atomic_worker_state_transitions() {
        let state = AtomicWorkerState::new_parked();
        assert!(!state.is_running());

        state.set_running();
        assert!(state.is_running());

        state.park();
        assert!(!state.is_running());
    }

    #[test]
    fn test_atomic_engine_wake_transitions() {
        let wake = AtomicEngineWake::new_sleeping();
        assert!(!wake.is_woken());

        wake.0.store(EngineWakeState::Woken as u32, SeqCst);
        assert!(wake.is_woken());

        wake.set_sleeping();
        assert!(!wake.is_woken());
    }

    #[test]
    fn test_atomic_yield_reason_roundtrip() {
        let atomic = AtomicYieldReason::new();
        for reason in [
            YieldReason::Cooperative,
            YieldReason::Preemption,
            YieldReason::Finished,
        ] {
            atomic.store(reason);
            assert_eq!(atomic.load(), reason);
        }
    }

    #[test]
    #[should_panic(expected = "invalid discriminant")]
    fn test_atomic_yield_reason_panics_on_invalid() {
        let atomic = AtomicYieldReason::new();
        // Raw 0 is not a valid YieldReason discriminant.
        let _ = atomic.load();
    }

    #[test]
    fn test_atomic_yielded_worker_roundtrip() {
        let atomic = AtomicYieldedWorker::new();
        // Sentinel value maps to WorkerId(u32::MAX as usize).
        let sentinel = atomic.load();
        assert_eq!(sentinel.0, u32::MAX as usize);

        atomic.store(WorkerId(42));
        assert_eq!(atomic.load(), WorkerId(42));
    }

    #[test]
    fn test_atomic_finished_mask_operations() {
        let mask = AtomicFinishedMask::new();
        assert_eq!(mask.load(), 0);
        assert!(!mask.is_all_done(3));

        mask.mark_finished(WorkerId(0));
        assert_eq!(mask.load(), 0b001);
        assert!(!mask.is_all_done(3));

        mask.mark_finished(WorkerId(1));
        mask.mark_finished(WorkerId(2));
        assert_eq!(mask.load(), 0b111);
        assert!(mask.is_all_done(3));
    }

    #[test]
    fn test_finished_mask_full_mask() {
        assert_eq!(AtomicFinishedMask::full_mask(1), 0b1);
        assert_eq!(AtomicFinishedMask::full_mask(3), 0b111);
        assert_eq!(AtomicFinishedMask::full_mask(64), u64::MAX);
    }
}
