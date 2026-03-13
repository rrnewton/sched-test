//! RAII wrapper for C `task_struct` pointers.
//!
//! `SimTaskHandle` owns a heap-allocated C `task_struct` (obtained via
//! `sim_task_alloc`) and exposes safe getters/setters for each field.
//! The raw pointer is freed automatically via `Drop`.
//!
//! **Not yet wired into `engine.rs`** — this is foundational work (T3 of
//! the safety-boundary epic sim-3ef7bc). The engine will be migrated to
//! use `SimTaskHandle` in a later task (T6).

use std::ffi::c_void;

use crate::ffi;
use crate::types::{CpuId, Pid, TimeNs, Vtime};

/// RAII handle to a heap-allocated C `task_struct`.
///
/// Created via [`SimTaskHandle::new`], freed on drop. All accessors are
/// safe `fn` — the `unsafe` FFI calls are encapsulated inside.
///
/// This type is `!Send` and `!Sync` because it contains a raw pointer
/// (`*mut c_void`), which is neither `Send` nor `Sync`. This is correct:
/// the underlying C struct has mutable aliasing assumptions.
pub struct SimTaskHandle {
    /// Non-null pointer to the C `task_struct`. Guaranteed non-null from
    /// construction until `drop`.
    raw: *mut c_void,
}

impl Default for SimTaskHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl SimTaskHandle {
    /// Allocate a new C `task_struct` on the heap.
    ///
    /// # Panics
    /// Panics if `sim_task_alloc` returns null.
    pub fn new() -> Self {
        // SAFETY: `sim_task_alloc` allocates a zeroed `task_struct` on the
        // heap. The returned pointer is valid until `sim_task_free` is called.
        let raw = unsafe { ffi::sim_task_alloc() };
        assert!(!raw.is_null(), "sim_task_alloc returned null");
        Self { raw }
    }

    /// Return the raw pointer for passing to FFI scheduler ops.
    ///
    /// The pointer is valid for the lifetime of this handle.
    pub fn as_raw(&self) -> *mut c_void {
        self.raw
    }

    // ------------------------------------------------------------------
    // PID
    // ------------------------------------------------------------------

    pub fn set_pid(&self, pid: Pid) {
        // SAFETY: `self.raw` is non-null and valid (invariant of this type).
        unsafe { ffi::sim_task_set_pid(self.raw, pid.0) }
    }

    pub fn pid(&self) -> Pid {
        // SAFETY: `self.raw` is non-null and valid.
        Pid(unsafe { ffi::sim_task_get_pid(self.raw) })
    }

    // ------------------------------------------------------------------
    // Comm (task name)
    // ------------------------------------------------------------------

    /// Set the task's `comm` field from a Rust string.
    ///
    /// The string is converted to a C string internally. If the string
    /// contains interior NUL bytes the comm is silently truncated.
    pub fn set_comm(&self, name: &str) {
        let cstr = std::ffi::CString::new(name).unwrap_or_default();
        // SAFETY: `self.raw` is non-null. `cstr.as_ptr()` is a valid
        // NUL-terminated C string that lives for the duration of this call.
        unsafe { ffi::sim_task_set_comm(self.raw, cstr.as_ptr()) }
    }

    // ------------------------------------------------------------------
    // Weight (kernel sched weight, cgroup-space scx.weight)
    // ------------------------------------------------------------------

    pub fn set_weight(&self, weight: u32) {
        // SAFETY: `self.raw` is non-null and valid.
        unsafe { ffi::sim_task_set_weight(self.raw, weight) }
    }

    pub fn weight(&self) -> u32 {
        // SAFETY: `self.raw` is non-null and valid.
        unsafe { ffi::sim_task_get_weight(self.raw) }
    }

    pub fn set_scx_weight(&self, weight: u32) {
        // SAFETY: `self.raw` is non-null and valid.
        unsafe { ffi::sim_task_set_scx_weight(self.raw, weight) }
    }

    pub fn scx_weight(&self) -> u32 {
        // SAFETY: `self.raw` is non-null and valid.
        unsafe { ffi::sim_task_get_scx_weight(self.raw) }
    }

    // ------------------------------------------------------------------
    // Priority
    // ------------------------------------------------------------------

    pub fn set_static_prio(&self, prio: i32) {
        // SAFETY: `self.raw` is non-null and valid.
        unsafe { ffi::sim_task_set_static_prio(self.raw, prio) }
    }

    // ------------------------------------------------------------------
    // Flags (PF_KTHREAD, PF_WQ_WORKER, etc.)
    // ------------------------------------------------------------------

    pub fn set_flags(&self, flags: u32) {
        // SAFETY: `self.raw` is non-null and valid.
        unsafe { ffi::sim_task_set_flags(self.raw, flags) }
    }

    pub fn set_scx_flags(&self, flags: u32) {
        // SAFETY: `self.raw` is non-null and valid.
        unsafe { ffi::sim_task_set_scx_flags(self.raw, flags) }
    }

    pub fn scx_flags(&self) -> u32 {
        // SAFETY: `self.raw` is non-null and valid.
        unsafe { ffi::sim_task_get_scx_flags(self.raw) }
    }

    // ------------------------------------------------------------------
    // CPU affinity
    // ------------------------------------------------------------------

    pub fn set_nr_cpus_allowed(&self, nr: i32) {
        // SAFETY: `self.raw` is non-null and valid.
        unsafe { ffi::sim_task_set_nr_cpus_allowed(self.raw, nr) }
    }

    pub fn nr_cpus_allowed(&self) -> i32 {
        // SAFETY: `self.raw` is non-null and valid.
        unsafe { ffi::sim_task_get_nr_cpus_allowed(self.raw) }
    }

    /// Initialise the cpumask pointer inside the `task_struct`.
    ///
    /// Must be called before `clear_cpumask` / `set_cpumask_cpu`.
    pub fn setup_cpus_ptr(&self) {
        // SAFETY: `self.raw` is non-null and valid.
        unsafe { ffi::sim_task_setup_cpus_ptr(self.raw) }
    }

    pub fn clear_cpumask(&self) {
        // SAFETY: `self.raw` is non-null and valid. `setup_cpus_ptr`
        // must have been called first.
        unsafe { ffi::sim_task_clear_cpumask(self.raw) }
    }

    pub fn set_cpumask_cpu(&self, cpu: CpuId) {
        // SAFETY: `self.raw` is non-null and valid.
        unsafe { ffi::sim_task_set_cpumask_cpu(self.raw, cpu.0 as i32) }
    }

    /// Return a read-only pointer to the task's `cpus_ptr` cpumask.
    pub fn cpus_ptr(&self) -> *const c_void {
        // SAFETY: `self.raw` is non-null and valid.
        unsafe { ffi::sim_task_get_cpus_ptr(self.raw) }
    }

    // ------------------------------------------------------------------
    // Scheduling slice and DSQ vtime
    // ------------------------------------------------------------------

    pub fn set_slice(&self, slice_ns: TimeNs) {
        // SAFETY: `self.raw` is non-null and valid.
        unsafe { ffi::sim_task_set_slice(self.raw, slice_ns) }
    }

    pub fn slice(&self) -> TimeNs {
        // SAFETY: `self.raw` is non-null and valid.
        unsafe { ffi::sim_task_get_slice(self.raw) }
    }

    pub fn set_dsq_vtime(&self, vtime: Vtime) {
        // SAFETY: `self.raw` is non-null and valid.
        unsafe { ffi::sim_task_set_dsq_vtime(self.raw, vtime.0) }
    }

    pub fn dsq_vtime(&self) -> Vtime {
        // SAFETY: `self.raw` is non-null and valid.
        Vtime(unsafe { ffi::sim_task_get_dsq_vtime(self.raw) })
    }

    // ------------------------------------------------------------------
    // Execution time accounting
    // ------------------------------------------------------------------

    pub fn set_sum_exec_runtime(&self, ns: TimeNs) {
        // SAFETY: `self.raw` is non-null and valid.
        unsafe { ffi::sim_task_set_sum_exec_runtime(self.raw, ns) }
    }

    pub fn sum_exec_runtime(&self) -> TimeNs {
        // SAFETY: `self.raw` is non-null and valid.
        unsafe { ffi::sim_task_get_sum_exec_runtime(self.raw) }
    }

    // ------------------------------------------------------------------
    // Address space (mm_struct)
    // ------------------------------------------------------------------

    /// Set the `mm` pointer. Pass `std::ptr::null_mut()` for kernel
    /// threads (which have no address space).
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub fn set_mm(&self, mm: *mut c_void) {
        // SAFETY: `self.raw` is non-null and valid. `mm` may be null
        // (kernel threads).
        unsafe { ffi::sim_task_set_mm(self.raw, mm) }
    }

    pub fn mm(&self) -> *mut c_void {
        // SAFETY: `self.raw` is non-null and valid.
        unsafe { ffi::sim_task_get_mm(self.raw) }
    }

    // ------------------------------------------------------------------
    // Parent-child relationship
    // ------------------------------------------------------------------

    /// Set this task's `real_parent` pointer to another task's raw struct.
    ///
    /// `parent` must be a valid `task_struct` pointer (from another
    /// `SimTaskHandle::as_raw()`).
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub fn set_real_parent(&self, parent: *mut c_void) {
        // SAFETY: Both `self.raw` and `parent` must be valid
        // `task_struct` pointers. The caller (engine) ensures this.
        unsafe { ffi::sim_task_set_real_parent(self.raw, parent) }
    }

    // ------------------------------------------------------------------
    // Migration disabled
    // ------------------------------------------------------------------

    pub fn set_migration_disabled(&self, val: u16) {
        // SAFETY: `self.raw` is non-null and valid.
        unsafe { ffi::sim_task_set_migration_disabled(self.raw, val) }
    }

    pub fn migration_disabled(&self) -> u16 {
        // SAFETY: `self.raw` is non-null and valid.
        unsafe { ffi::sim_task_get_migration_disabled(self.raw) }
    }

    // ------------------------------------------------------------------
    // Cgroup
    // ------------------------------------------------------------------

    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub fn set_cgroup(&self, cgrp: *mut c_void) {
        // SAFETY: `self.raw` is non-null and valid. `cgrp` must be a
        // valid cgroup pointer allocated by `sim_cgroup_alloc`.
        unsafe { ffi::sim_task_set_cgroup(self.raw, cgrp) }
    }

    pub fn cgroup(&self) -> *mut c_void {
        // SAFETY: `self.raw` is non-null and valid.
        unsafe { ffi::sim_task_get_cgroup(self.raw) }
    }
}

impl Drop for SimTaskHandle {
    fn drop(&mut self) {
        // SAFETY: `self.raw` was obtained from `sim_task_alloc` and has
        // not been freed yet (we are the sole owner). After this call the
        // pointer is dangling, but we are being dropped so no further
        // access is possible.
        unsafe { ffi::sim_task_free(self.raw) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SIM_LOCK;

    // All tests that touch the C task_struct must hold SIM_LOCK because
    // the C code has global state (lazy-initialized struct layout tables).

    #[test]
    fn test_alloc_and_drop() {
        let _lock = SIM_LOCK.lock().unwrap();
        let handle = SimTaskHandle::new();
        assert!(!handle.as_raw().is_null());
        // Drop runs sim_task_free — should not panic or double-free.
    }

    #[test]
    fn test_pid_roundtrip() {
        let _lock = SIM_LOCK.lock().unwrap();
        let handle = SimTaskHandle::new();
        handle.set_pid(Pid(42));
        assert_eq!(handle.pid(), Pid(42));
    }

    #[test]
    fn test_weight_roundtrip() {
        let _lock = SIM_LOCK.lock().unwrap();
        let handle = SimTaskHandle::new();
        handle.set_weight(1024);
        assert_eq!(handle.weight(), 1024);
    }

    #[test]
    fn test_scx_weight_roundtrip() {
        let _lock = SIM_LOCK.lock().unwrap();
        let handle = SimTaskHandle::new();
        handle.set_scx_weight(100);
        assert_eq!(handle.scx_weight(), 100);
    }

    #[test]
    fn test_slice_roundtrip() {
        let _lock = SIM_LOCK.lock().unwrap();
        let handle = SimTaskHandle::new();
        handle.set_slice(5_000_000);
        assert_eq!(handle.slice(), 5_000_000);
    }

    #[test]
    fn test_dsq_vtime_roundtrip() {
        let _lock = SIM_LOCK.lock().unwrap();
        let handle = SimTaskHandle::new();
        handle.set_dsq_vtime(Vtime(999));
        assert_eq!(handle.dsq_vtime(), Vtime(999));
    }

    #[test]
    fn test_nr_cpus_allowed_roundtrip() {
        let _lock = SIM_LOCK.lock().unwrap();
        let handle = SimTaskHandle::new();
        handle.set_nr_cpus_allowed(4);
        assert_eq!(handle.nr_cpus_allowed(), 4);
    }

    #[test]
    fn test_sum_exec_runtime_roundtrip() {
        let _lock = SIM_LOCK.lock().unwrap();
        let handle = SimTaskHandle::new();
        handle.set_sum_exec_runtime(123_456_789);
        assert_eq!(handle.sum_exec_runtime(), 123_456_789);
    }

    #[test]
    fn test_migration_disabled_roundtrip() {
        let _lock = SIM_LOCK.lock().unwrap();
        let handle = SimTaskHandle::new();
        handle.set_migration_disabled(2);
        assert_eq!(handle.migration_disabled(), 2);
    }

    #[test]
    fn test_scx_flags_roundtrip() {
        let _lock = SIM_LOCK.lock().unwrap();
        let handle = SimTaskHandle::new();
        handle.set_scx_flags(0xAB);
        assert_eq!(handle.scx_flags(), 0xAB);
    }

    #[test]
    fn test_mm_default_null() {
        let _lock = SIM_LOCK.lock().unwrap();
        let handle = SimTaskHandle::new();
        // Freshly allocated task_struct should have mm = NULL.
        assert!(handle.mm().is_null());
    }

    #[test]
    fn test_multiple_handles_independent() {
        let _lock = SIM_LOCK.lock().unwrap();
        let h1 = SimTaskHandle::new();
        let h2 = SimTaskHandle::new();
        h1.set_pid(Pid(1));
        h2.set_pid(Pid(2));
        assert_eq!(h1.pid(), Pid(1));
        assert_eq!(h2.pid(), Pid(2));
    }
}
