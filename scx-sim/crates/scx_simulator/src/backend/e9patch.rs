//! e9patch software RBC preemption backend.
//!
//! Uses e9patch-instrumented scheduler `.so` files that call a C trampoline
//! at every conditional branch (Jcc). The trampoline decrements a shared
//! counter and yields via `e9_preempt_yield` when it expires.
//!
//! **State sharing**: The trampoline binary (compiled with e9compile.sh and
//! injected by e9tool) has a "mailbox" array initialized with a magic value
//! (`0xE9PATCH00C0FFEE`). After the `.so` is loaded, `install_mailbox()`
//! scans the process memory for this magic value and writes the pointers
//! to `E9_SHARED_RBC` and `e9_preempt_yield` into the mailbox. The
//! trampoline reads these on every Jcc.
//!
//! The `.so`'s `e9_arm()` / `e9_disarm()` / `e9_worker_setup()` functions
//! (from `sim_rbc_trampoline.c`) write to the same `E9_SHARED_RBC` struct,
//! ensuring consistent state between the Rust backend and the injected
//! trampoline.

use std::ffi::c_void;

use tracing::{debug, info};

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
/// The functions write to `E9_SHARED_RBC` (exported from the main binary).
#[derive(Clone, Copy)]
pub struct E9PatchFns {
    worker_setup: WorkerSetupFn,
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

/// Magic value placed in the e9 trampoline's mailbox array.
/// The Rust backend scans process memory for this value to locate the
/// mailbox and write shared state pointers into it.
const E9_MAILBOX_MAGIC: u64 = 0xE90A7C_00C0FFEE;

/// Scan process memory for the e9 trampoline's mailbox and write the
/// `E9_SHARED_RBC` pointer and `e9_preempt_yield` function pointer into it.
///
/// The mailbox is a `uint64_t[4]` array in the trampoline binary's data
/// section, initialized with `E9_MAILBOX_MAGIC` at index 0. After the
/// `_e9.so` is loaded, the trampoline's data pages are mapped into the
/// process. We find the magic value by scanning writable pages that
/// belong to the `_e9.so` file.
///
/// Returns true if the mailbox was found and written, false otherwise.
pub fn install_mailbox() -> bool {
    let maps = match std::fs::read_to_string("/proc/self/maps") {
        Ok(s) => s,
        Err(_) => return false,
    };

    let state_ptr = &raw const crate::preempt::E9_SHARED_RBC as u64;
    let yield_ptr = crate::preempt::e9_preempt_yield as *const () as u64;

    // Scan writable pages from _e9.so files for the magic value.
    for line in maps.lines() {
        // Only check pages from _e9.so files.
        if !line.contains("_e9.so") {
            continue;
        }
        // Check permissions field (4th column) contains 'w' (writable).
        let perms = line.split_whitespace().nth(1).unwrap_or("");
        if !perms.contains('w') {
            continue;
        }

        // Parse "start-end" address range.
        let Some(dash) = line.find('-') else {
            continue;
        };
        let Ok(start) = u64::from_str_radix(&line[..dash], 16) else {
            continue;
        };
        let space = line.find(' ').unwrap_or(line.len());
        let Ok(end) = u64::from_str_radix(&line[dash + 1..space], 16) else {
            continue;
        };

        // Scan for the magic value (8-byte aligned).
        let len = (end - start) as usize;
        if len < 32 {
            continue;
        }
        let slice = unsafe { std::slice::from_raw_parts(start as *const u64, len / 8) };
        for (i, &val) in slice.iter().enumerate() {
            if val == E9_MAILBOX_MAGIC {
                // Found the mailbox. Write pointers.
                let mailbox =
                    unsafe { std::slice::from_raw_parts_mut((start as *mut u64).add(i), 4) };
                mailbox[0] = state_ptr;
                mailbox[1] = yield_ptr;
                info!(
                    state_ptr = format_args!("0x{state_ptr:x}"),
                    yield_ptr = format_args!("0x{yield_ptr:x}"),
                    mailbox_addr = format_args!("0x{:x}", start + (i * 8) as u64),
                    "e9patch: mailbox installed"
                );
                return true;
            }
        }
    }

    false
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
        if !install_mailbox() {
            panic!(
                "e9patch: failed to find trampoline mailbox in process memory. \
                 Ensure the _e9.so was built with the current e9_rbc_trampoline.c."
            );
        }
    }

    fn worker_setup(&self, ring: &PreemptRing, worker_id: WorkerId) -> E9PatchWorkerCtx {
        preempt::install(
            ring,
            worker_id,
            -1,
            -1,
            self.timeslice_min,
            self.timeslice_max,
        );
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
        let yield_calls = preempt::e9_yield_call_count();
        let (final_counter, final_armed) = unsafe {
            let p = &raw const crate::preempt::E9_SHARED_RBC;
            ((*p).counter, (*p).armed)
        };
        info!(
            signal_preemptions = ring.signal_preemptions(),
            cooperative_yields = ring.cooperative_yields(),
            e9_yield_calls = yield_calls,
            final_counter,
            final_armed,
            "e9patch interleave: complete"
        );
    }
}
