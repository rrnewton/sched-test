# Safety Refactor: Fold All Shared State into SimState

**Issue**: sim-safety-refactor
**Status**: Complete

## Summary

The simulator previously used `unsafe` raw pointer patterns to share
`SimulatorState`, `CgroupRegistry`, `HashMap<Pid, SimTask>`, and `EventQueue`
across threads and between the engine and kfuncs. This refactor:

1. **Bundles all 4 components** into a single `SimState` struct
2. **Eliminates raw pointer context save/restore** in yield paths
   (signal handler, cooperative yield, preemptive yield) using
   `CallbackContext` (a `Cell<Copy>` thread-local)
3. **Eliminates `CGROUP_REGISTRY: AtomicPtr`** — cgroup callbacks
   access the registry through the `SimState` bundle
4. **Eliminates 3 of 4 `SendPtr`** instances for shared state in
   concurrent dispatch
5. **Converts 31 handler methods** from taking 4 separate params
   to `s: &mut SimState`

## Completed Phases

| Phase | Description | Commit |
|-------|-------------|--------|
| 1a | SimState, CallbackContext, thread-local infrastructure | `829db50` |
| 1b | ENGINE_SIM_ARC bridge (enter_sim/exit_sim install CALLBACK_CTX) | `ba63189` |
| 3a | CallbackContext conversion (interleave/preempt yield paths) | `13f1ccd` |
| 2a | SimState bundling in run_internal | `7cc3280` |
| 2b | Handler signature conversion (31 methods) | `41be970` |
| 2c | Concurrent dispatch SendPtr consolidation | `d439456` |
| 3b | Eliminate CGROUP_REGISTRY AtomicPtr | `8b0b980` |
| 4  | Cleanup: remove unused Arc/Mutex infrastructure | (this commit) |

## Remaining unsafe (inherent, not architectural)

The following `unsafe` remains and is inherent to the simulator's design:

- **FFI calls to C scheduler code** — `self.scheduler.XXX()` calls
- **FFI calls to C helper functions** — `ffi::sim_task_*`, `ffi::scx_test_*`
- **`SendPtr<SimulatorState>`** — for concurrent dispatch workers.
  Token-passing serializes access; the raw pointer crosses thread boundaries.
- **`SendPtr<Simulator<S>>` / `SendPtr<S>`** — scheduler pointers for FFI
- **`*const TokenRing` / `*const PreemptRing`** in thread-locals —
  ring pointer lifetime vs `'static` thread-local conflict
- **`SIM_STATE: Cell<*mut SimulatorState>`** — raw pointer thread-local
  for kfuncs. Token-passing serializes access.
- **`sp as *mut SimState`** cast in batch_worker_body — relies on `sim`
  being the first field of SimState
- **Signal handler** (`libc::sigaction`, `libc::ioctl`, `libc::syscall`)
- **`CgroupInfo`**'s raw C pointer — inherent to C interop

## Deferred: Arc<Mutex<>> wrapping (Phase 3c)

The original plan included wrapping SimState in `Arc<Mutex<SimState>>`
so the engine releases the lock before C calls and kfuncs reacquire it.
This was deferred because:

1. The engine holds `&mut SimState` for its entire run and must drop it
   before every C call — requiring restructuring the entire control flow
2. The safety benefits are marginal over the current state (token-passing
   already serializes access; the remaining raw pointers are documented)
3. The `std::sync::Mutex` deadlock-on-same-thread property (the original
   motivation) can be tested without the full wrapping
