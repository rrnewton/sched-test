//! Typed atomic wrappers for self-documenting concurrent state.
//!
//! These types wrap `AtomicU32` / `AtomicU64` behind domain-specific enums
//! and methods. They are functionally equivalent to raw atomics but prevent
//! accidental misuse (e.g. storing a `WorkerState` in a `YieldReason` field).
//!
//! None of these types perform unsafe operations -- they live in the safe
//! partition purely as type-safety infrastructure.
//!
//! Types whose *full* API requires futex syscalls (e.g. `AtomicWorkerState`,
//! `AtomicEngineWake`) expose the safe subset here (load/store/is_X) and
//! provide a `pub(crate) fn inner(&self) -> &AtomicU32` accessor so that
//! the futex-using methods can be added in `engine_ring.rs`.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::SeqCst};

use crate::interleave::WorkerId;

// ---------------------------------------------------------------------------
// TimeslicePrng
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
    pub fn from_u32(v: u32) -> Option<Self> {
        match v {
            1 => Some(Self::Cooperative),
            2 => Some(Self::Preemption),
            3 => Some(Self::Finished),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// WorkerState + AtomicWorkerState
// ---------------------------------------------------------------------------

/// Per-worker state: parked (blocked on futex) or running (holds token).
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkerState {
    Parked = 0,
    Running = 1,
}

/// Atomic wrapper for [`WorkerState`]. Enforces that only valid states
/// are stored. All operations use SeqCst and are async-signal-safe.
///
/// The futex-based blocking methods (`wait_until_running`, `futex_wake_one`)
/// live in `engine_ring.rs` and access the inner `AtomicU32` via
/// [`inner()`](Self::inner).
#[repr(transparent)]
pub(crate) struct AtomicWorkerState(AtomicU32);

impl AtomicWorkerState {
    pub(crate) fn new_parked() -> Self {
        Self(AtomicU32::new(WorkerState::Parked as u32))
    }

    pub(crate) fn park(&self) {
        self.0.store(WorkerState::Parked as u32, SeqCst);
    }

    pub(crate) fn set_running(&self) {
        self.0.store(WorkerState::Running as u32, SeqCst);
    }

    pub(crate) fn is_running(&self) -> bool {
        self.0.load(SeqCst) == WorkerState::Running as u32
    }

    /// Access the underlying `AtomicU32` for futex operations in
    /// `engine_ring.rs`. This does NOT perform any unsafe operation itself.
    pub(crate) fn inner(&self) -> &AtomicU32 {
        &self.0
    }
}

// ---------------------------------------------------------------------------
// EngineWakeState + AtomicEngineWake
// ---------------------------------------------------------------------------

/// Engine wake state: sleeping (waiting for work) or woken (worker yielded).
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EngineWakeState {
    Sleeping = 0,
    Woken = 1,
}

/// Atomic wrapper for [`EngineWakeState`]. Controls the engine's futex-based
/// sleep/wake protocol. All operations use SeqCst and are async-signal-safe.
///
/// The futex-based blocking methods (`wake`, `futex_wait_sleeping`) live in
/// `engine_ring.rs` and access the inner `AtomicU32` via
/// [`inner()`](Self::inner).
#[repr(transparent)]
pub(crate) struct AtomicEngineWake(AtomicU32);

impl AtomicEngineWake {
    pub(crate) fn new_sleeping() -> Self {
        Self(AtomicU32::new(EngineWakeState::Sleeping as u32))
    }

    /// Atomically set Sleeping and return the previous state.
    ///
    /// This is the standard edge-triggered wakeup pattern: the caller
    /// checks the returned old value to detect pending wakes that arrived
    /// between the last processing and this call.
    pub(crate) fn swap_sleeping(&self) -> EngineWakeState {
        let old = self.0.swap(EngineWakeState::Sleeping as u32, SeqCst);
        match old {
            x if x == EngineWakeState::Sleeping as u32 => EngineWakeState::Sleeping,
            x if x == EngineWakeState::Woken as u32 => EngineWakeState::Woken,
            _ => panic!("AtomicEngineWake: invalid state {old}"),
        }
    }

    /// Access the underlying `AtomicU32` for futex operations in
    /// `engine_ring.rs`. This does NOT perform any unsafe operation itself.
    pub(crate) fn inner(&self) -> &AtomicU32 {
        &self.0
    }
}

// ---------------------------------------------------------------------------
// AtomicYieldReason
// ---------------------------------------------------------------------------

/// Atomic storage for [`YieldReason`]. Wraps an `AtomicU32` and enforces
/// that only valid `YieldReason` discriminants are stored.
/// All operations use SeqCst and are async-signal-safe.
#[repr(transparent)]
pub(crate) struct AtomicYieldReason(AtomicU32);

impl AtomicYieldReason {
    pub(crate) fn new() -> Self {
        Self(AtomicU32::new(0))
    }

    pub(crate) fn store(&self, reason: YieldReason) {
        self.0.store(reason as u32, SeqCst);
    }

    /// Load the stored reason. Panics on invalid discriminant (data corruption).
    pub(crate) fn load(&self) -> YieldReason {
        let raw = self.0.load(SeqCst);
        YieldReason::from_u32(raw)
            .unwrap_or_else(|| panic!("AtomicYieldReason: invalid discriminant {raw}"))
    }
}

// ---------------------------------------------------------------------------
// AtomicYieldedWorker
// ---------------------------------------------------------------------------

/// Sentinel: no worker has yielded yet.
pub(crate) const NO_WORKER: u32 = u32::MAX;

/// Atomic storage for the yielded worker index. Stores a `WorkerId` index
/// or [`NO_WORKER`] sentinel. All operations use SeqCst and are
/// async-signal-safe.
#[repr(transparent)]
pub(crate) struct AtomicYieldedWorker(AtomicU32);

impl AtomicYieldedWorker {
    pub(crate) fn new() -> Self {
        Self(AtomicU32::new(NO_WORKER))
    }

    pub(crate) fn store(&self, worker: WorkerId) {
        self.0.store(worker.0 as u32, SeqCst);
    }

    pub(crate) fn load(&self) -> WorkerId {
        WorkerId(self.0.load(SeqCst) as usize)
    }
}

// ---------------------------------------------------------------------------
// AtomicFinishedMask
// ---------------------------------------------------------------------------

/// Atomic bitmask of finished workers (up to 64). Each bit corresponds
/// to a `WorkerId`. All operations use SeqCst and are async-signal-safe.
#[repr(transparent)]
pub(crate) struct AtomicFinishedMask(AtomicU64);

impl AtomicFinishedMask {
    pub(crate) fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    /// Atomically set the bit for the given worker.
    pub(crate) fn mark_finished(&self, worker: WorkerId) {
        self.0.fetch_or(1u64 << worker.0, SeqCst);
    }

    /// Load the raw bitmask.
    pub(crate) fn load(&self) -> u64 {
        self.0.load(SeqCst)
    }

    /// Whether all `total` workers have their bits set.
    pub(crate) fn is_all_done(&self, total: usize) -> bool {
        self.load().count_ones() as usize == total
    }

    /// Build the full bitmask for `total` workers (all bits set).
    pub(crate) fn full_mask(total: usize) -> u64 {
        if total == 64 {
            u64::MAX
        } else {
            (1u64 << total) - 1
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering::SeqCst;

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
    fn test_atomic_worker_state_transitions() {
        let state = AtomicWorkerState::new_parked();
        assert!(!state.is_running());

        state.set_running();
        assert!(state.is_running());

        state.park();
        assert!(!state.is_running());
    }

    #[test]
    fn test_atomic_engine_wake_swap_sleeping() {
        let wake = AtomicEngineWake::new_sleeping();

        // Swap from Sleeping -> Sleeping returns Sleeping.
        assert_eq!(wake.swap_sleeping(), EngineWakeState::Sleeping);

        // Simulate a worker wake signal.
        wake.inner().store(EngineWakeState::Woken as u32, SeqCst);

        // Swap from Woken -> Sleeping returns Woken (consumed the signal).
        assert_eq!(wake.swap_sleeping(), EngineWakeState::Woken);

        // Second swap without intervening wake returns Sleeping.
        assert_eq!(wake.swap_sleeping(), EngineWakeState::Sleeping);
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

    #[test]
    fn test_timeslice_prng_determinism() {
        let prng1 = TimeslicePrng::new(42);
        let prng2 = TimeslicePrng::new(42);
        for _ in 0..100 {
            assert_eq!(prng1.next(), prng2.next());
        }
    }

    #[test]
    fn test_timeslice_prng_zero_seed_promoted() {
        // Seed 0 would break xorshift; verify it's promoted to 1.
        let prng = TimeslicePrng::new(0);
        let val = prng.next();
        assert_ne!(val, 0, "zero seed should be promoted to 1");
    }

    #[test]
    fn test_timeslice_prng_roll_range() {
        let prng = TimeslicePrng::new(123);
        for _ in 0..200 {
            let v = prng.roll_timeslice(10, 20);
            assert!(v >= 10 && v <= 20, "roll_timeslice out of range: {v}");
        }
    }

    #[test]
    fn test_timeslice_prng_roll_degenerate() {
        let prng = TimeslicePrng::new(1);
        assert_eq!(prng.roll_timeslice(5, 5), 5, "min == max should return min");
    }
}
