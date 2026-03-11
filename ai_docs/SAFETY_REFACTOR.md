# Safety Refactor: Arc<Mutex<SimState>> + Fold All Shared State

**Issue**: sim-safety-refactor
**Status**: Complete

## Summary

The simulator previously used `unsafe` raw pointer patterns to share
`SimulatorState`, `CgroupRegistry`, `HashMap<Pid, SimTask>`, and `EventQueue`
across threads and between the engine and kfuncs. This refactor:

1. **Bundles all 4 components** into a single `SimState` struct
2. **Wraps SimState in `Arc<Mutex<>>`** — the engine drops the lock before
   C scheduler calls via `sim_callback!` macro; kfuncs reacquire through
   the `SIM_ARC` thread-local
3. **Eliminates raw pointer context save/restore** in yield paths using
   `CallbackContext` (Cell<Copy>, async-signal-safe)
4. **Eliminates `CGROUP_REGISTRY: AtomicPtr`** — cgroup callbacks access
   the registry through the SimState bundle
5. **Eliminates 3 of 4 `SendPtr`** instances for shared state in
   concurrent dispatch
6. **Converts 31 handler methods** to `s: &mut SimState`

## Architecture

### Two access paths (dual-path `with_sim`)

Kfuncs use `with_sim()` which checks two thread-locals:

1. **SIM_STATE (raw pointer)** — checked first. Set by `enter_sim()` when the
   engine holds the MutexGuard. Handler methods use this path because they
   receive `&mut SimState` (a deref of the guard) and can't drop the lock.

2. **SIM_ARC (Arc<Mutex<SimState>>)** — fallback. Set by `sim_callback!` which
   drops the guard before the C call. Used during the init/shutdown phase
   C calls in `run_internal`.

Both paths are sound: SIM_STATE is valid because the engine holds exclusive
access via the MutexGuard; SIM_ARC works because the guard is dropped first.

### `sim_callback!` macro

```rust
sim_callback!(guard, sim_arc, cpu, {
    self.scheduler.init();
});
```

Expands to: set current_cpu + CALLBACK_CTX -> drop guard -> install SIM_ARC ->
unsafe C call -> clear SIM_ARC -> relock -> sync CALLBACK_CTX back.

### Remaining `unsafe`

- **SIM_STATE raw pointer** in `with_sim` fallback path — sound because
  engine holds MutexGuard -> exclusive access guaranteed
- **`enter_sim`/`exit_sim`** — creates raw pointer from `&mut SimulatorState`
- **SendPtr<SimulatorState>** — concurrent dispatch workers; serialized by token-passing
- **SendPtr<Simulator<S>> / SendPtr<S>** — scheduler pointers for FFI
- **`sp as *mut SimState`** cast in `batch_worker_body` — sim is first field
- **FFI calls** to C scheduler code — inherent
- **Signal handler** — inherent
- **CgroupInfo raw C pointer** — inherent to C interop
- **`unsafe impl Send/Sync for SimState`** — raw pointers behind Mutex

### Future work

The remaining `enter_sim`/`exit_sim` sites (31 in handler methods) can be
incrementally converted to `sim_callback!` by changing handler signatures to
take `&SimArc` and managing their own locking. This would eliminate the
SIM_STATE raw pointer path entirely, leaving only the Arc path.
