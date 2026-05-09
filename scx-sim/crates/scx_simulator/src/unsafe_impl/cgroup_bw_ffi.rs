//! FFI shims that redirect LAVD wrapper.c `scx_cgroup_bw_*` calls into the
//! engine-owned [`BandwidthManager`] (Diff 4 of the 5-diff cgroup_bw stack).
//!
//! # Background
//!
//! Diffs 1-3 built up engine-side cgroup bandwidth state machinery:
//!
//! * Diff 1 (456cf74) — `safe::cgroup_bw::BandwidthManager` foundation.
//! * Diff 2 (9e5ee3c) — `configure_from_cgroup_defs` bridge that synthesizes
//!   implicit cgroups from rt-app-rs taskgroup-only specs.
//! * Diff 3 (cf93b57) — engine enforcement: charge in `stop_and_reenqueue` and
//!   `handle_task_phase_complete`; admission gate `pid_is_bw_throttled`; slice
//!   cap `pid_bw_max_run_ns`; periodic `EventKind::CgroupBwRefill`.
//!
//! Through Diff 3, the engine fully throttles tasks based on `cpu.max`. But the
//! LAVD scheduler under `schedulers/lavd/wrapper.c` still has its own set of
//! weak `scx_cgroup_bw_*` C stubs that just `return 0` (i.e., "never
//! throttled, never charged"). That means the LAVD-side cgroup bandwidth view
//! and the engine-side view are *incoherent*: the engine throttles a cgroup
//! while LAVD believes it has unlimited budget, or vice versa.
//!
//! Diff 4 fixes that incoherence by providing `sim_cgroup_bw_*` extern "C"
//! shims here in Rust. The wrapper.c stubs delegate to these shims via
//! `extern int sim_cgroup_bw_*(...)` declarations, so a single source of truth
//! (the engine's `BandwidthManager`) drives both controllers in the dual-
//! controller H6 matrix.
//!
//! # Symbol mapping
//!
//! | LAVD wrapper.c stub                           | This shim                            | Engine call                                       |
//! |-----------------------------------------------|--------------------------------------|---------------------------------------------------|
//! | `scx_cgroup_bw_lib_init`                      | `sim_cgroup_bw_lib_init`             | (no-op; library config is implicit in scxsim)     |
//! | `scx_cgroup_bw_init(cgrp, args)`              | `sim_cgroup_bw_init(cgrp_raw)`       | (no-op; allocation is in `sim_cgroup_registry_*`) |
//! | `scx_cgroup_bw_exit(cgrp)`                    | `sim_cgroup_bw_exit(cgrp_raw)`       | `BandwidthManager::remove`                        |
//! | `scx_cgroup_bw_set(cgrp, period, quota, brst)`| `sim_cgroup_bw_set(...)`             | `BandwidthManager::configure`                     |
//! | `scx_cgroup_bw_throttled(cgrp, p)`            | `sim_cgroup_bw_throttled(cgrp_raw)`  | `BandwidthManager::is_throttled` (hierarchical)   |
//! | `scx_cgroup_bw_consume(cgrp, runtime)`        | `sim_cgroup_bw_consume(...)`         | `BandwidthManager::charge` (hierarchical)         |
//! | `scx_cgroup_bw_put_aside(p, taskc, vt, cgrp)` | `sim_cgroup_bw_put_aside(...)`       | (no-op; engine head-of-line blocks in DSQ)        |
//! | `scx_cgroup_bw_reenqueue()`                   | `sim_cgroup_bw_reenqueue()`          | (no-op; engine refill drives unthrottle)          |
//! | `scx_cgroup_bw_cancel(taskc)`                 | `sim_cgroup_bw_cancel(taskc)`        | (no-op; cancellation has no engine-side state)    |
//! | `scx_cgroup_bw_move(p, taskc, from, to)`      | `sim_cgroup_bw_move(...)`            | (no-op for first cut; task_to_cgid is rebuilt)    |
//! | `scx_cgroup_bw_is_cgroup_throttled(cgid)`     | `sim_cgroup_bw_is_cgroup_throttled`  | `BandwidthManager::is_throttled` (hierarchical)   |
//! | `scx_cgroup_bw_is_task_throttled(taskc)`      | `sim_cgroup_bw_is_task_throttled`    | (no-op for first cut; LAVD never observes True)   |
//!
//! # Return-value contract
//!
//! `scx_cgroup_bw_throttled` returns `0` for "not throttled" and `-EAGAIN`
//! (`-11`) for "throttled" per the contract in
//! `scx/scheds/include/lib/cgroup.h`. The other configure/consume entries
//! return `0` on success and `-errno` on failure. We mirror that here.
//!
//! # Locking
//!
//! These shims follow the exact same `try_lock()` pattern as `cgroup_ffi.rs`:
//! kfuncs may be called while the engine already holds the `SimState` mutex
//! (via `with_sim`), so re-entrant `try_lock()` is the safe choice. On lock
//! contention we return `0` (not throttled / no-op) — matching the existing
//! `cgroup_ffi` policy of preferring conservative behavior over deadlock.

use std::ffi::c_void;

use crate::cgroup::CgroupId;

/// `-EAGAIN` (per `scx/scheds/include/lib/cgroup.h`):
/// `scx_cgroup_bw_throttled()` returns this when the cgroup is throttled.
const EAGAIN_NEG: i32 = -11;

/// Resolve a raw `struct cgroup *` pointer from C into a [`CgroupId`].
///
/// Returns `None` if the registry cannot be locked or the pointer is unknown.
/// Holding the lock briefly is fine here — callers immediately drop it.
fn cgid_from_raw(cgrp_raw: *mut c_void) -> Option<CgroupId> {
    let arc = crate::kfuncs::clone_sim_arc()?;
    let guard = arc.try_lock().ok()?;
    if cgrp_raw.is_null() {
        return Some(CgroupId::ROOT);
    }
    guard.cgroup_registry.find_cgid_by_raw(cgrp_raw)
}

/// Build an ancestor closure suitable for `BandwidthManager::is_throttled` /
/// `charge` / `max_run_ns`.
///
/// Mirrors the closure constructed in `engine.rs::charge_cgroup_bw` so that
/// hierarchical semantics are *identical* across the engine-driven and
/// wrapper-driven entry points (the entire point of Diff 4).
fn ancestor_lookup(
    registry: &crate::cgroup::CgroupRegistry,
) -> impl Fn(CgroupId) -> Option<CgroupId> + '_ {
    move |cg: CgroupId| -> Option<CgroupId> {
        registry.get(cg).and_then(|info| {
            if info.parent_cgid.0 == 0 || info.cgid == CgroupId::ROOT {
                None
            } else {
                Some(info.parent_cgid)
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Library + lifecycle
// ---------------------------------------------------------------------------

/// `scx_cgroup_bw_lib_init` redirect. No engine-side work — library
/// initialization is implicit at scxsim startup.
#[no_mangle]
pub extern "C" fn sim_cgroup_bw_lib_init() -> i32 {
    0
}

/// `scx_cgroup_bw_init` redirect. The wrapper still calls
/// `sim_cgroup_registry_allocate` separately for ENOMEM simulation; this
/// shim has no additional bookkeeping (the engine-side `BandwidthManager`
/// only tracks cgroups that have a finite quota — i.e., after `_set` is
/// called, not at `_init` time).
#[no_mangle]
pub extern "C" fn sim_cgroup_bw_init(_cgrp_raw: *mut c_void) -> i32 {
    0
}

/// `scx_cgroup_bw_exit` redirect. Drops the cgroup's bandwidth state if any.
#[no_mangle]
pub extern "C" fn sim_cgroup_bw_exit(cgrp_raw: *mut c_void) -> i32 {
    let Some(cgid) = cgid_from_raw(cgrp_raw) else {
        return 0;
    };
    let Some(arc) = crate::kfuncs::clone_sim_arc() else {
        return 0;
    };
    let Ok(mut guard) = arc.try_lock() else {
        return 0;
    };
    guard.bw_manager.remove(cgid);
    0
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// `scx_cgroup_bw_set` redirect. Routes a `cpu.max` write into
/// [`BandwidthManager::configure`], which mirrors the kernel CFS view that
/// Diff 3's enforcement loop already reads.
///
/// The `burst_us` parameter is accepted for ABI parity but currently
/// unused — the engine model treats burst as zero (per Diff 1's design doc:
/// "Burst: stored quota from unused periods (always 0 for now)"). When burst
/// fidelity matters for H6 cell C, extend `BandwidthManager` to take it.
#[no_mangle]
pub extern "C" fn sim_cgroup_bw_set(
    cgrp_raw: *mut c_void,
    period_us: u64,
    quota_us: u64,
    _burst_us: u64,
) -> i32 {
    let Some(cgid) = cgid_from_raw(cgrp_raw) else {
        return 0;
    };
    let Some(arc) = crate::kfuncs::clone_sim_arc() else {
        return 0;
    };
    let Ok(mut guard) = arc.try_lock() else {
        return 0;
    };
    let now_ns = guard.sim.clock;
    guard
        .bw_manager
        .configure(cgid, period_us, quota_us, now_ns);
    0
}

// ---------------------------------------------------------------------------
// Throttling queries
// ---------------------------------------------------------------------------

/// `scx_cgroup_bw_throttled(cgrp, p)` redirect.
///
/// Returns `-EAGAIN` (-11) when the cgroup *or any finite ancestor* is
/// throttled — matching the kernel header contract and matching the
/// engine's own `pid_is_bw_throttled` admission gate. This is the single
/// most important coherence point of Diff 4: LAVD's `cgroup_throttled()`
/// must agree with the engine's DSQ-pop admission decision.
#[no_mangle]
pub extern "C" fn sim_cgroup_bw_throttled(cgrp_raw: *mut c_void) -> i32 {
    // POC (concurrent-mode exploration, agent/scxsim-concurrent-mode-poc):
    // Expose the cgroup-bw throttle-check race surface to scxsim's
    // existing `--interleave` and `--preemptive` interleaving machinery.
    // Without this yield point, the existing concurrency modes never
    // explore interleavings between LAVD's "is the cgroup throttled?"
    // check and the engine's refill/throttle state mutations.
    crate::preempt::set_current_kfunc("cgroup_bw_throttled");
    crate::interleave::maybe_yield();
    let Some(cgid) = cgid_from_raw(cgrp_raw) else {
        return 0;
    };
    let Some(arc) = crate::kfuncs::clone_sim_arc() else {
        return 0;
    };
    let Ok(guard) = arc.try_lock() else {
        return 0;
    };
    let ancestor = ancestor_lookup(&guard.cgroup_registry);
    if guard.bw_manager.is_throttled(cgid, ancestor) {
        EAGAIN_NEG
    } else {
        0
    }
}

/// `scx_cgroup_bw_is_cgroup_throttled(cgid)` redirect.
///
/// Pure boolean form (returns 1/0) used by introspection paths. Same
/// hierarchical semantics as [`sim_cgroup_bw_throttled`].
#[no_mangle]
pub extern "C" fn sim_cgroup_bw_is_cgroup_throttled(cgid_raw: u64) -> i32 {
    let cgid = CgroupId(cgid_raw);
    let Some(arc) = crate::kfuncs::clone_sim_arc() else {
        return 0;
    };
    let Ok(guard) = arc.try_lock() else {
        return 0;
    };
    let ancestor = ancestor_lookup(&guard.cgroup_registry);
    if guard.bw_manager.is_throttled(cgid, ancestor) {
        1
    } else {
        0
    }
}

/// `scx_cgroup_bw_is_task_throttled(taskc)` redirect.
///
/// First-cut: the engine does not track per-task BTQ membership through
/// LAVD's wrapper today (engine head-of-line blocks at the DSQ). Return 0
/// so LAVD's introspection paths see the task as runnable. The engine's
/// own admission gate is what actually defers execution, so this is
/// correct under the H6 single-cgroup reproducer scope.
#[no_mangle]
pub extern "C" fn sim_cgroup_bw_is_task_throttled(_taskc: u64) -> i32 {
    0
}

// ---------------------------------------------------------------------------
// Runtime accounting
// ---------------------------------------------------------------------------

/// `scx_cgroup_bw_consume(cgrp, runtime_ns)` redirect.
///
/// Routes LAVD's `account_task_runtime` charge into the engine
/// `BandwidthManager::charge` path. Note that the engine *also* charges via
/// `charge_cgroup_bw` on `stop_and_reenqueue` / `handle_task_phase_complete`
/// (Diff 3). That intentional double-charge mirrors the production setup
/// where kernel CFS and LAVD BPF independently observe the same task
/// execution and independently enforce — exactly the dual-controller
/// surface H6 needs to reproduce.
///
/// This shim does not attribute to a specific PID — the closure-driven
/// hierarchical charge only needs the cgid and delta — so the trace event
/// recorded for engine-side charging is sufficient for correlation.
#[no_mangle]
pub extern "C" fn sim_cgroup_bw_consume(cgrp_raw: *mut c_void, runtime_ns: u64) -> i32 {
    // POC (concurrent-mode exploration): expose the LAVD-side
    // cgroup-bw consume race surface. Specifically races interesting
    // here include: the gap between LAVD reporting consumed runtime
    // and the engine's refill firing, which under
    // `agent/charge-granularity-experiment`'s steady-state
    // investigation is the source of "phantom 100ms charges per refill"
    // (see STEADY_STATE_INVESTIGATION.md §5.1).
    crate::preempt::set_current_kfunc("cgroup_bw_consume");
    crate::interleave::maybe_yield();
    if runtime_ns == 0 {
        return 0;
    }
    let Some(cgid) = cgid_from_raw(cgrp_raw) else {
        return 0;
    };
    let Some(arc) = crate::kfuncs::clone_sim_arc() else {
        return 0;
    };
    let Ok(mut guard) = arc.try_lock() else {
        return 0;
    };
    // Build the ancestor closure against an immutable borrow of the
    // registry, but release it before reborrowing the manager mutably.
    // The two halves of `SimState` are independent fields, so a single
    // `mut` borrow that reborrows `cgroup_registry` immutably and
    // `bw_manager` mutably is sound — but the borrow checker cannot see
    // that through `MutexGuard::deref_mut`. We work around with a
    // split-borrow via `SimState::fields()`.
    let fields = guard.fields();
    let cgroup_registry: &crate::cgroup::CgroupRegistry = &*fields.cgroup_registry;
    let ancestor = |cg: CgroupId| -> Option<CgroupId> {
        cgroup_registry.get(cg).and_then(|info| {
            if info.parent_cgid.0 == 0 || info.cgid == CgroupId::ROOT {
                None
            } else {
                Some(info.parent_cgid)
            }
        })
    };
    let _newly_exhausted = fields.bw_manager.charge(cgid, runtime_ns, ancestor);
    // Engine-side `charge_cgroup_bw` already records the trace event and
    // marks `throttle(pid)` when newly exhausted on its own charge call.
    // The wrapper-side charge is a coherence echo, not the primary
    // enforcement path — leaving the trace event to engine charging
    // avoids double-counting in the per-pid throttled_pids list.
    0
}

// ---------------------------------------------------------------------------
// BTQ operations (no-ops in first cut)
// ---------------------------------------------------------------------------

/// `scx_cgroup_bw_put_aside(p, taskc, vtime, cgrp)` redirect.
///
/// In the production LAVD model, this queues `p` on a per-LLC backlog
/// (BTQ) when the cgroup is throttled. In scxsim's first cut, the engine
/// head-of-line blocks at the DSQ instead: the DSQ-pop admission gate
/// (Diff 3) refuses denied tasks and the CPU goes idle. That's the exact
/// Bug-1 shape we want for H6 — a runnable task that fails to run.
///
/// Return 0 (success) so LAVD's caller treats put-aside as having
/// happened. The engine's own state machine ensures the task does not
/// execute while throttled.
#[no_mangle]
pub extern "C" fn sim_cgroup_bw_put_aside(
    _p: *mut c_void,
    _taskc: u64,
    _vtime: u64,
    _cgrp_raw: *mut c_void,
) -> i32 {
    // POC (concurrent-mode exploration): expose put-aside race surface.
    // The hidden-livelock investigation (STEADY_STATE_INVESTIGATION.md
    // §5.1) traced the steady-state pathology to put-aside's no-op
    // shim. Even with a no-op body, exposing this site to
    // maybe_yield() lets the interleaving machinery explore
    // orderings between LAVD's "put aside" decision and the engine's
    // dispatch / refill events.
    crate::preempt::set_current_kfunc("cgroup_bw_put_aside");
    crate::interleave::maybe_yield();
    0
}

/// `scx_cgroup_bw_reenqueue()` redirect.
///
/// Production LAVD calls this from dispatch to drain at most
/// `CBW_REENQ_MAX_BATCH = 2` BTQ tasks per dispatch. Engine-side, the
/// `EventKind::CgroupBwRefill` handler already does the equivalent work
/// at period boundaries (Diff 3), and admission re-enables tasks in DSQs
/// automatically once `is_throttled` flips back to false. So this is a
/// no-op here. Return 0 to indicate "no tasks reenqueued."
#[no_mangle]
pub extern "C" fn sim_cgroup_bw_reenqueue() -> i32 {
    // POC (concurrent-mode exploration): expose reenqueue race surface.
    crate::preempt::set_current_kfunc("cgroup_bw_reenqueue");
    crate::interleave::maybe_yield();
    0
}

/// `scx_cgroup_bw_cancel(taskc)` redirect.
///
/// Called by LAVD on dequeue to clean up any per-task throttle state. The
/// engine model has no per-task throttle state attached to `taskc` (the
/// throttle is on the cgroup), so this is a no-op.
#[no_mangle]
pub extern "C" fn sim_cgroup_bw_cancel(_taskc: u64) -> i32 {
    0
}

/// `scx_cgroup_bw_move(p, taskc, from, to)` redirect.
///
/// Called when LAVD observes a cgroup migration (`lavd_cgroup_move`). The
/// engine rebuilds `task_to_cgid` from scenario state at load time and
/// updates it on cgroup-migration scenario events; the wrapper hook here
/// is informational and currently a no-op. If a future diff needs the
/// LAVD-driven migration path to update engine state, route through
/// `task_to_cgid` here.
#[no_mangle]
pub extern "C" fn sim_cgroup_bw_move(
    _p: *mut c_void,
    _taskc: u64,
    _from_raw: *mut c_void,
    _to_raw: *mut c_void,
) -> i32 {
    0
}

/// `scx_cgroup_bw_dump(cgid, descendant, accurate, indent)` redirect.
///
/// Production LAVD's dump-on-watchdog path (`bb0da4f4`/`c14b6315`) prints
/// debugging state. Engine-side, the `BandwidthManager` exposes
/// `iter()` and per-state fields if a future diff wants to surface the
/// same content. No-op for now.
#[no_mangle]
pub extern "C" fn sim_cgroup_bw_dump(
    _cgid_raw: u64,
    _descendant: bool,
    _accurate: bool,
    _indent: bool,
) -> i32 {
    0
}
