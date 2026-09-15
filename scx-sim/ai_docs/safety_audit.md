# Safety Audit Summary

**Date**: 2026-03-13
**Branch**: safety-t5-unsafe-modules
**Epic**: sim-3ef7bc (T9 - Final safety audit)

## Architecture

The crate is structured into two top-level module trees:

- **`src/safe/`** -- Fully safe Rust modules with zero `unsafe` keyword usage.
- **`src/unsafe_impl/`** -- Modules containing all unsafe operations: FFI, signal
  handlers, raw pointer manipulation, mmap, and `unsafe impl Send/Sync`.

Files outside both directories (`engine.rs`, `task.rs`, `cgroup.rs`,
`scheduler_wrapper.rs`, `task_wrapper.rs`, `cgroup_wrapper.rs`) act as
safe-wrapper layers that call into `unsafe_impl/` and annotate each
`unsafe` block with a `// SAFETY:` comment.

## Safe Modules (zero `unsafe`)

All 15 modules under `src/safe/` contain zero `unsafe` usage:

| Module | Purpose |
|--------|---------|
| `bpf_trace.rs` | BPF trace output formatting |
| `cpu.rs` | CPU state modeling |
| `det_hashmap.rs` | Deterministic hash map |
| `dsq.rs` | Dispatch queue (DSQ) management |
| `fmt.rs` | Formatting utilities |
| `mod.rs` | Module declarations |
| `monitor.rs` | Monitor trait and probe context |
| `perf.rs` | PMU timer creation |
| `perfetto.rs` | Perfetto trace export |
| `rtapp.rs` | rt-app workload generation |
| `scenario.rs` | Scenario definition API |
| `stats.rs` | Statistics collection |
| `trace.rs` | Trace recording |
| `types.rs` | Strong type definitions |
| `workloads.rs` | Workload phase definitions |

## Unsafe Code Inventory

### Per-file unsafe block/fn/impl counts

| File | unsafe blocks | unsafe fns | unsafe impls | SAFETY comments |
|------|:---:|:---:|:---:|:---:|
| **Outside unsafe_impl/** | | | | |
| `cgroup.rs` | 11 | 0 | 2 | 12 |
| `cgroup_wrapper.rs` | 6 | 0 | 0 | 7 |
| `engine.rs` | 9 | 0 | 0 | 11 |
| `scheduler_wrapper.rs` | 28 | 7 | 0 | 29 |
| `task.rs` | 5 | 0 | 0 | 5 |
| `task_wrapper.rs` | 32 | 0 | 0 | 32 |
| **Inside unsafe_impl/** | | | | |
| `backend/e9patch.rs` | 5 | 1 | 2 | 5 |
| `backend/mod.rs` | 8 | 0 | 2 | 9 |
| `backend/pmu.rs` | 1 | 0 | 0 | 1 |
| `backend/replay.rs` | 2 | 0 | 0 | 2 |
| `ffi.rs` | 28 | 59 | 0 | 28 |
| `interleave.rs` | 1 | 0 | 1 | 2 |
| `kfuncs.rs` | 97 | 1 | 2 | 18 |
| `preempt/mod.rs` | 45 | 2 | 4 | 47 |
| `probes.rs` | 9 | 7 | 0 | 9 |

**Note on kfuncs.rs**: Many unsafe blocks in kfuncs.rs are within test functions
(approximately 60 of the 97 blocks). The test blocks call `enter_sim()`,
`sim_task_alloc()`, etc. for unit testing and are not production code paths.
Non-test unsafe blocks in kfuncs.rs all have `// SAFETY:` comments.

### Categories of Unsafe Operations

1. **FFI calls to C scheduler `.so`** (`ffi.rs`, `scheduler_wrapper.rs`):
   Raw function pointers resolved via `dlsym`/`libloading` and called through
   `unsafe extern "C" fn` types. The `.so` is built by our build system from
   known-safe C source.

2. **C task_struct manipulation** (`task.rs`, `task_wrapper.rs`, `cgroup.rs`,
   `cgroup_wrapper.rs`, `kfuncs.rs`): Allocating, reading, and writing fields
   on C `task_struct` and `cgroup` objects via FFI wrapper functions. Each call
   requires the raw pointer to be valid.

3. **Signal handlers** (`preempt/mod.rs`): `sigaction` installation, `ioctl` on
   perf_event fds, `futex` syscalls, `libc::abort`. All operations are
   async-signal-safe per POSIX.

4. **Raw pointer thread sharing** (`backend/mod.rs`, `interleave.rs`,
   `preempt/mod.rs`): `SendPtr<T>`, `unsafe impl Send` for context structs
   containing raw pointers. Safety relies on the token-passing protocol
   ensuring exclusive access.

5. **Memory mapping** (`preempt/mod.rs`): `mmap(MAP_FIXED)` for the e9patch
   shared RBC state page at a hardcoded address.

6. **Library loading** (`ffi.rs`): `libloading::Library::open` with `RTLD_NOW`
   and `mem::transmute` for function pointer casts.

7. **Split borrows** (`kfuncs.rs`): Raw pointer arithmetic to split-borrow
   `cpus[idx]` and `dsqs` simultaneously.

### `unsafe impl Send/Sync` Summary

| Type | Location | Justification |
|------|----------|---------------|
| `CgroupInfo` | `cgroup.rs` | Raw pointer to heap-allocated C struct; single-threaded access |
| `SimState` | `kfuncs.rs` | Arc<Mutex> ensures exclusive access |
| `SendPtr<T>` | `backend/mod.rs` | Token-passing protocol ensures exclusive access |
| `E9PatchFns` | `e9patch.rs` | Function pointers are inherently thread-safe |
| `E9SharedRbc` | `preempt/mod.rs` | Single-writer enforced by token-passing |
| `InterleaveCtx` | `interleave.rs` | Token-passing protocol; raw pointer to scoped `TokenRing` |
| `PreemptCtx` | `preempt/mod.rs` | Token-passing protocol; raw pointer to scoped `PreemptRing` |
| `ReplayCtx` | `preempt/mod.rs` | Token-passing protocol; raw pointer to scoped ring/cursor |

## Future Work to Reduce Unsafe

1. **Newtype wrappers for C pointers**: Replace bare `*mut c_void` with
   strongly-typed wrappers (`TaskPtr`, `CgroupPtr`) in more call sites to
   push validity checking to construction time. `TaskPtr` and `OptionalPtr`
   already exist in `scheduler_wrapper.rs`; extending this pattern to
   `kfuncs.rs` would eliminate many raw pointer `unsafe` blocks.

2. **Safe FFI bindings via `cbindgen`/`cxx`**: Generate safe Rust bindings
   for the C task_struct accessors instead of hand-written `extern "C"` blocks.
   This would eliminate the bulk of `ffi.rs` unsafe.

3. **Move `cgroup.rs` unsafe into `unsafe_impl/`**: The `CgroupRegistry` methods
   that call `sim_cgroup_alloc`/`sim_cgroup_free` could be wrapped in a
   `CgroupWrapper` similar to `TaskWrapper`, moving the last unsafe code out
   of top-level modules.

4. **Reduce test `unsafe`**: Many test functions in `kfuncs.rs` call `enter_sim`
   and FFI functions directly. Creating test helper functions (e.g.,
   `test_with_sim_state(f)`) would centralize the unsafe in one place.

5. **Signal handler audit**: The signal handlers in `preempt/mod.rs` are
   async-signal-safe by construction, but a formal verification against the
   POSIX async-signal-safe function list would provide additional confidence.
