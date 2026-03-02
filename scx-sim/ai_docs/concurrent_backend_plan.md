# Native Concurrency Backend: Implementation Plan

## Goal

Add a new "native concurrency" backend to scx-sim where worker threads (one
per simulated CPU) run truly concurrently with real lock-based
synchronization, rather than being serialized by a token-ring. This enables
external determinism/chaos tools (`hermit`, `rr`) for replay and
fuzz-testing of cross-CPU scheduler interactions.

---

## Research Summary

### 1. Current Backend / Trait Architecture

**Backend Trait**: `PreemptionBackend` in
`crates/scx_simulator/src/backend/mod.rs` (lines 100-170).

```rust
pub(crate) trait PreemptionBackend: Sync {
    type WorkerCtx: Send;
    fn global_setup(&self) {}
    fn global_teardown(&self) {}
    fn worker_setup(&self, ring: &PreemptRing, worker_id: WorkerId) -> Self::WorkerCtx;
    fn build_target(&self, ctx: &Self::WorkerCtx, ring: &PreemptRing) -> Option<PreemptTarget>;
    fn arm(&self, ctx: &mut Self::WorkerCtx, target: PreemptTarget);
    fn disarm(&self, ctx: &mut Self::WorkerCtx) -> StructopDelta;
    fn worker_teardown(&self, ctx: Self::WorkerCtx);
    fn log_completion(&self, ring: &PreemptRing);
    fn is_precise(&self) -> bool { false }
    fn read_count(&self, _ctx: &Self::WorkerCtx) -> u64 { 0 }
    fn reset_count(&self, _ctx: &mut Self::WorkerCtx) {}
}
```

**Existing Backends** (3 total):

| Backend | File | Purpose |
|---------|------|---------|
| `PmuBackend` | `backend/pmu.rs` | PMU timer preemption via `SIGSTKFLT` (nondeterministic due to skid) |
| `ReplayBackend` | `backend/replay.rs` | Exact replay via HW breakpoints from a recorded trace |
| `E9PatchBackend` | `backend/e9patch.rs` | Software RBC counting via e9-instrumented `.so` (fully deterministic) |

All three backends are tightly coupled to the `PreemptRing` (futex-based
token ring). The trait's `worker_setup` takes `&PreemptRing`, and
`build_target` uses `ring.roll_timeslice()`. The generic drivers
`run_preemptive_dispatch` and `run_preemptive_batch` (lines 188-348 of
`backend/mod.rs`) hardcode the `PreemptRing` lifecycle:
`ring.wait_for_token`, `ring.finish`, `ring.start`, `ring.wait_all_done`.

**Cooperative (non-preemptive) path**: Uses `TokenRing`
(`interleave.rs` lines 57-188), a Mutex/Condvar-based token ring. Called
from `dispatch_concurrent_cooperative` (engine.rs line 3046) and
`process_batch_concurrent_cooperative` (engine.rs line 3270).

**Key insight**: The codebase has TWO separate token-ring implementations:
- `TokenRing` (interleave.rs): Mutex/Condvar, used by cooperative mode
- `PreemptRing` (preempt/mod.rs lines 1126-1364): atomics/futex, used by
  preemptive backends

Both enforce serialized execution (only one thread runs at a time).

### 2. Token-Ring Synchronization

**PreemptRing** (preempt/mod.rs lines 1126-1364):
- Per-worker atomic state: `PARKED` / `RUNNING`
- PRNG-driven `pick_next()` selects the next worker (xorshift32 in
  `AtomicU32`)
- `yield_token()`: parks self, picks next, wakes next, waits for re-selection
- `finish()`: marks worker done, wakes next or signals `all_done`
- `start()` / `wait_all_done()`: orchestrator entry/exit

**TokenRing** (interleave.rs lines 57-188):
- Same semantics but Mutex/Condvar-based instead of futex

Both ensure only ONE worker touches `SimulatorState` at a time.

### 3. Worker Thread Lifecycle

Workers are spawned inside `std::thread::scope` blocks. The lifecycle is:

**Dispatch workers** (backend/mod.rs `run_preemptive_dispatch`, lines 188-258):
1. `backend.worker_setup(ring, worker_id)` -- install TLS
2. `ring.wait_for_token(worker_id)` -- block until selected
3. `kfuncs::enter_sim(state, cpu)` -- set TLS pointer to `SimulatorState`
4. `build_and_arm(backend, ctx, ring)` -- arm preemption
5. `dispatch_worker_body(sp, schp, cpu)` -- call `scheduler.dispatch()`
6. `backend.disarm(ctx)` -- disable instrumentation, get deltas
7. Drain structop accum, clear ops_context
8. `ring.finish(worker_id)` -- release token, wake next
9. `kfuncs::exit_sim_no_clear_ops()` -- clear TLS pointer
10. `backend.worker_teardown(ctx)` -- cleanup

**Batch workers** (backend/mod.rs `run_preemptive_batch`, lines 264-348):
Same as dispatch but step 5 calls `batch_worker_body()` which processes
multiple events for the CPU.

**Cooperative workers** (engine.rs `dispatch_concurrent_cooperative`,
lines 3046-3108): Similar but uses `interleave::install(ring, worker_id)`
for TLS and `interleave::maybe_yield()` for cooperative yields.

### 4. Interleave / Preemption System

**Cooperative yielding** (`maybe_yield` in interleave.rs lines 237-277):
- Called at the top of every kfunc (before `with_sim()`)
- Saves `current_cpu`, `ops_context`, `waker_task_raw` from `SimulatorState`
- Calls `ring.yield_token(worker_id)` to release token and block
- On resume, restores saved context to `SimulatorState`

**Preemptive yielding** (`maybe_yield_preemptive` in preempt/mod.rs
lines 1506-1508):
- Delegates to `cooperative_yield_impl(KfuncYieldPhase::Pre)`
- Uses `PreemptRing.yield_token()` (futex-based, signal-safe)
- Additionally handles structop boundary tracking and PMU timer management

**PMU signal handler** (in preempt/mod.rs): fires `SIGSTKFLT` on RBC
overflow, yields token from signal context using the same
`PreemptRing.yield_token()`.

**Post-kfunc yield** (`maybe_yield_preemptive_post` in preempt/mod.rs
line 1523): Additional yield point inside `with_sim()`, after the kfunc
body completes. Doubles interleaving coverage.

### 5. The Logical Clock / Timing Model

**Per-CPU local clocks**: `SimCpu.local_clock: TimeNs` (cpu.rs line 46).
Each CPU maintains an independent logical clock in nanoseconds.

**`advance_cpu_clock`** (kfuncs.rs lines 411-414):
```rust
pub fn advance_cpu_clock(&mut self, cpu: CpuId) {
    let idx = cpu.0 as usize;
    self.cpus[idx].local_clock = self.cpus[idx].local_clock.max(self.clock);
}
```
Sets `local_clock = max(local_clock, event_queue_time)`. The global
`state.clock` is the current event queue time.

**`charge_sched_time`** (engine.rs lines 448-500): After each scheduler
callback, reads the RBC counter and adds `rbc_ns + kfunc_ns` to
`cpu.local_clock`. This models scheduler overhead. Also has a fallback
(kfunc cost only) when PMU is unavailable.

**Where `local_clock` is updated** (engine.rs):
- `advance_cpu_clock()` -- sync with event time (line 1372)
- `charge_sched_time()` -- scheduler callback overhead (lines 465, 491)
- CSW overhead: `local_clock += overhead` (line 2503, 3497)
- Tick jitter: tick interval computed from `local_clock` (line 1538)
- Initial setup: temporary `local_clock = 1` for LAVD idle detection
  (lines 1014, 1021)

### 6. Thread-Local State

**`enter_sim` / `exit_sim`** (kfuncs.rs lines 682-734):
- `enter_sim(state, cpu)`: sets `state.current_cpu = cpu`, stores raw
  pointer to `SimulatorState` in thread-local `SIM_STATE`, updates
  `SIM_CONTEXT` for logging
- `exit_sim()`: pauses PMU timer, clears ops_context on `SimulatorState`,
  clears TLS pointer
- `exit_sim_no_clear_ops()`: variant for concurrent paths where
  ops_context must be cleared before `finish()`

**`with_sim`** (kfuncs.rs lines 802-858): Every kfunc uses this to access
`SimulatorState`. It pauses RBC counters and timers during the kfunc body,
then resumes them on return.

**`SimulatorState`** (kfuncs.rs lines 171-266): The central shared state:
- `cpus: Vec<SimCpu>` -- per-CPU state including `local_clock`
- `dsqs: DsqManager` -- all dispatch queues
- `current_cpu: CpuId` -- which CPU is "current"
- `trace: Trace` -- event trace
- `clock: TimeNs` -- global event queue time
- `task_raw_to_pid`, `task_pid_to_raw` -- task pointer maps
- `rng: SmallRng` -- deterministic PRNG
- `ops_context: OpsContext` -- current callback type
- `pending_dispatch: Option<PendingDispatch>` -- deferred dispatch
- `kicked_cpus: BTreeMap<CpuId, KickFlags>` -- accumulated kicks
- `in_concurrent_batch: bool` -- suppresses nested dispatch/kicks

**Shared vs thread-local**:
- Shared (via raw pointer): ALL of `SimulatorState` is shared. Token
  passing ensures single-writer access.
- Thread-local: `SIM_STATE` pointer, `SIM_CONTEXT` (for logging),
  `PREEMPT_CTX` / `INTERLEAVE_CTX` (for yield context), all
  `STRUCTOP_*` counters, `CURRENT_OPS_CONTEXT`, `CURRENT_KFUNC_NAME`.

### 7. Engine Event Loop

**Main loop** (engine.rs lines 1144-1242):
1. Pop all events at the minimum timestamp (`events.drain_at(t)`)
2. If interleaving enabled: partition into global + per-CPU
   - Global events: processed sequentially
   - Per-CPU events (2+ CPUs): `process_batch_concurrent()`
3. If interleaving disabled: all events processed sequentially

**`dispatch_concurrent`** (engine.rs lines 2918-3040):
1. Filter CPUs needing dispatch (idle + empty local DSQ)
2. Create `SendPtr` wrappers around shared state
3. Select backend (replay > e9patch > PMU > cooperative)
4. Run workers via `run_preemptive_dispatch()` or
   `dispatch_concurrent_cooperative()`
5. Phase 2 (sequential): global DSQ fallback, start_running, kicks

**`process_batch_concurrent`** (engine.rs lines 3126-3266):
Same pattern as dispatch_concurrent but for batch event processing.

---

## Proposed Design: Native Concurrency Backend

### Architecture Overview

The native concurrency backend lets all CPU workers run truly
simultaneously, with no token ring. Synchronization comes from:

1. **Real locks** (Mutex, RwLock) on shared data structures
2. **Window-based clock throttling**: if any CPU's `local_clock` exceeds
   the minimum `local_clock` across all CPUs by more than a configurable
   window (e.g., 10ms), that CPU spins/sleeps until it is back within
   the window

This is inherently nondeterministic (true thread races), which is the
point: external tools like `hermit` or `rr` record/replay the exact
execution to reproduce bugs.

---

## Phase 1: Refactors (Must Pass Existing Tests)

Phase 1 prepares the infrastructure for the new backend without changing
any existing behavior. All existing tests must continue to pass.

### 1.1 Extract Synchronization Strategy from Backend Trait

**Problem**: The `PreemptionBackend` trait and its generic drivers
(`run_preemptive_dispatch`, `run_preemptive_batch`) hardcode the
`PreemptRing` token-passing protocol. The concurrent backend needs no
token ring at all.

**Solution**: Introduce a `SyncStrategy` abstraction that encapsulates
thread synchronization, separating it from the preemption/instrumentation
concern.

**Files to change**:

- **`crates/scx_simulator/src/backend/mod.rs`** (lines 1-348):
  - Define a new trait `SyncStrategy` that captures the thread
    synchronization protocol:
    ```rust
    pub(crate) trait SyncStrategy: Sync {
        /// Per-worker synchronization context.
        type SyncCtx: Send;

        /// Create sync context for a worker.
        fn worker_sync_setup(&self, worker_id: WorkerId) -> Self::SyncCtx;

        /// Wait to begin execution (token acquire / barrier / no-op).
        fn wait_to_begin(&self, ctx: &Self::SyncCtx);

        /// Yield execution to another worker (cooperative yield point).
        /// Returns true if a different worker was actually selected.
        fn yield_point(&self, ctx: &Self::SyncCtx) -> bool;

        /// Signal that this worker has completed.
        fn finish(&self, ctx: &Self::SyncCtx);

        /// Orchestrator: signal all workers to begin.
        fn start(&self);

        /// Orchestrator: wait for all workers to complete.
        fn wait_all_done(&self);
    }
    ```
  - Create `PreemptRingSyncStrategy` wrapper that delegates to
    `PreemptRing` (trivial adapter; existing behavior)
  - Create `TokenRingSyncStrategy` wrapper that delegates to `TokenRing`
  - Modify `run_preemptive_dispatch` and `run_preemptive_batch` to take
    a `&dyn SyncStrategy` or be generic over `SyncStrategy` instead of
    hardcoding `PreemptRing`
  - The `PreemptionBackend::worker_setup` signature should change from
    taking `&PreemptRing` to taking `&dyn SyncStrategy` (or the sync
    strategy's context)

  **Rationale**: The existing backends currently receive `&PreemptRing`
  in `worker_setup` and `build_target` specifically to call
  `ring.roll_timeslice()` (PRNG consumption) and
  `ring.record_preemption()` (instrumentation). These two concerns
  (PRNG and preemption recording) should be factored out of the sync
  strategy. The simplest approach: keep `PreemptRing` as the
  instrumentation/PRNG holder, and have the sync strategy be a separate
  parameter. Alternatively, move PRNG consumption into the backend
  trait itself.

  **Concrete refactor**: Rather than a full `SyncStrategy` trait
  (which would be a large refactor), a simpler approach is to make the
  generic drivers (`run_preemptive_dispatch`, `run_preemptive_batch`)
  accept an optional `PreemptRing` and a closure for the "wait/finish"
  protocol. But this is awkward with closures.

  **Recommended approach**: Keep the `PreemptRing` parameter in
  `PreemptionBackend` for PRNG/recording, but add a separate
  `ThreadOrchestrator` trait that controls the wait/yield/finish
  protocol. The generic drivers become generic over both:
  ```rust
  fn run_preemptive_dispatch<S, B, O>(
      ...,
      backend: &B,
      orchestrator: &O,
  ) where
      S: Scheduler,
      B: PreemptionBackend,
      O: ThreadOrchestrator,
  ```
  The existing `PreemptRing` and `TokenRing` both implement
  `ThreadOrchestrator`. The new `NativeOrchestrator` also implements it
  but with no-op `wait`/`yield`/`finish` (workers run freely).

### 1.2 Factor Out Token-Ring Usage from Generic Drivers

**Problem**: `run_preemptive_dispatch` (backend/mod.rs lines 188-258)
directly calls:
- `PreemptRing::new()` (line 198)
- `ring.wait_for_token()` (line 214)
- `ring.finish()` (line 245)
- `ring.start()` (line 252)
- `ring.wait_all_done()` (line 253)

Similarly `run_preemptive_batch` (lines 264-348).

**Solution**: Replace these direct calls with calls through the
`ThreadOrchestrator` trait. The `PreemptRing` is still created (for
PRNG/recording), but the synchronization protocol goes through the
orchestrator.

**Files to change**:

- **`crates/scx_simulator/src/backend/mod.rs`**:
  - Add `ThreadOrchestrator` trait (see 1.1)
  - Refactor `run_preemptive_dispatch` to accept `&O: ThreadOrchestrator`
  - Refactor `run_preemptive_batch` to accept `&O: ThreadOrchestrator`
  - Implement `ThreadOrchestrator for PreemptRing` (delegates to existing
    methods)

- **`crates/scx_simulator/src/interleave.rs`**:
  - Implement `ThreadOrchestrator for TokenRing`

- **`crates/scx_simulator/src/engine.rs`**:
  - Update `dispatch_concurrent` (lines 2918-3040) to pass the
    orchestrator to the generic drivers
  - Update `process_batch_concurrent` (lines 3126-3266) similarly
  - Update `dispatch_concurrent_cooperative` (lines 3046-3108) to use
    `ThreadOrchestrator`
  - Update `process_batch_concurrent_cooperative` (lines 3270-3354)
    similarly

### 1.3 Decouple `SimulatorState` Shared Fields for Concurrent Access

**Problem**: In the current design, `SimulatorState` is a single struct
accessed via raw pointer under the assumption of single-writer (token
serialization). The native concurrency backend requires multiple writers.

**Key fields that need protection**:

Per-CPU fields (naturally partitioned, no lock needed if workers only
touch their own CPU):
- `cpus[cpu_idx].local_clock`
- `cpus[cpu_idx].current_task`
- `cpus[cpu_idx].prev_task`
- `cpus[cpu_idx].local_dsq`
- `cpus[cpu_idx].task_started_at`
- `cpus[cpu_idx].task_original_slice`

Shared mutable state (needs locking):
- `dsqs: DsqManager` -- dispatch queues accessed by any CPU
- `trace: Trace` -- event recording from any CPU
- `kicked_cpus: BTreeMap` -- accumulated kicks
- `pending_dispatch: Option<PendingDispatch>` -- deferred dispatch
- `rng: SmallRng` -- PRNG (used in noise/overhead calculations)
- `ops_context: OpsContext` -- current callback context
- `task_ops_state: BTreeMap` -- per-task ops state
- `current_cpu: CpuId` -- which CPU is "current"

**Phase 1 refactor**: Do NOT add locks yet. Instead, organize
`SimulatorState` fields into groups that will later be locked or
partitioned:

- **`crates/scx_simulator/src/kfuncs.rs`** (SimulatorState definition,
  lines 171-266):
  - Add doc-comments classifying each field as `PER-CPU` or `SHARED`
  - Consider grouping per-CPU mutable state into a `PerCpuState` struct
    that is part of `SimCpu` (cpu.rs). This makes it clear which state
    can be accessed without locking in the concurrent backend.
  - Ensure `current_cpu` is truly per-worker (it already is via TLS in
    `SIM_CONTEXT`, but it's also in `SimulatorState`). In the concurrent
    backend, `current_cpu` must come from TLS, not the shared state.

- **`crates/scx_simulator/src/cpu.rs`** (SimCpu, lines 30-88):
  - No changes needed yet, but document that per-CPU fields are safe to
    access without locking when the worker owns that CPU.

### 1.4 Add Clock Window Throttling Hook

**Problem**: The concurrent backend needs a "window check" that throttles
any CPU whose `local_clock` gets too far ahead of the slowest CPU. This
check needs to be called ubiquitously whenever `local_clock` advances.

**Solution**: Add a no-op hook at every `local_clock` update point. The
hook will be activated only when the concurrent backend is in use.

**Files to change**:

- **`crates/scx_simulator/src/kfuncs.rs`**:
  - Add a function `clock_window_check(cpu: CpuId, local_clock: TimeNs)`
    that in Phase 1 is a no-op. In Phase 2, it checks the window
    constraint.
  - Call this function from:
    - `advance_cpu_clock()` (line 413, after the max)
    - The concrete implementation will be in a new module
  - Add a thread-local or global `AtomicBool` that gates the check
    (false = no-op, true = active). This avoids overhead when not using
    the concurrent backend.

- **`crates/scx_simulator/src/engine.rs`**:
  - Call `clock_window_check()` at every point where `local_clock` is
    incremented:
    - After `charge_sched_time()` (lines 465, 491)
    - After CSW overhead addition (lines 2503, 3497)
  - This can be done by adding the call inside `charge_sched_time()`
    itself (it already updates `local_clock`) and a helper for CSW
    overhead.

### 1.5 Make `dispatch_concurrent` and `process_batch_concurrent` Backend-Aware

**Problem**: Engine methods `dispatch_concurrent` (lines 2918-3040) and
`process_batch_concurrent` (lines 3126-3266) contain large
if/else chains selecting the backend (replay > e9patch > PMU > cooperative).
Adding a new backend means adding more branches.

**Solution**: Factor the backend selection into a helper that returns a
trait object or uses an enum dispatch pattern.

**Files to change**:

- **`crates/scx_simulator/src/engine.rs`**:
  - Create a helper function `select_backend_and_run_dispatch(...)` that
    encapsulates the backend selection logic from
    `dispatch_concurrent` (lines 2963-3007)
  - Create a similar helper for `process_batch_concurrent`
    (lines 3187-3258)
  - This reduces duplication between dispatch and batch paths and makes
    adding the new backend a single-point change.

### 1.6 Add `ConcurrencyMode` Enum to Scenario

**Problem**: Currently the scenario has `interleave: bool` and
`preemptive: Option<PreemptiveConfig>`. Need a way to select the new
native concurrency mode.

**Solution**: Add a variant to `PreemptMode` or a new top-level enum.

**Files to change**:

- **`crates/scx_simulator/src/scenario.rs`**:
  - Add `NativeConcurrent` variant to `PreemptMode` enum (line 195):
    ```rust
    pub enum PreemptMode {
        Pmu,
        E9patch,
        NativeConcurrent,
    }
    ```
  - Or add a new config struct `NativeConcurrentConfig` with the window
    size parameter:
    ```rust
    pub struct NativeConcurrentConfig {
        /// Maximum logical time (ns) any CPU can be ahead of the slowest.
        pub window_ns: TimeNs,
    }
    ```
  - Add builder method:
    ```rust
    pub fn native_concurrent(mut self, config: NativeConcurrentConfig) -> Self
    ```
  - Add field to `Scenario`:
    ```rust
    pub native_concurrent: Option<NativeConcurrentConfig>,
    ```

- **`crates/scx_simulator/src/kfuncs.rs`**:
  - Add field to `SimulatorState`:
    ```rust
    pub native_concurrent: Option<NativeConcurrentConfig>,
    ```

### 1.7 Factor the `enter_sim`/`with_sim` Context for Per-CPU Safety

**Problem**: `enter_sim` writes `state.current_cpu` on the shared
`SimulatorState` (kfuncs.rs line 683). In the concurrent backend,
multiple workers would race on this field.

**Solution**: The concurrent backend should NOT write `current_cpu` to
shared state. Instead, kfuncs should read the current CPU from TLS.

**Files to change**:

- **`crates/scx_simulator/src/kfuncs.rs`**:
  - `enter_sim` already updates `SIM_CONTEXT` TLS with the CPU (line 684)
  - Kfuncs that read `sim.current_cpu` (used in many places) need an
    alternative: read from TLS via `sim_cpu()` (line 753)
  - Phase 1: Audit all `sim.current_cpu` reads inside kfuncs. Ensure
    they can alternatively use the TLS `sim_cpu()`.
  - Add a function `current_cpu_for_worker() -> CpuId` that:
    - In token-ring mode: reads `sim.current_cpu` (existing behavior)
    - In concurrent mode: reads from TLS
  - For Phase 1, this is just a wrapper that always reads
    `sim.current_cpu` but prepares the call sites for Phase 2.

---

## Phase 2: New Backend Implementation

### 2.1 Implement `NativeOrchestrator`

**File**: `crates/scx_simulator/src/backend/native.rs` (new file)

The `NativeOrchestrator` implements `ThreadOrchestrator` with concurrent
semantics:

```rust
pub(crate) struct NativeOrchestrator {
    /// Per-CPU local clocks (atomic, updated by workers).
    clocks: Box<[AtomicU64]>,
    /// Window size in nanoseconds.
    window_ns: u64,
    /// Number of workers.
    total: usize,
    /// Barrier for startup synchronization.
    start_barrier: std::sync::Barrier,
    /// Count of finished workers.
    finished: AtomicUsize,
    /// Condvar for wait_all_done.
    done_mu: Mutex<bool>,
    done_cv: Condvar,
}
```

**`ThreadOrchestrator` implementation**:

- `wait_to_begin()`: wait on `start_barrier`
- `yield_point()`: no-op (workers don't yield to each other). But we
  DO check the clock window here -- see 2.2.
- `finish()`: `finished.fetch_add(1)`, notify `done_cv` if all done
- `start()`: unblock all workers (the barrier handles this)
- `wait_all_done()`: wait on `done_cv`

### 2.2 Clock Window Throttling

**File**: `crates/scx_simulator/src/backend/native.rs`

The window-check mechanism ensures no CPU races too far ahead:

```rust
impl NativeOrchestrator {
    /// Called whenever a worker's local_clock is updated.
    /// If this CPU is too far ahead of the minimum, spin-wait.
    pub fn clock_window_check(&self, cpu_idx: usize, new_clock: u64) {
        // Update our atomic clock
        self.clocks[cpu_idx].store(new_clock, Ordering::Release);

        // Find minimum clock across all CPUs
        loop {
            let min_clock = self.clocks.iter()
                .map(|c| c.load(Ordering::Acquire))
                .min()
                .unwrap_or(0);

            if new_clock <= min_clock + self.window_ns {
                break; // Within window, proceed
            }

            // Too far ahead: yield to let other CPUs catch up
            std::hint::spin_loop();
            // OR: std::thread::yield_now();
        }
    }
}
```

**Integration points**: The global clock-window-check hook from Phase 1.4
is activated when `native_concurrent` is `Some`. The hook reads the
orchestrator reference from a thread-local or global and calls
`clock_window_check()`.

**Where to place the check**:

Option A: Inside `charge_sched_time()` and CSW overhead application.
These are the primary `local_clock` update points.

Option B: Inside `maybe_yield()` / `maybe_yield_preemptive()`. These are
already called at every kfunc boundary. The concurrent backend replaces
the yield logic with a window check.

**Recommended**: Option B -- place the window check in the yield-point
path. This is called frequently enough (every kfunc boundary) to prevent
excessive drift, and it is already the natural place for CPU
synchronization logic. The `yield_point` method of `ThreadOrchestrator`
becomes the window check in the native backend.

### 2.3 Protect Shared State with Locks

**File**: `crates/scx_simulator/src/kfuncs.rs` and related files

In the native concurrent backend, `SimulatorState` fields need real
synchronization. The recommended approach:

**A. Per-CPU state (no locking needed)**:
Each worker exclusively owns its `SimCpu`. Workers access `cpus[cpu_idx]`
where `cpu_idx` is their assigned CPU. This is safe because:
- Each worker has a unique CPU assignment
- Workers don't access other CPUs' `SimCpu` directly

**However**: Some kfuncs DO access other CPUs' state (e.g.,
`scx_bpf_select_cpu_dfl` scans all CPUs for idle ones,
`scx_bpf_kick_cpu` sets flags on other CPUs). These cross-CPU
accesses need protection.

**B. Shared state requiring locks**:

| Field | Lock type | Rationale |
|-------|-----------|-----------|
| `dsqs: DsqManager` | `Mutex<DsqManager>` | Any CPU can insert/consume from any DSQ |
| `trace: Trace` | `Mutex<Trace>` | All CPUs record events |
| `kicked_cpus` | `Mutex<BTreeMap>` | Any CPU can kick any other |
| `task_ops_state` | `Mutex<BTreeMap>` | Cross-CPU state transitions |
| `pending_dispatch` | Per-CPU (move into `SimCpu`) | Only the current CPU's pending dispatch matters |
| `current_cpu` | Remove from shared state, use TLS only | Each worker knows its CPU |
| `rng` | Per-CPU PRNG (derive from seed + cpu_id) | Eliminates contention |
| `ops_context` | Per-CPU TLS (already exists as `CURRENT_OPS_CONTEXT`) | |
| `cpus[i]` for i != own | `RwLock<Vec<SimCpu>>` or atomic fields | Cross-CPU reads need protection |
| `waker_task_raw` | Per-CPU (move into TLS) | Only used during current callback |

**C. `with_sim()` changes**:
The current `with_sim()` gives `&mut SimulatorState` to the closure,
which is incompatible with concurrent access. In the native backend,
kfuncs that access shared state must explicitly acquire the relevant lock.

**Approach**: Do not change `with_sim()` in Phase 1. In Phase 2, when
the native backend is active:
- Keep `with_sim()` but have it give a read-only reference or a wrapper
  that provides lock-guarded access to shared state
- OR: Replace the single `&mut SimulatorState` with a `SimAccess` proxy
  that bundles per-CPU mutable state (no lock) with shared state behind
  locks

This is the most invasive change. The recommended strategy is to start
with a coarse-grained `Mutex<SimulatorState>` that wraps the entire
state (same semantics as token ring but using a real lock), then
progressively refine to finer-grained locks.

### 2.4 Implement `NativeConcurrentBackend`

**File**: `crates/scx_simulator/src/backend/native.rs` (new file)

```rust
pub(crate) struct NativeConcurrentBackend {
    pub window_ns: u64,
}

pub(crate) struct NativeWorkerCtx {
    cpu: CpuId,
    cpu_idx: usize,
}

impl PreemptionBackend for NativeConcurrentBackend {
    type WorkerCtx = NativeWorkerCtx;

    fn worker_setup(&self, ring: &PreemptRing, worker_id: WorkerId) -> NativeWorkerCtx {
        // Install TLS, but no token-ring context
        // The worker knows its CPU from the worker_id -> cpu mapping
        NativeWorkerCtx { ... }
    }

    fn build_target(&self, _ctx: &NativeWorkerCtx, _ring: &PreemptRing) -> Option<PreemptTarget> {
        None // No preemption targets in native mode
    }

    fn arm(&self, _ctx: &mut NativeWorkerCtx, _target: PreemptTarget) {
        // No-op: no instrumentation
    }

    fn disarm(&self, _ctx: &mut NativeWorkerCtx) -> StructopDelta {
        StructopDelta::default()
    }

    fn worker_teardown(&self, _ctx: NativeWorkerCtx) {
        // Clean up TLS
    }

    fn log_completion(&self, _ring: &PreemptRing) {
        tracing::info!("native concurrent: complete");
    }
}
```

### 2.5 Engine Integration

**File**: `crates/scx_simulator/src/engine.rs`

Add handling for the native concurrent mode in:

- **`dispatch_concurrent`** (lines 2918-3040):
  Add a branch for `NativeConcurrent` that:
  1. Creates a `NativeOrchestrator` with the window size
  2. Spawns workers with `std::thread::scope`
  3. Each worker enters sim, runs dispatch, exits sim -- all truly
     concurrent
  4. Phase 2 post-processing remains sequential (after all workers
     join)

- **`process_batch_concurrent`** (lines 3126-3266):
  Same pattern: create orchestrator, spawn truly concurrent workers.

- **Main event loop** (lines 1142-1242):
  The `interleave_enabled` check (line 1142) should also be true when
  native concurrent mode is active. The batching logic
  (group_events_by_cpu) works the same way.

- **`enter_sim` changes**: In native mode, do NOT write
  `state.current_cpu` to shared state. Write only to TLS.

### 2.6 `maybe_yield` in Native Mode

**File**: `crates/scx_simulator/src/interleave.rs` and
`crates/scx_simulator/src/preempt/mod.rs`

In native concurrent mode, `maybe_yield()` should:
1. NOT yield the token (there is no token)
2. Perform the clock window check (call into `NativeOrchestrator`)
3. Save/restore per-worker context is unnecessary (each worker has its
   own TLS, and `SimulatorState` is protected by locks)

The simplest approach: when native concurrent mode is active, the
`INTERLEAVE_CTX` and `PREEMPT_CTX` thread-locals are not installed.
Instead, a new `NATIVE_CTX` thread-local is installed with a pointer to
the `NativeOrchestrator`. The `maybe_yield()` function checks:
1. Preemptive context? -> cooperative yield via PreemptRing
2. Cooperative context? -> yield via TokenRing
3. Native context? -> clock window check
4. None? -> no-op

### 2.7 Testing Strategy

**A. Unit tests for the orchestrator**:
- `NativeOrchestrator` with 2-4 workers, verify all start and finish
- Clock window check: verify a fast worker blocks when too far ahead
- Verify a slow worker unblocks a waiting fast worker when it advances

**B. Integration tests**:
- Run existing simple scheduler scenarios with native concurrent mode
- Compare traces: the event sequence will differ from sequential mode
  (that's expected), but structural invariants should hold:
  - Every task that wakes up eventually gets scheduled
  - No task runs on two CPUs simultaneously
  - Dispatch is called for idle CPUs
  - `local_clock` monotonically increases per CPU
  - Tasks complete their phases

**C. Determinism with `hermit`**:
- Run the same scenario twice under `hermit run` and verify identical
  traces
- This validates that the native backend is compatible with external
  determinism tools

**D. Stress tests**:
- Run with many CPUs (8-16) and many tasks
- Verify no deadlocks (window check doesn't cause livelock)
- Verify no data races (run under `TSAN` or `miri` for the pure-Rust
  portions)

**E. Regression tests**:
- All existing tests must pass unchanged (Phase 1)
- Add new test files:
  - `crates/scx_simulator/tests/native_concurrent.rs`

**Test files to create**:
- `crates/scx_simulator/tests/native_concurrent.rs` -- integration tests
- Unit tests in `crates/scx_simulator/src/backend/native.rs` -- module
  tests for `NativeOrchestrator`

---

## Phase Summary: Files Changed

### Phase 1 (Refactors)

| File | Changes |
|------|---------|
| `crates/scx_simulator/src/backend/mod.rs` | Add `ThreadOrchestrator` trait; implement for `PreemptRing`; refactor generic drivers to accept orchestrator parameter |
| `crates/scx_simulator/src/interleave.rs` | Implement `ThreadOrchestrator` for `TokenRing` |
| `crates/scx_simulator/src/engine.rs` | Refactor `dispatch_concurrent`, `process_batch_concurrent`, and their cooperative variants to use `ThreadOrchestrator`; factor backend selection into helper; add clock-window-check call sites |
| `crates/scx_simulator/src/kfuncs.rs` | Document per-CPU vs shared fields on `SimulatorState`; add `clock_window_check()` no-op hook; audit `current_cpu` usage; add `NativeConcurrentConfig` field |
| `crates/scx_simulator/src/scenario.rs` | Add `NativeConcurrent` to `PreemptMode` or add `NativeConcurrentConfig`; add builder method |
| `crates/scx_simulator/src/cpu.rs` | Document per-CPU safety guarantees |

### Phase 2 (New Backend)

| File | Changes |
|------|---------|
| `crates/scx_simulator/src/backend/native.rs` | **NEW**: `NativeOrchestrator`, `NativeConcurrentBackend`, clock window throttling |
| `crates/scx_simulator/src/backend/mod.rs` | Add `pub mod native;`; implement `ThreadOrchestrator` for `NativeOrchestrator` |
| `crates/scx_simulator/src/engine.rs` | Add native concurrent branches in `dispatch_concurrent` and `process_batch_concurrent`; modify `enter_sim` for native mode |
| `crates/scx_simulator/src/kfuncs.rs` | Activate `clock_window_check()`; modify `with_sim()` for concurrent access (coarse Mutex initially); read `current_cpu` from TLS in native mode |
| `crates/scx_simulator/src/interleave.rs` | Add native context check in `maybe_yield()` |
| `crates/scx_simulator/src/preempt/mod.rs` | Add native context check in `maybe_yield_preemptive()` |
| `crates/scx_simulator/tests/native_concurrent.rs` | **NEW**: integration tests |

---

## Risk Analysis

### High Risk: `SimulatorState` Concurrent Access

The biggest challenge is making `SimulatorState` safe for concurrent
access. The current design assumes single-writer access (guaranteed by
token passing). Every kfunc accesses `SimulatorState` through `with_sim()`,
and many kfuncs read/write multiple fields.

**Mitigation**: Start with a coarse-grained `Mutex<SimulatorState>` in
the concurrent backend. This makes the native backend functionally
equivalent to a real mutex (not a PRNG-driven token ring), which is
already useful for external determinism tools. Fine-grained locking can
be added incrementally.

### Medium Risk: Cross-CPU Kfunc Interactions

Kfuncs like `scx_bpf_select_cpu_dfl` (scans all CPUs for idle ones),
`scx_bpf_kick_cpu` (kicks another CPU), and `scx_bpf_dsq_insert`
(inserts into shared DSQs) access state from multiple CPUs.

**Mitigation**: With the coarse Mutex approach, these are automatically
serialized. With fine-grained locking, each cross-CPU operation needs
explicit lock acquisition.

### Medium Risk: Window Check Livelock

If the window is too small and workers have uneven progress rates, the
fast worker might spin-wait indefinitely.

**Mitigation**: Use a reasonable default window (10ms of simulated time).
The window check should use `thread::yield_now()` rather than tight
spin-loop to avoid CPU waste. Add a timeout/panic if a worker waits
too long (e.g., 1 second of wall time).

### Low Risk: TLS Incompatibilities

The codebase uses many thread-locals (`SIM_STATE`, `SIM_CONTEXT`,
`PREEMPT_CTX`, `INTERLEAVE_CTX`, `STRUCTOP_*`). These work correctly
in the concurrent backend because each worker thread has its own TLS.

**No mitigation needed**: TLS is naturally per-thread.

---

## Recommended Implementation Order

1. **Phase 1.6**: Add `NativeConcurrentConfig` to scenario (small, self-contained)
2. **Phase 1.1 + 1.2**: Extract `ThreadOrchestrator` trait and refactor drivers
3. **Phase 1.5**: Factor backend selection into helpers
4. **Phase 1.3**: Document and organize `SimulatorState` fields
5. **Phase 1.4**: Add clock window check hook (no-op)
6. **Phase 1.7**: Audit `current_cpu` usage in kfuncs
7. **Phase 2.1**: Implement `NativeOrchestrator`
8. **Phase 2.4**: Implement `NativeConcurrentBackend`
9. **Phase 2.3**: Add coarse-grained Mutex to `SimulatorState` (gated on native mode)
10. **Phase 2.5**: Engine integration
11. **Phase 2.6**: `maybe_yield` in native mode
12. **Phase 2.2**: Clock window throttling
13. **Phase 2.7**: Testing
