//! e9patch software RBC preemption backend.
//!
//! Uses e9patch-instrumented scheduler `.so` files that call a C trampoline
//! at every conditional branch (Jcc). The trampoline decrements a shared
//! counter and yields via `e9_preempt_yield` when it expires.
//!
//! Contains two backends:
//! - [`E9PatchBackend`]: Recording mode — random PRNG timeslices.
//! - [`E9PatchReplayBackend`]: Replay mode — reads targets from a recorded
//!   trace and arms the counter at exact branch deltas. No PMU hardware,
//!   no retry logic, fully deterministic.
//!
//! **State sharing**: Both the e9-injected trampoline and the `.so`'s
//! arm/disarm functions access a shared `E9SharedRbc` struct at a fixed
//! mmap'd address ([`E9_SHARED_ADDR`]). No RIP-relative addressing, no
//! dlsym — just a hardcoded `movabs` load. The Rust backend mmaps the
//! page before loading the `_e9.so`.
//!
//! **Worker identity**: The shared struct does NOT contain ring_ptr or
//! worker_id — those come from Rust thread-local storage (`PREEMPT_CTX`)
//! inside `e9_preempt_yield`. This avoids races where one worker's arm()
//! overwrites another worker's identity while the first worker is blocked
//! in yield_token().
//!
//! [`E9_SHARED_ADDR`]: crate::preempt::E9_SHARED_ADDR

use std::cell::Cell;

use tracing::{debug, info};

use crate::backend::{PreemptTarget, PreemptionBackend, RbcTarget, RelativeRbc, StructopDelta};
use crate::interleave::WorkerId;
use crate::preempt::trace::PreemptionTrace;
use crate::preempt::{self, PreemptRing, ReplayCursor};

/// Function pointer types for the C trampoline API in the scheduler `.so`.
type ArmFn = unsafe extern "C" fn(u64);
type DisarmFn = unsafe extern "C" fn();

/// Resolved function pointers for the e9patch C trampoline API.
///
/// These are resolved from the loaded scheduler `.so` via `libloading`.
/// The functions write to the shared state at [`E9_SHARED_ADDR`].
///
/// [`E9_SHARED_ADDR`]: crate::preempt::E9_SHARED_ADDR
#[derive(Clone, Copy)]
pub struct E9PatchFns {
    arm: ArmFn,
    disarm: DisarmFn,
}

// Function pointers are inherently Send+Sync — the pointed-to functions
// are in a loaded .so and thread-safe (they access a global struct that's
// serialized by the PreemptRing token protocol).
unsafe impl Send for E9PatchFns {}
unsafe impl Sync for E9PatchFns {}

impl E9PatchFns {
    /// Resolve e9patch C trampoline function pointers from a loaded library.
    ///
    /// # Safety
    /// The library must contain `e9_arm`, `e9_disarm` symbols with the
    /// expected signatures (from `sim_rbc_trampoline.c`).
    pub unsafe fn resolve(lib: &libloading::Library) -> Option<Self> {
        let arm: ArmFn = {
            let sym = lib.get::<*const ()>(b"e9_arm").ok()?;
            std::mem::transmute::<*const (), ArmFn>(*sym)
        };
        let disarm: DisarmFn = {
            let sym = lib.get::<*const ()>(b"e9_disarm").ok()?;
            std::mem::transmute::<*const (), DisarmFn>(*sym)
        };
        Some(E9PatchFns { arm, disarm })
    }
}

/// e9patch software RBC preemption backend.
///
/// Each worker's C trampoline (compiled into the `_e9.so`) decrements a
/// shared counter at every Jcc and calls `e9_preempt_yield()` when
/// it expires. No PMU hardware, no signal handlers, fully deterministic.
pub(crate) struct E9PatchBackend {
    pub timeslice_min: u64,
    pub timeslice_max: u64,
    pub fns: E9PatchFns,
}

/// Per-worker state for the e9patch backend (no hardware resources needed).
pub(crate) struct E9PatchWorkerCtx;

impl PreemptionBackend for E9PatchBackend {
    type WorkerCtx = E9PatchWorkerCtx;

    fn global_setup(&self) {
        // SAFETY: `mmap_shared_rbc()` has been called before any e9patch
        // backend usage, making E9_SHARED_ADDR a valid pointer.
        let p = unsafe { preempt::e9_shared_rbc() };
        // SAFETY: `p` is a valid pointer to the mmap'd shared state page.
        let counter = unsafe { (*p).counter };
        info!(
            addr = format_args!("{:#x}", preempt::E9_SHARED_ADDR),
            counter, "e9patch: shared state page verified"
        );
    }

    fn worker_setup(&self, ring: &PreemptRing, worker_id: WorkerId) -> E9PatchWorkerCtx {
        // Install Rust-side preempt TLS. The C-side shared state is set
        // in arm() after the token is acquired.
        preempt::install(
            ring,
            worker_id,
            -1,
            -1,
            self.timeslice_min,
            self.timeslice_max,
        );
        debug!(worker = worker_id.0, "e9patch: worker setup");
        E9PatchWorkerCtx
    }

    fn build_target(&self, _ctx: &E9PatchWorkerCtx, ring: &PreemptRing) -> Option<PreemptTarget> {
        let timeslice = ring.roll_timeslice(self.timeslice_min, self.timeslice_max);
        Some(PreemptTarget {
            count_rbc: RbcTarget::Relative(RelativeRbc(timeslice)),
            target_rip: None,
        })
    }

    fn arm(&self, _ctx: &mut E9PatchWorkerCtx, target: PreemptTarget) {
        let timeslice = match target.count_rbc {
            RbcTarget::Relative(RelativeRbc(n)) => n,
            RbcTarget::Absolute(_) => {
                panic!("E9PatchBackend::arm() expects RbcTarget::Relative, got Absolute");
            }
        };
        // SAFETY: `self.fns.arm` is a valid function pointer resolved from
        // the loaded `.so` via `E9PatchFns::resolve`. Token held.
        unsafe { (self.fns.arm)(timeslice) };
    }

    fn disarm(&self, _ctx: &mut E9PatchWorkerCtx) -> StructopDelta {
        // SAFETY: `self.fns.disarm` is a valid function pointer. Token held.
        unsafe { (self.fns.disarm)() };
        StructopDelta {
            rbc_total: 0,
            interleave_count: preempt::structop_info().interleave_count,
        }
    }

    fn worker_teardown(&self, _ctx: E9PatchWorkerCtx) {
        preempt::uninstall();
    }

    fn global_teardown(&self) {
        // Do NOT munmap the shared page here — the .so is still loaded and
        // later scheduler callbacks (tick, stopping, etc.) will trigger
        // the trampoline which accesses the fixed address. The page is
        // one 4K page; leaking it is harmless.
        //
        // Disarm so the trampoline becomes a no-op (armed=0 -> early return).
        // SAFETY: `self.fns.disarm` is a valid function pointer.
        unsafe { (self.fns.disarm)() };
    }

    fn log_completion(&self, ring: &PreemptRing) {
        let yield_calls = preempt::e9_yield_call_count();
        info!(
            signal_preemptions = ring.signal_preemptions(),
            cooperative_yields = ring.cooperative_yields(),
            e9_yield_calls = yield_calls,
            "e9patch interleave: complete"
        );
    }
}

// ---------------------------------------------------------------------------
// E9PatchReplayBackend — deterministic replay via software branch counting
// ---------------------------------------------------------------------------

/// e9patch replay preemption backend.
///
/// Replays a recorded preemption trace using e9patch software branch
/// counting. Each worker's C trampoline fires at exact branch deltas
/// computed from the trace's cumulative `structop_rbc` values.
///
/// Advantages over PMU + HW breakpoint replay:
/// - **No PMU hardware needed** — works in VMs, containers, CI
/// - **Fully deterministic** — software counting, no skid
/// - **No retry logic** — never overshoots
/// - **Single mechanism** — no two-signal coordination
pub(crate) struct E9PatchReplayBackend {
    /// Per-worker cursors into the replay trace.
    cursors: Vec<ReplayCursor>,
    /// Per-worker accumulated RBC (cumulative from dispatch round start).
    ///
    /// `Cell` is safe because each worker only accesses its own cell,
    /// enforced by the token-passing protocol.
    accumulated_rbc: Vec<Cell<u64>>,
    /// Minimum timeslice from the recording scenario (for PRNG sync).
    timeslice_min: u64,
    /// Maximum timeslice from the recording scenario (for PRNG sync).
    timeslice_max: u64,
    /// Resolved e9patch function pointers.
    fns: E9PatchFns,
}

// SAFETY: E9PatchReplayBackend fields are accessed under the token-passing
// protocol: each worker accesses only its own cursor and accumulated_rbc
// Cell. The E9PatchFns are thread-safe function pointers. The Vec<Cell<u64>>
// is not Sync by default, but single-writer access is guaranteed by the
// token ring.
unsafe impl Sync for E9PatchReplayBackend {}

impl E9PatchReplayBackend {
    /// Create a new e9patch replay backend from a recorded preemption trace.
    ///
    /// Builds per-worker cursors from the trace, one per dispatch CPU.
    /// `timeslice_min` / `timeslice_max` must match the recording scenario's
    /// preemptive config to keep the PRNG sequence in sync.
    pub fn new(
        trace: &PreemptionTrace,
        num_workers: usize,
        timeslice_min: u64,
        timeslice_max: u64,
        fns: E9PatchFns,
    ) -> Self {
        let cursors = (0..num_workers)
            .map(|i| {
                let targets = trace.worker_trace(WorkerId(i)).to_vec();
                ReplayCursor::new(targets)
            })
            .collect();
        let accumulated_rbc = (0..num_workers).map(|_| Cell::new(0)).collect();
        E9PatchReplayBackend {
            cursors,
            accumulated_rbc,
            timeslice_min,
            timeslice_max,
            fns,
        }
    }

    /// Reset all cursors and accumulated RBC to the beginning (for retry).
    #[allow(dead_code)] // Infrastructure for potential future retry logic.
    pub fn reset_cursors(&self) {
        for c in &self.cursors {
            c.reset();
        }
        for a in &self.accumulated_rbc {
            a.set(0);
        }
    }
}

/// Per-worker state for the e9patch replay backend.
pub(crate) struct E9PatchReplayWorkerCtx {
    worker_idx: usize,
}

impl PreemptionBackend for E9PatchReplayBackend {
    type WorkerCtx = E9PatchReplayWorkerCtx;

    fn global_setup(&self) {
        // Switch yield_fn to the replay variant before workers start.
        preempt::set_e9_replay_yield();

        // Verify the shared RBC page is accessible.
        let p = unsafe { preempt::e9_shared_rbc() };
        let counter = unsafe { (*p).counter };
        info!(
            addr = format_args!("{:#x}", preempt::E9_SHARED_ADDR),
            counter, "e9patch replay: shared state page verified"
        );
    }

    fn worker_setup(&self, ring: &PreemptRing, worker_id: WorkerId) -> E9PatchReplayWorkerCtx {
        let i = worker_id.0;
        let cursor = &self.cursors[i];
        let accum = &self.accumulated_rbc[i];

        // Install PREEMPT_CTX for cooperative yields at kfunc boundaries.
        // Uses replay_mode so rearm_timer consumes PRNG without resetting
        // the e9 counter.
        preempt::install_replay_preempt(
            ring,
            worker_id,
            -1, // No timer fd needed — e9patch doesn't use PMU.
            self.timeslice_min,
            self.timeslice_max,
        );

        // Install E9_REPLAY_CTX so `e9_replay_yield()` can access the
        // cursor and accumulated_rbc.
        preempt::install_e9_replay(cursor, accum);

        debug!(
            worker = i,
            targets = cursor.len(),
            "e9patch replay: worker setup"
        );

        E9PatchReplayWorkerCtx { worker_idx: i }
    }

    fn build_target(
        &self,
        ctx: &E9PatchReplayWorkerCtx,
        ring: &PreemptRing,
    ) -> Option<PreemptTarget> {
        // Consume PRNG to match recording's PmuBackend::arm() which calls
        // roll_timeslice. Without this, PRNG sequences diverge and
        // pick_next returns different worker IDs.
        let _timeslice = ring.roll_timeslice(self.timeslice_min, self.timeslice_max);

        let cursor = &self.cursors[ctx.worker_idx];
        let first = cursor.current_target()?;
        let accumulated = self.accumulated_rbc[ctx.worker_idx].get();
        let delta = first.structop_rbc.saturating_sub(accumulated);

        Some(PreemptTarget {
            count_rbc: RbcTarget::Relative(RelativeRbc(delta)),
            target_rip: Some(first.instruction_pointer),
        })
    }

    fn arm(&self, _ctx: &mut E9PatchReplayWorkerCtx, target: PreemptTarget) {
        let delta = match target.count_rbc {
            RbcTarget::Relative(RelativeRbc(n)) => n,
            RbcTarget::Absolute(_) => {
                panic!(
                    "E9PatchReplayBackend::arm() expects RbcTarget::Relative, \
                     got Absolute"
                );
            }
        };
        // Arm the e9 counter to fire after `delta` branches.
        // SAFETY: `self.fns.arm` is a valid function pointer resolved from
        // the loaded `.so` via `E9PatchFns::resolve`. Token held.
        unsafe { (self.fns.arm)(delta) };
    }

    fn disarm(&self, _ctx: &mut E9PatchReplayWorkerCtx) -> StructopDelta {
        // SAFETY: `self.fns.disarm` is a valid function pointer. Token held.
        unsafe { (self.fns.disarm)() };
        StructopDelta {
            rbc_total: 0,
            interleave_count: preempt::structop_info().interleave_count,
        }
    }

    fn worker_teardown(&self, _ctx: E9PatchReplayWorkerCtx) {
        preempt::uninstall_e9_replay();
        preempt::uninstall();
    }

    fn global_teardown(&self) {
        // Disarm so the trampoline becomes a no-op (armed=0 -> early return).
        // SAFETY: `self.fns.disarm` is a valid function pointer.
        unsafe { (self.fns.disarm)() };
    }

    fn log_completion(&self, ring: &PreemptRing) {
        let yield_calls = preempt::e9_yield_call_count();
        info!(
            signal_preemptions = ring.signal_preemptions(),
            cooperative_yields = ring.cooperative_yields(),
            e9_yield_calls = yield_calls,
            "e9patch replay interleave: complete"
        );
    }

    fn is_precise(&self) -> bool {
        true
    }

    fn read_count(&self, _ctx: &E9PatchReplayWorkerCtx) -> u64 {
        preempt::e9_read_counter() as u64
    }
}
