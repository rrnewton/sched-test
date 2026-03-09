# Safety Refactor: Arc<Mutex<SimState>> + Fold All Shared State

**Issue**: sim-safety-refactor

## Goal

Move all shared simulator state behind a single `Arc<Mutex<SimState>>`.
The engine releases the lock before calling into C scheduler code;
kfuncs and cgroup callbacks reacquire it through a thread-local Arc.
This eliminates `SendPtr`, the raw pointer thread-local, and the
`CGROUP_REGISTRY` AtomicPtr.

## Phases

### Phase 1a: Infrastructure types ✅
- `SimState` struct bundling SimulatorState + tasks + events + cgroup_registry
- `SimArc = Arc<Mutex<SimState>>` type alias
- `CallbackContext` Copy struct for signal-safe context save/restore
- `SIM_ARC` and `CALLBACK_CTX` thread-locals with accessors
- `EventQueue`, `Event`, `EventKind` made `pub(crate)`

### Phase 1b: ENGINE_SIM_ARC bridge ✅
- `ENGINE_SIM_ARC` thread-local + set/clear/get functions
- `enter_sim` installs `SIM_ARC` + `CALLBACK_CTX`
- `exit_sim` syncs `CALLBACK_CTX` back and clears both

### Phase 3a: CallbackContext conversion ✅
- `interleave::maybe_yield` uses get/install_callback_ctx
- `preempt::cooperative_yield_impl` same
- `preempt::preempt_handler` (signal handler) same — fully async-signal-safe
- `preempt::replay_bp_handler` same
- `preempt::e9_yield_call` same
- **Eliminates all unsafe raw pointer dereference sites in yield paths**

### Phase 2a: SimState bundling in run_internal ✅
- `run_internal` bundles state+tasks+events+cgroup_registry into SimState
- All access in run_internal body through s.sim, s.tasks, s.events, s.cgroup_registry

### Phase 2b: Handler signature conversion ✅
- 31 handler methods converted from 4 separate params to `s: &mut SimState`
- `batch_worker_body` reconstructs SimState from raw pointers via ManuallyDrop
- `advance_to_run_phase` takes separate sim/events params (borrow split)
- Concurrent dispatch worker methods retain SendPtr-based signatures

### Phase 2c: Concurrent dispatch SendPtr consolidation ⬜
- Replace 4 separate SendPtr (state/tasks/events/cgroup) with SendPtr<SimState>
- Update dispatch_concurrent_cooperative, dispatch_concurrent_preemptive
- Update process_batch_concurrent_cooperative, process_batch_concurrent_preemptive
- Update dispatch_native_concurrent and backend module

### Phase 3b: Eliminate CGROUP_REGISTRY AtomicPtr ⬜
- Convert 7 cgroup extern C functions to use clone_sim_arc()
- Remove CGROUP_REGISTRY static, install_cgroup_registry, clear_cgroup_registry

### Phase 3c: Wrap SimState in Arc<Mutex<>> ⬜
- Engine creates SimArc, locks for work, drops lock before C calls
- sim_callback! macro for lock-release-call-reacquire pattern
- Workers receive Arc::clone instead of SendPtr

### Phase 4: Cleanup ⬜
- Remove SendPtr struct (or reduce to scheduler pointer only)
- Remove enter_sim/exit_sim/sim_state_ptr (replace with SimArc path)
- Convert with_sim from SIM_STATE raw pointer to SIM_ARC
- Update lib.rs re-exports
- Audit remaining unsafe
