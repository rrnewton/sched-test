---
id: sim-safety-refactor
title: "Safety refactor: Arc<Mutex<SimState>> + fold all shared state"
status: in_progress
created: 2026-03-03
---

# Safety Refactor: Arc<Mutex<SimState>> + Fold All Shared State

## Completed (4 commits)

### Phase 1a: Infrastructure types (commit 4bdfe6f9)
- `SimState` struct bundling SimulatorState + tasks + events + cgroup_registry
- `SimArc = Arc<Mutex<SimState>>` type alias
- `CallbackContext` Copy struct for signal-safe context save/restore
- `SIM_ARC` and `CALLBACK_CTX` thread-locals with accessors
- `EventQueue`, `Event`, `EventKind` made `pub(crate)`

### Phase 1b: ENGINE_SIM_ARC bridge (commit fac3c3cf)
- `ENGINE_SIM_ARC` thread-local + set/clear/get functions
- `enter_sim` installs `SIM_ARC` + `CALLBACK_CTX`
- `exit_sim` syncs `CALLBACK_CTX` back and clears both

### Phase 3a: CallbackContext conversion (commit d67eabc5)
- `interleave::maybe_yield` uses get/install_callback_ctx
- `preempt::maybe_yield_preemptive` same
- `preempt::preempt_handler` (signal handler) same — fully async-signal-safe
- **Eliminates 3 unsafe raw pointer dereference sites**

### Phase 2a: SimState bundling in run_internal (commit a5c864b5)
- `run_internal` bundles state+tasks+events+cgroup_registry into SimState
- All access in run_internal body through s.sim, s.tasks, s.events, s.cgroup_registry

## In Progress (WIP commit — does not compile)

### Phase 2b: Handler signature conversion
- 25 handler methods converted from 4 separate params to `s: &mut SimState`
- ~7 compile errors remain:
  - concurrent dispatch workers still pass old-style args to process_event
  - one double mutable borrow issue
  - cgroup_registry immutable ref edge case

## Remaining Work

### Phase 2c: Fix remaining compile errors
- Fix 3 process_event call sites in concurrent dispatch worker closures
- Fix double mutable borrow in dispatch_concurrent
- Fix cgroup_registry immutable reference patterns

### Phase 2d: Concurrent dispatch SendPtr consolidation
- Replace 4 separate SendPtr (state/tasks/events/cgroup) with SendPtr<SimState>
- Update dispatch_concurrent_cooperative, dispatch_concurrent_preemptive
- Update process_batch_concurrent_cooperative, process_batch_concurrent_preemptive

### Phase 3b: Eliminate CGROUP_REGISTRY AtomicPtr
- Convert 7 cgroup extern C functions to use clone_sim_arc()
- Remove CGROUP_REGISTRY static, install_cgroup_registry, clear_cgroup_registry

### Phase 3c: Wrap SimState in Arc<Mutex<>>
- Engine creates SimArc, locks for work, drops lock before C calls
- sim_callback! macro for lock-release-call-reacquire pattern
- Workers receive Arc::clone instead of SendPtr

### Phase 4: Cleanup
- Remove SendPtr struct (or reduce to scheduler pointer only)
- Remove enter_sim/exit_sim/sim_state_ptr (replace with SimArc path)
- Convert with_sim from SIM_STATE raw pointer to SIM_ARC
- Update lib.rs re-exports
- Audit remaining unsafe
