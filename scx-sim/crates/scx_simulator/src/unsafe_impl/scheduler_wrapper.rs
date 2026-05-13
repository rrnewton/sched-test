//! Safe wrapper around the `Scheduler` trait.
//!
//! The `Scheduler` trait methods are `unsafe fn` because they call into
//! C code via FFI (either through dynamically-loaded `.so` function
//! pointers or through statically-linked C stubs). This module provides
//! a safe facade that internalises the `unsafe` blocks with documented
//! safety invariants.
//!
//! **Not yet wired into `engine.rs`** — this is foundational work (T2 of
//! the safety-boundary epic sim-3ef7bc). The engine will be migrated to
//! use `SchedulerWrapper` in a later task (T6).

use std::ffi::c_void;

use crate::ffi::Scheduler;

/// Opaque, non-null pointer to a C `task_struct`.
///
/// The engine guarantees that every task pointer passed to scheduler
/// callbacks was obtained from `sim_task_alloc` and has not been freed.
/// This newtype documents that invariant at the type level.
#[derive(Debug, Clone, Copy)]
pub struct TaskPtr(*mut c_void);

impl TaskPtr {
    /// Wrap a raw task pointer, panicking if null.
    ///
    /// # Panics
    /// Panics if `p` is null.
    pub fn new(p: *mut c_void) -> Self {
        assert!(!p.is_null(), "TaskPtr::new called with null pointer");
        Self(p)
    }

    /// Return the inner raw pointer for FFI calls.
    pub fn as_raw(self) -> *mut c_void {
        self.0
    }
}

/// An optional pointer that may be null (e.g. `prev` in dispatch).
#[derive(Debug, Clone, Copy)]
pub struct OptionalPtr(*mut c_void);

impl OptionalPtr {
    /// Wrap a raw pointer that is allowed to be null.
    pub fn new(p: *mut c_void) -> Self {
        Self(p)
    }

    /// Null pointer.
    pub fn null() -> Self {
        Self(std::ptr::null_mut())
    }

    /// Return the inner raw pointer for FFI calls.
    pub fn as_raw(self) -> *mut c_void {
        self.0
    }
}

/// Safe wrapper around a boxed `Scheduler` trait object.
///
/// Each public method corresponds to one `Scheduler` trait callback.
/// The `unsafe` call is contained inside the method body with a
/// `// SAFETY:` comment explaining why the call is sound.
pub struct SchedulerWrapper<S: Scheduler> {
    inner: S,
}

impl<S: Scheduler> SchedulerWrapper<S> {
    /// Create a new wrapper around the given scheduler.
    pub fn new(scheduler: S) -> Self {
        Self { inner: scheduler }
    }

    /// Consume the wrapper and return the inner scheduler.
    pub fn into_inner(self) -> S {
        self.inner
    }

    /// Borrow the inner scheduler (e.g. for `DynamicScheduler`-specific
    /// configuration methods like `lavd_configure`).
    pub fn inner(&self) -> &S {
        &self.inner
    }

    // ------------------------------------------------------------------
    // Mandatory callbacks
    // ------------------------------------------------------------------

    /// Initialize the scheduler (`ops.init`). Called once before simulation.
    pub fn init(&self) -> i32 {
        // SAFETY: `init` takes no pointer arguments. The scheduler `.so`
        // has been loaded and its function pointers are valid for the
        // lifetime of the `Scheduler` value we hold.
        unsafe { self.inner.init() }
    }

    /// Select a CPU for a waking task (`ops.select_cpu`).
    pub fn select_cpu(&self, p: TaskPtr, prev_cpu: i32, wake_flags: u64) -> i32 {
        // SAFETY: `p` is guaranteed non-null by `TaskPtr::new`.
        // The engine ensures it points to a live `task_struct`.
        unsafe { self.inner.select_cpu(p.as_raw(), prev_cpu, wake_flags) }
    }

    /// Enqueue a task (`ops.enqueue`).
    pub fn enqueue(&self, p: TaskPtr, enq_flags: u64) {
        // SAFETY: `p` is guaranteed non-null by `TaskPtr::new`.
        unsafe { self.inner.enqueue(p.as_raw(), enq_flags) }
    }

    /// CPU is looking for work (`ops.dispatch`).
    ///
    /// `prev` may be null when there is no previously-running task.
    pub fn dispatch(&self, cpu: i32, prev: OptionalPtr) {
        // SAFETY: `prev` is allowed to be null per the trait contract.
        // When non-null it must point to a valid `task_struct`; the
        // engine upholds this invariant.
        unsafe { self.inner.dispatch(cpu, prev.as_raw()) }
    }

    /// A task started running (`ops.running`).
    pub fn running(&self, p: TaskPtr) {
        // SAFETY: `p` is guaranteed non-null by `TaskPtr::new`.
        unsafe { self.inner.running(p.as_raw()) }
    }

    /// A task stopped running (`ops.stopping`).
    pub fn stopping(&self, p: TaskPtr, runnable: bool) {
        // SAFETY: `p` is guaranteed non-null by `TaskPtr::new`.
        unsafe { self.inner.stopping(p.as_raw(), runnable) }
    }

    // ------------------------------------------------------------------
    // Optional callbacks — task lifecycle
    // ------------------------------------------------------------------

    /// Enable a task for scheduling (`ops.enable`).
    pub fn enable(&self, p: TaskPtr) {
        // SAFETY: `p` is guaranteed non-null by `TaskPtr::new`.
        unsafe { self.inner.enable(p.as_raw()) }
    }

    /// A task was dequeued (`ops.dequeue`).
    pub fn dequeue(&self, p: TaskPtr, deq_flags: u64) {
        // SAFETY: `p` is guaranteed non-null by `TaskPtr::new`.
        unsafe { self.inner.dequeue(p.as_raw(), deq_flags) }
    }

    /// A task went to sleep (`ops.quiescent`).
    pub fn quiescent(&self, p: TaskPtr, deq_flags: u64) {
        // SAFETY: `p` is guaranteed non-null by `TaskPtr::new`.
        unsafe { self.inner.quiescent(p.as_raw(), deq_flags) }
    }

    /// A task became runnable (`ops.runnable`).
    pub fn runnable(&self, p: TaskPtr, enq_flags: u64) {
        // SAFETY: `p` is guaranteed non-null by `TaskPtr::new`.
        unsafe { self.inner.runnable(p.as_raw(), enq_flags) }
    }

    /// Initialize a task (`ops.init_task`).
    pub fn init_task(&self, p: TaskPtr) -> i32 {
        // SAFETY: `p` is guaranteed non-null by `TaskPtr::new`.
        unsafe { self.inner.init_task(p.as_raw()) }
    }

    /// Initialize a task in a specific cgroup (`ops.init_task` with
    /// cgroup override).
    pub fn init_task_in_cgroup(&self, p: TaskPtr, cgrp: TaskPtr) -> i32 {
        // SAFETY: Both pointers are guaranteed non-null by `TaskPtr::new`.
        unsafe { self.inner.init_task_in_cgroup(p.as_raw(), cgrp.as_raw()) }
    }

    /// A task is exiting scheduling (`ops.exit_task`).
    pub fn exit_task(&self, p: TaskPtr) -> i32 {
        // SAFETY: `p` is guaranteed non-null by `TaskPtr::new`.
        unsafe { self.inner.exit_task(p.as_raw()) }
    }

    // ------------------------------------------------------------------
    // Optional callbacks — CPU lifecycle
    // ------------------------------------------------------------------

    /// A CPU was released by a higher scheduling class (`ops.cpu_release`).
    pub fn cpu_release(&self, cpu: i32, args: OptionalPtr) {
        // SAFETY: `args` may be null per the trait contract.
        unsafe { self.inner.cpu_release(cpu, args.as_raw()) }
    }

    /// A CPU was acquired back (`ops.cpu_acquire`).
    pub fn cpu_acquire(&self, cpu: i32, args: OptionalPtr) {
        // SAFETY: `args` may be null per the trait contract.
        unsafe { self.inner.cpu_acquire(cpu, args.as_raw()) }
    }

    /// A CPU came online (`ops.cpu_online`).
    pub fn cpu_online(&self, cpu: i32) {
        // SAFETY: No pointer arguments; cpu ID is validated by the engine.
        unsafe { self.inner.cpu_online(cpu) }
    }

    /// A CPU went offline (`ops.cpu_offline`).
    pub fn cpu_offline(&self, cpu: i32) {
        // SAFETY: No pointer arguments; cpu ID is validated by the engine.
        unsafe { self.inner.cpu_offline(cpu) }
    }

    /// Phase 2 Stage C (tg `compile-scx-cgroup-bw-library-into-scxsim-phase2`):
    /// query the scheduler-loaded cgroup_bw library for the throttle
    /// state of `cgrp_id`. `Some(true)` / `Some(false)` if the
    /// scheduler models cgroup_bw and answered; `None` if the
    /// scheduler does not link the library at all.
    ///
    /// The engine's DSQ-pop admission gate (`pid_is_bw_throttled`)
    /// consults this so the library is the single source of truth for
    /// throttle state -- replacing the engine-side
    /// `BandwidthManager::is_throttled` direct read.
    pub fn is_cgroup_throttled(&self, cgrp_id: u64) -> Option<bool> {
        // No unsafe needed: the trait method handles the FFI call site.
        self.inner.is_cgroup_throttled(cgrp_id)
    }

    /// Phase 2 Stage E diagnostic: query the library's per-cgroup state
    /// for `(cgrp_id, llc_id)` via the `scxsim_probe_cbw_state` exported
    /// forwarder. Returns None if the loaded scheduler does not expose
    /// the probe.
    pub fn probe_cbw_state(
        &self,
        cgrp_id: u64,
        llc_id: i32,
        out: &mut crate::ffi::CbwProbeResult,
    ) -> Option<i32> {
        self.inner.probe_cbw_state(cgrp_id, llc_id, out)
    }

    /// Library-driven slice-cap budget query
    /// (`scxsim_cgroup_bw_budget_remaining`). Returns the cgroup's
    /// remaining cpu.max budget for the current period in
    /// nanoseconds, or `u64::MAX` (the wrapper.c sentinel) when no
    /// cap should apply. Returns `None` when the loaded scheduler
    /// does not link the cgroup_bw library.
    pub fn cgroup_bw_budget_remaining(&self, cgrp_id: u64) -> Option<u64> {
        self.inner.cgroup_bw_budget_remaining(cgrp_id)
    }

    /// Snapshot the cgroup_bw library state for a single cgroup
    /// identified by its RAW cgrp pointer (from scxsim's
    /// `cgroup_registry`). The `cgid` argument is informational only
    /// (copied into `out->cgid`).
    ///
    /// Returns `None` when the loaded scheduler does not link the
    /// cgroup_bw library; `Some(rc)` otherwise (0 = success, negative
    /// errno-style codes for "not registered" / "unlimited quota" --
    /// see the trait docs for the full mapping).
    ///
    /// tg `wprof-r2-add-cgroup-bw-replenish-tracekind-smoking-gun`.
    pub fn snapshot_by_raw_cgrp(
        &self,
        cgid: u64,
        cgrp_raw: *mut std::ffi::c_void,
        out: &mut crate::ffi::CbwCgroupSnapshot,
    ) -> Option<i32> {
        self.inner.snapshot_by_raw_cgrp(cgid, cgrp_raw, out)
    }

    /// CPU idle state changed (`ops.update_idle`).
    pub fn update_idle(&self, cpu: i32, idle: bool) {
        // SAFETY: No pointer arguments; cpu ID is validated by the engine.
        unsafe { self.inner.update_idle(cpu, idle) }
    }

    // ------------------------------------------------------------------
    // Optional callbacks — periodic / timer
    // ------------------------------------------------------------------

    /// Fire a pending BPF timer callback for `slot` (`ops.fire_timer(slot)`).
    ///
    /// `slot` (0..MAX_BPF_TIMERS) selects which of the scheduler's
    /// per-scheduler timer slots fired. Phase 1 BPF infra scale-up
    /// items 1+2: schedulers maintain a slot table in their wrapper.c
    /// keyed by `(struct bpf_timer *)`; the engine routes the slot
    /// through so the wrapper dispatches to the right callback.
    /// Single-timer schedulers (mitosis, cosmos, the legacy LAVD path)
    /// always receive `slot = 0`.
    pub fn fire_timer(&self, slot: u8) {
        // SAFETY: `slot` is a small integer; the scheduler's wrapper.c
        // dispatches based on it.
        unsafe { self.inner.fire_timer(slot as u32) }
    }

    /// Periodic tick (`ops.tick`).
    pub fn tick(&self, p: TaskPtr) {
        // SAFETY: `p` is guaranteed non-null by `TaskPtr::new`.
        unsafe { self.inner.tick(p.as_raw()) }
    }

    // ------------------------------------------------------------------
    // Optional callbacks — cpumask
    // ------------------------------------------------------------------

    /// Notify scheduler of a task cpumask change (`ops.set_cpumask`).
    ///
    /// `cpumask` is a raw pointer to a kernel `cpumask` struct. It must
    /// remain valid for the duration of the call.
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub fn set_cpumask(&self, p: TaskPtr, cpumask: *const c_void) {
        // SAFETY: `p` is guaranteed non-null. `cpumask` is a read-only
        // pointer to a cpumask allocated by the engine (via
        // `sim_task_get_cpus_ptr`). The engine guarantees the cpumask
        // outlives the call.
        unsafe { self.inner.set_cpumask(p.as_raw(), cpumask) }
    }

    // ------------------------------------------------------------------
    // Optional callbacks — debugging
    // ------------------------------------------------------------------

    /// Dump scheduler state (`ops.dump`).
    pub fn dump(&self, dctx: OptionalPtr) {
        // SAFETY: `dctx` may be null per the trait contract.
        unsafe { self.inner.dump(dctx.as_raw()) }
    }

    /// Dump per-task state (`ops.dump_task`).
    pub fn dump_task(&self, dctx: OptionalPtr, p: TaskPtr) {
        // SAFETY: `dctx` may be null. `p` is guaranteed non-null.
        unsafe { self.inner.dump_task(dctx.as_raw(), p.as_raw()) }
    }

    // ------------------------------------------------------------------
    // Optional callbacks — scheduler lifecycle
    // ------------------------------------------------------------------

    /// Scheduler is being unloaded (`ops.exit`).
    pub fn exit(&self) {
        // SAFETY: No pointer arguments.
        unsafe { self.inner.exit() }
    }

    // ------------------------------------------------------------------
    // Optional callbacks — cgroup
    // ------------------------------------------------------------------

    /// Initialize a cgroup (`ops.cgroup_init`).
    pub fn cgroup_init(&self, cgrp: TaskPtr, args: OptionalPtr) -> i32 {
        // SAFETY: `cgrp` is guaranteed non-null. `args` may be null.
        unsafe { self.inner.cgroup_init(cgrp.as_raw(), args.as_raw()) }
    }

    /// Exit a cgroup (`ops.cgroup_exit`).
    pub fn cgroup_exit(&self, cgrp: TaskPtr) {
        // SAFETY: `cgrp` is guaranteed non-null.
        unsafe { self.inner.cgroup_exit(cgrp.as_raw()) }
    }

    /// A task moved between cgroups (`ops.cgroup_move`).
    pub fn cgroup_move(&self, p: TaskPtr, from: TaskPtr, to: TaskPtr) {
        // SAFETY: All three pointers are guaranteed non-null.
        unsafe {
            self.inner
                .cgroup_move(p.as_raw(), from.as_raw(), to.as_raw())
        }
    }

    /// Cgroup bandwidth was configured (`ops.cgroup_set_bandwidth`).
    pub fn cgroup_set_bandwidth(
        &self,
        cgrp: TaskPtr,
        period_us: u64,
        quota_us: u64,
        burst_us: u64,
    ) {
        // SAFETY: `cgrp` is guaranteed non-null.
        unsafe {
            self.inner
                .cgroup_set_bandwidth(cgrp.as_raw(), period_us, quota_us, burst_us)
        }
    }

    // ------------------------------------------------------------------
    // Non-FFI methods (already safe on the trait)
    // ------------------------------------------------------------------

    /// Resolve e9patch C trampoline function pointers.
    pub fn resolve_e9_fns(&self) -> Option<crate::backend::e9patch::E9PatchFns> {
        self.inner.resolve_e9_fns()
    }

    /// Return debugger metadata for `--wait-debugger` support.
    pub fn debugger_info(&self) -> Option<crate::ffi::DebuggerInfo> {
        self.inner.debugger_info()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal stub scheduler for unit testing the wrapper.
    ///
    /// Tracks which callbacks were invoked so tests can assert coverage
    /// without loading a real `.so`.
    struct StubScheduler {
        init_called: std::cell::Cell<bool>,
        select_cpu_result: i32,
        enqueue_called: std::cell::Cell<bool>,
        dispatch_called: std::cell::Cell<bool>,
        running_called: std::cell::Cell<bool>,
        stopping_called: std::cell::Cell<bool>,
    }

    impl StubScheduler {
        fn new() -> Self {
            Self {
                init_called: std::cell::Cell::new(false),
                select_cpu_result: 0,
                enqueue_called: std::cell::Cell::new(false),
                dispatch_called: std::cell::Cell::new(false),
                running_called: std::cell::Cell::new(false),
                stopping_called: std::cell::Cell::new(false),
            }
        }
    }

    impl Scheduler for StubScheduler {
        unsafe fn init(&self) -> i32 {
            self.init_called.set(true);
            0
        }

        unsafe fn select_cpu(&self, _p: *mut c_void, _prev_cpu: i32, _wake_flags: u64) -> i32 {
            self.select_cpu_result
        }

        unsafe fn enqueue(&self, _p: *mut c_void, _enq_flags: u64) {
            self.enqueue_called.set(true);
        }

        unsafe fn dispatch(&self, _cpu: i32, _prev: *mut c_void) {
            self.dispatch_called.set(true);
        }

        unsafe fn running(&self, _p: *mut c_void) {
            self.running_called.set(true);
        }

        unsafe fn stopping(&self, _p: *mut c_void, _runnable: bool) {
            self.stopping_called.set(true);
        }

        unsafe fn enable(&self, _p: *mut c_void) {}
    }

    /// Non-null sentinel pointer for testing. We never dereference it;
    /// the stub scheduler ignores pointer values entirely.
    fn dummy_task_ptr() -> TaskPtr {
        TaskPtr::new(0x1000 as *mut c_void)
    }

    #[test]
    fn test_init_delegates() {
        let stub = StubScheduler::new();
        let wrapper = SchedulerWrapper::new(stub);
        assert_eq!(wrapper.init(), 0);
        assert!(wrapper.inner().init_called.get());
    }

    #[test]
    fn test_select_cpu_delegates() {
        let mut stub = StubScheduler::new();
        stub.select_cpu_result = 3;
        let wrapper = SchedulerWrapper::new(stub);
        let result = wrapper.select_cpu(dummy_task_ptr(), 1, 0);
        assert_eq!(result, 3);
    }

    #[test]
    fn test_enqueue_delegates() {
        let stub = StubScheduler::new();
        let wrapper = SchedulerWrapper::new(stub);
        wrapper.enqueue(dummy_task_ptr(), 42);
        assert!(wrapper.inner().enqueue_called.get());
    }

    #[test]
    fn test_dispatch_with_null_prev() {
        let stub = StubScheduler::new();
        let wrapper = SchedulerWrapper::new(stub);
        wrapper.dispatch(0, OptionalPtr::null());
        assert!(wrapper.inner().dispatch_called.get());
    }

    #[test]
    fn test_running_delegates() {
        let stub = StubScheduler::new();
        let wrapper = SchedulerWrapper::new(stub);
        wrapper.running(dummy_task_ptr());
        assert!(wrapper.inner().running_called.get());
    }

    #[test]
    fn test_stopping_delegates() {
        let stub = StubScheduler::new();
        let wrapper = SchedulerWrapper::new(stub);
        wrapper.stopping(dummy_task_ptr(), true);
        assert!(wrapper.inner().stopping_called.get());
    }

    #[test]
    #[should_panic(expected = "TaskPtr::new called with null pointer")]
    fn test_task_ptr_rejects_null() {
        TaskPtr::new(std::ptr::null_mut());
    }

    #[test]
    fn test_optional_ptr_allows_null() {
        let p = OptionalPtr::null();
        assert!(p.as_raw().is_null());
    }

    #[test]
    fn test_into_inner() {
        let stub = StubScheduler::new();
        let wrapper = SchedulerWrapper::new(stub);
        let _ = wrapper.into_inner();
    }

    #[test]
    fn test_exit_delegates() {
        let stub = StubScheduler::new();
        let wrapper = SchedulerWrapper::new(stub);
        // exit is a no-op on StubScheduler, just verify it doesn't panic
        wrapper.exit();
    }

    #[test]
    fn test_fire_timer_delegates() {
        let stub = StubScheduler::new();
        let wrapper = SchedulerWrapper::new(stub);
        // Phase 1 BPF infra scale-up items 1+2: fire_timer takes a slot id.
        wrapper.fire_timer(0);
    }
}
