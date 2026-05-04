// Copyright (c) Meta Platforms, Inc. and affiliates.
// SPDX-License-Identifier: GPL-2.0-only

//! e9patch software RBC preemption backend.
//!
//! Uses e9patch-instrumented scheduler `.so` files that call a C trampoline
//! at every conditional branch (Jcc). The trampoline decrements a shared
//! counter and yields via `e9_preempt_yield` when it expires.
//!
//! Contains two backends:
//! - [`E9PatchBackend`]: Recording mode — random PRNG timeslices.
//! - [`E9PatchReplayBackend`]: Replay mode — supports two sub-modes:
//!   - **Branch-count mode** (`break_on: rbc`): arms the counter at exact
//!     branch deltas from the trace's cumulative `structop_rbc` values.
//!   - **RIP mode** (`break_on: insn`): patches specific instruction
//!     addresses from the trace and fires when execution reaches the armed
//!     RIP. Uses a separate shared state page at [`E9_RIP_SHARED_ADDR`].
//!
//! **State sharing**: Both the e9-injected trampoline and the `.so`'s
//! arm/disarm functions access a shared `E9SharedRbc` struct at a fixed
//! mmap'd address ([`E9_SHARED_ADDR`]). No RIP-relative addressing, no
//! dlsym — just a hardcoded `movabs` load. The Rust backend mmaps the
//! page before loading the `_e9.so`.
//!
//! The RIP replay sub-mode uses a *second* shared page at
//! [`E9_RIP_SHARED_ADDR`] with an `armed_rip` field that the RIP
//! trampoline checks on each patched instruction.
//!
//! **Worker identity**: The shared struct does NOT contain ring_ptr or
//! worker_id — those come from Rust thread-local storage (`PREEMPT_CTX`)
//! inside `e9_preempt_yield`. This avoids races where one worker's arm()
//! overwrites another worker's identity while the first worker is blocked
//! in yield_token().
//!
//! [`E9_SHARED_ADDR`]: crate::preempt::E9_SHARED_ADDR

use std::cell::Cell;
use std::path::{Path, PathBuf};

use tracing::{debug, info};

use crate::backend::{PreemptTarget, PreemptionBackend, RbcTarget, RelativeRbc, StructopDelta};
use crate::engine_ring::EngineRing;
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
#[allow(dead_code)] // Fields used by PreemptionBackend impls; callers temporarily removed
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
#[allow(dead_code)] // PreemptionBackend impl; callers temporarily removed
pub(crate) struct E9PatchBackend {
    pub timeslice_min: u64,
    pub timeslice_max: u64,
    pub fns: E9PatchFns,
}

/// Per-worker state for the e9patch backend (no hardware resources needed).
#[allow(dead_code)] // PreemptionBackend impl; callers temporarily removed
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

    fn worker_setup(
        &self,
        ring: &PreemptRing,
        engine: &EngineRing,
        worker_id: WorkerId,
    ) -> E9PatchWorkerCtx {
        // Install Rust-side preempt TLS. The C-side shared state is set
        // in arm() after the token is acquired.
        preempt::install(
            ring,
            engine,
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
// E9PatchReplayBackend — deterministic replay (branch-count or RIP-targeted)
// ---------------------------------------------------------------------------

/// Fixed mmap address for the RIP-targeted replay shared state.
///
/// One page (0x1000) above [`E9_SHARED_ADDR`] to avoid collision with the
/// branch-counting shared state.
///
/// [`E9_SHARED_ADDR`]: crate::preempt::E9_SHARED_ADDR
pub const E9_RIP_SHARED_ADDR: usize = 0x1E9_001_000;

/// Shared state for RIP-targeted e9patch replay.
///
/// Mapped at [`E9_RIP_SHARED_ADDR`] via `mmap(MAP_FIXED)`. The RIP
/// trampoline (`e9_rip_trampoline.c`) reads `armed_rip` on every hit
/// and calls `yield_fn` when the current instruction address matches.
///
/// This struct is separate from [`E9SharedRbc`] because the RIP replay
/// mechanism is independent of branch counting: it fires on specific
/// instruction addresses rather than at branch-count thresholds.
///
/// [`E9SharedRbc`]: crate::preempt::E9SharedRbc
#[repr(C)]
pub struct E9RipShared {
    /// The target instruction address (0 = disarmed).
    pub armed_rip: u64,
    /// Yield function pointer (`e9_replay_yield`).
    pub yield_fn: *const std::ffi::c_void,
}

// SAFETY: E9RipShared is a plain-old-data struct at a fixed mmap'd address.
// Single-writer access is enforced by the PreemptRing token-passing protocol.
unsafe impl Send for E9RipShared {}
unsafe impl Sync for E9RipShared {}

/// Get a pointer to the RIP shared state.
///
/// # Safety
/// [`mmap_rip_shared`] must have been called first.
#[allow(dead_code)] // Used by E9PatchReplayBackend impl
unsafe fn e9_rip_shared() -> *mut E9RipShared {
    E9_RIP_SHARED_ADDR as *mut E9RipShared
}

/// Map the RIP-targeted shared state page at the fixed address.
///
/// Must be called before loading the `_e9rip.so` (the RIP-patched
/// scheduler variant). The RIP trampoline reads from this page at
/// each patched instruction.
pub fn mmap_rip_shared() -> *mut E9RipShared {
    let addr = E9_RIP_SHARED_ADDR as *mut std::ffi::c_void;
    // SAFETY: `mmap` with MAP_FIXED at our chosen address. The address
    // is one page above E9_SHARED_ADDR in an obscure gap.
    let ptr = unsafe {
        libc::mmap(
            addr,
            std::mem::size_of::<E9RipShared>(),
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED,
            -1,
            0,
        )
    };
    assert!(
        ptr != libc::MAP_FAILED,
        "e9patch RIP: mmap at {E9_RIP_SHARED_ADDR:#x} failed (address in use?)"
    );
    let shared = ptr as *mut E9RipShared;
    // SAFETY: `shared` points to the freshly mmap'd page.
    unsafe {
        (*shared).armed_rip = 0;
        (*shared).yield_fn = preempt::e9_replay_yield as *const std::ffi::c_void;
    }
    shared
}

/// Arm the RIP trampoline to fire at the given instruction address.
///
/// # Safety
/// [`mmap_rip_shared`] must have been called. Token must be held.
#[allow(dead_code)] // Used by E9PatchReplayBackend impl
unsafe fn arm_rip(rip: u64) {
    (*e9_rip_shared()).armed_rip = rip;
}

/// Disarm the RIP trampoline (set armed_rip to 0).
///
/// # Safety
/// [`mmap_rip_shared`] must have been called. Token must be held.
#[allow(dead_code)] // Used by E9PatchReplayBackend impl
unsafe fn disarm_rip() {
    (*e9_rip_shared()).armed_rip = 0;
}

/// e9patch replay preemption backend.
///
/// Replays a recorded preemption trace using e9patch. Supports two modes:
///
/// **Branch-count mode** (`rip_mode = false`, for `break_on: rbc` traces):
/// Each worker's C trampoline fires at exact branch deltas computed from
/// the trace's cumulative `structop_rbc` values.
///
/// **RIP mode** (`rip_mode = true`, for `break_on: insn` traces):
/// The `.so` is additionally patched at specific instruction addresses
/// from the trace. When execution reaches an armed RIP, the RIP trampoline
/// calls `e9_replay_yield()` to yield and advance the cursor. Branch
/// counting still runs (for progress tracking) but doesn't trigger yields.
///
/// Advantages over PMU + HW breakpoint replay:
/// - **No PMU hardware needed** — works in VMs, containers, CI
/// - **Fully deterministic** — software counting/patching, no skid
/// - **No retry logic** — never overshoots
/// - **Single mechanism** — no two-signal coordination
#[allow(dead_code)] // PreemptionBackend impl; callers temporarily removed
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
    /// Whether to use RIP-targeted mode (for `break_on: insn` traces).
    ///
    /// When true, the backend arms the RIP shared page instead of the
    /// branch counter. The `.so` must have been patched at the target
    /// RIP addresses (via `create_e9rip_so`).
    rip_mode: bool,
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
    ///
    /// Set `rip_mode = true` for `break_on: insn` traces where preemption
    /// points can be at arbitrary instruction addresses.
    pub fn new(
        trace: &PreemptionTrace,
        num_workers: usize,
        timeslice_min: u64,
        timeslice_max: u64,
        fns: E9PatchFns,
        rip_mode: bool,
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
            rip_mode,
        }
    }

    /// Whether this backend is in RIP-targeted mode.
    #[allow(dead_code)] // Exposed for diagnostic / testing use.
    pub fn rip_mode(&self) -> bool {
        self.rip_mode
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
#[allow(dead_code)] // PreemptionBackend impl; callers temporarily removed
pub(crate) struct E9PatchReplayWorkerCtx {
    worker_idx: usize,
}

impl PreemptionBackend for E9PatchReplayBackend {
    type WorkerCtx = E9PatchReplayWorkerCtx;

    fn global_setup(&self) {
        // Switch yield_fn to the replay variant before workers start.
        preempt::set_e9_replay_yield();

        if self.rip_mode {
            // RIP mode: set up the RIP shared page.
            unsafe {
                (*e9_rip_shared()).yield_fn = preempt::e9_replay_yield as *const std::ffi::c_void;
            }

            // Disarm the Jcc counter — in RIP mode, the Jcc trampoline
            // still decrements the counter for progress tracking, but we
            // keep it disarmed (armed=0) so it never yields. Only the RIP
            // trampoline yields.
            unsafe {
                let rbc = preempt::e9_shared_rbc();
                (*rbc).armed = 0;
                (*rbc).counter = i64::MAX;
            }

            info!(
                rip_addr = format_args!("{E9_RIP_SHARED_ADDR:#x}"),
                rbc_addr = format_args!("{:#x}", preempt::E9_SHARED_ADDR),
                "e9patch RIP replay: shared state pages verified"
            );
        } else {
            // Branch-count mode: verify the shared RBC page.
            let p = unsafe { preempt::e9_shared_rbc() };
            let counter = unsafe { (*p).counter };
            info!(
                addr = format_args!("{:#x}", preempt::E9_SHARED_ADDR),
                counter, "e9patch replay: shared state page verified"
            );
        }
    }

    fn worker_setup(
        &self,
        ring: &PreemptRing,
        engine: &EngineRing,
        worker_id: WorkerId,
    ) -> E9PatchReplayWorkerCtx {
        let i = worker_id.0;
        let cursor = &self.cursors[i];
        let accum = &self.accumulated_rbc[i];

        // Install PREEMPT_CTX for cooperative yields at kfunc boundaries.
        // Uses replay_mode so rearm_timer consumes PRNG without resetting
        // the e9 counter.
        preempt::install_replay_preempt(
            ring,
            engine,
            worker_id,
            -1, // No timer fd needed — e9patch doesn't use PMU.
            self.timeslice_min,
            self.timeslice_max,
        );

        // Install E9_REPLAY_CTX so `e9_replay_yield()` can access the
        // cursor and accumulated_rbc.
        preempt::install_e9_replay(cursor, accum);

        let mode_str = if self.rip_mode { "RIP" } else { "branch-count" };
        debug!(
            worker = i,
            targets = cursor.len(),
            mode = mode_str,
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

        if self.rip_mode {
            // RIP mode: fire when execution reaches the target RIP.
            // The branch count is irrelevant — use Relative(0) as sentinel.
            Some(PreemptTarget {
                count_rbc: RbcTarget::Relative(RelativeRbc(0)),
                target_rip: Some(first.instruction_pointer),
            })
        } else {
            // Branch-count mode: compute delta to the target's cumulative RBC.
            let accumulated = self.accumulated_rbc[ctx.worker_idx].get();
            let delta = first.structop_rbc.saturating_sub(accumulated);
            Some(PreemptTarget {
                count_rbc: RbcTarget::Relative(RelativeRbc(delta)),
                target_rip: Some(first.instruction_pointer),
            })
        }
    }

    fn arm(&self, _ctx: &mut E9PatchReplayWorkerCtx, target: PreemptTarget) {
        if self.rip_mode {
            let rip = target
                .target_rip
                .expect("E9PatchReplayBackend RIP mode requires target_rip");

            // Arm the RIP trampoline at this specific address.
            // SAFETY: mmap_rip_shared() was called during setup. Token held.
            unsafe { arm_rip(rip) };

            // Keep the Jcc counter disarmed — only the RIP trampoline yields.
            // SAFETY: disarm is a valid function pointer. Token held.
            unsafe { (self.fns.disarm)() };
        } else {
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
            // SAFETY: arm is a valid function pointer. Token held.
            unsafe { (self.fns.arm)(delta) };
        }
    }

    fn disarm(&self, _ctx: &mut E9PatchReplayWorkerCtx) -> StructopDelta {
        if self.rip_mode {
            // Disarm the RIP trampoline.
            // SAFETY: mmap_rip_shared() was called during setup. Token held.
            unsafe { disarm_rip() };
        } else {
            // SAFETY: disarm is a valid function pointer. Token held.
            unsafe { (self.fns.disarm)() };
        }
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
        if self.rip_mode {
            // Disarm both trampolines.
            unsafe {
                disarm_rip();
                (self.fns.disarm)();
            }
        } else {
            // Disarm so the trampoline becomes a no-op.
            // SAFETY: disarm is a valid function pointer.
            unsafe { (self.fns.disarm)() };
        }
    }

    fn log_completion(&self, ring: &PreemptRing) {
        let yield_calls = preempt::e9_yield_call_count();
        let mode_str = if self.rip_mode { "RIP" } else { "branch-count" };
        info!(
            signal_preemptions = ring.signal_preemptions(),
            cooperative_yields = ring.cooperative_yields(),
            e9_yield_calls = yield_calls,
            mode = mode_str,
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

// ---------------------------------------------------------------------------
// E9tool helpers — runtime .so patching for RIP-targeted replay
// ---------------------------------------------------------------------------

/// Extract unique instruction addresses from a preemption trace.
///
/// Returns a sorted, deduplicated list of all non-zero RIPs across all
/// workers. Used to determine which addresses need e9patch instrumentation
/// for RIP-targeted replay.
pub fn collect_trace_rips(trace: &PreemptionTrace) -> Vec<u64> {
    let mut rips: Vec<u64> = (0..trace.num_workers())
        .flat_map(|i| {
            trace
                .worker_trace(WorkerId(i))
                .iter()
                .map(|r| r.instruction_pointer)
                .filter(|&rip| rip != 0)
        })
        .collect();
    rips.sort_unstable();
    rips.dedup();
    rips
}

/// Classify whether instruction bytes represent a Jcc (conditional branch).
///
/// Returns `true` if the first bytes of `insn` encode a conditional branch:
/// - `0x70..=0x7F`: short Jcc (2 bytes: `7x rel8`)
/// - `0x0F 0x80..=0x0F 0x8F`: near Jcc (6 bytes: `0F 8x rel32`)
/// - `0xE3`: JCXZ/JECXZ/JRCXZ (2 bytes: `E3 rel8`)
///
/// This matters for e9patch RIP replay: when a target RIP is a Jcc, both
/// the Jcc trampoline (branch counting) and RIP trampoline (replay yield)
/// match the same instruction. e9patch composes them correctly — see
/// [`build_e9_rip_command`] doc comment for details.
///
/// # Arguments
/// * `insn` - Raw instruction bytes at the target RIP (at least 2 bytes).
pub fn is_jcc_instruction(insn: &[u8]) -> bool {
    match insn.first() {
        Some(&b) if (0x70..=0x7F).contains(&b) => true,
        Some(&0xE3) => true,
        Some(&0x0F) => matches!(insn.get(1), Some(&b) if (0x80..=0x8F).contains(&b)),
        _ => false,
    }
}

/// Count how many of the target RIPs in a trace are Jcc instructions.
///
/// Iterates all preemption records and checks the instruction bytes at
/// each preemption point. Returns the count of records where the
/// instruction is a Jcc (conditional branch).
///
/// This is a diagnostic to understand how often the Jcc-at-RIP
/// composition case arises in practice.
pub fn count_jcc_rips(trace: &PreemptionTrace) -> usize {
    (0..trace.num_workers())
        .flat_map(|i| trace.worker_trace(WorkerId(i)).iter())
        .filter(|r| r.instruction_pointer != 0)
        .filter(|r| is_jcc_instruction(&r.insn_bytes))
        .count()
}

/// Build an e9tool command to create a RIP-patched scheduler `.so`.
///
/// The output `.so` has two kinds of instrumentation:
/// 1. Every Jcc patched with `rbc_trampoline` (for branch counting, same
///    as the standard `_e9.so`).
/// 2. Each target RIP from the trace patched with `rip_trampoline(addr)`
///    (for precise RIP-targeted yield).
///
/// # Jcc-at-RIP composition
///
/// When a target RIP happens to be a Jcc instruction, **both** trampolines
/// match the same instruction. e9patch handles this correctly via trampoline
/// composition (see e9tool-user-guide.md, "Composing Trampolines"):
///
/// ```text
/// rbc_trampoline(); rip_trampoline(addr); <original Jcc>; break;
/// ```
///
/// Both trampolines use the default `before` position, so they execute in
/// **command-line order** before the original instruction. The Jcc
/// trampoline comes first and always returns immediately in RIP mode
/// (counter = `i64::MAX`, armed = 0). The RIP trampoline then checks
/// `armed_rip` and yields if matched.
///
/// This is safe because:
/// - The Jcc trampoline's fast path (`counter > 0`) exits immediately
///   without side effects when the counter is at `i64::MAX`.
/// - The RIP trampoline independently checks `armed_rip` from its own
///   shared page at [`E9_RIP_SHARED_ADDR`].
/// - No state is shared between the two trampolines.
///
/// Returns `(command, output_path)`.
pub fn build_e9_rip_command(
    input_so: &Path,
    rips: &[u64],
    e9tool_path: &Path,
    rbc_trampoline_bin: &Path,
    rip_trampoline_bin: &Path,
) -> (std::process::Command, PathBuf) {
    let output = derive_e9rip_path(input_so);
    let mut cmd = std::process::Command::new(e9tool_path);

    // First: instrument all Jcc with the branch-counting trampoline.
    // IMPORTANT: Jcc match must come first so that for Jcc instructions
    // that are also RIP targets, the branch counter fires before the RIP
    // check. In RIP mode the Jcc trampoline is a no-op (armed=0), but the
    // ordering ensures branch counting stays consistent if we ever need it.
    cmd.arg("-M").arg("jcc");
    cmd.arg("-P")
        .arg(format!("rbc_trampoline()@{}", rbc_trampoline_bin.display()));

    // Then: instrument each target RIP with the RIP trampoline.
    // For Jcc instructions, e9patch composes this with the Jcc trampoline
    // above — both fire in sequence (see doc comment).
    for &rip in rips {
        cmd.arg("-M").arg(format!("addr={rip:#x}"));
        cmd.arg("-P").arg(format!(
            "rip_trampoline(addr)@{}",
            rip_trampoline_bin.display()
        ));
    }

    cmd.arg("-o").arg(&output);
    cmd.arg(input_so);

    (cmd, output)
}

/// Derive the `_e9rip.so` path from a base `.so` path.
///
/// Transforms `libscx_foo.so` into `libscx_foo_e9rip.so`.
/// Transforms `libscx_foo_e9.so` into `libscx_foo_e9rip.so`.
pub fn derive_e9rip_path(base: &Path) -> PathBuf {
    let stem = base
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");
    let clean_stem = stem.strip_suffix("_e9").unwrap_or(stem);
    base.with_file_name(format!("{clean_stem}_e9rip.so"))
}

/// Attempt to find e9tool in the standard locations.
///
/// Checks `third_party/e9patch/e9tool` relative to the simulator root,
/// then falls back to `$PATH`.
pub fn find_e9tool() -> Option<PathBuf> {
    // Check relative to the manifest directory (compile-time).
    let third_party = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|root| root.join("third_party/e9patch/e9tool"));
    if let Some(ref p) = third_party {
        if p.exists() {
            return Some(p.clone());
        }
    }
    // Fall back to PATH.
    which_in_path("e9tool")
}

/// Attempt to find a binary on `$PATH`.
fn which_in_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(name))
            .find(|p| p.exists())
    })
}

/// Locate the compiled e9patch trampoline binary for branch counting.
///
/// Checks `schedulers/build/e9_rbc_trampoline` relative to the simulator
/// root.
pub fn find_rbc_trampoline() -> Option<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())?;
    let path = root.join("schedulers/build/e9_rbc_trampoline");
    path.exists().then_some(path)
}

/// Locate the compiled e9patch trampoline binary for RIP-targeted replay.
///
/// Checks `schedulers/build/e9_rip_trampoline` relative to the simulator
/// root.
pub fn find_rip_trampoline() -> Option<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())?;
    let path = root.join("schedulers/build/e9_rip_trampoline");
    path.exists().then_some(path)
}

/// Create a RIP-patched `.so` by running e9tool on the base scheduler `.so`.
///
/// Instruments the base `.so` with both Jcc branch-counting trampolines
/// and RIP-specific trampolines at each target address from the trace.
///
/// Returns the path to the newly created `_e9rip.so`, or an error string
/// if any tool is missing or e9tool fails.
pub fn create_e9rip_so(base_so: &Path, rips: &[u64]) -> Result<PathBuf, String> {
    let e9tool = find_e9tool()
        .ok_or_else(|| "e9tool not found. Build e9patch: make install-e9patch".to_string())?;

    let rbc_tramp = find_rbc_trampoline().ok_or_else(|| {
        "e9_rbc_trampoline binary not found. Build with: make -C schedulers e9".to_string()
    })?;

    let rip_tramp = find_rip_trampoline().ok_or_else(|| {
        "e9_rip_trampoline binary not found. \
         Build with: make -C schedulers e9-rip-trampoline"
            .to_string()
    })?;

    let (mut cmd, output_path) =
        build_e9_rip_command(base_so, rips, &e9tool, &rbc_tramp, &rip_tramp);

    info!(
        base_so = %base_so.display(),
        output = %output_path.display(),
        num_rips = rips.len(),
        "e9patch RIP replay: creating instrumented .so"
    );

    let result = cmd
        .output()
        .map_err(|e| format!("failed to run e9tool: {e}"))?;

    if !result.status.success() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        return Err(format!(
            "e9tool failed (exit {}): {}",
            result.status, stderr
        ));
    }

    if !output_path.exists() {
        return Err(format!(
            "e9tool succeeded but output not found: {}",
            output_path.display()
        ));
    }

    info!(
        output = %output_path.display(),
        "e9patch RIP replay: instrumented .so created"
    );
    Ok(output_path)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_derive_e9rip_path_from_base() {
        let base = Path::new("/path/to/libscx_simple.so");
        let result = derive_e9rip_path(base);
        assert_eq!(result, PathBuf::from("/path/to/libscx_simple_e9rip.so"));
    }

    #[test]
    fn test_derive_e9rip_path_from_e9() {
        let base = Path::new("/path/to/libscx_simple_e9.so");
        let result = derive_e9rip_path(base);
        assert_eq!(result, PathBuf::from("/path/to/libscx_simple_e9rip.so"));
    }

    #[test]
    fn test_collect_trace_rips_deduplication() {
        use crate::preempt::PreemptionRecord;

        let records = vec![
            PreemptionRecord {
                rbc_count: 100,
                instruction_pointer: 0x1000,
                cpu_id: crate::types::CpuId(0),
                worker_id: WorkerId(0),
                sequence: 0,
                structop_local: 1,
                structop_global: 1,
                structop_rbc: 100,
                ops_context: crate::kfuncs::OpsContext::None,
                kfunc_name: "",
                kfunc_count: 0,
                insn_bytes: [0; crate::preempt::INSN_BYTES_LEN],
            },
            PreemptionRecord {
                rbc_count: 200,
                instruction_pointer: 0x2000,
                cpu_id: crate::types::CpuId(0),
                worker_id: WorkerId(0),
                sequence: 1,
                structop_local: 2,
                structop_global: 2,
                structop_rbc: 300,
                ops_context: crate::kfuncs::OpsContext::None,
                kfunc_name: "",
                kfunc_count: 0,
                insn_bytes: [0; crate::preempt::INSN_BYTES_LEN],
            },
            PreemptionRecord {
                rbc_count: 150,
                instruction_pointer: 0x1000, // duplicate
                cpu_id: crate::types::CpuId(1),
                worker_id: WorkerId(1),
                sequence: 2,
                structop_local: 1,
                structop_global: 3,
                structop_rbc: 150,
                ops_context: crate::kfuncs::OpsContext::None,
                kfunc_name: "",
                kfunc_count: 0,
                insn_bytes: [0; crate::preempt::INSN_BYTES_LEN],
            },
        ];

        let trace =
            PreemptionTrace::from_records(&records, 2, crate::perf::PmuEvent::InstructionsRetired);

        let rips = collect_trace_rips(&trace);
        assert_eq!(rips, vec![0x1000, 0x2000]);
    }

    #[test]
    fn test_collect_trace_rips_excludes_zero() {
        use crate::preempt::PreemptionRecord;

        let records = vec![PreemptionRecord {
            rbc_count: 0,
            instruction_pointer: 0, // cooperative yield, no RIP
            cpu_id: crate::types::CpuId(0),
            worker_id: WorkerId(0),
            sequence: 0,
            structop_local: 1,
            structop_global: 1,
            structop_rbc: 0,
            ops_context: crate::kfuncs::OpsContext::None,
            kfunc_name: "",
            kfunc_count: 0,
            insn_bytes: [0; crate::preempt::INSN_BYTES_LEN],
        }];

        let trace =
            PreemptionTrace::from_records(&records, 1, crate::perf::PmuEvent::InstructionsRetired);

        let rips = collect_trace_rips(&trace);
        assert!(rips.is_empty());
    }

    #[test]
    fn test_e9_rip_shared_layout() {
        // Verify the shared struct layout matches what the C trampoline expects.
        assert_eq!(std::mem::size_of::<E9RipShared>(), 16);
        assert_eq!(
            std::mem::offset_of!(E9RipShared, armed_rip),
            0,
            "armed_rip must be at offset 0"
        );
        assert_eq!(
            std::mem::offset_of!(E9RipShared, yield_fn),
            8,
            "yield_fn must be at offset 8"
        );
    }

    #[test]
    fn test_e9_rip_shared_addr_no_collision() {
        // The RIP shared address must not collide with the RBC shared address.
        assert_ne!(
            E9_RIP_SHARED_ADDR,
            preempt::E9_SHARED_ADDR,
            "E9_RIP_SHARED_ADDR must differ from E9_SHARED_ADDR"
        );
        // And they must be at least one page apart.
        let diff = E9_RIP_SHARED_ADDR.abs_diff(preempt::E9_SHARED_ADDR);
        assert!(
            diff >= 4096,
            "shared addresses must be at least one page apart"
        );
    }

    // -----------------------------------------------------------------------
    // is_jcc_instruction tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_jcc_short_all_variants() {
        // Short Jcc: 0x70..=0x7F followed by rel8.
        for opcode in 0x70u8..=0x7F {
            let insn = [opcode, 0x0A, 0x00, 0x00, 0x00];
            assert!(
                is_jcc_instruction(&insn),
                "short Jcc opcode {opcode:#04x} should be classified as Jcc"
            );
        }
    }

    #[test]
    fn test_is_jcc_near_all_variants() {
        // Near Jcc: 0x0F 0x80..=0x0F 0x8F followed by rel32.
        for second in 0x80u8..=0x8F {
            let insn = [0x0F, second, 0x65, 0x03, 0x00];
            assert!(
                is_jcc_instruction(&insn),
                "near Jcc 0x0F {second:#04x} should be classified as Jcc"
            );
        }
    }

    #[test]
    fn test_is_jcc_jcxz() {
        // JCXZ/JECXZ/JRCXZ: 0xE3 rel8.
        let insn = [0xE3, 0x10, 0x00, 0x00, 0x00];
        assert!(is_jcc_instruction(&insn));
    }

    #[test]
    fn test_is_jcc_non_jcc_instructions() {
        // push rbp
        assert!(!is_jcc_instruction(&[0x55, 0x41, 0x57, 0x41, 0x56]));
        // call [rip+disp32]
        assert!(!is_jcc_instruction(&[0xFF, 0x15, 0x9D, 0x62, 0x31]));
        // REX.W prefix (cmp rdx, rbx)
        assert!(!is_jcc_instruction(&[0x48, 0x39, 0xDA, 0x74, 0x30]));
        // movups xmmword ptr [rsp+...]
        assert!(!is_jcc_instruction(&[0x0F, 0x11, 0x84, 0x24, 0x88]));
        // setcc (0F 94 — NOT a Jcc despite 0x0F prefix)
        assert!(!is_jcc_instruction(&[0x0F, 0x94, 0xC3, 0xE8, 0xE2]));
        // nop padding
        assert!(!is_jcc_instruction(&[0x66, 0x66, 0x66, 0x64, 0x48]));
        // lea
        assert!(!is_jcc_instruction(&[0x48, 0x8D, 0x80, 0x98, 0xF1]));
        // ret
        assert!(!is_jcc_instruction(&[0xC3, 0x00, 0x00, 0x00, 0x00]));
        // unconditional jmp (near)
        assert!(!is_jcc_instruction(&[0xE9, 0x10, 0x00, 0x00, 0x00]));
        // unconditional jmp (short)
        assert!(!is_jcc_instruction(&[0xEB, 0x10, 0x00, 0x00, 0x00]));
        // call near
        assert!(!is_jcc_instruction(&[0xE8, 0x10, 0x00, 0x00, 0x00]));
    }

    #[test]
    fn test_is_jcc_empty_and_short() {
        // Edge cases: empty slice, single byte.
        assert!(!is_jcc_instruction(&[]));
        assert!(is_jcc_instruction(&[0x74])); // short je (only 1 byte)
        assert!(!is_jcc_instruction(&[0x0F])); // incomplete near Jcc
    }

    #[test]
    fn test_is_jcc_real_trace_data() {
        // Instruction bytes from actual preemption trace (see task description):
        // seq=9:  jne +0x0a (short) at rip=0x7ffff7e69dd7
        assert!(is_jcc_instruction(&[0x75, 0x0A, 0x48, 0x83, 0xF8]));
        // seq=12: jl +0x365 (near) at rip=0x7ffff7e63a1d
        assert!(is_jcc_instruction(&[0x0F, 0x8C, 0x65, 0x03, 0x00]));
        // seq=0:  push rbp (not Jcc)
        assert!(!is_jcc_instruction(&[0x55, 0x41, 0x57, 0x41, 0x56]));
        // seq=5:  test rcx,rcx (not Jcc — 48 85 c9)
        assert!(!is_jcc_instruction(&[0x48, 0x85, 0xC9, 0x75, 0x0A]));
    }

    // -----------------------------------------------------------------------
    // count_jcc_rips tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_count_jcc_rips_mixed_trace() {
        use crate::preempt::PreemptionRecord;

        let records = vec![
            // Non-Jcc: push rbp (0x55)
            PreemptionRecord {
                rbc_count: 100,
                instruction_pointer: 0x1000,
                cpu_id: crate::types::CpuId(0),
                worker_id: WorkerId(0),
                sequence: 0,
                structop_local: 1,
                structop_global: 1,
                structop_rbc: 100,
                ops_context: crate::kfuncs::OpsContext::None,
                kfunc_name: "",
                kfunc_count: 0,
                insn_bytes: [0x55, 0x41, 0x57, 0x41, 0x56],
            },
            // Jcc: jne short (0x75 0x0A)
            PreemptionRecord {
                rbc_count: 200,
                instruction_pointer: 0x2000,
                cpu_id: crate::types::CpuId(0),
                worker_id: WorkerId(0),
                sequence: 1,
                structop_local: 2,
                structop_global: 2,
                structop_rbc: 200,
                ops_context: crate::kfuncs::OpsContext::None,
                kfunc_name: "",
                kfunc_count: 0,
                insn_bytes: [0x75, 0x0A, 0x48, 0x83, 0xF8],
            },
            // Jcc: jl near (0x0F 0x8C)
            PreemptionRecord {
                rbc_count: 300,
                instruction_pointer: 0x3000,
                cpu_id: crate::types::CpuId(1),
                worker_id: WorkerId(1),
                sequence: 2,
                structop_local: 1,
                structop_global: 3,
                structop_rbc: 300,
                ops_context: crate::kfuncs::OpsContext::None,
                kfunc_name: "",
                kfunc_count: 0,
                insn_bytes: [0x0F, 0x8C, 0x65, 0x03, 0x00],
            },
            // Cooperative yield (rip=0, should be excluded)
            PreemptionRecord {
                rbc_count: 0,
                instruction_pointer: 0,
                cpu_id: crate::types::CpuId(0),
                worker_id: WorkerId(0),
                sequence: 3,
                structop_local: 3,
                structop_global: 4,
                structop_rbc: 0,
                ops_context: crate::kfuncs::OpsContext::None,
                kfunc_name: "",
                kfunc_count: 0,
                insn_bytes: [0; crate::preempt::INSN_BYTES_LEN],
            },
        ];

        let trace =
            PreemptionTrace::from_records(&records, 2, crate::perf::PmuEvent::InstructionsRetired);

        assert_eq!(
            count_jcc_rips(&trace),
            2,
            "should find 2 Jcc preemption points"
        );
    }

    // -----------------------------------------------------------------------
    // build_e9_rip_command Jcc-at-RIP composition tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_e9_rip_command_jcc_precedes_rip() {
        // Verify that Jcc trampoline comes before RIP trampolines in the
        // command line, ensuring correct composition ordering.
        let input = Path::new("/path/to/libscx_lavd.so");
        let rips = &[0x7ffff7e69dd7, 0x7ffff7e63a1d]; // Two Jcc addresses
        let e9tool = Path::new("/usr/bin/e9tool");
        let rbc_tramp = Path::new("/build/e9_rbc_trampoline");
        let rip_tramp = Path::new("/build/e9_rip_trampoline");

        let (cmd, _output) = build_e9_rip_command(input, rips, e9tool, rbc_tramp, rip_tramp);

        // get_args() returns each flag and value as separate entries:
        // ["-M", "jcc", "-P", "rbc_trampoline()@...", "-M", "addr=0x...", ...]
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();

        // Find positions of key arguments.
        let jcc_match_pos = args.iter().position(|a| a == "jcc").unwrap();
        let rbc_patch_pos = args
            .iter()
            .position(|a| a.contains("rbc_trampoline"))
            .unwrap();
        let first_rip_match_pos = args.iter().position(|a| a.starts_with("addr=")).unwrap();
        let first_rip_patch_pos = args
            .iter()
            .position(|a| a.contains("rip_trampoline"))
            .unwrap();

        // Jcc match and patch must precede all RIP matches and patches.
        assert!(
            jcc_match_pos < first_rip_match_pos,
            "Jcc match (-M jcc) at position {jcc_match_pos} must precede \
             first RIP match at position {first_rip_match_pos}. \
             e9patch composes trampolines in command-line order, so the Jcc \
             trampoline must fire first to maintain branch counting."
        );
        assert!(
            rbc_patch_pos < first_rip_patch_pos,
            "rbc_trampoline patch at position {rbc_patch_pos} must precede \
             first rip_trampoline patch at position {first_rip_patch_pos}."
        );
    }

    #[test]
    fn test_build_e9_rip_command_generates_all_rip_patches() {
        // Verify each target RIP gets its own -M/-P pair.
        let input = Path::new("/path/to/libscx_lavd.so");
        let rips = &[0x1000, 0x2000, 0x3000];
        let e9tool = Path::new("/usr/bin/e9tool");
        let rbc_tramp = Path::new("/build/e9_rbc_trampoline");
        let rip_tramp = Path::new("/build/e9_rip_trampoline");

        let (cmd, _output) = build_e9_rip_command(input, rips, e9tool, rbc_tramp, rip_tramp);

        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();

        // Should have exactly 3 addr= matches.
        let addr_matches: Vec<_> = args.iter().filter(|a| a.starts_with("addr=")).collect();
        assert_eq!(addr_matches.len(), 3);
        assert_eq!(addr_matches[0], "addr=0x1000");
        assert_eq!(addr_matches[1], "addr=0x2000");
        assert_eq!(addr_matches[2], "addr=0x3000");

        // Each addr match should be followed by a rip_trampoline patch.
        let rip_patches: Vec<_> = args
            .iter()
            .filter(|a| a.contains("rip_trampoline"))
            .collect();
        assert_eq!(rip_patches.len(), 3);
    }
}
