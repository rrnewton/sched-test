# Widened Concurrency: Simulator-in-the-Loop Architecture

## Issue: sim-a730ac (revision)

This plan supersedes the original widened concurrency plan. The key
architectural change is **simulator-in-the-loop**: every preemption or
structop completion returns control to the simulator, which decides what
to run next. This eliminates post-processing, makes the concurrency
window fully dynamic, and removes the `in_concurrent_batch` flag.

## 1. Architecture Overview

### 1.1 Current Model: Scheduler-to-Scheduler

The current token ring switches directly between scheduler threads:

```
scheduler C (CPU1) → [yield] → scheduler C (CPU2) → [yield] → scheduler C (CPU1)
```

The simulator is blind between switches. No clock updates, no event
queue inspection, no state validation. The token ring's PRNG picks the
next worker, and the winner runs its scheduler code until the next yield
point. Post-processing (global DSQ fallback, `start_running`,
`process_kicked_cpus`) happens sequentially after **all** workers
complete.

### 1.2 New Model: Simulator-in-the-Loop

Every yield/preemption returns control to the **simulator engine**, which
then decides what to run next:

```
scheduler C (CPU1) → [yield] → SIMULATOR → [decide] → scheduler C (CPU2)
                                   ↑
                        1. Update CPU1's local_clock
                        2. Check event queue
                        3. Pick next CPU/event
                        4. Schedule deferred work
```

At each handoff back to the simulator:

1. **Update local_clock** -- the preempted CPU's clock advances to
   reflect the work done (via `charge_sched_time` / RBC).
2. **Check event queue** -- are there pending events whose timestamps
   fall within the current execution window (i.e., before any CPU's
   `local_clock`)?
3. **Decide next action** -- resume same CPU, switch to a different
   CPU's structop, process a newly-eligible event, or schedule deferred
   work.
4. **Schedule deferred work** -- kicks become per-CPU events; DSQ
   consume and `ops.running()` are timed events on the target CPU's
   timeline.

### 1.3 Why This Matters

The simulator-in-the-loop model has three major advantages:

1. **Eliminates post-processing** -- DSQ consume, `start_running`, and
   kick processing become normal timed events, raceable by other CPUs.
2. **Dynamic concurrency window** -- no fixed `[T, T+W)` window; the
   simulator sees all CPU clocks and picks the earliest pending event.
3. **Eliminates `in_concurrent_batch`** -- the simulator is always in
   control between structop slices, so re-entrant operations are
   impossible.

### 1.4 Design Decision: `SendPtr` vs `Mutex<SimulatorState>`

#### The Problem

The current architecture uses `SendPtr<T>` (backend/mod.rs:116) -- a
raw `*mut T` wrapper with `unsafe impl Send + Sync` -- to share
`SimulatorState`, `EventQueue`, `HashMap<Pid, SimTask>`,
`CgroupRegistry`, and `Simulator<S>` across worker threads. Safety
relies on the token ring ensuring single-writer access. There are 6
different types wrapped in `SendPtr` (engine.rs:3072-3073, 3304-3308),
and the raw pointer is dereferenced in 12+ `unsafe` blocks across
`backend/mod.rs` and `engine.rs`.

With simulator-in-the-loop, the engine thread owns the state and hands
controlled access to whichever worker thread is active. A `Mutex`
would make the single-writer invariant safe by construction, replacing
runtime discipline (token protocol) with compile-time enforcement
(lock ownership).

#### Analysis: Why Mutex Does Not Fit This Plan

A `Mutex<SimulatorState>` is the wrong abstraction here for a
fundamental reason: **the FFI boundary**.

The 33 kfunc call sites (e.g., `sim_scx_bpf_dsq_insert`,
`sim_scx_bpf_kick_cpu`) are `extern "C"` functions called from
compiled scheduler C code. The call chain is:

```
Engine (Rust) → enter_sim(state, cpu)   // stores *mut in TLS
             → scheduler.dispatch()     // calls C code
                → C code calls kfunc    // extern "C" fn
                   → with_sim(|sim| ..) // reads *mut from TLS
             → exit_sim()              // clears TLS
```

There is no Rust call stack to thread a `MutexGuard<'_, SimulatorState>`
through. The C code boundary forces a raw-pointer-in-TLS pattern
regardless of what happens on the Rust side. Even with a `Mutex`, we
would need:

1. Lock the `Mutex` on the worker thread.
2. Store the raw `*mut SimulatorState` (from `MutexGuard::deref_mut`)
   in the TLS cell.
3. Drop the `MutexGuard` -- otherwise the scheduler C code can't
   yield (the lock would be held across the yield point, deadlocking).
4. But dropping the guard means TLS holds a dangling pointer until the
   next lock acquisition.

This creates a paradox: we need the guard for safety, but we need to
drop it for the yield protocol to work. The TLS raw pointer is the
actual access path, and the `Mutex` would be a fiction -- it wouldn't
actually protect the pointer dereferences in `with_sim`.

**Alternative: `Mutex` around the hand-off only.** We could use a
`Mutex` for the engine↔worker transition (not for kfunc access). The
engine locks the mutex, picks the next worker, unlocks it, and the
worker locks it. But this is exactly what `yield_to_engine` already
does (via condvar signaling). A `Mutex` would add a lock/unlock pair
at every yield without eliminating any `unsafe` code, because the
33 kfunc sites still use the TLS raw pointer.

**Quantifying the scope.** Replacing `SendPtr` with `Mutex` would
touch:

- `SimulatorState` (kfuncs.rs:179) -- wrap in `Arc<Mutex<...>>`
- `with_sim` (kfuncs.rs:956) -- 33 call sites, each needs
  `.lock().unwrap()` (uncontested but measurable overhead)
- `enter_sim` / `exit_sim` -- must acquire/release lock
- All `SendPtr` creation sites (engine.rs: 6 sites)
- All `SendPtr` dereference sites (backend/mod.rs: 12+ unsafe blocks)
- `dispatch_worker_body` / `batch_worker_body` -- receive
  `Arc<Mutex<...>>` instead of raw pointers
- Worker closures in `run_dispatch_with_orchestrator`,
  `run_batch_with_orchestrator`, cooperative dispatch/batch

This is a pervasive refactor across `engine.rs`, `kfuncs.rs`, and
`backend/mod.rs`. It would NOT reduce the `unsafe` footprint (the TLS
raw pointer remains) and would add lock overhead at every kfunc call.

#### Decision: Note as Follow-Up

Replacing `SendPtr` with `Mutex` is a **follow-up task**, not part of
this plan, for three reasons:

1. **The FFI boundary makes it incomplete.** A `Mutex` cannot protect
   the TLS raw pointer access path used by 33 kfunc sites. The
   `unsafe` blocks would remain.

2. **It is orthogonal to the architectural change.** The
   simulator-in-the-loop design changes *who decides which worker runs
   next*. Whether state is shared via `SendPtr` or `Mutex` is
   independent of this decision logic.

3. **It adds scope without reducing risk.** The `SendPtr` pattern is
   well-understood and enforced by the token protocol (and in the new
   design, by the engine's exclusive control). Adding `Mutex` would
   increase the changeset size substantially without eliminating the
   core `unsafe` blocks.

**What would actually eliminate `unsafe`**: a redesign where kfuncs do
NOT access `SimulatorState` through a raw pointer. Instead, each kfunc
would return a "request" value to the engine (via a channel or return
value), and the engine would execute the request on its own thread.
This is a much larger architectural change (effectively making kfuncs
asynchronous) and is out of scope for this plan. File as a separate
issue.

## 2. Phase 1: Refactor Yield to Return to Simulator

### 2.1 Goal

Change the yield/preemption path so that control always returns to the
simulator engine between scheduler C code executions, instead of
switching directly scheduler→scheduler. This phase is
**backward-compatible**: the simulator immediately re-dispatches to the
PRNG-selected worker, preserving existing deterministic interleaving.

### 2.2 Current Yield Architecture

There are two yield paths:

**Cooperative (interleave.rs)**

`maybe_yield()` (line 259) calls `ring.yield_token(ctx.worker_id)`,
which directly transfers the token to another worker via PRNG selection
inside `TokenRing::yield_token()` (line 155). The exchange is
worker↔worker; the engine thread is blocked in
`TokenRing::wait_all_done()` (line 182).

**Preemptive (preempt/mod.rs)**

`maybe_yield_preemptive()` (line 1506) calls
`cooperative_yield_impl()` (line 1531), which calls
`ring.yield_token(ctx.worker_id)` on the `PreemptRing`. Same pattern:
worker↔worker, engine blocked in `PreemptRing::wait_all_done()`.

### 2.3 New Yield Architecture: Yield-to-Simulator

Instead of the token ring picking the next worker directly, yield
returns control to the simulator engine. The engine then runs its
decision logic and dispatches the next worker.

**New flow:**

```
Worker A: yield_to_simulator(worker_id)
  → saves per-callback context (cpu, ops_context, waker_task_raw)
  → signals the engine thread ("worker A is paused")
  → blocks until the engine signals "resume"

Engine: receives "worker A paused" signal
  → reads Worker A's CPU clock (already advanced by charge_sched_time)
  → runs decision logic (Phase 3: check events, pick next)
  → signals Worker B to resume (or re-signals Worker A)

Worker B: resumes
  → restores its per-callback context
  → continues scheduler C code
```

**Implementation: `SimulatorOrchestrator` trait extension**

Add a new method to `ThreadOrchestrator`:

```rust
/// Worker: yield to the simulator engine for a decision.
///
/// Unlike `yield_token` (which picks the next worker via PRNG),
/// this returns control to the engine thread. The engine inspects
/// state and decides which worker to resume.
///
/// Default: delegates to `yield_token` for backward compatibility.
fn yield_to_engine(&self, worker_id: WorkerId) -> bool {
    self.yield_token(worker_id)
}
```

In Phase 1, the default implementation delegates to `yield_token`,
preserving existing behavior. Phases 2-3 override this with the
simulator-in-the-loop logic.

**Concrete mechanism: engine-mediated token passing**

The key insight is that the engine thread is currently blocked in
`wait_all_done()`. To participate in yield decisions, it must be woken
on every yield, not just on finish.

New `SimRing` struct (replaces `TokenRing` / wraps `PreemptRing`):

```rust
pub struct SimRing {
    inner: TokenState,  // reuse existing PRNG + finished mask
    // NEW: channel for workers to notify the engine
    yield_notify: Condvar,
    // NEW: engine decision result
    resume_worker: Option<WorkerId>,
    // NEW: callback for the engine to run on each yield
    on_yield: Option<Box<dyn Fn(WorkerId, &mut TokenState) -> WorkerId + Send + Sync>>,
}
```

When `on_yield` is `None` (Phase 1 backward compat), `yield_token`
behaves exactly as today. When `on_yield` is `Some(f)`, the yield
wakes the engine, which calls `f(yielding_worker, state)` to decide
the next worker. The default `f` is the PRNG picker (identical to
current behavior). Phases 2-3 replace `f` with logic that checks the
event queue.

### 2.4 Changes to `interleave.rs`

| Line | Current | Change |
|------|---------|--------|
| 57-188 | `TokenRing` struct | Add `on_yield` callback field. `yield_token` checks: if `on_yield` is set, wake engine and block; otherwise use PRNG directly. |
| 259-299 | `maybe_yield()` | No change in Phase 1 (the `on_yield` callback is None, so behavior is identical). |

### 2.5 Changes to `preempt/mod.rs`

| Line | Current | Change |
|------|---------|--------|
| 1506-1525 | `maybe_yield_preemptive()` / `maybe_yield_preemptive_post()` | No change in Phase 1. |
| 1531-1602 | `cooperative_yield_impl()` | No change in Phase 1. The `PreemptRing::yield_token` delegates to the same PRNG logic. |

### 2.6 Changes to `backend/mod.rs`

| Line | Current | Change |
|------|---------|--------|
| 61-86 | `ThreadOrchestrator` trait | Add `yield_to_engine()` with default impl delegating to `yield_token()`. |
| 334-389 | `run_dispatch_with_orchestrator()` | No change in Phase 1. |
| 436-509 | `run_batch_with_orchestrator()` | No change in Phase 1. |

### 2.7 Backward Compatibility

Phase 1 is a **pure refactor**: the `on_yield` callback defaults to
`None`, and `yield_to_engine` defaults to `yield_token`. All existing
tests pass identically. The new infrastructure is exercised only when
Phases 2-3 install a custom callback.

### 2.8 Testing

- All existing tests pass with no changes (the default path is
  identical to the current path).
- New unit test: `SimRing` with `on_yield = None` produces same
  interleaving as `TokenRing` for the same seed.
- New unit test: `SimRing` with `on_yield = Some(custom)` routes yield
  through the engine.

## 3. Phase 2: Eliminate Post-Processing

### 3.1 Goal

Move the four post-processing actions from the sequential engine thread
into timed per-CPU events on the normal event timeline:

1. **Global DSQ fallback** (consume from global DSQ)
2. **`ops.running()`** (`start_running`)
3. **Kicked CPUs** (`process_kicked_cpus`)
4. **`in_concurrent_batch` flag** (eliminated)

### 3.2 Current Post-Processing (engine.rs)

After `process_batch_concurrent` completes (line 3406):

```rust
state.in_concurrent_batch = false;
// Process deferred kicked CPUs
self.process_kicked_cpus(None, state, tasks, events, monitor);
```

After `dispatch_concurrent` Phase 1 completes (line 3143):

```rust
// Phase 2: sequential post-processing
for &cpu in &dispatch_cpus {
    // Balance trace, monitor
    self.post_dispatch_run(cpu, false, state, tasks, events, monitor);
}
self.process_kicked_cpus(None, state, tasks, events, monitor);
```

`post_dispatch_run` (line 2965):

```rust
// 1. Global DSQ fallback (if local DSQ empty)
// 2. Pick task from local DSQ
// 3. start_running(cpu, pid, ...)  ← calls ops.running()
// 4. Or: CPU idle + update_idle
```

### 3.3 New Model: Post-Processing as Timed Per-CPU Events

#### 3.3.1 Cost Table

Each deferred action consumes logical time. Define a new cost table in
`engine.rs` (or a new `cost.rs` module):

```rust
/// Nanosecond costs for post-dispatch simulator actions.
///
/// These model the kernel overhead that currently happens "instantly"
/// in post-processing. Making them timed events ensures they consume
/// logical time and are raceable by other CPUs.
pub mod action_cost {
    /// Global DSQ consume: cache-line transfer + dequeue.
    pub const DSQ_CONSUME_NS: u64 = 100;
    /// ops.running() callback overhead (in addition to the callback's
    /// own RBC-measured cost).
    pub const RUNNING_OVERHEAD_NS: u64 = 50;
    /// IPI delivery latency: time between scx_bpf_kick_cpu() and the
    /// target CPU processing the reschedule interrupt.
    pub const IPI_DELIVERY_NS: u64 = 200;
    /// ops.update_idle() callback overhead.
    pub const UPDATE_IDLE_OVERHEAD_NS: u64 = 50;
}
```

These values are configurable via `Scenario` (with the above as
defaults). The cost table is part of the `OverheadConfig`.

#### 3.3.2 New Event Kinds

Add new `EventKind` variants:

```rust
/// Per-CPU event: consume from global DSQ into local DSQ.
/// Scheduled after ops.dispatch() returns with an empty local DSQ.
DsqConsume { cpu: CpuId },

/// Per-CPU event: run the picked task (ops.running + start execution).
/// Scheduled after a task is picked from the local DSQ.
StartRunning { cpu: CpuId, pid: Pid },

/// Per-CPU event: process a kick (IPI delivered to this CPU).
/// Scheduled when scx_bpf_kick_cpu(target, flags) is called.
KickDelivered { cpu: CpuId, flags: KickFlags },
```

#### 3.3.3 Global DSQ Fallback → `DsqConsume` Event

**Current** (engine.rs:2974-2990, `post_dispatch_run`):

After `ops.dispatch()` returns, if the local DSQ is empty, the engine
immediately consumes from the global DSQ. This is instant and
unraceable.

**New**:

After `ops.dispatch()` returns with an empty local DSQ, schedule a
`DsqConsume` event at `local_clock + DSQ_CONSUME_NS`:

```rust
if state.cpus[cpu_idx].local_dsq.is_empty() {
    let consume_time = state.cpus[cpu_idx].local_clock + action_cost::DSQ_CONSUME_NS;
    events.push(consume_time, EventKind::DsqConsume { cpu });
}
```

The `DsqConsume` handler:

```rust
EventKind::DsqConsume { cpu } => {
    state.advance_cpu_clock(cpu);
    let cpu_idx = cpu.0 as usize;
    let cpus_ptr = state.cpus.as_mut_ptr();
    let sim_cpu = unsafe { &mut *cpus_ptr.add(cpu_idx) };
    let consumed = state.dsqs.move_to_local(DsqId::GLOBAL, sim_cpu);
    if consumed {
        state.trace.record(local_t, cpu, TraceKind::DsqMoveToLocal { ... });
        // Schedule StartRunning for the consumed task
        if let Some(pid) = state.cpus[cpu_idx].local_dsq.front().copied() {
            let run_time = state.cpus[cpu_idx].local_clock + action_cost::RUNNING_OVERHEAD_NS;
            events.push(run_time, EventKind::StartRunning { cpu, pid });
        }
    }
    // If nothing consumed, CPU becomes idle
}
```

**Raceability**: between `ops.dispatch()` returning and `DsqConsume`
firing, another CPU might insert into the global DSQ, changing what
gets consumed. This matches kernel behavior where the global DSQ is
contested.

#### 3.3.4 `start_running` / `ops.running()` → `StartRunning` Event

**Current** (engine.rs:3688-3821, `start_running`):

Called immediately after picking a task from the local DSQ. Calls
`ops.running()`, sets up slice timers, etc. This is instant and
unraceable.

**New**:

After a task is picked from the local DSQ (either from dispatch or
from `DsqConsume`), schedule a `StartRunning` event at
`local_clock + RUNNING_OVERHEAD_NS`:

```rust
if let Some(pid) = state.cpus[cpu_idx].local_dsq.pop_front() {
    let run_time = state.cpus[cpu_idx].local_clock + action_cost::RUNNING_OVERHEAD_NS;
    events.push(run_time, EventKind::StartRunning { cpu, pid });
}
```

The `StartRunning` handler calls the existing `start_running()` body
(which handles `ops.running()`, slice timer setup, etc.).

**Raceability**: between picking the task and `StartRunning` firing,
another CPU might kick this CPU with `PREEMPT`, which would need to
be handled (the kick event would arrive first and the `StartRunning`
would see the CPU is no longer idle).

#### 3.3.5 Kicked CPUs → `KickDelivered` Events

**Current** (engine.rs:3500-3533, `process_kicked_cpus`):

After a concurrent batch or callback completes, the engine iterates
`state.kicked_cpus` and immediately handles each kick. During
concurrent batches, kicks are deferred entirely (the
`in_concurrent_batch` check at line 3511).

**New**:

When `scx_bpf_kick_cpu(target, flags)` is called in `kfuncs.rs` (the
`sim_scx_bpf_kick_cpu` function), instead of storing the kick in
`state.kicked_cpus`, schedule a `KickDelivered` event on the target
CPU's timeline:

```rust
// In sim_scx_bpf_kick_cpu (kfuncs.rs):
let source_cpu = current_cpu_from_tls();
let source_clock = sim.cpus[source_cpu.0 as usize].local_clock;
let delivery_time = source_clock + action_cost::IPI_DELIVERY_NS;
// Push directly to the event queue (requires &mut EventQueue access)
events.push(delivery_time, EventKind::KickDelivered { cpu: target, flags });
```

The `KickDelivered` handler:

```rust
EventKind::KickDelivered { cpu, flags } => {
    state.advance_cpu_clock(cpu);
    if flags.contains(KickFlags::PREEMPT)
        && state.cpus[cpu.0 as usize].current_task.is_some()
    {
        self.preempt_current(cpu, state, tasks, events, monitor);
    } else if flags.contains(KickFlags::IDLE) {
        if state.cpus[cpu.0 as usize].current_task.is_none() {
            self.try_dispatch_and_run(cpu, state, tasks, events, monitor);
        }
    } else {
        self.try_dispatch_and_run(cpu, state, tasks, events, monitor);
    }
}
```

**Raceability**: the IPI delivery takes `IPI_DELIVERY_NS`. During
that time, another CPU might dispatch to the kicked CPU, or the kicked
CPU might start a task on its own. This matches kernel behavior where
IPIs have non-zero latency.

**Challenge**: kfuncs currently cannot push to the event queue because
they only have `&mut SimulatorState`, not `&mut EventQueue`. Two
options:

1. **Add an event staging area to `SimulatorState`**: kicks are
   collected in `state.staged_events: Vec<(TimeNs, EventKind)>` and
   flushed to the event queue after each callback returns. This is
   simpler and avoids threading `EventQueue` through kfuncs.

2. **Thread `EventQueue` through kfuncs**: more invasive, requires
   changing `with_sim` and all kfunc signatures. Not recommended.

Option 1 is the right approach. The staging area replaces
`state.kicked_cpus`.

#### 3.3.6 `in_concurrent_batch` Suppression No Longer Needed

Currently, `process_kicked_cpus` returns immediately when
`in_concurrent_batch` is true (line 3511). With kicks as timed events,
there is no `process_kicked_cpus` to suppress. The kick events are
scheduled on the timeline and processed by the event loop in the
normal course of execution. The `in_concurrent_batch` flag is not
needed.

Similarly, `dispatch_concurrent` is suppressed during concurrent
batches (the `!state.in_concurrent_batch` check at line 2455 in
`handle_task_wake`). With simulator-in-the-loop, the engine controls
all dispatch decisions and there is no risk of nested concurrent
dispatch.

### 3.4 Changes to `engine.rs`

| Location | Current | Change |
|----------|---------|--------|
| `EventKind` enum (line 239) | No `DsqConsume`, `StartRunning`, `KickDelivered` | Add three new variants |
| `EventKind::cpu()` (line 302) | Does not handle new variants | Return `Some(cpu)` for all three |
| `process_event` (line 1453) | Does not handle new variants | Add match arms |
| `post_dispatch_run` (line 2965) | Immediate DSQ consume + `start_running` | Replace with event scheduling |
| `dispatch_concurrent` Phase 2 (line 3143) | Sequential post-processing loop | Replace: schedule `DsqConsume` events per CPU |
| `process_batch_concurrent` (line 3249) | Sets/clears `in_concurrent_batch`, calls `process_kicked_cpus` | Remove `in_concurrent_batch` toggle, remove `process_kicked_cpus` call |
| `process_kicked_cpus` (line 3500) | Iterates `state.kicked_cpus` | Delete function entirely |
| `start_running` (line 3688) | Called immediately | Called from `StartRunning` event handler |

### 3.5 Changes to `kfuncs.rs`

| Location | Current | Change |
|----------|---------|--------|
| `sim_scx_bpf_kick_cpu` | Inserts into `state.kicked_cpus` | Push `(delivery_time, KickDelivered)` to `state.staged_events` |
| `SimulatorState` | Has `kicked_cpus: BTreeMap<CpuId, KickFlags>` | Replace with `staged_events: Vec<(TimeNs, EventKind)>` |
| `SimulatorState` | Has `in_concurrent_batch: bool` | Remove field |

### 3.6 New `staged_events` Flush Point

After each callback (inside `process_event`, after `charge_sched_time`
returns), flush `state.staged_events` into the event queue:

```rust
for (time, kind) in state.staged_events.drain(..) {
    events.push(time, kind);
}
```

This is a centralized flush point, called from the engine (which owns
both `state` and `events`). Kfuncs only append to the staging area.

### 3.7 Cost Table Configuration

Add to `OverheadConfig` (in `scenario.rs`):

```rust
/// Nanosecond cost for global DSQ consume operation.
pub dsq_consume_ns: u64,
/// Overhead for ops.running() callback dispatch.
pub running_overhead_ns: u64,
/// IPI delivery latency for scx_bpf_kick_cpu.
pub ipi_delivery_ns: u64,
/// Overhead for ops.update_idle() callback dispatch.
pub update_idle_overhead_ns: u64,
```

Default values as specified in section 3.3.1. When `overhead.enabled`
is false, all costs are 0 (instant, matching current behavior for
`instant_timing()` scenarios).

### 3.8 Testing

- **Backward compatibility**: run all existing tests with
  `overhead.enabled = false`. Since all action costs are 0, events
  fire at the same time as the triggering callback, producing
  equivalent behavior.
- **Timed events**: new test with `overhead.enabled = true` and
  non-zero costs. Verify that DSQ consume happens at
  `local_clock + dsq_consume_ns`, not instantly.
- **Raceability**: new test where CPU0 kicks CPU1, and CPU2 dispatches
  to CPU1 before the IPI arrives. Verify CPU1 runs CPU2's task, and
  the belated kick becomes a no-op.
- **Kick timing**: verify that `KickDelivered` events appear at
  `source_cpu.local_clock + ipi_delivery_ns`, not at the global clock.

## 4. Phase 3: Dynamic Concurrency Window

### 4.1 Goal

With simulator-in-the-loop (Phase 1) and no post-processing (Phase 2),
the concurrency window becomes fully dynamic. There is no fixed
`[T, T+W)` window. The simulator always knows every CPU's
`local_clock` and picks the earliest pending event.

### 4.2 Current Event Loop (engine.rs:1248)

```rust
'event_loop: while let Some(t) = events.peek_time() {
    if t > scenario.duration_ns { break; }
    state.clock = t;
    let batch = events.drain_at(t);  // exact same nanosecond
    // partition, process global, process per-CPU concurrent
}
```

Events at different timestamps are processed strictly sequentially.
A tick on CPU0 at T=4,000,000ns completes fully before a tick on CPU1
at T=4,000,050ns starts, even though in the real kernel they overlap.

### 4.3 New Event Loop: Dynamic Scheduling

Replace `drain_at(t)` with a dynamic event scheduler that interleaves
per-CPU events based on `local_clock` overlap:

```rust
'event_loop: while let Some(t) = events.peek_time() {
    if t > scenario.duration_ns { break; }
    state.clock = t;

    // Pop the minimum-time event
    let event = events.pop().unwrap();

    if !interleave_enabled || event.kind.cpu().is_none() {
        // Global or non-interleaved: process sequentially
        self.process_event(event, ...);
        continue;
    }

    let cpu = event.kind.cpu().unwrap();
    self.begin_structop_on_cpu(cpu, event, ...);

    // The structop advances cpu's local_clock via charge_sched_time.
    // At each yield point, the engine checks:
    // - Are there events on OTHER CPUs with timestamps <=
    //   max(local_clock[c] for all active CPUs)?
    // - If so, those events join the concurrent set.

    // The engine picks the CPU with the earliest pending event
    // or the earliest local_clock among paused structops.
    loop {
        let next = self.pick_next_action(&events, &active_structops);
        match next {
            Action::ResumeStructop(worker_id) => { ... }
            Action::StartNewStructop(event) => { ... }
            Action::AllDone => break,
        }
    }
}
```

### 4.4 How the Simulator Decides What to Run Next

At each decision point (after a worker yields), the simulator has:

- **Active structops**: workers paused at yield points, each with a
  `local_clock` and a CPU ID.
- **Event queue**: pending events sorted by timestamp.
- **CPU clocks**: `state.cpus[i].local_clock` for all CPUs.

The decision algorithm:

```rust
fn pick_next_action(&self) -> Action {
    // 1. Find the maximum local_clock among all active structops
    let max_clock = active_structops.iter()
        .map(|w| state.cpus[w.cpu.0 as usize].local_clock)
        .max()
        .unwrap_or(0);

    // 2. Check event queue: any events with timestamp <= max_clock
    //    on CPUs that don't already have an active structop?
    while let Some(t) = events.peek_time() {
        if t > max_clock { break; }
        let event = events.peek().unwrap();
        if let Some(cpu) = event.kind.cpu() {
            if !active_structops.has_cpu(cpu) {
                // New event on an uninvolved CPU within the window
                let event = events.pop().unwrap();
                return Action::StartNewStructop(event);
            }
        }
        // Global event or CPU already active: process later
        break;
    }

    // 3. No new events to pull in. Resume an existing structop.
    //    Use PRNG to pick which active structop to resume
    //    (preserves deterministic interleaving).
    let worker = prng_pick(&active_structops);
    Action::ResumeStructop(worker)
}
```

### 4.5 How New Events Are Discovered During Structop Execution

The feedback loop:

1. Worker A (CPU0) executes a structop. `charge_sched_time` advances
   `CPU0.local_clock` by 200ns.
2. At the next yield point, the engine wakes and checks the event
   queue.
3. Event queue has a tick on CPU1 at T+150ns. Since
   `CPU0.local_clock` is now T+200ns, CPU1's tick falls within the
   execution window.
4. Engine pops CPU1's tick, spawns (or resumes) a worker for CPU1.
5. Both workers are now "active" -- the PRNG interleaves between them.

The window grows organically based on structop execution cost. There
is no static `concurrency_window_ns` parameter -- the window is
exactly the set of events whose timestamps are <= any active CPU's
`local_clock`.

### 4.6 Multiple Simultaneous Active Structops

Multiple CPUs' structops can be "in flight" simultaneously, each
paused at a yield point:

```
CPU0: structop at yield point 3, local_clock = T+200
CPU1: structop at yield point 1, local_clock = T+180
CPU2: structop at yield point 5, local_clock = T+350
```

The PRNG picks which one to resume at each decision point. This
models the real kernel where all three CPUs are executing concurrently.

### 4.7 Worker Management

**Current**: workers are pre-spawned for the batch and join when all
finish. Fixed count, fixed lifetime.

**New**: workers are spawned on demand as new events enter the
concurrent set. A worker finishes when its structop completes, but
new workers can be spawned while existing ones are still active (paused
at yield points).

Implementation options:

1. **Thread pool**: pre-spawn `nr_cpus` workers, assign events to
   idle workers. Workers block on a condvar until assigned.
2. **Scoped spawn-on-demand**: use `std::thread::scope` with a
   maximum of `nr_cpus` threads. Spawn new threads as events join
   the concurrent set.

Option 1 is simpler and avoids repeated thread creation overhead.

### 4.8 Changes to `engine.rs`

| Location | Current | Change |
|----------|---------|--------|
| Main event loop (line 1248) | `drain_at(t)` batching | Replace with dynamic event scheduler |
| `process_batch_concurrent` (line 3249) | Fixed batch, pre-spawned workers | Replace with dynamic worker pool |
| `dispatch_concurrent` (line 3037) | Filters idle CPUs, spawns workers | Merge into the dynamic event scheduler |
| `group_events_by_cpu` (line 326) | Partitions batch | No longer needed (events processed individually) |

### 4.9 Determinism

Determinism is preserved because:

1. The event queue is deterministic (same seed → same tiebreakers).
2. The PRNG for picking which active structop to resume is seeded
   from `state.next_prng()`.
3. New events entering the concurrent set are determined by
   `local_clock` values, which are deterministic (same RBC costs →
   same clock advancement).
4. The decision algorithm (`pick_next_action`) is a deterministic
   function of the event queue, active structops, and PRNG state.

### 4.10 Replay Trace Compatibility

The replay trace format must record the dynamic event discovery
sequence. Each preemption record already captures `structop_rbc` and
`ops_context`. The new format additionally records:

- Which events joined the concurrent set at each decision point.
- The `local_clock` of each CPU at each yield point.

This allows exact replay: the same events join at the same yield
points, and the same PRNG picks the same resume order.

### 4.11 Testing

- **Dynamic discovery**: scenario where CPU0's dispatch callback
  advances `local_clock` by 200ns, and CPU1 has a tick at T+150ns.
  Verify CPU1's tick is discovered and processed concurrently.
- **Window growth**: scenario with 4 CPUs. CPU0's callback takes
  500ns. Events on CPU1 (T+100), CPU2 (T+200), CPU3 (T+400) should
  all be discovered dynamically. CPU1 first, then CPU2, then CPU3 as
  CPU0's clock advances.
- **Termination**: verify the discovery loop terminates (no infinite
  growth -- bounded by BPF instruction limit and finite event queue).
- **Determinism**: same seed produces identical traces across 10 runs
  with dynamic discovery.

## 5. Phase 4: Remove `in_concurrent_batch` Flag

### 5.1 Why It Is No Longer Needed

The `in_concurrent_batch` flag (line 385 of `kfuncs.rs`) exists for
two purposes:

1. **Suppress `process_kicked_cpus`** during concurrent batches
   (engine.rs:3511) -- kicks accumulate and are processed after the
   batch.
2. **Suppress `dispatch_concurrent`** during concurrent batches
   (engine.rs:2455) -- prevents nested concurrent dispatch from
   `handle_task_wake`.

With simulator-in-the-loop:

- **Purpose 1 is eliminated**: kicks become timed events (Phase 2).
  There is no `process_kicked_cpus` to suppress.
- **Purpose 2 is eliminated**: the simulator engine controls all
  dispatch decisions. There is no risk of nested concurrent dispatch
  because workers never call `dispatch_concurrent` directly -- they
  yield to the engine, which decides whether to dispatch.

### 5.2 Changes

| File | Location | Change |
|------|----------|--------|
| `kfuncs.rs` | `SimulatorState` field `in_concurrent_batch` (line 385) | Delete field |
| `engine.rs` | `process_batch_concurrent` sets/clears flag (line 3285, 3406) | Remove |
| `engine.rs` | `process_kicked_cpus` checks flag (line 3511) | Remove (function deleted in Phase 2) |
| `engine.rs` | `handle_task_wake` checks flag (line 2455) | Remove check |
| `engine.rs` | `run_internal` initializes field (line 913) | Remove |

### 5.3 Testing

All existing tests pass without the flag. The flag's behavior is
subsumed by the simulator-in-the-loop architecture.

## 6. Testing Strategy

### 6.1 Phase 1 Testing (Yield Refactor)

- **Regression**: all existing tests pass identically (the default
  `on_yield = None` path is the same as the current path).
- **Unit**: `SimRing` with custom `on_yield` callback routes decisions
  through the engine.
- **Determinism**: same seed produces identical traces with the new
  yield path.

### 6.2 Phase 2 Testing (Eliminate Post-Processing)

- **Backward compat**: with `overhead.enabled = false`, all action
  costs are 0, producing equivalent behavior to the current
  sequential post-processing.
- **Timed events**: with non-zero costs, verify events fire at
  `local_clock + cost_ns`.
- **Raceability**: kick latency creates a window for another CPU to
  act first.
- **Global DSQ contention**: two CPUs compete for the same global DSQ
  task. Only one should win.

### 6.3 Phase 3 Testing (Dynamic Window)

- **Dynamic discovery**: verify events are pulled in based on
  `local_clock` advancement.
- **Termination**: verify no infinite loops.
- **Determinism**: same seed → identical traces with dynamic discovery.
- **Multi-seed sweep**: 100 seeds, verify no panics or assertion
  failures.

### 6.4 Phase 4 Testing (Remove Flag)

- All existing tests pass without `in_concurrent_batch`.
- No new tests needed (subsumed by Phases 1-3 tests).

### 6.5 Integration Testing

- **LAVD**: run existing LAVD scenarios with each phase enabled.
  Verify valid (but possibly different) traces.
- **Preemptive**: combine with preemptive interleaving for maximum
  interleaving coverage.
- **Replay**: record a trace with the new architecture, replay it,
  verify identical execution.

## 7. Concrete File Changes by Phase

### Phase 1: Yield Refactor

| File | Changes |
|------|---------|
| `interleave.rs` | Add `on_yield` callback to `TokenRing`. Add `yield_to_engine` method. Default: delegate to `yield_token`. |
| `backend/mod.rs` | Add `yield_to_engine()` to `ThreadOrchestrator` trait with default impl. |
| `preempt/mod.rs` | No changes (PreemptRing's yield delegates to same PRNG logic). |
| `engine.rs` | No changes (uses default `on_yield = None`). |

### Phase 2: Eliminate Post-Processing

| File | Changes |
|------|---------|
| `engine.rs` | Add `DsqConsume`, `StartRunning`, `KickDelivered` to `EventKind`. Add handlers in `process_event`. Refactor `post_dispatch_run` to schedule events instead of running them. Delete `process_kicked_cpus`. Remove `in_concurrent_batch` from `process_batch_concurrent`. |
| `kfuncs.rs` | Replace `kicked_cpus: BTreeMap<CpuId, KickFlags>` with `staged_events: Vec<(TimeNs, EventKind)>` in `SimulatorState`. Modify `sim_scx_bpf_kick_cpu` to stage a `KickDelivered` event. Remove `in_concurrent_batch` field. |
| `scenario.rs` | Add `dsq_consume_ns`, `running_overhead_ns`, `ipi_delivery_ns`, `update_idle_overhead_ns` to `OverheadConfig`. |
| `engine.rs` | Add staged-event flush point after each callback in `process_event` and `batch_worker_body`. |

### Phase 3: Dynamic Concurrency Window

| File | Changes |
|------|---------|
| `engine.rs` | Replace main event loop with dynamic event scheduler. Replace `process_batch_concurrent` with dynamic worker pool. Delete `group_events_by_cpu`. Merge `dispatch_concurrent` into dynamic scheduler. |
| `interleave.rs` | Wire `on_yield` to the engine's `pick_next_action` logic. |
| `backend/mod.rs` | Implement `yield_to_engine` for `SimRing` to wake the engine thread. |
| `preempt/mod.rs` | Implement `yield_to_engine` for `PreemptRing` variant. |
| `preempt/trace.rs` | Extend replay format with dynamic event discovery records. |

### Phase 4: Remove `in_concurrent_batch`

| File | Changes |
|------|---------|
| `kfuncs.rs` | Delete `in_concurrent_batch` field from `SimulatorState`. |
| `engine.rs` | Remove all `in_concurrent_batch` reads/writes (already done in Phase 2, this phase is cleanup verification). |

## 8. Implementation Order

### Phase 1: Yield Refactor (2-3 steps)

1. Add `on_yield` callback infrastructure to `TokenRing` and
   `ThreadOrchestrator`. Default `None` preserves current behavior.
2. Add `yield_to_engine` method with default delegation.
3. Unit tests verifying backward compatibility and custom callback
   routing.

### Phase 2: Eliminate Post-Processing (4-5 steps)

1. Add new `EventKind` variants and their `cpu()` mappings.
2. Add `staged_events` to `SimulatorState`, replace `kicked_cpus`.
3. Modify `sim_scx_bpf_kick_cpu` to stage events instead of storing
   kicks.
4. Add staged-event flush point, refactor `post_dispatch_run` and
   `dispatch_concurrent` to schedule events.
5. Add cost table to `OverheadConfig` and `Scenario`.

### Phase 3: Dynamic Concurrency Window (4-5 steps)

1. Implement `pick_next_action` decision logic.
2. Replace main event loop with dynamic scheduler.
3. Implement worker pool with spawn-on-demand.
4. Wire `on_yield` to engine decision logic.
5. Extend replay format and tests.

### Phase 4: Remove `in_concurrent_batch` (1 step)

1. Delete the field and all references. Verify tests pass.

## Appendix A: Code References

All paths relative to `crates/scx_simulator/src/`:

**Engine (engine.rs)**
- Line 156: `EventQueue` struct
- Line 215: `EventQueue::drain_at()` -- current same-ns batching
- Line 239: `EventKind` enum
- Line 302: `EventKind::cpu()` -- per-CPU vs global classification
- Line 326: `group_events_by_cpu()` -- batch partitioning
- Line 494: `charge_sched_time()` -- clock advancement
- Line 1248: Main event loop (`'event_loop`)
- Line 1453: `process_event()` -- event dispatch
- Line 2273: `handle_task_wake()` -- wake handling + concurrent dispatch
- Line 2881: `try_dispatch_and_run()` -- sequential dispatch
- Line 2965: `post_dispatch_run()` -- DSQ consume + start_running
- Line 3037: `dispatch_concurrent()` -- concurrent dispatch
- Line 3249: `process_batch_concurrent()` -- concurrent batch
- Line 3500: `process_kicked_cpus()` -- kick processing
- Line 3688: `start_running()` -- task start + ops.running()

**Kfuncs (kfuncs.rs)**
- Line 179: `SimulatorState` struct
- Line 251: `kicked_cpus: BTreeMap<CpuId, KickFlags>`
- Line 385: `in_concurrent_batch: bool`
- Line 537: `advance_cpu_clock()` -- per-CPU clock sync
- Line 939: `clock_window_check()` -- Phase 2 hook (no-op)
- Line 956: `with_sim()` -- kfunc state access pattern
- Line 809: `enter_sim()` / line 831: `exit_sim()`

**Interleave (interleave.rs)**
- Line 57: `TokenRing` struct
- Line 155: `yield_token()` -- PRNG-driven worker selection
- Line 182: `wait_all_done()` -- engine blocks here
- Line 259: `maybe_yield()` -- cooperative yield entry point

**Preempt (preempt/mod.rs)**
- Line 1506: `maybe_yield_preemptive()` -- preemptive yield
- Line 1523: `maybe_yield_preemptive_post()` -- post-kfunc yield
- Line 1531: `cooperative_yield_impl()` -- shared yield body

**Backend (backend/mod.rs)**
- Line 61: `ThreadOrchestrator` trait
- Line 116: `SendPtr<T>` -- raw pointer wrapper with unsafe Send+Sync
- Line 272: `drain_structop_accum()` -- unsafe worker accounting
- Line 294: `clear_ops_and_finish()` -- unsafe ops cleanup
- Line 334: `run_dispatch_with_orchestrator()`
- Line 436: `run_batch_with_orchestrator()`

**Scenario (scenario.rs)**
- `OverheadConfig` -- context switch overhead settings
- `Scenario` -- simulation parameters
