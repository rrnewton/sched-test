//! e9patch software RBC preemption backend.
//!
//! Uses e9patch-instrumented scheduler `.so` files that call a C trampoline
//! at every conditional branch (Jcc). The trampoline decrements a thread-local
//! software RBC counter and yields via [`e9_preempt_yield`] when it expires.
//!
//! Unlike the PMU backend, this is fully deterministic (no hardware skid) and
//! preserves debugger compatibility (no Frida Stalker rewriting).
//!
//! The C functions (`e9_worker_setup`, `e9_arm`, `e9_disarm`) live in the
//! scheduler `.so` and access `.so`-local thread-local storage. They are
//! resolved at runtime via `libloading` when the backend is created.
//!
//! [`e9_preempt_yield`]: crate::preempt::e9_preempt_yield

use std::ffi::c_void;

use tracing::debug;

use crate::backend::{PreemptionBackend, StructopDelta};
use crate::interleave::WorkerId;
use crate::preempt::{self, PreemptRing};

/// Function pointer types for the C trampoline API in the scheduler `.so`.
type WorkerSetupFn = unsafe extern "C" fn(*const c_void, i32);
type ArmFn = unsafe extern "C" fn(u64);
type DisarmFn = unsafe extern "C" fn();

/// Resolved function pointers for the e9patch C trampoline API.
///
/// These are resolved from the loaded scheduler `.so` via `libloading`.
/// The functions access `.so`-local thread-local storage for the software
/// RBC counter.
#[derive(Clone, Copy)]
pub struct E9PatchFns {
    worker_setup: WorkerSetupFn,
    arm: ArmFn,
    disarm: DisarmFn,
}

// Function pointers are inherently Send+Sync — the pointed-to functions
// are in a loaded .so and thread-safe (they access thread-local storage).
unsafe impl Send for E9PatchFns {}
unsafe impl Sync for E9PatchFns {}

impl E9PatchFns {
    /// Resolve e9patch C trampoline function pointers from a loaded library.
    ///
    /// # Safety
    /// The library must contain `e9_worker_setup`, `e9_arm`, `e9_disarm`
    /// symbols with the expected signatures (from `sim_rbc_trampoline.c`).
    pub unsafe fn resolve(lib: &libloading::Library) -> Option<Self> {
        let worker_setup: WorkerSetupFn = {
            let sym = lib.get::<*const ()>(b"e9_worker_setup").ok()?;
            std::mem::transmute::<*const (), WorkerSetupFn>(*sym)
        };
        let arm: ArmFn = {
            let sym = lib.get::<*const ()>(b"e9_arm").ok()?;
            std::mem::transmute::<*const (), ArmFn>(*sym)
        };
        let disarm: DisarmFn = {
            let sym = lib.get::<*const ()>(b"e9_disarm").ok()?;
            std::mem::transmute::<*const (), DisarmFn>(*sym)
        };
        Some(E9PatchFns {
            worker_setup,
            arm,
            disarm,
        })
    }
}

/// e9patch software RBC preemption backend.
///
/// Each worker's C trampoline (compiled into the `_e9.so`) decrements a
/// thread-local counter at every Jcc and calls `e9_preempt_yield()` when
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

    fn worker_setup(&self, ring: &PreemptRing, worker_id: WorkerId) -> E9PatchWorkerCtx {
        // Install the preempt TLS (ring pointer, worker ID, timeslice range).
        // timer_fd = -1, measure_fd = -1: no PMU hardware.
        preempt::install(
            ring,
            worker_id,
            -1,
            -1,
            self.timeslice_min,
            self.timeslice_max,
        );

        // Set up the C-side thread-local state (ring pointer, worker ID).
        unsafe {
            (self.fns.worker_setup)(
                ring as *const PreemptRing as *const c_void,
                worker_id.0 as i32,
            );
        }

        debug!(worker = worker_id.0, "e9patch: worker setup (software RBC)");

        E9PatchWorkerCtx
    }

    fn arm(&self, _ctx: &mut E9PatchWorkerCtx, ring: &PreemptRing) {
        let ts = ring.roll_timeslice(self.timeslice_min, self.timeslice_max);
        unsafe { (self.fns.arm)(ts) };
    }

    fn disarm(&self, _ctx: &mut E9PatchWorkerCtx) -> StructopDelta {
        unsafe { (self.fns.disarm)() };
        StructopDelta {
            rbc_total: 0,
            interleave_count: preempt::structop_info().interleave_count,
        }
    }

    fn worker_teardown(&self, _ctx: E9PatchWorkerCtx) {
        preempt::uninstall();
    }

    fn log_completion(&self, ring: &PreemptRing) {
        debug!(
            signal_preemptions = ring.signal_preemptions(),
            cooperative_yields = ring.cooperative_yields(),
            "e9patch interleave: complete"
        );
    }
}
