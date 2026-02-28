# Widened Concurrency Window: Implementation Plan

## Issue: sim-a730ac (extension)

## 1. Problem Statement

The simulator currently batches events for concurrent processing only when they
share the **exact same nanosecond timestamp** (`drain_at(t)` in
`engine.rs:215`). But in the real kernel, events on different CPUs at nearby
timestamps (e.g., T and T+1ns) execute concurrently because scheduler callbacks
take hundreds of nanoseconds. A tick handler starting at T=4,000,000ns on CPU0
is still executing when CPU1's slice expiry at T=4,000,050ns begins -- these
callbacks interleave in the real kernel but are processed strictly sequentially
by the simulator.

This limits the simulator's ability to find concurrency bugs that only manifest
when callbacks on different CPUs overlap in time.

## 2. Key Design Insight: Dynamic, Clock-Driven Window

Rather than a static time window (e.g., "batch all events within 100ns"), the
window should be **dynamic and driven by the cost model**:

1. The engine pops the minimum-time event and starts executing its structop on
   the assigned CPU.
2. As the structop executes, `charge_sched_time` advances the CPU's
   `local_clock` by the measured/modeled cost of each callback.
3. At each preemption/yield point, the engine checks the event queue: are there
   pending events on OTHER CPUs whose timestamps fall between the batch's base
   time and the executing CPU's current `local_clock`?
4. If so, those events become eligible for concurrent interleaving.
5. The concurrent set grows organically as the structop runs -- it is
   **unbounded** in principle (bounded only by BPF's instruction limit per
   callback).

This is architecturally a **conservative PDES protocol** where the CPU's
elapsed execution time serves as the lookahead.

## 3. Current Architecture Summary

### 3.1 Event Loop (engine.rs:1233)

```
while let Some(t) = events.peek_time() {
    state.clock = t;
    let batch = events.drain_at(t);       // EXACT same nanosecond
    let (global, per_cpu) = group_events_by_cpu(batch);
    // 1. Process global events sequentially
    // 2. Process per-CPU events concurrently if 2+ CPUs
}
```

### 3.2 Concurrent Batch Processing (engine.rs:3220)

`process_batch_concurrent` spawns one worker thread per CPU. Workers are
serialized by a token ring (cooperative mode) or preempt ring (preemptive mode)
-- only one thread runs at a time. The batch is formed BEFORE workers spawn;
workers cannot discover additional events mid-execution.

### 3.3 Cost Model and Clock Advancement

- `advance_cpu_clock(cpu)` (kfuncs.rs:537): sets `local_clock = max(local_clock, state.clock)`.
- `charge_sched_time(cpu, ops)` (engine.rs:482): reads RBC count (or kfunc
  cost fallback) and adds the elapsed time to `local_clock`.
- `clock_window_check(cpu, local_clock)` (kfuncs.rs:939): a Phase 2 hook
  called after every clock advance. Currently a no-op.

### 3.4 Two Concurrent Entry Points

1. **Event-loop batching**: Groups same-timestamp per-CPU events for
   concurrent processing.
2. **Dispatch concurrent** (engine.rs:3008): When a task wakes and 2+ CPUs are
   idle, dispatch callbacks run concurrently. This path already fires
   regardless of timestamps.

### 3.5 Token Ring and Worker Lifecycle

Workers follow a strict lifecycle:
1. `worker_setup` -- create instrumentation
2. `wait_for_token` -- block until selected
3. `enter_sim(state, cpu)` -- enter simulation context
4. Worker body runs (dispatch or batch event processing)
5. `disarm` + `drain_structop_accum` + `clear_ops_and_finish`
6. `worker_teardown`

The number of workers is fixed at batch creation time and cannot change
mid-batch.

## 4. Detailed Design

### 4.1 Overview: Two-Phase Approach

**Phase A (Static Widening)**: Replace `drain_at(t)` with a wider drain that
captures events within a configurable time window. This is a simple change that
exercises the concurrency machinery with larger batches. The window represents
the **minimum possible callback cost** -- a conservative estimate of how long
any structop takes.

**Phase B (Dynamic Widening)**: At yield points during concurrent execution,
check whether the executing CPU's `local_clock` has advanced far enough to
bring new events from the queue into the concurrent set. This is the full
dynamic window from the design insight.

Phase A is a prerequisite for Phase B. Phase A can be shipped independently as
a useful improvement.

### 4.2 Phase A: Static Window Widening

#### 4.2.1 New `drain_up_to` Method

Add to `EventQueue`:

```rust
/// Pop all events with timestamps in [t, t + window].
/// Returns events sorted by (time_ns, seq) for deterministic ordering.
fn drain_up_to(&mut self, t: TimeNs, window: TimeNs) -> Vec<Event> {
    let cutoff = t.saturating_add(window);
    let mut batch = Vec::new();
    while let Some(Reverse(e)) = self.heap.peek() {
        if e.time_ns > cutoff {
            break;
        }
        batch.push(self.heap.pop().unwrap().0);
    }
    batch
}
```

Note: when `window == 0`, this is equivalent to the current `drain_at(t)`.

#### 4.2.2 New Scenario Parameter

Add to `Scenario` and `ScenarioBuilder`:

```rust
/// Concurrency window: maximum time span (ns) for batching events
/// from different CPUs into a concurrent group.
///
/// Events within [T, T + concurrency_window_ns] are eligible for
/// concurrent interleaving. A value of 0 recovers the current strict
/// same-nanosecond batching.
///
/// Default: 0 (backward compatible).
pub concurrency_window_ns: TimeNs,
```

Add a `--concurrency-window <NS>` flag to the `scxsim` CLI.

#### 4.2.3 Event Loop Changes

In the main event loop (engine.rs:1233), replace:

```rust
let batch = events.drain_at(t);
```

with:

```rust
let batch = events.drain_up_to(t, concurrency_window_ns);
```

The rest of the pipeline (partitioning by `group_events_by_cpu`, global-first
sequential processing, per-CPU concurrent processing) remains unchanged.

#### 4.2.4 Clock Handling for Mixed-Timestamp Batches

When a batch spans multiple timestamps, `state.clock` is set to the **earliest**
timestamp (the `t` from `peek_time()`). This is correct because:

- `advance_cpu_clock(cpu)` already does `local_clock = max(local_clock, state.clock)`.
- Per-CPU events at later timestamps within the batch will have their
  `local_clock` set to `max(local_clock, event.time_ns)` inside
  `process_event` (engine.rs:1451-1462).

**Important**: The current `advance_cpu_clock` uses `state.clock` (the global
clock). For mixed-timestamp batches, we need to advance the CPU clock to the
**event's** timestamp, not the batch's base timestamp. This requires a small
change in `process_event`:

Currently (engine.rs:1451-1462):
```rust
EventKind::SliceExpired { cpu } | ... => {
    state.advance_cpu_clock(*cpu);  // uses state.clock
}
```

We need `advance_cpu_clock` to accept an explicit timestamp argument:

```rust
pub fn advance_cpu_clock_to(&mut self, cpu: CpuId, event_time: TimeNs) {
    let idx = cpu.0 as usize;
    self.cpus[idx].local_clock = self.cpus[idx].local_clock.max(event_time);
    clock_window_check(cpu, self.cpus[idx].local_clock);
}
```

And in `process_event`, use the event's own timestamp:

```rust
EventKind::SliceExpired { cpu } | ... => {
    state.advance_cpu_clock_to(*cpu, event.time_ns);
    kfuncs::set_sim_clock(state.cpus[cpu.0 as usize].local_clock, Some(*cpu));
}
```

The existing `advance_cpu_clock` (which uses `state.clock`) remains for
compatibility with callers in `dispatch_concurrent` and
`process_batch_concurrent` where the global clock is the right reference.

#### 4.2.5 `bpf_ktime_get_ns` and `scx_bpf_now` Semantics

Both `bpf_ktime_get_ns()` (kfuncs.rs:1504) and `scx_bpf_now()` (kfuncs.rs:1374)
already return the per-CPU `local_clock`, not `state.clock`. This is correct
for mixed-timestamp batches -- each CPU sees its own clock.

#### 4.2.6 Replay Trace Compatibility

The concurrency window must be recorded in replay trace metadata. When
replaying, the same window must be used to produce the same batch structure.
If the window differs, the number of workers per batch changes, invalidating
recorded interleaving sequences.

Add `concurrency_window_ns` to `PreemptionTrace` header.

#### 4.2.7 Determinism

Phase A is fully deterministic for a given `(seed, concurrency_window_ns)`:

- The batch contents are determined by `drain_up_to(t, window)`, which is a
  deterministic function of the event queue state and the window parameter.
- Within-batch ordering uses the existing PRNG-based tiebreaking.
- Token ring interleaving is seed-driven.

### 4.3 Phase B: Dynamic Window (Clock-Driven Event Discovery)

Phase B activates the `clock_window_check` hook to dynamically discover new
concurrent events during structop execution.

#### 4.3.1 The Discovery Loop

The fundamental idea: after a worker's `charge_sched_time` advances its
CPU's `local_clock`, check whether the event queue contains events on OTHER
CPUs with timestamps that fall between the batch's base time and any CPU's
current `local_clock`. If so, those events should join the concurrent batch.

This creates a feedback loop:
```
1. Worker A (CPU0) executes a structop, advancing CPU0.local_clock
2. Engine checks: events on CPU1 at time <= CPU0.local_clock?
3. If yes: spawn Worker B for CPU1
4. Worker B executes, advancing CPU1.local_clock
5. Engine checks: events on CPU2 at time <= max(CPU0.local_clock, CPU1.local_clock)?
6. If yes: spawn Worker C for CPU2
7. ... continues until no more events qualify
```

The loop terminates because:
- BPF has a finite instruction limit per callback (~1M instructions).
- Each structop has bounded execution cost.
- The event queue is finite.

#### 4.3.2 Why Dynamic Discovery is Hard with Current Architecture

The current architecture pre-computes the batch before spawning workers. Adding
workers to an in-progress concurrent batch requires:

1. Growing the token ring (adding new worker slots).
2. Spawning new OS threads mid-batch.
3. Synchronizing the new worker with existing workers.
4. Handling the case where a new worker's event is on a CPU already in the batch.

This is complex and fragile.

#### 4.3.3 Recommended Approach: Multi-Round Micro-Batching

Instead of modifying workers mid-execution, use multiple rounds:

```
loop {
    // Determine window: max(local_clock[cpu] for cpu in active_cpus)
    let max_clock = active_cpus.iter()
        .map(|cpu| state.cpus[cpu].local_clock)
        .max()
        .unwrap_or(batch_base_time);

    // Check for new events within the window
    let new_events = events.drain_up_to(max_clock, 0);
    let (global, per_cpu) = group_events_by_cpu(new_events);

    if global.is_empty() && per_cpu.is_empty() {
        break; // No more events in the window
    }

    // Process global events sequentially
    for event in global { process_event(event, ...); }

    // Merge per_cpu events with any leftover from active_cpus
    // Process the merged per-cpu set concurrently
    if per_cpu.len() >= 2 {
        process_batch_concurrent(per_cpu, ...);
    } else {
        // Sequential
    }

    // Update active_cpus based on which CPUs are now involved
}
```

Each round:
1. Processes a concurrent batch.
2. Workers advance their CPUs' `local_clock` via `charge_sched_time`.
3. The engine checks for new events that fall within the widened window.
4. New events become a new micro-batch.

This preserves the existing batch-then-process architecture. The token ring
is created fresh for each micro-batch (cheap -- it's just a mutex + condvar).

#### 4.3.4 Alternative: Activate `clock_window_check` as a Notification Hook

Instead of the engine polling for new events, make `clock_window_check` a
notification channel. When `charge_sched_time` advances `local_clock`, the
hook checks the event queue and signals the engine that new events are
available.

However, this has a fundamental problem: during concurrent batch execution,
the engine thread is blocked in `ring.wait_all_done()`. It cannot process
signals from workers. The multi-round approach avoids this by checking between
rounds.

#### 4.3.5 Interaction with the Token Ring

The token ring model is compatible with multi-round micro-batching:

- Each round creates a fresh token ring for the workers in that round.
- Workers in round N complete before round N+1 begins.
- Between rounds, the engine has exclusive access to `SimulatorState` and can
  safely inspect the event queue.
- The PRNG for each round derives from `state.next_prng()`, maintaining
  determinism.

#### 4.3.6 Interaction with Sequential Post-Processing

Currently, after `process_batch_concurrent` completes:
1. `in_concurrent_batch` is cleared.
2. Kicked CPUs are processed via `process_kicked_cpus`.
3. Each CPU's balance/dispatch post-processing runs.

With multi-round micro-batching, post-processing should happen AFTER all rounds
complete, not after each round. Between rounds, only the event-discovery check
runs. This means `in_concurrent_batch` stays true across all rounds:

```
state.in_concurrent_batch = true;
loop {
    let batch = discover_concurrent_events(...);
    if batch.is_empty() { break; }
    process_micro_batch(batch, ...);
}
state.in_concurrent_batch = false;
// NOW: post-process kicked CPUs, balance, etc.
```

However, this introduces a subtlety: events discovered in later rounds may
have been generated by kicked-CPU processing from earlier rounds. Since
`in_concurrent_batch` suppresses kick processing, the kicks accumulate and
are only processed at the end. This matches the current behavior within a
single batch and should be correct.

#### 4.3.7 Global Events in the Dynamic Window

Global events (TaskWake, TimerFired) discovered in the dynamic window should
be processed sequentially before each micro-batch's per-CPU events, consistent
with the current model. The question is: can a global event at time T+50ns
actually overlap with a per-CPU callback at time T?

In the real kernel: yes. A task wakeup on CPU0 involves `select_cpu` and
`enqueue` callbacks that run in the waker's context. These can race with an
independent tick handler on CPU1. However, global events modify shared state
(DSQ queues, task lists) accessed by all CPUs, making them inherently
sequential in the current model.

**Recommendation**: For Phase B, continue processing global events sequentially
between micro-batch rounds. Adding global events to concurrent batches is a
future enhancement requiring deeper analysis of which global events can safely
interleave.

### 4.4 Detailed Change Inventory

#### engine.rs

| Change | Phase | Description |
|--------|-------|-------------|
| `EventQueue::drain_up_to` | A | New method for windowed draining |
| `advance_cpu_clock_to(cpu, time)` | A | Explicit timestamp variant |
| Main event loop | A | Use `drain_up_to` instead of `drain_at` |
| `process_event` | A | Use event's timestamp for clock advancement |
| Multi-round wrapper | B | Loop around `process_batch_concurrent` checking for new events |

#### kfuncs.rs

| Change | Phase | Description |
|--------|-------|-------------|
| `advance_cpu_clock_to` | A | New method on `SimulatorState` |
| `clock_window_check` | B | Activate as notification channel (optional) |

#### scenario.rs

| Change | Phase | Description |
|--------|-------|-------------|
| `concurrency_window_ns` field | A | New `Scenario` + `ScenarioBuilder` field |
| Builder method | A | `.concurrency_window(ns)` |
| Validation | A | Assert `concurrency_window_ns` compatible with replay |

#### bin/scxsim/main.rs

| Change | Phase | Description |
|--------|-------|-------------|
| `--concurrency-window <NS>` | A | CLI flag |

#### preempt/trace.rs

| Change | Phase | Description |
|--------|-------|-------------|
| Trace header extension | A | Record `concurrency_window_ns` |
| Replay validation | A | Assert window matches recording |

## 5. Implementation Order

### Phase A: Static Window (estimated: 4-6 steps)

1. **Add `drain_up_to`**: New method on `EventQueue`. Add unit tests verifying
   it returns all events in [t, t+window] and is equivalent to `drain_at` when
   window=0.

2. **Add `advance_cpu_clock_to`**: New method on `SimulatorState`. Keep
   existing `advance_cpu_clock` as a convenience that delegates to
   `advance_cpu_clock_to(cpu, self.clock)`.

3. **Add `concurrency_window_ns` to Scenario**: New field with default 0.
   Add builder method. Add CLI flag.

4. **Wire up the event loop**: Replace `drain_at(t)` with
   `drain_up_to(t, concurrency_window_ns)`. Update `process_event` to use
   `advance_cpu_clock_to(cpu, event.time_ns)`.

5. **Replay trace compatibility**: Add `concurrency_window_ns` to trace header.
   Validate on replay.

6. **Tests**: Add integration tests that verify:
   - `concurrency_window_ns=0` produces identical traces to the current behavior.
   - `concurrency_window_ns=100` groups events within 100ns into concurrent
     batches (verify via trace inspection: events at T and T+50 are interleaved).
   - Determinism: same seed + same window produces identical traces across runs.
   - A scenario where ticks on CPU0 and CPU1 are staggered by 50ns: verify they
     are processed concurrently with `concurrency_window_ns=100` but
     sequentially with `concurrency_window_ns=0`.

### Phase B: Dynamic Window (estimated: 4-6 steps)

1. **Multi-round discovery loop**: Wrap the concurrent batch processing in a
   loop that checks for new events after each round.

2. **Window calculation**: Define the window as `max(local_clock[cpu]) - batch_base_time`
   across all CPUs that participated in the just-completed round.

3. **PRNG sequencing**: Ensure each micro-batch round consumes PRNG state
   deterministically. Each round gets its own `state.next_prng()` seed.

4. **Post-processing deferral**: Move kick processing and balance to after all
   rounds complete.

5. **Tests**: Add integration tests that verify:
   - Dynamic discovery: a scenario where CPU0's tick callback advances
     `local_clock` by 200ns, causing CPU1's event at T+150ns to be discovered
     and processed concurrently in a second round.
   - Round termination: verify the loop terminates (no infinite discovery).
   - Determinism: same seed produces identical multi-round traces.

6. **Metrics**: Add counters for:
   - Number of micro-batch rounds per event-loop iteration.
   - Number of events discovered dynamically (not in the initial batch).
   - Maximum observed window width.

## 6. Interaction with Existing Interleaving Modes

### 6.1 Cooperative (TokenRing)

Compatible without changes. The token ring is created per-batch (or per-round
in Phase B). Widening the batch just means more workers per ring. The PRNG
determines interleaving order as before.

### 6.2 Preemptive (PreemptRing / PMU timer)

Compatible without changes. The preempt ring, like the token ring, is created
per-batch. More workers per ring means more interleaving opportunities. The
PMU timer fires at RBC intervals within each worker's execution, independent of
batch formation.

### 6.3 Native Concurrent (NativeOrchestrator)

Compatible without changes. The native orchestrator uses a barrier for
start-of-batch synchronization. A wider batch means more workers hit the
barrier together. Clock-window throttling (`NativeConcurrentConfig::window_ns`)
is orthogonal to the concurrency window -- it controls how far CPUs can race
ahead of each other during native concurrent execution, while
`concurrency_window_ns` controls which events are batched together.

### 6.4 No Interleaving

When `interleave=false` and `preemptive=None`, all events are processed
sequentially. The concurrency window has no effect in this mode (the batch is
still formed but processed sequentially). This is by design -- widening the
window without interleaving would change event ordering without exercising
concurrent interleavings.

**Decision**: When `interleave=false`, `concurrency_window_ns` should be
silently ignored (not an error). Document this in the field's doc comment.

## 7. Edge Cases and Risks

### 7.1 Same-CPU Events at Different Timestamps

If `concurrency_window_ns=100` and events exist at T=1000 on CPU0 and T=1050
on CPU0, both end up in the same batch. `group_events_by_cpu` puts them in the
same per-CPU bucket. They are processed sequentially within that CPU's worker
(by `batch_worker_body` which iterates events in order). This is correct --
same-CPU events are never concurrent.

### 7.2 Very Large Windows

A large `concurrency_window_ns` (e.g., 1ms) could pull in hundreds of events
across many timestamps. This creates very large batches with many workers.
Risks:
- Performance: many OS threads, many context switches in the token ring.
- Memory: each worker's event list is cloned.
- Determinism: no risk (PRNG-driven), but trace analysis becomes harder.

**Mitigation**: Document that typical values are 50-500ns. Consider adding a
maximum batch size (e.g., 64 workers) with a warning if exceeded.

### 7.3 Interaction with IRQ Events

IRQ events (`IrqStart`, `IrqEnd`) are per-CPU. They should participate in
concurrent batching like other per-CPU events. No special handling needed.

### 7.4 Interaction with Cgroup Events

Cgroup events are global (no CPU). They are processed sequentially before
per-CPU events. The widened window may pull in cgroup events at later
timestamps, which will be processed sequentially before the per-CPU concurrent
batch. This is correct.

### 7.5 `state.clock` Semantics

`state.clock` is set to the `peek_time()` value (the earliest event's
timestamp). With a widened window, later events in the batch have timestamps
greater than `state.clock`. This could affect:

- **Watchdog**: Checks `state.clock` against task runnable times. Setting it to
  the earliest time is conservative (may detect stalls earlier than necessary).
  This is safe.
- **Global event clock**: `bpf_ktime_get_ns()` and `scx_bpf_now()` return
  `local_clock`, not `state.clock`, so this is not affected.
- **Timer scheduling**: Timers scheduled during the batch use `state.clock`.
  A timer at T+100 scheduled during a batch spanning [T, T+50] will fire in
  the correct future iteration. This is fine.

### 7.6 Regression Risk

The default `concurrency_window_ns=0` preserves exact backward compatibility.
All existing tests pass without changes. This is the critical invariant for
Phase A.

## 8. Testing Strategy

### 8.1 Unit Tests

- `EventQueue::drain_up_to` equivalence with `drain_at` when window=0.
- `drain_up_to` includes events at exactly `t + window`.
- `drain_up_to` excludes events at `t + window + 1`.
- `drain_up_to` handles empty queue.
- `advance_cpu_clock_to` advances correctly.
- `advance_cpu_clock_to` does not move clock backward.

### 8.2 Integration Tests

- **Backward compatibility**: Run all existing tests with `concurrency_window_ns=0`.
  Verify identical traces.
- **Window effect**: Create a scenario with 2 CPUs, ticks staggered by 50ns.
  Run with `concurrency_window_ns=0` (sequential) and `concurrency_window_ns=100`
  (concurrent). Verify the latter produces interleaved trace events.
- **Determinism**: Run the windowed scenario 10 times with the same seed.
  Verify identical traces each time.
- **LAVD compatibility**: Run existing LAVD tests with
  `concurrency_window_ns=100`. Verify no regressions (LAVD may produce
  different but valid traces due to different interleaving).
- **Dynamic window (Phase B)**: Scenario where CPU0's callback takes 200ns
  (high kfunc cost), and CPU1 has an event at T+150ns. Verify CPU1's event is
  discovered and processed concurrently.

### 8.3 Stress Tests

- Multi-seed sweep with `concurrency_window_ns` values from 0 to 1000ns.
  Look for panics, assertion failures, and watchdog stalls.
- Combine with preemptive interleaving for maximum interleaving coverage.

## 9. Future Enhancements

### 9.1 Cost-Model-Derived Default Window

Instead of a fixed default, derive the window from the cost model:
`concurrency_window_ns = overhead.sched_overhead_rbc_ns * avg_rbc_per_callback`.
This makes the window self-adjusting based on scheduler complexity.

### 9.2 Global Event Interleaving

Allow certain global events (e.g., TaskWake) to participate in concurrent
batches. This requires analyzing which global events can safely interleave with
per-CPU events. The main concern is shared state (DSQ queues, task lists) that
is protected by token-passing serialization but requires careful ordering.

### 9.3 Per-CPU Window

Instead of a global window, use each CPU's `local_clock` as the window
boundary. This is more precise (a CPU that has been busy has a higher clock,
creating a wider window) but adds complexity to batch formation.

## 10. Summary

| Phase | Scope | Complexity | Value |
|-------|-------|------------|-------|
| A: Static Window | `drain_up_to` + scenario param + event loop | Low | High: immediately captures near-simultaneous events |
| B: Dynamic Window | Multi-round discovery loop + post-processing deferral | Medium | Medium: captures cost-model-driven concurrency |

Phase A is the critical deliverable. It is backward compatible (default
window=0), low risk, and immediately enables testing with wider concurrency
batches. Phase B builds on Phase A and adds the dynamic discovery loop for
maximum realism.

## Appendix A: Code References

All paths relative to `crates/scx_simulator/src/`:

- `engine.rs:215` -- `EventQueue::drain_at()`: current same-ns batching
- `engine.rs:314` -- `group_events_by_cpu()`: global vs per-CPU partition
- `engine.rs:482` -- `charge_sched_time()`: cost model clock advancement
- `engine.rs:1233` -- Main event loop
- `engine.rs:3008` -- `dispatch_concurrent()`: concurrent dispatch for idle CPUs
- `engine.rs:3220` -- `process_batch_concurrent()`: concurrent per-CPU batch
- `engine.rs:3385` -- `process_batch_concurrent_cooperative()`: cooperative path
- `kfuncs.rs:537` -- `advance_cpu_clock()`: per-CPU clock sync
- `kfuncs.rs:939` -- `clock_window_check()`: Phase 2 hook (currently no-op)
- `kfuncs.rs:1374` -- `scx_bpf_now()`: returns per-CPU local_clock
- `kfuncs.rs:1504` -- `bpf_ktime_get_ns()`: returns per-CPU local_clock
- `interleave.rs:259` -- `maybe_yield()`: cooperative yield point
- `preempt/mod.rs:1506` -- `maybe_yield_preemptive()`: preemptive yield point
- `cpu.rs:46` -- `SimCpu::local_clock`: per-CPU logical clock
- `scenario.rs:466` -- `Scenario` struct
- `backend/mod.rs` -- `ThreadOrchestrator` trait, generic dispatch/batch drivers
- `backend/native.rs` -- `NativeOrchestrator`: barrier-based native concurrency
