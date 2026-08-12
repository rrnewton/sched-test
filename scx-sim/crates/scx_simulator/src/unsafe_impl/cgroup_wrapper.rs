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

// Safety: CgroupPtr is a non-owning, non-null pointer to a C `struct cgroup`.
// All cgroup pointers are only accessed while the simulator mutex is held
// (or from C callbacks that run inside the mutex-protected sim_callback!
// macro). The single-writer / mutex-protected access model makes sharing
// across threads safe.
unsafe impl Send for CgroupPtr {}
unsafe impl Sync for CgroupPtr {}

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

// Safety: SimCgroupHandle is an owning RAII wrapper around a heap-allocated
// C `struct cgroup`. The simulator accesses these exclusively while holding
// the SimState mutex (or from C callbacks inside the mutex-protected
// sim_callback! macro). The single-writer / mutex-protected access model
// makes sending and sharing across threads safe.
unsafe impl Send for SimCgroupHandle {}
unsafe impl Sync for SimCgroupHandle {}

impl std::fmt::Debug for SimCgroupHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimCgroupHandle")
            .field("raw", &self.raw)
            .finish()
    }
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

    /// Publish the cgroup's directory-entry name into `cgrp->kn->name`.
    ///
    /// This is what a BPF scheduler walks to reconstruct the cgroup path
    /// (scx_layered's `format_cgrp_path()` → `MATCH_CGROUP_*` rules), so a
    /// cgroup created without a name is invisible to path matching.
    /// Interior NULs truncate the name, as they would a C string.
    pub fn set_name(&self, name: &str) {
        let cstr = std::ffi::CString::new(name).unwrap_or_default();
        // SAFETY: `self.raw` came from `sim_cgroup_alloc`, whose kernfs_node
        // carries co-allocated name storage. The C side copies out of `cstr`
        // before returning, so it need not outlive the call.
        unsafe { ffi::sim_cgroup_set_name(self.raw, cstr.as_ptr()) }
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

// ---------------------------------------------------------------------------
// CgroupAlloc — discriminated ownership for root vs. heap-allocated cgroups
// ---------------------------------------------------------------------------

/// Discriminated ownership wrapper for cgroup C allocations.
///
/// The root cgroup is statically allocated in C (`sim_get_root_cgroup`) and
/// must never be freed. All other cgroups are heap-allocated via
/// `sim_cgroup_alloc` and must be freed exactly once. This enum encodes
/// that distinction at the type level, eliminating the need for manual
/// `is_root` checks and `unsafe impl Send/Sync` on the containing struct.
///
/// `CgroupAlloc` is `Send + Sync` because its inner types are.
#[derive(Debug)]
pub enum CgroupAlloc {
    /// Statically-allocated root cgroup (never freed).
    Root(CgroupPtr),
    /// Heap-allocated cgroup (freed on drop via [`SimCgroupHandle`]).
    Owned(SimCgroupHandle),
}

impl CgroupAlloc {
    /// Get the raw `*mut c_void` pointer for FFI calls.
    pub fn as_raw(&self) -> *mut c_void {
        match self {
            CgroupAlloc::Root(ptr) => ptr.as_raw(),
            CgroupAlloc::Owned(handle) => handle.as_raw(),
        }
    }

    /// Get a [`CgroupPtr`] for this allocation.
    pub fn as_ptr(&self) -> CgroupPtr {
        match self {
            CgroupAlloc::Root(ptr) => *ptr,
            CgroupAlloc::Owned(handle) => handle.as_ptr(),
        }
    }

    /// Set the cpuset (allowed CPUs) for this cgroup.
    ///
    /// Delegates to [`SimCgroupHandle::set_cpuset`] for owned cgroups,
    /// or calls the FFI function directly for the root.
    pub fn set_cpuset(&self, cpus: &[CpuId]) {
        match self {
            CgroupAlloc::Root(ptr) => {
                let cpu_ids: Vec<u32> = cpus.iter().map(|c| c.0).collect();
                // SAFETY: Root pointer is always valid (statically allocated).
                unsafe {
                    ffi::sim_cgroup_set_cpuset(
                        ptr.as_raw(),
                        cpu_ids.as_ptr(),
                        cpu_ids.len() as u32,
                    );
                }
            }
            CgroupAlloc::Owned(handle) => handle.set_cpuset(cpus),
        }
    }

    /// Consume this allocation and return the raw pointer WITHOUT freeing.
    ///
    /// For `Owned` variants, this calls [`SimCgroupHandle::into_raw`] to
    /// prevent the destructor from firing. The caller becomes responsible
    /// for eventually calling [`free_cgroup_raw`].
    ///
    /// # Panics
    /// Panics if called on the `Root` variant (the root cgroup must never
    /// be detached from the registry).
    pub fn into_raw(self) -> *mut c_void {
        match self {
            CgroupAlloc::Root(_) => panic!("cannot detach the root cgroup"),
            CgroupAlloc::Owned(handle) => handle.into_raw(),
        }
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

/// Safe wrapper for `sim_get_default_cgroup_init_args`.
///
/// Phase 2 (tg `compile-scx-cgroup-bw-library-into-scxsim-phase2`):
/// returns a pointer to the C-side static `struct scx_cgroup_init_args`
/// (defined in `csrc/sim_cgroup.c`) that the engine passes to
/// `scheduler.cgroup_init` in place of the pre-Phase-2 NULL pointer.
/// The pointer targets a static singleton; the library reads it once
/// per cgroup_init call and does not retain it.
///
/// Safe wrapper because the underlying C function returns a pointer to
/// a process-lifetime static -- no mutation risk, no aliasing UB.
pub fn default_cgroup_init_args() -> *mut c_void {
    // SAFETY: target is a process-lifetime static populated at link
    // time; the function has no preconditions.
    unsafe { ffi::sim_get_default_cgroup_init_args() }
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
    /// Prepare the CSS iterator with the given root and pre-order +
    /// post-order descendant lists.
    ///
    /// # Arguments
    /// * `root` - The root cgroup for the iteration.
    /// * `descendants_pre` - Cgroup pointers in pre-order (including root).
    /// * `descendants_post` - Cgroup pointers in post-order (including root).
    ///
    /// Both lists must be supplied so that subsequent
    /// `bpf_for_each(css, pos, root, flags)` loops can pick the right
    /// traversal order via the flags bit. Phase 1 BPF infra scale-up
    /// item 3 -- POST is required by Phase 2's compiled-in
    /// `cgroup_bw.bpf.c` (charges + replenish walk POST).
    ///
    /// The returned guard is a witness that the iterator has been
    /// populated. It has no runtime cost.
    pub fn prepare(
        root: CgroupPtr,
        descendants_pre: &[CgroupPtr],
        descendants_post: &[CgroupPtr],
    ) -> Self {
        // SAFETY: These C functions manipulate two static iteration
        // buffers. We call them in the correct order: reset (clears
        // both), set_root, then append each descendant to the matching
        // buffer. All pointers are guaranteed non-null by `CgroupPtr`.
        unsafe {
            ffi::sim_css_iter_reset();
            ffi::sim_css_iter_set_root(root.as_raw());
            for cgrp in descendants_pre {
                ffi::sim_css_iter_add(cgrp.as_raw());
            }
            for cgrp in descendants_post {
                ffi::sim_css_iter_add_post(cgrp.as_raw());
            }
        }
        Self { _private: () }
    }

    /// Prepare the CSS iterator from a single root (no descendants).
    ///
    /// Useful when only the root itself should appear in the iteration.
    /// Both pre-order and post-order buffers are populated identically
    /// (single-element traversal is the same in either order).
    // Exercised by this module's tests; a single-root CSS-iter constructor kept
    // alongside `prepare` for completeness.
    #[allow(dead_code)]
    pub fn prepare_single(root: CgroupPtr) -> Self {
        Self::prepare(root, &[root], &[root])
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

    /// Regression test for the `sim_cgroup_alloc` heap-overflow bug.
    ///
    /// The kernel's `struct cgroup` (vmlinux.h) ends with a flexible
    /// array member `struct cgroup *ancestors[0]`, so
    /// `sizeof(struct cgroup)` does NOT include any storage for
    /// ancestor pointers. The previous `sim_cgroup_alloc` implementation
    /// only `calloc(1, sizeof(struct cgroup))`'d, so every write to
    /// `cgrp->ancestors[i]` scribbled past the end of the allocation,
    /// corrupting whatever heap chunk happened to be adjacent. The
    /// corruption surfaced later as `malloc(): corrupted top size`,
    /// `double free or corruption`, or SIGSEGV in unrelated code paths.
    ///
    /// Discovered by stress-harness fuzzing 2026-05-08 — random rt-app
    /// specs that nested taskgroups beyond the root crashed ~53% of
    /// the time once Diff 2 started synthesizing implicit cgroup
    /// hierarchies (`/grand/parent/leaf`). Fixed by allocating
    /// `sizeof(struct cgroup) + CGROUP_ANCESTOR_MAX * sizeof(struct cgroup *)`.
    ///
    /// The test builds a deep chain (well past the previous level >= 2
    /// danger zone), interleaves siblings and per-cgroup cpuset writes
    /// to maximise the chance of adjacent-chunk corruption, then drops
    /// everything. With the bug present this aborts under glibc's malloc
    /// integrity checks; with the fix it completes cleanly.
    #[test]
    fn test_deeply_nested_hierarchy_no_heap_corruption() {
        let _lock = SIM_LOCK.lock().unwrap();
        let root = SimCgroupHandle::root();

        // Build a 16-level deep chain. Each level also gets two siblings
        // and a non-trivial cpuset, so malloc churn maximises the chance
        // that ancestors[] writes would have stomped on the next chunk.
        let mut chain: Vec<SimCgroupHandle> = Vec::new();
        let mut parent = root;
        for level in 1u32..=16 {
            let main = SimCgroupHandle::new(1000 + level as u64, level, parent);
            let cpus: Vec<CpuId> = (0..(level % 8 + 1)).map(CpuId).collect();
            main.set_cpuset(&cpus);

            // Two siblings at this level so the level write is exercised
            // repeatedly with the same parent.
            let sib_a = SimCgroupHandle::new(2000 + level as u64, level, parent);
            sib_a.set_cpuset(&[CpuId(0)]);
            let sib_b = SimCgroupHandle::new(3000 + level as u64, level, parent);
            sib_b.set_cpuset(&[CpuId(0), CpuId(1)]);
            chain.push(sib_a);
            chain.push(sib_b);

            parent = main.as_ptr();
            chain.push(main);
        }

        // Drop in reverse insertion order to give the allocator another
        // chance to detect any latent corruption.
        while chain.pop().is_some() {}
    }

    #[test]
    fn test_css_iter_guard_prepare() {
        let _lock = SIM_LOCK.lock().unwrap();
        let root = SimCgroupHandle::root();
        let h1 = SimCgroupHandle::new(30, 1, root);
        let h2 = SimCgroupHandle::new(31, 1, root);
        let descendants_pre = [root, h1.as_ptr(), h2.as_ptr()];
        // Post-order is just the reverse for this flat root + 2 children
        // shape (children before parent).
        let descendants_post = [h1.as_ptr(), h2.as_ptr(), root];
        let _guard = CssIterGuard::prepare(root, &descendants_pre, &descendants_post);
        // Guard created successfully — both iterators are populated.
    }

    #[test]
    fn test_css_iter_guard_prepare_single() {
        let _lock = SIM_LOCK.lock().unwrap();
        let root = SimCgroupHandle::root();
        let _guard = CssIterGuard::prepare_single(root);
    }
}
