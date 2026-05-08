//! Unsafe-heavy modules grouped by safety boundary.
//!
//! # Safety
//!
//! This module consolidates the crate's modules that contain significant
//! `unsafe` code. Grouping them into a single subtree makes auditing,
//! `#[deny(unsafe_op_in_unsafe_fn)]` enforcement, and future `unsafe`
//! reduction work easier to scope.
//!
//! ## Contained modules
//!
//! | Module | Unsafe surface |
//! |-------------|------------------------------------------------------|
//! | `ffi` | `extern "C"` declarations, raw pointer manipulation, `dlopen`/`dlsym` calls |
//! | `cgroup_ffi`| `#[no_mangle] extern "C"` cgroup lookup/registry entry points called from C |
//! | `cgroup_bw_ffi` | `#[no_mangle] extern "C"` cgroup-bandwidth shims that redirect LAVD wrapper `scx_cgroup_bw_*` calls into the engine-owned `BandwidthManager` (Diff 4/5) |
//! | `kfuncs` | `#[no_mangle] extern "C"` kfunc entry points, thread-local raw-pointer state |
//! | `preempt` | Signal handlers, futex syscalls, atomics, inline assembly |
//! | `backend` | PMU perf_event ioctls, hardware breakpoints, `/proc` mmap, e9patch binary patching |
//! | `engine_ring`| Futex-based engine-mediated thread orchestrator (futex + atomics only) |
//! | `interleave`| `UnsafeCell`-based token ring, raw thread synchronization |
//! | `probes` | `dlsym` function-pointer resolution, raw C function calls |
//! | `cgroup_wrapper` | Raw-pointer `scx_cgroup` manipulation, C struct interop |
//! | `scheduler_wrapper` | Raw `sched_ext_ops` pointer management, `dlsym`-loaded callbacks |
//! | `sim_task` | `SimTask` runtime type: raw `task_struct` allocation, FFI field access, deallocation |
//! | `task_wrapper` | Raw `task_struct` pointer wrapping, unsafe field accessors |
//! | `worker_pool` | Persistent worker thread pool with futex-based park/wake protocol |

pub mod backend;
pub mod cgroup_bw_ffi;
pub mod cgroup_ffi;
pub mod cgroup_wrapper;
pub mod dispatch_pool;
pub mod engine_ring;
pub mod ffi;
pub mod interleave;
pub mod kfuncs;
pub mod preempt;
pub mod probes;
pub mod scheduler_wrapper;
pub mod sim_task;
pub mod task_wrapper;
pub mod worker_pool;
