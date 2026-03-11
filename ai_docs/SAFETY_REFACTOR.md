# Safety Refactor: SimState + Arc<Mutex<>> + CallbackContext

**Issue**: sim-safety-refactor
**Status**: Complete

## Architecture

All shared simulator state (SimulatorState, tasks, events, cgroup_registry)
is bundled into a single `SimState` struct and wrapped in `Arc<Mutex<SimState>>`
(type alias `SimArc`).

### Dual-path `with_sim`

Kfuncs access `SimulatorState` through `with_sim()`, which has two paths:

1. **SIM_STATE (raw pointer)** — checked first.
   Used when the engine holds the MutexGuard and C code calls kfuncs.
   `enter_sim()` installs the pointer from `&mut s.sim` before C calls.
   No lock contention — the engine already has exclusive access via the guard.

2. **SIM_ARC (Arc<Mutex<SimState>>)** — fallback.
   Used when the MutexGuard has been dropped (via `sim_callback!` macro).
   Kfuncs lock the mutex to access state. The guard is released before
   yielding so other workers can interleave.

Both paths are needed because Rust's `MutexGuard` cannot be "temporarily
released" from a borrow. Handler methods receive `&mut SimState` (a deref
of the guard), not the guard itself, so they cannot drop the lock.
The raw pointer path allows kfuncs to access state without locking.

### Where each path is used

| Context | Path | Why |
|---------|------|-----|
| Handler C calls (`enter_sim`/`exit_sim`) | SIM_STATE | Handler has `&mut SimState`, engine holds guard |
| Init/shutdown C calls (`sim_callback!`) | SIM_ARC | `run_internal` drops guard before C call |
| Concurrent dispatch workers | SIM_STATE | Worker calls `enter_sim` with raw pointer |
| Unit tests | SIM_STATE | Tests use `enter_sim` with bare `SimulatorState` |
| Cgroup callbacks (when guard held) | SIM_STATE | Recovers SimState from raw pointer cast |
| Cgroup callbacks (in `sim_callback!`) | SIM_ARC | Locks mutex to access cgroup_registry |

### `sim_callback!` macro

Drops the MutexGuard, installs SIM_ARC for kfuncs, calls C code, clears
SIM_ARC, relocks. Used in `run_internal` for init/shutdown C calls where
the engine owns the guard directly.

```rust
sim_callback!(guard, sim_arc, cpu, {
    self.scheduler.init();
});
```

### Safety analysis

**`SIM_STATE` raw pointer**: Sound because:
- Engine holds `MutexGuard<SimState>` -> exclusive access
- `enter_sim` creates raw ptr from `&mut s.sim` -> valid while guard held
- `exit_sim` clears it -> no dangling pointer
- Token-passing in concurrent dispatch -> serialized access

**`SIM_ARC` mutex lock**: Sound because:
- `sim_callback!` drops guard before installing SIM_ARC
- Kfuncs lock the mutex -> exclusive access
- Guard released before yield -> no deadlock with other workers

**`unsafe impl Send/Sync for SimState`**: Required because SimState
contains raw pointers (`idle_task_raw`, cgroup raw ptrs). These are
only accessed while the mutex is held.

## What was eliminated

| Before | After | Eliminated |
|--------|-------|-----------|
| `CGROUP_REGISTRY: AtomicPtr` | SimState bundle + SIM_STATE/SIM_ARC dual path | Global AtomicPtr |
| 4x `SendPtr` (state/tasks/events/cgroup) | 1x `SendPtr<SimulatorState>` + SimState bundle | 3 SendPtrs |
| Raw ptr deref in signal handler | `CallbackContext` (Cell<Copy>) | 5 unsafe deref sites |
| Raw ptr deref in yield paths | `CallbackContext` | 3 unsafe deref sites |
| Separate state variables | `SimState` struct | Fragmented state |

## Remaining `unsafe`

### Inherent (FFI boundary)
- C scheduler callbacks (`self.scheduler.xxx()`) via `enter_sim` or `sim_callback!`
- C helper functions (`ffi::sim_task_*`, `ffi::scx_test_*`)
- `unsafe fn` declarations in ffi.rs
- libc syscalls (sigaction, ioctl, futex)
- `unsafe impl Send/Sync` for FFI types

### Architectural (could be reduced with more work)
- `SIM_STATE` raw pointer in `with_sim` and `enter_sim`/`exit_sim`
- `SendPtr<SimulatorState>` in concurrent dispatch workers
- `sp as *mut SimState` cast in `batch_worker_body`
- `unsafe {}` blocks wrapping scheduler C calls in handler methods

### Future improvements
- Safe `Scheduler` trait wrapper (move `unsafe` into trait impl)
- Safe ffi.rs wrappers (expose safe API, keep `unsafe` private)
- Convert handler C calls from `enter_sim` to `sim_callback!` where
  handler refactoring allows (requires passing SimArc through call chain)
