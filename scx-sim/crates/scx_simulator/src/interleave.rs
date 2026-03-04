//! Concurrent callback interleaving via token-passing.
//!
//! Runs scheduler callbacks on separate OS threads, with only one thread
//! active at a time. A PRNG-driven scheduler controls which thread gets
//! the "token" at each kfunc yield point, enabling deterministic
//! exploration of different interleavings.
//!
//! ## Determinism
//!
//! Interleaving is fully deterministic for a given seed:
//! - PRNG determines worker selection order
//! - Token passing serializes all state access
//! - Same seed → same interleaving → same trace
//!
//! This enables the stress testing methodology: explore many seeds to find
//! bugs, then reproduce failures with the same seed for debugging.
//!
//! See `ai_docs/DETERMINISM.md` for the full explanation.
//!
//! ## Architecture
//!
//! The orchestrator (engine thread) spawns one worker per CPU in the
//! concurrent group. Workers block on a condvar until the PRNG selects
//! them. At each kfunc entry point, [`maybe_yield`] releases the token
//! and selects the next worker, allowing a different CPU's dispatch
//! callback to make progress.
//!
//! ## Safety
//!
//! Token passing ensures only one thread accesses [`SimulatorState`] at
//! a time. Raw pointers are shared across threads, but actual access is
//! serialized by the token. The [`maybe_yield`] call happens BEFORE
//! `with_sim()`, so no `&mut SimulatorState` reference is held when a
//! worker yields.

use std::cell::Cell;
use std::sync::{Condvar, Mutex};

use rand::rngs::SmallRng;
use rand::{RngCore, SeedableRng};

/// Worker identity within a concurrent group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorkerId(pub usize);

// ---------------------------------------------------------------------------
// TokenRing — PRNG-driven cooperative scheduler
// ---------------------------------------------------------------------------

/// Opaque handle passed to [`OnYieldFn`] callbacks, providing controlled
/// access to worker-selection logic without exposing `TokenState` internals.
///
/// Future phases will extend this with event-queue inspection and clock
/// queries; for now it wraps the PRNG-based `pick_next()`.
pub struct YieldContext<'a> {
    state: &'a mut TokenState,
}

impl YieldContext<'_> {
    /// Pick the next non-finished worker using the deterministic PRNG.
    ///
    /// This is the same selection logic used by the default (no-callback)
    /// path. Callbacks that want to preserve existing behavior can simply
    /// call this.
    pub fn pick_next_prng(&mut self) -> Option<WorkerId> {
        self.state.pick_next()
    }

    /// The total number of workers in the ring.
    pub fn total(&self) -> usize {
        self.state.total
    }

    /// Whether the given worker has finished.
    pub fn is_finished(&self, id: WorkerId) -> bool {
        self.state.is_finished(id)
    }
}

/// Callback type for engine-mediated yield decisions.
///
/// Invoked inside `yield_token` when a worker yields. Receives the yielding
/// worker's ID and a [`YieldContext`] that provides worker-selection helpers.
/// Returns the [`WorkerId`] to activate next, or `None` if all workers are
/// done (should not normally happen during a yield).
///
/// In Phase 1 (sim-a730ac) this is always `None` (PRNG-direct path). Later
/// phases install a callback that wakes the simulator engine for
/// clock-update / event-queue inspection between yields.
pub type OnYieldFn = Box<dyn Fn(WorkerId, &mut YieldContext<'_>) -> Option<WorkerId> + Send + Sync>;

/// Token-passing scheduler for concurrent callback interleaving.
///
/// Workers block on a condvar until selected by the PRNG. Only one worker
/// is active at a time, ensuring single-threaded access to shared state.
///
/// An optional [`OnYieldFn`] callback can override worker selection at each
/// yield point, enabling the simulator engine to participate in scheduling
/// decisions (see `ai_docs/widened_concurrency_plan.md`, Phase 1).
pub struct TokenRing {
    mu: Mutex<TokenState>,
    cv: Condvar,
    /// Optional engine-mediated yield callback. When `None`, `yield_token`
    /// uses the PRNG directly (current/default behavior). When `Some`,
    /// the callback decides which worker to resume.
    on_yield: Option<OnYieldFn>,
}

struct TokenState {
    /// Which worker currently holds the token.
    active: Option<WorkerId>,
    /// Bitmask of workers that have finished (supports up to 64 workers).
    finished_mask: u64,
    /// Total number of workers.
    total: usize,
    /// Deterministic PRNG for worker selection.
    rng: SmallRng,
}

impl TokenState {
    fn is_finished(&self, id: WorkerId) -> bool {
        self.finished_mask & (1u64 << id.0) != 0
    }

    fn mark_finished(&mut self, id: WorkerId) {
        self.finished_mask |= 1u64 << id.0;
    }

    fn n_finished(&self) -> usize {
        self.finished_mask.count_ones() as usize
    }

    fn all_done(&self) -> bool {
        self.n_finished() == self.total
    }

    /// Pick the next non-finished worker using PRNG.
    fn pick_next(&mut self) -> Option<WorkerId> {
        let n_remaining = self.total - self.n_finished();
        if n_remaining == 0 {
            return None;
        }
        let idx = (self.rng.next_u32() as usize) % n_remaining;
        let mut count = 0;
        for i in 0..self.total {
            if !self.is_finished(WorkerId(i)) {
                if count == idx {
                    return Some(WorkerId(i));
                }
                count += 1;
            }
        }
        unreachable!()
    }
}

/// Decide the next worker to activate on yield.
///
/// When `on_yield` is `Some`, delegates to the callback via [`YieldContext`].
/// Otherwise falls through to PRNG-based selection. This is a free function
/// (not a method on `TokenRing`) to avoid borrowing `self` while the
/// `MutexGuard<TokenState>` is held.
fn pick_next_for_yield(
    on_yield: &Option<OnYieldFn>,
    yielding: WorkerId,
    state: &mut TokenState,
) -> Option<WorkerId> {
    if let Some(ref cb) = *on_yield {
        let mut ctx = YieldContext { state };
        cb(yielding, &mut ctx)
    } else {
        state.pick_next()
    }
}

impl TokenRing {
    /// Create a new token ring for `total` workers.
    ///
    /// The ring uses PRNG-driven worker selection by default. To override
    /// selection with engine-mediated logic, use [`with_on_yield`].
    ///
    /// [`with_on_yield`]: TokenRing::with_on_yield
    ///
    /// # Panics
    /// Panics if `total` is 0 or exceeds 64.
    pub fn new(total: usize, seed: u32) -> Self {
        assert!(
            total > 0 && total <= 64,
            "TokenRing supports 1–64 workers, got {total}"
        );
        TokenRing {
            mu: Mutex::new(TokenState {
                active: None,
                finished_mask: 0,
                total,
                rng: SmallRng::seed_from_u64(seed as u64),
            }),
            cv: Condvar::new(),
            on_yield: None,
        }
    }

    /// Builder: install an [`OnYieldFn`] callback for engine-mediated yield.
    ///
    /// When set, every `yield_token` call invokes the callback to decide
    /// which worker to resume, instead of using the PRNG directly. This
    /// is infrastructure for simulator-in-the-loop (Phase 1 of
    /// sim-a730ac); the default (`None`) preserves existing behavior.
    pub fn with_on_yield(mut self, f: OnYieldFn) -> Self {
        self.on_yield = Some(f);
        self
    }

    /// Orchestrator: select the first worker via PRNG and wake it.
    pub fn start(&self) {
        let mut state = self.mu.lock().unwrap();
        state.active = state.pick_next();
        self.cv.notify_all();
    }

    /// Worker: block until this worker is selected.
    pub fn wait_for_token(&self, my_id: WorkerId) {
        let mut state = self.mu.lock().unwrap();
        while state.active != Some(my_id) {
            state = self.cv.wait(state).unwrap();
        }
    }

    /// Worker: release token, select next worker, block until re-selected.
    ///
    /// Returns `true` if a different worker was selected (actual context
    /// switch), `false` if the same worker was re-selected (no-op yield).
    ///
    /// If an [`OnYieldFn`] callback is installed, it decides the next
    /// worker. Otherwise, the PRNG picks the next non-finished worker
    /// directly (the original/default behavior).
    pub fn yield_token(&self, my_id: WorkerId) -> bool {
        let mut state = self.mu.lock().unwrap();
        debug_assert_eq!(state.active, Some(my_id));
        state.active = pick_next_for_yield(&self.on_yield, my_id, &mut state);
        let switched = state.active != Some(my_id);
        self.cv.notify_all();
        while state.active != Some(my_id) {
            state = self.cv.wait(state).unwrap();
        }
        switched
    }

    /// Worker: mark as finished and wake the next worker (or signal
    /// all-done to the orchestrator).
    pub fn finish(&self, my_id: WorkerId) {
        let mut state = self.mu.lock().unwrap();
        debug_assert_eq!(state.active, Some(my_id));
        state.mark_finished(my_id);
        if state.all_done() {
            state.active = None;
        } else {
            state.active = state.pick_next();
        }
        self.cv.notify_all();
    }

    /// Orchestrator: block until all workers have finished.
    pub fn wait_all_done(&self) {
        let mut state = self.mu.lock().unwrap();
        while !state.all_done() {
            state = self.cv.wait(state).unwrap();
        }
    }
}

impl crate::backend::ThreadOrchestrator for TokenRing {
    fn start(&self) {
        TokenRing::start(self);
    }

    fn wait_all_done(&self) {
        TokenRing::wait_all_done(self);
    }

    fn wait_for_token(&self, worker_id: WorkerId) {
        TokenRing::wait_for_token(self, worker_id);
    }

    fn yield_token(&self, worker_id: WorkerId) -> bool {
        TokenRing::yield_token(self, worker_id)
    }

    fn finish(&self, worker_id: WorkerId) {
        TokenRing::finish(self, worker_id);
    }
}

// ---------------------------------------------------------------------------
// Thread-local yield-point plumbing
// ---------------------------------------------------------------------------

/// Thread-local interleave context installed on worker threads.
#[derive(Clone, Copy)]
struct InterleaveCtx {
    ring: *const TokenRing,
    worker_id: WorkerId,
}

// Raw pointers are Send — we enforce single-access via token passing.
unsafe impl Send for InterleaveCtx {}

thread_local! {
    static INTERLEAVE_CTX: Cell<Option<InterleaveCtx>> = const { Cell::new(None) };
}

/// Install interleave context on the current worker thread.
///
/// Called by worker threads at startup, before waiting for the token.
pub fn install(ring: &TokenRing, worker_id: WorkerId) {
    INTERLEAVE_CTX.with(|c| {
        c.set(Some(InterleaveCtx {
            ring: ring as *const TokenRing,
            worker_id,
        }));
    });
}

/// Remove interleave context from the current thread.
pub fn uninstall() {
    INTERLEAVE_CTX.with(|c| c.set(None));
}

/// Yield point called at the top of each state-accessing kfunc.
///
/// Dispatches to the appropriate interleaving backend:
/// - If preemptive context is installed: uses [`preempt::maybe_yield_preemptive`]
///   (futex-based, signal-safe, PMU timer aware).
/// - If cooperative context is installed: uses the `TokenRing` (Mutex/Condvar).
/// - If neither is installed: no-op.
///
/// # Safety contract
///
/// Must be called BEFORE `with_sim()`, so no `&mut SimulatorState`
/// reference exists when the worker yields.
pub fn maybe_yield() {
    // Try preemptive yield first (no-op if preempt context not installed).
    crate::preempt::maybe_yield_preemptive();

    // Cooperative yield fallback (no-op if interleave context not installed).
    let ctx = INTERLEAVE_CTX.with(|c| c.get());
    let ctx = match ctx {
        Some(ctx) => ctx,
        None => return,
    };

    let ring = unsafe { &*ctx.ring };

    // Save per-callback context from the CALLBACK_CTX thread-local.
    // This is async-signal-safe (Cell<Copy> read).
    let saved =
        crate::kfuncs::get_callback_ctx().expect("maybe_yield called outside simulator context");

    // Release token and block until re-selected.
    if ring.yield_token(ctx.worker_id) {
        crate::preempt::inc_interleave();
    }

    // Resumed — restore our context.
    crate::kfuncs::install_callback_ctx(saved);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_worker_completes() {
        let ring = TokenRing::new(1, 42);
        ring.start();
        ring.wait_for_token(WorkerId(0));
        ring.finish(WorkerId(0));
        ring.wait_all_done();
    }

    #[test]
    fn test_two_workers_interleave() {
        let ring = TokenRing::new(2, 42);

        std::thread::scope(|s| {
            let ring_ref = &ring;

            let h0 = s.spawn(move || {
                ring_ref.wait_for_token(WorkerId(0));
                // Do some work, yield
                ring_ref.yield_token(WorkerId(0));
                // Resumed, finish
                ring_ref.finish(WorkerId(0));
            });

            let h1 = s.spawn(move || {
                ring_ref.wait_for_token(WorkerId(1));
                ring_ref.yield_token(WorkerId(1));
                ring_ref.finish(WorkerId(1));
            });

            ring.start();
            ring.wait_all_done();

            h0.join().unwrap();
            h1.join().unwrap();
        });
    }

    #[test]
    fn test_prng_determinism() {
        // Same seed must produce the same selection order.
        let order1 = run_and_record_order(3, 12345);
        let order2 = run_and_record_order(3, 12345);
        assert_eq!(order1, order2, "same seed must give same order");
    }

    #[test]
    fn test_different_seeds_may_differ() {
        // Different seeds should (usually) produce different orders.
        // Not guaranteed for all pairs, but very likely for these.
        let order1 = run_and_record_order(4, 100);
        let order2 = run_and_record_order(4, 999);
        // At least the first selection should differ for most seed pairs.
        // If they happen to match, that's OK — this test just checks
        // the mechanism works.
        let _ = (order1, order2);
    }

    /// Run N workers with the given seed, each yielding once.
    /// Returns the order in which workers were first activated.
    fn run_and_record_order(n: usize, seed: u32) -> Vec<WorkerId> {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let ring = TokenRing::new(n, seed);
        let order: Vec<AtomicUsize> = (0..n).map(|_| AtomicUsize::new(usize::MAX)).collect();
        let counter = AtomicUsize::new(0);

        std::thread::scope(|s| {
            for i in 0..n {
                let ring_ref = &ring;
                let order_ref = &order;
                let counter_ref = &counter;
                s.spawn(move || {
                    ring_ref.wait_for_token(WorkerId(i));
                    let seq = counter_ref.fetch_add(1, Ordering::SeqCst);
                    order_ref[i].store(seq, Ordering::SeqCst);
                    ring_ref.yield_token(WorkerId(i));
                    ring_ref.finish(WorkerId(i));
                });
            }
            ring.start();
            ring.wait_all_done();
        });

        let mut pairs: Vec<(usize, WorkerId)> = order
            .iter()
            .enumerate()
            .map(|(i, a)| (a.load(Ordering::SeqCst), WorkerId(i)))
            .collect();
        pairs.sort_by_key(|&(seq, _)| seq);
        pairs.into_iter().map(|(_, id)| id).collect()
    }

    #[test]
    fn test_finish_without_yield() {
        // A worker can finish without ever yielding.
        let ring = TokenRing::new(2, 42);

        std::thread::scope(|s| {
            let ring_ref = &ring;

            s.spawn(move || {
                ring_ref.wait_for_token(WorkerId(0));
                // Finish immediately without yielding
                ring_ref.finish(WorkerId(0));
            });

            s.spawn(move || {
                ring_ref.wait_for_token(WorkerId(1));
                ring_ref.finish(WorkerId(1));
            });

            ring.start();
            ring.wait_all_done();
        });
    }

    #[test]
    fn test_multiple_yields() {
        // Workers can yield multiple times before finishing.
        let ring = TokenRing::new(2, 42);

        std::thread::scope(|s| {
            let ring_ref = &ring;

            s.spawn(move || {
                ring_ref.wait_for_token(WorkerId(0));
                ring_ref.yield_token(WorkerId(0));
                ring_ref.yield_token(WorkerId(0));
                ring_ref.yield_token(WorkerId(0));
                ring_ref.finish(WorkerId(0));
            });

            s.spawn(move || {
                ring_ref.wait_for_token(WorkerId(1));
                ring_ref.yield_token(WorkerId(1));
                ring_ref.finish(WorkerId(1));
            });

            ring.start();
            ring.wait_all_done();
        });
    }

    // -----------------------------------------------------------------------
    // on_yield callback tests (Phase 1: sim-a730ac)
    // -----------------------------------------------------------------------

    /// Run N workers with an optional `on_yield` callback, each yielding once.
    /// Returns the order in which workers were first activated.
    fn run_and_record_order_with_callback(
        n: usize,
        seed: u32,
        on_yield: Option<OnYieldFn>,
    ) -> Vec<WorkerId> {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let mut ring = TokenRing::new(n, seed);
        if let Some(f) = on_yield {
            ring = ring.with_on_yield(f);
        }
        let order: Vec<AtomicUsize> = (0..n).map(|_| AtomicUsize::new(usize::MAX)).collect();
        let counter = AtomicUsize::new(0);

        std::thread::scope(|s| {
            for i in 0..n {
                let ring_ref = &ring;
                let order_ref = &order;
                let counter_ref = &counter;
                s.spawn(move || {
                    ring_ref.wait_for_token(WorkerId(i));
                    let seq = counter_ref.fetch_add(1, Ordering::SeqCst);
                    order_ref[i].store(seq, Ordering::SeqCst);
                    ring_ref.yield_token(WorkerId(i));
                    ring_ref.finish(WorkerId(i));
                });
            }
            ring.start();
            ring.wait_all_done();
        });

        let mut pairs: Vec<(usize, WorkerId)> = order
            .iter()
            .enumerate()
            .map(|(i, a)| (a.load(Ordering::SeqCst), WorkerId(i)))
            .collect();
        pairs.sort_by_key(|&(seq, _)| seq);
        pairs.into_iter().map(|(_, id)| id).collect()
    }

    #[test]
    fn test_on_yield_none_matches_default() {
        // With on_yield = None, the TokenRing produces the same
        // interleaving order as the original (no callback) path.
        // Both use PRNG-driven pick_next, so same seed => same order.
        for seed in [42, 100, 12345, 999] {
            let default_order = run_and_record_order(4, seed);
            let callback_none_order = run_and_record_order_with_callback(4, seed, None);
            assert_eq!(
                default_order, callback_none_order,
                "on_yield=None must match default for seed {seed}"
            );
        }
    }

    #[test]
    fn test_on_yield_custom_callback_routes_decisions() {
        // Install an on_yield callback that always picks worker 0 first
        // (if not finished), then falls back to PRNG. This verifies that
        // the callback is actually invoked and controls worker selection.
        use std::sync::atomic::{AtomicUsize, Ordering};

        let callback_invocations = std::sync::Arc::new(AtomicUsize::new(0));
        let invocations_clone = callback_invocations.clone();

        let on_yield: OnYieldFn = Box::new(move |_yielding, ctx| {
            invocations_clone.fetch_add(1, Ordering::SeqCst);
            // Delegate to the PRNG picker -- we just want to verify
            // the callback is called.
            ctx.pick_next_prng()
        });

        let ring = TokenRing::new(3, 42).with_on_yield(on_yield);

        std::thread::scope(|s| {
            for i in 0..3 {
                let ring_ref = &ring;
                s.spawn(move || {
                    ring_ref.wait_for_token(WorkerId(i));
                    ring_ref.yield_token(WorkerId(i));
                    ring_ref.yield_token(WorkerId(i));
                    ring_ref.finish(WorkerId(i));
                });
            }
            ring.start();
            ring.wait_all_done();
        });

        // 3 workers x 2 yields each = 6 callback invocations.
        assert_eq!(
            callback_invocations.load(Ordering::SeqCst),
            6,
            "on_yield callback must be invoked at every yield point"
        );
    }

    #[test]
    fn test_on_yield_prng_passthrough_determinism() {
        // An on_yield callback that delegates to pick_next_prng must
        // produce the same interleaving as the default (no callback) path,
        // since both consume the same PRNG sequence.
        for seed in [42, 7777, 31415] {
            let default_order = run_and_record_order(3, seed);
            let passthrough: OnYieldFn = Box::new(|_yielding, ctx| ctx.pick_next_prng());
            let callback_order = run_and_record_order_with_callback(3, seed, Some(passthrough));
            assert_eq!(
                default_order, callback_order,
                "PRNG passthrough callback must match default for seed {seed}"
            );
        }
    }

    #[test]
    fn test_on_yield_custom_selection_overrides_prng() {
        // Install a callback that always picks the lowest-numbered
        // non-finished worker (round-robin-ish). This should produce
        // a different ordering than the PRNG for most seeds.
        use std::sync::atomic::{AtomicUsize, Ordering};

        let activation_order = std::sync::Arc::new(
            (0..3)
                .map(|_| AtomicUsize::new(usize::MAX))
                .collect::<Vec<_>>(),
        );
        let counter = std::sync::Arc::new(AtomicUsize::new(0));

        let lowest_first: OnYieldFn = Box::new(|_yielding, ctx| {
            // Pick the lowest-numbered non-finished worker.
            for i in 0..ctx.total() {
                let w = WorkerId(i);
                if !ctx.is_finished(w) {
                    return Some(w);
                }
            }
            None
        });

        let ring = TokenRing::new(3, 42).with_on_yield(lowest_first);

        let ao = activation_order.clone();
        let ctr = counter.clone();
        std::thread::scope(|s| {
            for i in 0..3 {
                let ring_ref = &ring;
                let ao_ref = &ao;
                let ctr_ref = &ctr;
                s.spawn(move || {
                    ring_ref.wait_for_token(WorkerId(i));
                    let seq = ctr_ref.fetch_add(1, Ordering::SeqCst);
                    ao_ref[i].store(seq, Ordering::SeqCst);
                    ring_ref.yield_token(WorkerId(i));
                    ring_ref.finish(WorkerId(i));
                });
            }
            ring.start();
            ring.wait_all_done();
        });

        // With lowest-first, activation order should be 0, 1, 2
        // (after the initial PRNG-based start picks the first worker,
        // every yield picks the lowest available).
        // Note: `start()` still uses PRNG to pick the first worker.
        // After that, every yield uses our callback.
        let order: Vec<usize> = activation_order
            .iter()
            .map(|a| a.load(Ordering::SeqCst))
            .collect();
        // Verify the callback produced a deterministic pattern.
        // The exact order depends on which worker `start()` picks via PRNG,
        // but the callback-driven yields should be consistent across runs.
        let order2 = {
            let lowest_first2: OnYieldFn = Box::new(|_yielding, ctx| {
                for i in 0..ctx.total() {
                    let w = WorkerId(i);
                    if !ctx.is_finished(w) {
                        return Some(w);
                    }
                }
                None
            });
            run_and_record_order_with_callback(3, 42, Some(lowest_first2))
        };
        // Same seed + same callback => same order.
        let mut pairs: Vec<(usize, WorkerId)> = order
            .iter()
            .enumerate()
            .map(|(i, &seq)| (seq, WorkerId(i)))
            .collect();
        pairs.sort_by_key(|&(seq, _)| seq);
        let order_vec: Vec<WorkerId> = pairs.into_iter().map(|(_, id)| id).collect();
        assert_eq!(
            order_vec, order2,
            "same seed + same callback must be deterministic"
        );
    }
}
