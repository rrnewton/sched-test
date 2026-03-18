# PRNG Sequence Divergence in Replay Mode

*Date: 2026-03-18*

## Executive Summary

This document investigates whether PRNG sequence divergence can cause replay
output mismatches. The analysis finds that **the PRNG architecture is carefully
designed to avoid divergence** in most cases, but **two confirmed issues exist
with the retry path** that can cause silent mismatches.

## 1. PRNG Architecture Overview

The simulator uses **three independent PRNG instances**, each with a distinct
role:

### 1.1. SimulatorState::rng (SmallRng, seed-derived)

- **Location**: `SimulatorState.rng` in `kfuncs.rs:253`
- **Seed**: `SmallRng::seed_from_u64(scenario.seed as u64)` (engine.rs:1333)
- **Consumers**:
  - `sim_bpf_get_prandom_u32()` -- called by scheduler C code during callbacks
  - `sample_normal_ns()` -- tick jitter, CSW overhead noise (4 PRNG calls each)
  - `next_prng()` -- called once per dispatch round to derive `interleave_seed`
    (engine.rs:3663, engine.rs:4023)
- **Thread safety**: accessed only under the `SimArc` mutex

### 1.2. PreemptRing::prng (xorshift32, AtomicU32)

- **Location**: `PreemptRing.prng` in `preempt/mod.rs:1142`
- **Seed**: `interleave_seed` (derived from `SimulatorState::rng` per dispatch round)
- **Consumers**:
  - `pick_next()` -- selects next worker on yield (both cooperative and signal-driven)
  - `roll_timeslice()` -- generates random PMU timeslice period (recording mode)
  - `build_target()` in all backends -- consumes PRNG via `roll_timeslice()`
  - `rearm_timer()` -- consumes PRNG at every kfunc boundary via `roll_timeslice()`
- **Thread safety**: CAS loop on AtomicU32 (signal-handler safe)
- **Lifetime**: created fresh per dispatch round (in `run_preemptive_dispatch`)

### 1.3. EventQueue::event_rng (SmallRng, seed-derived with offset)

- **Location**: `EventQueue.event_rng` in `engine.rs:474`
- **Seed**: `(scenario.seed as u64) ^ 0x5A5A_5A5A_5A5A_5A5A` (engine.rs:483)
- **Purpose**: randomized event tiebreaking at identical timestamps
- **Isolated**: completely independent from the other two PRNGs

## 2. PRNG Sync Between Recording and Replay

The replay system is designed to consume PRNG tokens at exactly the same points
as the recording. Here is how each sync point works:

### 2.1. PreemptRing::prng Sync

**Recording (PmuBackend):**
1. `build_target()` calls `ring.roll_timeslice(min, max)` -> consumes 1 PRNG
2. At each kfunc boundary, `rearm_timer()` calls `ring.roll_timeslice(min, max)` -> consumes 1 PRNG
3. At each yield (cooperative or signal-driven), `yield_token()` calls `pick_next()` -> consumes 1 PRNG
4. At `finish()`, calls `pick_next()` -> consumes 1 PRNG

**Replay (ReplayBackend):**
1. `build_target()` calls `ring.roll_timeslice(min, max)` then **discards** the result -> consumes 1 PRNG (SYNC)
2. At each kfunc boundary, `rearm_timer()` with `replay_mode=true` calls
   `ring.roll_timeslice()` then discards the result -> consumes 1 PRNG (SYNC)
3. Yields consume PRNG identically (same `pick_next()` logic)
4. `finish()` consumes PRNG identically

**Verdict**: The code correctly synchronizes `roll_timeslice` consumption.
Comments in `replay.rs:37-46` explicitly document this requirement.

### 2.2. SimulatorState::rng Sync

During a dispatch round, the scheduler C code may call `sim_bpf_get_prandom_u32()`
which consumes from `SimulatorState::rng`. If the scheduler makes the same calls
in the same order, the PRNG stays in sync.

**Critical insight**: `SimulatorState::rng` state is derived from the scenario
seed and consumed deterministically by the engine. The `interleave_seed` is
derived from it once per dispatch round BEFORE workers are spawned. Workers do
NOT consume `SimulatorState::rng` from the PreemptRing -- they only consume from
the PreemptRing's own PRNG.

However, workers CAN consume `SimulatorState::rng` indirectly via
`sim_bpf_get_prandom_u32()`. If replay has a different number of scheduler C code
calls (due to divergent behavior), `SimulatorState::rng` will desync.

## 3. Confirmed Issue: Retry Does Not Restore SimulatorState::rng

### The Problem

In `replay_dispatch_with_retry` (backend/mod.rs:765-841):

```
// engine.rs:3663 -- BEFORE calling replay_dispatch_with_retry
let interleave_seed = s.sim.next_prng();   // (A) consumes SimulatorState::rng

// backend/mod.rs -- retry loop
for attempt in 1..=REPLAY_PMU_MAX_RETRIES {
    reset_replay_state();        // resets REPLAY_OVERSHOT flag
    backend.reset_cursors();     // resets cursor positions
    run_preemptive_dispatch(     // runs workers who may call sim_bpf_get_prandom_u32()
        dispatch_cpus,
        state_send, sched_send, sim_arc,
        seed,       // <-- same seed each attempt (correct: PreemptRing is recreated)
        backend,
    );
    // ...
}
```

When a retry happens:
1. `PreemptRing` is recreated with the same `seed` -- **CORRECT**, the
   PreemptRing PRNG restarts identically.
2. `ReplayCursor` positions are reset -- **CORRECT**.
3. `SimulatorState::rng` is **NOT reset** -- **BUG if the scheduler called
   `sim_bpf_get_prandom_u32()` during the failed attempt**.

If the scheduler C code calls `sim_bpf_get_prandom_u32()` during dispatch (which
is common -- schedulers use it for load balancing, CPU selection, etc.), the
failed attempt consumes N PRNG values from `SimulatorState::rng`. On retry, the
state is different. Subsequent PRNG calls from the scheduler return different
values, causing scheduling decisions to diverge.

### Why This May Be Masked

The retry path is only invoked when `REPLAY_OVERSHOT` is set (PMU skid past
the target). For most dispatch rounds, the PMU lands correctly and no retry
occurs. When a retry does happen:

- If the scheduler does NOT call `sim_bpf_get_prandom_u32()`, there's no
  divergence (because `SimulatorState::rng` is untouched by the dispatch body).
- If noise/overhead is disabled (the common test configuration), `tick_jitter`
  and `csw_overhead` don't consume `SimulatorState::rng` during dispatch either.

So the bug only manifests when:
1. PMU overshoot triggers a retry, AND
2. The scheduler calls `sim_bpf_get_prandom_u32()` during dispatch

### Severity Assessment

**Medium-Low**. The retry path is a fallback for PMU skid, which is rare on
modern hardware. However, when it does trigger, the PRNG desync propagates
to ALL subsequent dispatch rounds (because `SimulatorState::rng` is used to
derive the next `interleave_seed`).

## 4. Confirmed Issue: SimulatorState Side Effects Are Not Rolled Back

Beyond `SimulatorState::rng`, the retry also does not restore:

- **DSQ state**: scheduler dispatch calls modify DSQs. If the failed attempt
  dispatched tasks to DSQs, those dispatches are NOT undone.
- **task_ops_state**: the ops_state map may have been modified.
- **pending_dispatch**: may have been consumed.
- **staged_events**: may have been populated.

These are arguably more severe than the PRNG issue since they affect the
structural state of the simulation, not just the PRNG sequence.

However, these side effects are likely masked because the dispatch round
operates within the worker thread's `dispatch_worker_body` which stages
events rather than applying them directly to the event queue. The main engine
thread applies staged events after the dispatch round completes. If the
dispatch round is retried, the staged events from the failed attempt would
need to be discarded.

**TODO**: Verify whether `staged_events` is properly cleared between retry
attempts.

## 5. Non-Issue: PreemptRing PRNG Is Correctly Reset

The `PreemptRing` is created fresh with `PreemptRing::new(n, seed)` inside
`run_preemptive_dispatch()`, which is called on each retry. This means the
PreemptRing PRNG is correctly reset to the initial `interleave_seed`. All
`pick_next()` and `roll_timeslice()` calls within the ring start from the
same state on each retry. **No issue here.**

## 6. Non-Issue: Cooperative Yield PRNG Consumption

In cooperative interleaving (non-preemptive mode), the `TokenRing` uses
`SmallRng` seeded from `interleave_seed`. Each `maybe_yield()` call
in `interleave.rs` calls `ring.yield_token()` which calls `state.pick_next()`
consuming one PRNG token.

In preemptive mode, `maybe_yield_preemptive()` calls `ring.yield_token()`
on the `PreemptRing` which uses `next_prng()` (xorshift32).

Both paths consume PRNG identically between recording and replay because:
- The yield points are at the same kfunc boundaries
- `rearm_timer()` explicitly consumes PRNG in both recording and replay mode
- The recording trace captures the exact sequence of yield/signal events

**No issue here**, as long as the dispatch round itself is not retried.

## 7. Non-Issue: PREEMPT_INHIBIT Deferred Preemption

When `PREEMPT_INHIBIT` is set (inside kfunc boundaries), the PMU signal handler
disables the timer and returns without yielding. The yield happens at the next
cooperative yield point (`maybe_yield_preemptive` or `maybe_yield_preemptive_post`).

This does NOT affect PRNG consumption because:
- The signal handler does NOT consume PRNG (no `pick_next()` call when inhibited)
- The deferred cooperative yield DOES consume PRNG (via `yield_token()`)
- The same deferral pattern occurs in both recording and replay (same C code,
  same kfunc boundaries)

**No issue here.**

## 8. Proposed Fixes

### Option A: Save/Restore SimulatorState on Retry (Recommended)

Before each dispatch round, checkpoint the relevant SimulatorState fields.
On retry, restore them:

```rust
// In replay_dispatch_with_retry, before the loop:
// Save: rng state, DSQ state, task_ops_state, pending_dispatch, staged_events
// On retry: restore from checkpoint
```

This is the most correct fix because it ensures the entire simulation state
matches the recording's starting state for each attempt.

**Complexity**: Medium -- `SmallRng` does not implement `Clone`, but the
state can be serialized/deserialized. Alternatively, re-seed from the
scenario seed and replay the exact consumption sequence up to this point
(expensive). Simpler: save the raw RNG bytes via `SmallRng::from_rng()`.

Actually, `SmallRng` in the `rand` crate does NOT implement `Clone`, but
it implements `SeedableRng`, so you can create a checkpoint via:
```rust
let rng_state = sim.rng.clone(); // if Clone is derived/implemented
// ... or save the seed consumption count and re-derive
```

### Option B: Separate Sub-PRNG for sim_bpf_get_prandom_u32

Decouple the scheduler's PRNG from the simulator's structural PRNG:

- `SimulatorState::rng` -- used ONLY by the engine (interleave_seed,
  tick_jitter, csw_overhead)
- `SimulatorState::sched_rng` -- used ONLY by `sim_bpf_get_prandom_u32()`

Both seeded from the scenario seed but with different offsets (like
EventQueue::event_rng already does).

**Advantage**: Even without retry restore, the engine's structural PRNG
is not consumed by scheduler code. The `interleave_seed` derivation is
unaffected by scheduler behavior.

**Disadvantage**: Changes the PRNG sequence for existing seeds, breaking
determinism comparison with older runs.

### Option C: Make the Token-Ring Deterministic Without PRNG (Round-Robin)

Not viable -- the PRNG-driven selection is intentional for stress testing
different interleavings. Round-robin would explore only one interleaving per
seed, defeating the purpose.

### Option D: Avoid Retries Entirely

Use breakpoint-only mode (`--no-pmu-signal`) which has no PMU skid and
therefore no retries. This is already the recommended fallback.

**Advantage**: Simplest fix, no code changes needed.
**Disadvantage**: Breakpoint-only mode is slower (single-stepping through
every instance of the target instruction).

## 9. Recommendations

1. **Short-term**: Default to `--no-pmu-signal` for replay to avoid the
   retry path entirely (Option D).

2. **Medium-term**: Implement Option A (SimulatorState checkpoint/restore)
   in the retry loop. This ensures correctness when PMU retries are needed.

3. **Long-term**: Consider Option B (separate scheduler PRNG) as a defense-
   in-depth measure. This isolates the engine's structural decisions from
   scheduler behavior, making the system more robust against future PRNG
   consumption mismatches.

## 10. Files Referenced

- `crates/scx_simulator/src/unsafe_impl/kfuncs.rs` -- SimulatorState::rng, next_prng(), sim_bpf_get_prandom_u32()
- `crates/scx_simulator/src/unsafe_impl/preempt/mod.rs` -- PreemptRing, rearm_timer(), signal handler
- `crates/scx_simulator/src/unsafe_impl/backend/mod.rs` -- replay_dispatch_with_retry()
- `crates/scx_simulator/src/unsafe_impl/backend/replay.rs` -- ReplayBackend, PRNG sync comments
- `crates/scx_simulator/src/unsafe_impl/backend/pmu.rs` -- PmuBackend::build_target()
- `crates/scx_simulator/src/unsafe_impl/backend/e9patch.rs` -- E9PatchReplayBackend
- `crates/scx_simulator/src/unsafe_impl/interleave.rs` -- TokenRing, maybe_yield()
- `crates/scx_simulator/src/safe/engine.rs` -- dispatch_concurrent, interleave_seed derivation
