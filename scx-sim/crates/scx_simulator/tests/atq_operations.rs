//! Tests for the ATQ (arena task queue) hold / drop / detach / fini operations
//! and the `scx_atq_pop(atq, hold)` API added in the recent scx update
//! (upstream `cd9c4600` "add task hold and drop helpers" and `2f085946`
//! "add DEAD state and detach/fini operations").
//!
//! ## What is exercised
//!
//! scxsim provides a real, no-stub userspace implementation of the whole
//! `scx_atq_*` surface in `scx-sim/csrc/sim_atq.c` — it is the *kernel
//! substrate* the compiled-in `scx/lib/cgroup_bw.bpf.c` resolves at dlopen
//! time (see the file header and `scx-sim/CLAUDE.md`'s No-Stub Rule). Its
//! semantics mirror `scx/scheds/include/lib/atq.h` one-for-one.
//!
//! No in-tree scheduler *consumes* the ATQ directly yet (the only consumer is
//! Phase-2's `cgroup_bw.bpf.c`, and Phase-3 concurrent mode will use it more
//! broadly — see the `sim_atq.c` header and the `scx_atq_*` extern block in
//! `src/unsafe_impl/kfuncs.rs`). There is therefore no end-to-end scheduler
//! dispatch path to drive it through today; these tests validate the substrate
//! directly via FFI, which is exactly the queue + lifecycle machinery a
//! dispatcher will rely on. The symbols are linked into this test binary via
//! the `#[used] SCX_ATQ_KEEPALIVE` static in `kfuncs.rs`.
//!
//! ## taskc layout
//!
//! `sim_atq.c` only touches two fields of the opaque `scx_task_common`: an
//! `int holdcnt` and a `scx_atq_t *atq` back-pointer, both at runtime-registered
//! offsets (`sim_atq_set_taskc_{holdcnt,atq}_offset`, normally set by
//! `lavd_register_cbw_maps` to the production offsets 56/64). Since no scheduler
//! is loaded here, each test registers offsets matching a local `FakeTaskc`
//! struct, so we control the exact layout the C code reads/writes. The
//! `common::setup_test()` `SIM_LOCK` guard serializes against any concurrent
//! scheduler-loading test that would otherwise re-register these global offsets.

use std::os::raw::c_void;

mod common;

// ---------------------------------------------------------------------------
// FFI to csrc/sim_atq.c (the real ATQ kernel substrate).
// ---------------------------------------------------------------------------
extern "C" {
    fn scx_atq_create_internal(fifo: i32, capacity: u64) -> u64;
    fn scx_atq_destroy(atq: u64) -> i32;
    fn scx_atq_insert_vtime(atq: u64, taskc: *mut c_void, vtime: u64) -> i32;
    fn scx_atq_insert(atq: u64, taskc: *mut c_void) -> i32;
    /// `hold != 0` pins the popped task (bumps `holdcnt`) — the new parameter.
    fn scx_atq_pop(atq: u64, hold: i32) -> u64;
    fn scx_atq_peek(atq: u64) -> u64;
    fn scx_atq_nr_queued(atq: u64) -> i32;
    fn scx_atq_cancel(taskc: *mut c_void) -> i32;
    fn scx_atq_task_hold(taskc: *mut c_void);
    fn scx_atq_task_drop(taskc: *mut c_void);
    fn scx_atq_task_detach(taskc: *mut c_void) -> i32;
    fn scx_atq_task_fini(taskc: *mut c_void) -> i32;
    fn sim_atq_set_taskc_atq_offset(off: u64);
    fn sim_atq_set_taskc_holdcnt_offset(off: u64);
}

/// `SCX_ATQ_INF_CAPACITY` (atq.h `enum scx_atq_consts`): unbounded queue.
const INF_CAPACITY: u64 = u64::MAX;
/// `SCX_ATQ_DEAD` sentinel latched into `taskc->atq` by `scx_atq_task_detach`.
const ATQ_DEAD: u64 = 0x1;

/// A stand-in `scx_task_common`: `holdcnt` at offset 0, `atq` back-pointer at
/// offset 8. We register these offsets so `sim_atq.c` reads/writes exactly
/// these fields (decoupled from the production struct layout).
#[repr(C, align(8))]
#[derive(Default)]
struct FakeTaskc {
    holdcnt: i32,
    _pad: i32,
    atq: u64,
    _tail: [u64; 2],
}
const HOLDCNT_OFF: u64 = 0;
const ATQ_OFF: u64 = 8;

/// Point the substrate at `FakeTaskc`'s layout. Idempotent; call at the start
/// of every test (under the `SIM_LOCK` guard).
unsafe fn register_fake_offsets() {
    sim_atq_set_taskc_holdcnt_offset(HOLDCNT_OFF);
    sim_atq_set_taskc_atq_offset(ATQ_OFF);
}

fn new_taskc() -> *mut FakeTaskc {
    Box::into_raw(Box::new(FakeTaskc::default()))
}

unsafe fn free_taskc(p: *mut FakeTaskc) {
    drop(Box::from_raw(p));
}

fn holdcnt(p: *mut FakeTaskc) -> i32 {
    unsafe { (*p).holdcnt }
}
fn atq_backptr(p: *mut FakeTaskc) -> u64 {
    unsafe { (*p).atq }
}
fn as_taskc(p: *mut FakeTaskc) -> *mut c_void {
    p as *mut c_void
}

// ===========================================================================
// 1. scx_atq_pop with the `hold` parameter.
// ===========================================================================

#[test]
fn test_atq_pop_hold_parameter() {
    let _lock = common::setup_test();
    unsafe {
        register_fake_offsets();

        let a = scx_atq_create_internal(0, INF_CAPACITY);
        assert!(a != 0, "atq create failed");

        let t_lo = new_taskc();
        let t_hi = new_taskc();
        assert_eq!(scx_atq_insert_vtime(a, as_taskc(t_lo), 10), 0);
        assert_eq!(scx_atq_insert_vtime(a, as_taskc(t_hi), 20), 0);
        assert_eq!(scx_atq_nr_queued(a), 2);

        // pop without hold: smallest-vtime task, holdcnt untouched, back-ptr cleared.
        let popped = scx_atq_pop(a, 0);
        assert_eq!(popped, t_lo as u64, "pop should return smallest-vtime task");
        assert_eq!(holdcnt(t_lo), 0, "hold=0 must not bump holdcnt");
        assert_eq!(
            atq_backptr(t_lo),
            0,
            "popped task's atq back-ptr must clear"
        );

        // pop WITH hold: task is pinned (holdcnt bumped to 1).
        let popped2 = scx_atq_pop(a, 1);
        assert_eq!(popped2, t_hi as u64);
        assert_eq!(holdcnt(t_hi), 1, "hold=1 must bump holdcnt to pin the task");
        assert_eq!(atq_backptr(t_hi), 0);

        assert_eq!(scx_atq_nr_queued(a), 0);
        assert_eq!(scx_atq_pop(a, 0), 0, "pop on empty atq returns 0");
        assert_eq!(scx_atq_pop(a, 1), 0, "pop(hold) on empty atq returns 0");

        assert_eq!(scx_atq_destroy(a), 0);
        free_taskc(t_lo);
        free_taskc(t_hi);
    }
}

// ===========================================================================
// 2. Hold / drop lifecycle (balanced holdcnt), incl. pop(hold)+drop pairing.
// ===========================================================================

#[test]
fn test_atq_hold_drop_lifecycle() {
    let _lock = common::setup_test();
    unsafe {
        register_fake_offsets();

        let t = new_taskc();
        // Nested holds accumulate; matching drops unwind exactly.
        scx_atq_task_hold(as_taskc(t));
        scx_atq_task_hold(as_taskc(t));
        scx_atq_task_hold(as_taskc(t));
        assert_eq!(holdcnt(t), 3, "three holds -> holdcnt 3");
        scx_atq_task_drop(as_taskc(t));
        assert_eq!(holdcnt(t), 2);
        scx_atq_task_drop(as_taskc(t));
        scx_atq_task_drop(as_taskc(t));
        assert_eq!(holdcnt(t), 0, "balanced hold/drop returns to 0");

        // The production pattern: pop(hold=true) pins the task, and the
        // consumer's paired scx_atq_task_drop() releases it.
        let a = scx_atq_create_internal(0, INF_CAPACITY);
        assert!(a != 0);
        assert_eq!(scx_atq_insert_vtime(a, as_taskc(t), 1), 0);
        let popped = scx_atq_pop(a, 1);
        assert_eq!(popped, t as u64);
        assert_eq!(holdcnt(t), 1, "pop(hold) pins the task");
        scx_atq_task_drop(as_taskc(t));
        assert_eq!(holdcnt(t), 0, "paired drop releases the pin");

        assert_eq!(scx_atq_destroy(a), 0);
        free_taskc(t);
    }
}

// ===========================================================================
// 3. detach cleanup: unlink from the queue and latch SCX_ATQ_DEAD.
// ===========================================================================

#[test]
fn test_atq_detach_cleanup() {
    let _lock = common::setup_test();
    unsafe {
        register_fake_offsets();

        let a = scx_atq_create_internal(0, INF_CAPACITY);
        assert!(a != 0);
        let t = new_taskc();
        assert_eq!(scx_atq_insert_vtime(a, as_taskc(t), 5), 0);
        assert_eq!(atq_backptr(t), a, "insert sets the atq back-pointer");
        assert_eq!(scx_atq_nr_queued(a), 1);

        // Detach a dying task: removed from its queue AND latched DEAD so it
        // can never be re-queued.
        assert_eq!(scx_atq_task_detach(as_taskc(t)), 0);
        assert_eq!(scx_atq_nr_queued(a), 0, "detach unlinks from the queue");
        assert_eq!(atq_backptr(t), ATQ_DEAD, "detach latches SCX_ATQ_DEAD");

        // Post-detach lifecycle calls are no-ops on a DEAD task.
        assert_eq!(scx_atq_task_fini(as_taskc(t)), 0, "fini on DEAD is a no-op");
        assert_eq!(scx_atq_cancel(as_taskc(t)), 0, "cancel on DEAD is a no-op");
        assert_eq!(atq_backptr(t), ATQ_DEAD, "still DEAD after no-op calls");

        // Detaching a not-queued task still latches DEAD and is safe.
        let t2 = new_taskc();
        assert_eq!(scx_atq_task_detach(as_taskc(t2)), 0);
        assert_eq!(atq_backptr(t2), ATQ_DEAD);

        assert_eq!(scx_atq_destroy(a), 0);
        free_taskc(t);
        free_taskc(t2);
    }
}

// ===========================================================================
// 4. fini cleanup: unlink but keep the task reusable (not DEAD).
// ===========================================================================

#[test]
fn test_atq_fini_cleanup() {
    let _lock = common::setup_test();
    unsafe {
        register_fake_offsets();

        let a = scx_atq_create_internal(0, INF_CAPACITY);
        assert!(a != 0);
        let t = new_taskc();
        assert_eq!(scx_atq_insert_vtime(a, as_taskc(t), 7), 0);
        assert_eq!(scx_atq_nr_queued(a), 1);

        // fini removes the queued task and reports it did so (1), clearing the
        // back-pointer to NULL (reusable, NOT DEAD).
        assert_eq!(
            scx_atq_task_fini(as_taskc(t)),
            1,
            "fini removes a queued task"
        );
        assert_eq!(scx_atq_nr_queued(a), 0);
        assert_eq!(atq_backptr(t), 0, "fini clears back-ptr to NULL (reusable)");

        // fini again — now not queued — reports nothing removed (0).
        assert_eq!(
            scx_atq_task_fini(as_taskc(t)),
            0,
            "fini on a non-queued task -> 0"
        );

        // The task is genuinely reusable: it can be re-queued after fini.
        assert_eq!(scx_atq_insert_vtime(a, as_taskc(t), 3), 0);
        assert_eq!(scx_atq_nr_queued(a), 1);
        assert_eq!(atq_backptr(t), a);

        // fini on a fresh, never-queued task -> 0.
        let fresh = new_taskc();
        assert_eq!(scx_atq_task_fini(as_taskc(fresh)), 0);

        assert_eq!(scx_atq_destroy(a), 0);
        free_taskc(t);
        free_taskc(fresh);
    }
}

// ===========================================================================
// 5. ATQ ordering semantics a dispatcher relies on (vtime priority + FIFO),
//    plus non-destructive peek.
// ===========================================================================

#[test]
fn test_atq_dispatch_ordering_and_peek() {
    let _lock = common::setup_test();
    unsafe {
        register_fake_offsets();

        // --- vtime-ordered atq: pop drains in ascending-vtime (priority) order.
        let a = scx_atq_create_internal(0, INF_CAPACITY);
        assert!(a != 0);
        let t30 = new_taskc();
        let t10 = new_taskc();
        let t20 = new_taskc();
        // Insert out of order.
        assert_eq!(scx_atq_insert_vtime(a, as_taskc(t30), 30), 0);
        assert_eq!(scx_atq_insert_vtime(a, as_taskc(t10), 10), 0);
        assert_eq!(scx_atq_insert_vtime(a, as_taskc(t20), 20), 0);
        assert_eq!(scx_atq_nr_queued(a), 3);

        // peek is non-destructive and returns the next-to-pop (smallest vtime).
        assert_eq!(
            scx_atq_peek(a),
            t10 as u64,
            "peek returns smallest-vtime task"
        );
        assert_eq!(scx_atq_nr_queued(a), 3, "peek must not dequeue");

        assert_eq!(scx_atq_pop(a, 0), t10 as u64);
        assert_eq!(scx_atq_pop(a, 0), t20 as u64);
        assert_eq!(scx_atq_pop(a, 0), t30 as u64);
        assert_eq!(scx_atq_nr_queued(a), 0);
        assert_eq!(scx_atq_peek(a), 0, "peek on empty atq returns 0");

        assert_eq!(scx_atq_destroy(a), 0);
        free_taskc(t30);
        free_taskc(t10);
        free_taskc(t20);

        // --- FIFO atq: pop drains in insertion order.
        let f = scx_atq_create_internal(1, INF_CAPACITY);
        assert!(f != 0);
        let a1 = new_taskc();
        let b1 = new_taskc();
        let c1 = new_taskc();
        assert_eq!(scx_atq_insert(f, as_taskc(a1)), 0);
        assert_eq!(scx_atq_insert(f, as_taskc(b1)), 0);
        assert_eq!(scx_atq_insert(f, as_taskc(c1)), 0);
        assert_eq!(scx_atq_nr_queued(f), 3);
        assert_eq!(scx_atq_pop(f, 0), a1 as u64, "FIFO pop #1");
        assert_eq!(scx_atq_pop(f, 0), b1 as u64, "FIFO pop #2");
        assert_eq!(scx_atq_pop(f, 0), c1 as u64, "FIFO pop #3");

        assert_eq!(scx_atq_destroy(f), 0);
        free_taskc(a1);
        free_taskc(b1);
        free_taskc(c1);
    }
}
