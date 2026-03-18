//! FFI export functions for cgroup operations (called from C).
//!
//! These `#[no_mangle] extern "C"` functions are entry points invoked by
//! the C scheduler code. They access the global [`CgroupRegistry`] through
//! the simulator state mutex and delegate to the safe registry API.
//!
//! Separated from the core cgroup module so that `cgroup.rs` can live in
//! the `safe/` directory (zero `unsafe`).

use std::ffi::c_void;
use std::ptr;

use crate::cgroup::{CgroupId, CgroupRegistry};
use crate::cgroup_wrapper::SimCgroupHandle;

// ---------------------------------------------------------------------------
// Global registry access helpers
// ---------------------------------------------------------------------------

/// Access the cgroup registry through the simulator state.
///
/// Uses `try_lock()` to avoid deadlocking when called from within
/// `with_sim()` (which already holds the mutex). Returns `None` if
/// `SIM_ARC` is not installed or if the lock is already held.
fn with_cgroup_registry<R>(f: impl FnOnce(&CgroupRegistry) -> R) -> Option<R> {
    let arc = crate::kfuncs::clone_sim_arc()?;
    let guard = arc.try_lock().ok()?;
    Some(f(&guard.cgroup_registry))
}

/// Mutable version of with_cgroup_registry.
///
/// Uses `try_lock()` to avoid re-entrant deadlock (same rationale as
/// [`with_cgroup_registry`]).
fn with_cgroup_registry_mut<R>(f: impl FnOnce(&mut CgroupRegistry) -> R) -> Option<R> {
    let arc = crate::kfuncs::clone_sim_arc()?;
    let mut guard = arc.try_lock().ok()?;
    Some(f(&mut guard.cgroup_registry))
}

// ---------------------------------------------------------------------------
// Cgroup lookup FFI exports
// ---------------------------------------------------------------------------

/// Look up a cgroup by ID (called from C).
///
/// Returns the raw cgroup pointer, or the root cgroup if not found.
#[no_mangle]
pub extern "C" fn sim_cgroup_lookup_by_id(cgid: u64) -> *mut c_void {
    with_cgroup_registry(|registry| {
        registry
            .get(CgroupId(cgid))
            .map(|info| info.raw())
            .unwrap_or_else(ptr::null_mut)
    })
    .unwrap_or_else(|| SimCgroupHandle::root().as_raw())
}

/// Look up a cgroup's ancestor at a given level (called from C).
///
/// Returns the ancestor's raw cgroup pointer, or null if invalid.
#[no_mangle]
pub extern "C" fn sim_cgroup_lookup_ancestor(cgrp: *mut c_void, level: u32) -> *mut c_void {
    if cgrp.is_null() {
        if level == 0 {
            return SimCgroupHandle::root().as_raw();
        }
        return ptr::null_mut();
    }
    with_cgroup_registry(|registry| {
        registry
            .find_cgid_by_raw(cgrp)
            .and_then(|id| registry.ancestor(id, level).map(|info| info.raw()))
            .unwrap_or_else(ptr::null_mut)
    })
    .unwrap_or_else(ptr::null_mut)
}

// ---------------------------------------------------------------------------
// BPF map entry allocation FFI exports
// ---------------------------------------------------------------------------

/// Try to allocate a BPF map entry for a cgroup (called from C).
///
/// Returns 0 on success, -12 (ENOMEM) if the maximum cgroup limit has been
/// reached. This simulates BPF hash map insertion failures.
#[no_mangle]
pub extern "C" fn sim_cgroup_registry_allocate() -> i32 {
    with_cgroup_registry_mut(|registry| match registry.try_allocate_bpf_entry() {
        Ok(()) => 0,
        Err(e) => e,
    })
    .unwrap_or(0)
}

/// Free a BPF map entry for a cgroup (called from C).
///
/// Decrements the allocated entry count. Safe to call even if no entry
/// was allocated.
#[no_mangle]
pub extern "C" fn sim_cgroup_registry_free() {
    with_cgroup_registry_mut(|registry| {
        registry.free_bpf_entry();
    });
}

/// Get the current number of allocated BPF entries (called from C).
///
/// Returns 0 if no registry is installed.
#[no_mangle]
pub extern "C" fn sim_cgroup_registry_allocated_count() -> u32 {
    with_cgroup_registry(|registry| registry.allocated_bpf_entries()).unwrap_or(0)
}

/// Get the maximum cgroup limit (called from C).
///
/// Returns 0 if no registry is installed.
#[no_mangle]
pub extern "C" fn sim_cgroup_registry_max() -> u32 {
    with_cgroup_registry(|registry| registry.max_cgroups()).unwrap_or(0)
}

/// Set the maximum cgroup limit (called from C).
///
/// This allows schedulers to dynamically configure the cgroup limit.
/// No-op if no registry is installed.
#[no_mangle]
pub extern "C" fn sim_cgroup_registry_set_max(max: u32) {
    with_cgroup_registry_mut(|registry| {
        registry.set_max_cgroups(max);
    });
}
