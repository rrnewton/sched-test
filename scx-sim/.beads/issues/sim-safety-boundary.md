---
title: 'Safety boundary refactor: safe/ + unsafe_impl/ module split'
status: open
priority: 1
issue_type: epic
labels:
- safety
- refactor
created_at: 2026-03-13T10:42:00.000000000+00:00
updated_at: 2026-03-13T10:42:00.000000000+00:00
---

# Description

Establish a clear architectural boundary between safe simulation logic and
unsafe FFI/hardware code in the scx_simulator crate.

## Completed safety work

### Phase 1: Arc<Mutex<SimState>> (sim-safety-refactor, closed)

- Bundled SimulatorState, tasks, events, cgroup_registry into `SimState`
- Wrapped in `Arc<Mutex<SimState>>` (type alias `SimArc`)
- `sim_callback!` macro: drops MutexGuard → installs SIM_ARC → calls C →
  clears SIM_ARC → relocks
- `CallbackContext` (Cell<Copy>) for signal-safe context in yield/preempt
- Converted all ~40 handler methods from raw pointer params to `&SimArc`
- Eliminated `CGROUP_REGISTRY: AtomicPtr`, 3 of 4 `SendPtr` instances
- `with_sim()` dual-path: SIM_ARC for production, SIM_STATE for unit tests

### Phase 2: Integration test fixes (0c81ae3 on safety-refactor-v2)

- Fixed mutex self-deadlock in `handle_slice_expired` (held guard across
  `stop_and_reenqueue` which locks internally)
- Fixed event loop exit self-deadlock (RHS of reassignment evaluated before
  LHS drop)
- Added missing `resolve_pending_dispatch` at 5 enqueue call sites
- Converted remaining `enter_sim`/`exit_sim` blocks in handlers to
  `sim_callback!`
- Result: 479+ tests pass (all except 2 PMU-hardware-dependent)

### Remaining unsafe inventory (~370 occurrences across 12 modules)

| Module | Count | Category |
|--------|-------|----------|
| ffi.rs | 109 | FFI declarations, libloading, transmute |
| kfuncs.rs | 101 | extern "C" kfunc stubs, SIM_STATE raw ptr |
| preempt/mod.rs | 50 | sigaction, futex, atomics, raw ptrs |
| engine.rs | 43 | Scheduler trait calls, sim_callback! |
| probes.rs | 21 | Symbol resolution, raw probe accessors |
| cgroup.rs | 14 | C struct alloc/free, unsafe Send/Sync |
| backend/mod.rs | 12 | SendPtr, raw ptr deref in workers |
| backend/e9patch.rs | 10 | Symbol resolution, transmute |
| task.rs | 5 | C task alloc/free/accessors |
| interleave.rs | 2 | Thread-local raw ptr |
| backend/replay.rs | 2 | PMU timer setup |
| backend/pmu.rs | 1 | Signal handler install |

## Goal

Split the crate into `src/safe/` (pure safe Rust, zero `unsafe`) and
`src/unsafe_impl/` (all unsafe code behind defensively safe interfaces).

### Design principles

1. **`unsafe_impl/` exports only safe fn signatures.** Callers never need
   `unsafe` to use these APIs.

2. **Defensive interfaces, not C re-exports.** Wrappers must validate inputs,
   own lifetimes (RAII), prevent double-free, prevent use-after-free.

3. **`safe/` never uses the `unsafe` keyword.**

4. **Each `unsafe_impl/` module has a `# Safety` doc section** documenting
   soundness arguments for each API entrypoint.

### Target structure

```
src/
├── safe/                    # Pure safe Rust — simulation logic
│   ├── mod.rs
│   ├── engine.rs            # Event loop, handlers (no unsafe)
│   ├── dsq.rs, cpu.rs, trace.rs, scenario.rs, types.rs
│   ├── task.rs, cgroup.rs   # Safe wrappers over unsafe_impl FFI
│   ├── det_hashmap.rs, stats.rs, workloads.rs, bpf_trace.rs
│   ├── fmt.rs, monitor.rs, perfetto.rs, rtapp.rs, perf.rs
│
├── unsafe_impl/             # All unsafe, safe public API
│   ├── mod.rs               # Safety documentation index
│   ├── scheduler.rs         # Safe Scheduler trait wrapper
│   ├── kfuncs.rs            # Kfunc stubs + with_sim + SimState/SimArc
│   ├── ffi.rs               # extern "C" declarations, DynamicScheduler
│   ├── task_ffi.rs          # SimTask C alloc/free/accessors (RAII)
│   ├── cgroup_ffi.rs        # CgroupRegistry C alloc/free (RAII)
│   ├── preempt/, backend/   # PMU, signals, futex, backends
│   ├── interleave.rs        # TokenRing thread-local raw ptrs
│   └── probes.rs            # LAVD symbol resolution
│
├── lib.rs
└── bin/scxsim/
```

## Subtasks

### T1: Move purely-safe modules into safe/
Move 14 already-safe modules, update lib.rs. Zero code changes inside modules.
- `types.rs`, `dsq.rs`, `cpu.rs`, `trace.rs`, `scenario.rs`, `bpf_trace.rs`,
  `det_hashmap.rs`, `fmt.rs`, `monitor.rs`, `perf.rs`, `perfetto.rs`,
  `rtapp.rs`, `stats.rs`, `workloads.rs`

### T2: Create safe scheduler trait wrapper (unsafe_impl/scheduler.rs)
Extract `self.scheduler.xxx()` calls into safe wrapper. Engine calls wrapper.
- Depends on: nothing. Unlocks: T6.

### T3: Create safe task FFI wrapper (unsafe_impl/task_ffi.rs)
RAII wrapper for `sim_task_alloc/free/get_*/set_*`. Drop frees, accessors safe.
- Depends on: nothing. Unlocks: T6.

### T4: Create safe cgroup FFI wrapper (unsafe_impl/cgroup_ffi.rs)
RAII wrapper for cgroup alloc/free + extern "C" callbacks.
- Depends on: nothing. Unlocks: T6.

### T5: Move preempt/, backend/, interleave.rs, probes.rs into unsafe_impl/
Add `# Safety` module docs. Ensure public API is safe fn.
- Depends on: nothing.

### T6: Move engine.rs into safe/
After T2/T3/T4, engine.rs has zero `unsafe`. Refactor `sim_callback!` to
export safe interface from `unsafe_impl/kfuncs.rs`.
- Depends on: T2, T3, T4.

### T7: Move kfuncs.rs + ffi.rs into unsafe_impl/
Add `# Safety` docs. Audit public fns for defensive checks.
- Depends on: nothing (coordinate with T6).

### T8: Eliminate SIM_STATE raw pointer path
Convert unit tests to SimArc. Remove `enter_sim`, `exit_sim`, `SIM_STATE`.
- Depends on: T7.

### T9: Final audit + documentation
Verify `safe/` has zero `unsafe`. Every `unsafe` block has `// SAFETY:`.
Close epic.
- Depends on: all.

### Parallelism

```
T1 ─────────────────────────────────────→
T2 (scheduler wrapper) ────────────────→ ╲
T3 (task FFI wrapper) ─────────────────→ ─→ T6 (engine safe) → T9
T4 (cgroup FFI wrapper) ───────────────→ ╱
T5 (move preempt/backend/interleave) ──→
T7 (move kfuncs/ffi) ─────────────────→ T8 (SIM_STATE removal) → T9
```
