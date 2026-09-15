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
pub mod cgroup_bw_replenish;
pub mod cgroup_ffi;
pub mod cgroup_wrapper;
pub mod dispatch_pool;
// Engine-mediated orchestration, dormant in the sequential engine: the
// EngineRing type is retained (backend imports it), its methods drive the future
// parallel path.
#[allow(dead_code)]
pub mod engine_ring;
pub mod ffi;
// Dormant parallel/race-interleaving path: re-homed (not deleted) in the
// sequential engine; retained for the future parallel backend. The token-ring
// wiring fns are unreachable from the sequential event loop (WorkerId stays live).
#[allow(dead_code)]
pub mod interleave;
pub mod kfuncs;
pub mod preempt;
// Standalone-only debug-inspection probes (LavdMonitor/LavdProbes). Gated so a
// `default-features = false` embed build does not compile this module (it has no
// embed consumer); the embed inspection path is the generic accessor surface,
// not these lavd-specific probes.
#[cfg(feature = "standalone")]
pub mod layered_probes;
#[cfg(feature = "standalone")]
pub mod probes;
pub mod scheduler_wrapper;
pub mod sim_task;
// Foundational safe `task_struct` accessor wrapper (SimTaskHandle). The full
// get/set accessor API is built but not yet wired into the engine, which still
// reads/writes task fields via raw SimTask/ffi; only `new_idle` and `as_raw`
// are currently consumed. Retained as the safe-boundary surface, not dead.
#[allow(dead_code)]
pub mod task_wrapper;
// Dormant persistent-thread-pool: re-homed (not deleted) in the sequential
// engine; retained for the future parallel backend. The whole module is
// unreachable from the sequential event loop.
#[allow(dead_code)]
pub mod worker_pool;

// In-crate relocation of the former tests/pending_dispatch.rs: an end-to-end
// engine test whose scheduler calls the in-crate-default kfunc
// scx_bpf_dsq_insert, so it must reach `crate::kfuncs`.
#[cfg(test)]
mod pending_dispatch_test;
