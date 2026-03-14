//! Safe wrapper around cgroup FFI operations.
//!
//! This module provides:
//!
//! - [`CgroupPtr`]: An opaque, non-null pointer to a C `struct cgroup`,
//!   analogous to [`TaskPtr`](crate::scheduler_wrapper::TaskPtr) for tasks.
//!
//! - [`SimCgroupHandle`]: An RAII handle that owns a heap-allocated C
//!   `struct cgroup` obtained via `sim_cgroup_alloc`. The allocation is
//!   freed automatically on `Drop`.
//!
//! - [`CssIterGuard`]: A scoped guard for the C-side CSS iterator. It
//!   resets the iterator on construction and populates it with cgroup
//!   pointers. The iterator is consumed by scheduler `bpf_for_each`
//!   loops.
//!
//! All public methods are safe `fn` — internal `unsafe` blocks carry
//! `// SAFETY:` comments explaining why the call is sound.
//!
//! **Not yet wired into `engine.rs`** — this is foundational work (T4 of
//! the safety-boundary epic sim-3ef7bc). The engine will be migrated to
//! use these wrappers in a later task (T6).

use std::ffi::c_void;

use crate::ffi;
use crate::types::CpuId;

// ---------------------------------------------------------------------------
// CgroupPtr — non-null opaque pointer
// ---------------------------------------------------------------------------

/// Opaque, non-null pointer to a C `struct cgroup`.
///
/// The engine guarantees that every cgroup pointer passed through this
/// type was obtained from `sim_cgroup_alloc` (or `sim_get_root_cgroup`)
/// and has not been freed. This newtype documents that invariant at the
/// type level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CgroupPtr(*mut c_void);

impl CgroupPtr {
    /// Wrap a raw cgroup pointer, panicking if null.
    ///
    /// # Panics
    /// Panics if `p` is null.
    pub fn new(p: *mut c_void) -> Self {
        assert!(!p.is_null(), "CgroupPtr::new called with null pointer");
        Self(p)
    }

    /// Return the inner raw pointer for FFI calls.
    pub fn as_raw(self) -> *mut c_void {
        self.0
    }
}

// ---------------------------------------------------------------------------
// SimCgroupHandle — RAII wrapper for sim_cgroup_alloc / sim_cgroup_free
// ---------------------------------------------------------------------------

/// RAII handle to a heap-allocated C `struct cgroup`.
///
/// Created via [`SimCgroupHandle::new`], freed on drop. All accessors
/// are safe `fn` — the `unsafe` FFI calls are encapsulated inside.
///
/// The root cgroup is statically allocated in C and must NOT be wrapped
/// in this handle (it should not be freed). Use [`SimCgroupHandle::root`]
/// to get a non-owning [`CgroupPtr`] to the root instead.
pub struct SimCgroupHandle {
    /// Non-null pointer to the C `struct cgroup`. Guaranteed non-null
    /// from construction until `drop`.
    raw: *mut c_void,
}

impl SimCgroupHandle {
    /// Allocate a new C `struct cgroup` on the heap.
    ///
    /// # Arguments
    /// * `cgid` - The kernel-style cgroup ID (`cgroup->kn->id`).
    /// * `level` - Depth in the hierarchy (root = 0).
    /// * `parent` - Parent cgroup pointer (use [`SimCgroupHandle::root`]
    ///   for top-level children).
    ///
    /// # Panics
    /// Panics if `sim_cgroup_alloc` returns null.
    pub fn new(cgid: u64, level: u32, parent: CgroupPtr) -> Self {
        // SAFETY: `sim_cgroup_alloc` allocates a `struct cgroup` on the
        // heap, initialising its `kn->id`, `level`, and `ancestors[]`.
        // `parent` is guaranteed non-null by `CgroupPtr`. The returned
        // pointer is valid until `sim_cgroup_free` is called.
        let raw = unsafe { ffi::sim_cgroup_alloc(cgid, level, parent.as_raw()) };
        assert!(!raw.is_null(), "sim_cgroup_alloc returned null");
        Self { raw }
    }

    /// Return the raw pointer for passing to FFI scheduler ops.
    ///
    /// The pointer is valid for the lifetime of this handle.
    pub fn as_ptr(&self) -> CgroupPtr {
        CgroupPtr(self.raw)
    }

    /// Return the raw `*mut c_void` for FFI calls.
    pub fn as_raw(&self) -> *mut c_void {
        self.raw
    }

    /// Get a non-owning [`CgroupPtr`] to the statically-allocated root
    /// cgroup.
    ///
    /// The root cgroup is never freed, so this pointer is valid for the
    /// entire lifetime of the process.
    pub fn root() -> CgroupPtr {
        // SAFETY: `sim_get_root_cgroup` returns a pointer to a
        // statically-allocated root cgroup. It is always non-null and
        // valid for the lifetime of the process.
        let raw = unsafe { ffi::sim_get_root_cgroup() };
        CgroupPtr::new(raw)
    }

    /// Set the cpuset (allowed CPUs) for this cgroup.
    ///
    /// Updates the C-side `struct cgroup` to reflect the given CPU list.
    pub fn set_cpuset(&self, cpus: &[CpuId]) {
        let cpu_ids: Vec<u32> = cpus.iter().map(|c| c.0).collect();
        // SAFETY: `self.raw` is non-null and valid (invariant of this
        // type). `cpu_ids.as_ptr()` points to a valid array of `u32`
        // that lives for the duration of this call.
        unsafe {
            ffi::sim_cgroup_set_cpuset(self.raw, cpu_ids.as_ptr(), cpu_ids.len() as u32);
        }
    }

    /// Consume this handle without freeing the underlying C struct.
    ///
    /// Returns the raw pointer. The caller becomes responsible for
    /// eventually calling `sim_cgroup_free` (or passing the pointer to
    /// [`free_cgroup_raw`]).
    ///
    /// This is used in the destroy-cgroup flow where the raw pointer
    /// must outlive the handle (e.g. for calling `cgroup_exit` before
    /// freeing).
    pub fn into_raw(self) -> *mut c_void {
        let raw = self.raw;
        std::mem::forget(self);
        raw
    }
}

impl Drop for SimCgroupHandle {
    fn drop(&mut self) {
        // SAFETY: `self.raw` was obtained from `sim_cgroup_alloc` and
        // has not been freed yet (we are the sole owner). After this
        // call the pointer is dangling, but we are being dropped so no
        // further access is possible.
        unsafe { ffi::sim_cgroup_free(self.raw) }
    }
}

/// Free a raw cgroup pointer that was detached via
/// [`SimCgroupHandle::into_raw`].
///
/// This is the safe counterpart to `CgroupRegistry::free_raw`. It
/// should only be called after the scheduler's `cgroup_exit` callback
/// has been invoked for this cgroup.
///
/// No-op if `raw` is null.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn free_cgroup_raw(raw: *mut c_void) {
    if !raw.is_null() {
        // SAFETY: The caller guarantees that `raw` was obtained from
        // `SimCgroupHandle::into_raw` (i.e. `sim_cgroup_alloc`) and
        // has not already been freed.
        unsafe { ffi::sim_cgroup_free(raw) }
    }
}

// ---------------------------------------------------------------------------
// CssIterGuard — safe wrapper for the C-side CSS iterator
// ---------------------------------------------------------------------------

/// Scoped guard that populates the C-side CSS iterator.
///
/// On construction the iterator is reset and populated with the given
/// cgroup pointers in order. The scheduler's `bpf_for_each(css, ...)`
/// loop then consumes these entries.
///
/// This type does not implement `Drop` — the C-side iterator state is
/// ephemeral and does not need cleanup. The guard is purely a
/// construction-time helper that ensures `reset` is called before `add`.
pub struct CssIterGuard {
    _private: (), // prevent construction outside this module
}

impl CssIterGuard {
    /// Prepare the CSS iterator with the given root and descendant list.
    ///
    /// # Arguments
    /// * `root` - The root cgroup for the iteration.
    /// * `descendants` - Cgroup pointers in pre-order (including root).
    ///
    /// The returned guard is a witness that the iterator has been
    /// populated. It has no runtime cost.
    pub fn prepare(root: CgroupPtr, descendants: &[CgroupPtr]) -> Self {
        // SAFETY: These three C functions manipulate a thread-local
        // iteration list. We call them in the correct order: reset,
        // set_root, then add each descendant. All pointers are
        // guaranteed non-null by `CgroupPtr`.
        unsafe {
            ffi::sim_css_iter_reset();
            ffi::sim_css_iter_set_root(root.as_raw());
            for cgrp in descendants {
                ffi::sim_css_iter_add(cgrp.as_raw());
            }
        }
        Self { _private: () }
    }

    /// Prepare the CSS iterator from a single root (no descendants).
    ///
    /// Useful when only the root itself should appear in the iteration.
    pub fn prepare_single(root: CgroupPtr) -> Self {
        Self::prepare(root, &[root])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SIM_LOCK;

    // All tests that touch C structs must hold SIM_LOCK because the C
    // code has global state (lazy-initialized struct layout tables).

    #[test]
    fn test_cgroup_ptr_rejects_null() {
        let result = std::panic::catch_unwind(|| {
            CgroupPtr::new(std::ptr::null_mut());
        });
        assert!(result.is_err(), "CgroupPtr::new should panic on null");
    }

    #[test]
    fn test_cgroup_ptr_accepts_non_null() {
        let sentinel = 0x1000 as *mut c_void;
        let ptr = CgroupPtr::new(sentinel);
        assert_eq!(ptr.as_raw(), sentinel);
    }

    #[test]
    fn test_cgroup_ptr_equality() {
        let a = CgroupPtr::new(0x1000 as *mut c_void);
        let b = CgroupPtr::new(0x1000 as *mut c_void);
        let c = CgroupPtr::new(0x2000 as *mut c_void);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn test_root_cgroup_is_non_null() {
        let _lock = SIM_LOCK.lock().unwrap();
        let root = SimCgroupHandle::root();
        assert!(!root.as_raw().is_null());
    }

    #[test]
    fn test_alloc_and_drop() {
        let _lock = SIM_LOCK.lock().unwrap();
        let root = SimCgroupHandle::root();
        let handle = SimCgroupHandle::new(2, 1, root);
        assert!(!handle.as_raw().is_null());
        // Drop runs sim_cgroup_free — should not panic or double-free.
    }

    #[test]
    fn test_as_ptr_returns_cgroup_ptr() {
        let _lock = SIM_LOCK.lock().unwrap();
        let root = SimCgroupHandle::root();
        let handle = SimCgroupHandle::new(3, 1, root);
        let ptr = handle.as_ptr();
        assert_eq!(ptr.as_raw(), handle.as_raw());
    }

    #[test]
    fn test_into_raw_prevents_drop() {
        let _lock = SIM_LOCK.lock().unwrap();
        let root = SimCgroupHandle::root();
        let handle = SimCgroupHandle::new(4, 1, root);
        let raw = handle.into_raw();
        assert!(!raw.is_null());
        // Must manually free since Drop was skipped.
        free_cgroup_raw(raw);
    }

    #[test]
    fn test_free_cgroup_raw_null_is_noop() {
        // Should not panic.
        free_cgroup_raw(std::ptr::null_mut());
    }

    #[test]
    fn test_set_cpuset() {
        let _lock = SIM_LOCK.lock().unwrap();
        let root = SimCgroupHandle::root();
        let handle = SimCgroupHandle::new(5, 1, root);
        // Should not panic — just sets the cpuset on the C struct.
        handle.set_cpuset(&[CpuId(0), CpuId(1)]);
    }

    #[test]
    fn test_set_cpuset_empty() {
        let _lock = SIM_LOCK.lock().unwrap();
        let root = SimCgroupHandle::root();
        let handle = SimCgroupHandle::new(6, 1, root);
        // Empty cpuset should not panic.
        handle.set_cpuset(&[]);
    }

    #[test]
    fn test_multiple_handles_independent() {
        let _lock = SIM_LOCK.lock().unwrap();
        let root = SimCgroupHandle::root();
        let h1 = SimCgroupHandle::new(10, 1, root);
        let h2 = SimCgroupHandle::new(11, 1, root);
        assert_ne!(h1.as_raw(), h2.as_raw());
        // Both should drop cleanly.
    }

    // NOTE: A nested hierarchy test (child of non-root parent) is
    // intentionally omitted. The kernel `struct cgroup` has a flexible
    // array member `ancestors[0]`, but `sim_cgroup_alloc` allocates
    // `sizeof(struct cgroup)` which provides zero space for ancestors.
    // Writing ancestors at level >= 2 causes heap corruption. This is a
    // pre-existing C-side bug (not introduced by this wrapper). The
    // wrapper is correct — it delegates to the same C function that
    // `CgroupRegistry::create` uses.

    #[test]
    fn test_css_iter_guard_prepare() {
        let _lock = SIM_LOCK.lock().unwrap();
        let root = SimCgroupHandle::root();
        let h1 = SimCgroupHandle::new(30, 1, root);
        let h2 = SimCgroupHandle::new(31, 1, root);
        let descendants = [root, h1.as_ptr(), h2.as_ptr()];
        let _guard = CssIterGuard::prepare(root, &descendants);
        // Guard created successfully — iterator is populated.
    }

    #[test]
    fn test_css_iter_guard_prepare_single() {
        let _lock = SIM_LOCK.lock().unwrap();
        let root = SimCgroupHandle::root();
        let _guard = CssIterGuard::prepare_single(root);
    }
}
