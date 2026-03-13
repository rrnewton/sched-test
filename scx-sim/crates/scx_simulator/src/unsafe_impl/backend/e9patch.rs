//! e9patch software RBC preemption backend.
//!
//! Uses e9patch-instrumented scheduler `.so` files that call a C trampoline
//! at every conditional branch (Jcc). The trampoline decrements a shared
//! counter and yields via `e9_preempt_yield` when it expires.
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

use tracing::{debug, info};

use crate::backend::{PreemptTarget, PreemptionBackend, RbcTarget, RelativeRbc, StructopDelta};
use crate::interleave::WorkerId;
use crate::preempt::{self, PreemptRing};

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
