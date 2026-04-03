---
title: Purge old batch/phase architecture, complete persistent worker model
status: open
priority: 0
issue_type: feature
created_at: 2026-04-03T18:54:50.069483355+00:00
updated_at: 2026-04-03T18:54:50.069483355+00:00
---

# Description

## Context

The centralize-dispatch refactoring (Phases A-E) introduced EngineRing for
centralized scheduling decisions, but the old batch/phase/window architecture
remains intact around it. We are in a "weird part of the design space" where
the new orchestrator is grafted onto the old dispatch machinery. This issue
tracks the complete purge of the old architecture.

## Target Architecture

1. **One persistent OS thread per simulated CPU**, stable for the entire
   simulation. TLS (PMU fds, preempt context) installed once at creation.
2. **No "concurrent dispatch rounds"**. The engine just processes the next
   event. Period.
3. **No Phase 1/Phase 2**. Engine handles DSQ fallback, start_running,
   update_idle uniformly after each worker yield.
4. **ALL CPUs always in scope**. No dispatch_cpus subset filtering.
5. **No event batching**. Engine pops one event, processes it. Interleaving
   happens naturally via min-clock worker selection.

## Current Violations (from architecture_violations.md analysis)

### A. Concurrent Dispatch Remnants (6 violations)
- A1. process_dynamic_window (engine.rs:4062): Multi-step batching + window expansion
- A2. dispatch_concurrent Phase 1/Phase 2 (engine.rs:3739): Concurrent dispatch then sequential DSQ fallback
- A3. process_batch_concurrent (engine.rs:4234): Per-round EngineRing/PreemptRing creation
- A4. group_events_by_cpu (engine.rs:732): Global/per-CPU event partitioning
- A5. drain_concurrent_window (engine.rs:544): Special EventQueue method for window expansion
- A6. drain_at (engine.rs:528): Batch all same-timestamp events

### B. Per-Round Lifecycle (8 violations)
- B1. EngineRing created per round (5 locations)
- B2. PreemptRing created per round (426KB allocation each time)
- B3-B4. Preempt + interleave TLS installed/uninstalled per round
- B5. PMU fds opened/closed per round (perf_event_open + close)
- B6. Signal handler installed/uninstalled per round (sigaction)
- B7. SIM_ARC reinstalled per round (7 locations)
- B8. Structop counters seeded per round

### C. Subset Dispatch (4 violations)
- C1. dispatch_cpus filtered to idle CPUs with empty local DSQ
- C2. Reactive dispatch on idle CPUs from handle_task_wake
- C3-C4. Fallback to sequential when < 2 CPUs

### D. 16 Dispatch Function Variants
- backend/mod.rs: 7 functions
- dispatch_pool.rs: 4 functions (2 dead_code)
- engine.rs: 5 routing/wrapper functions
- Target: ~2 functions (engine side + worker side)

### E. Event Batching (3 violations)
- E1. drain_at groups same-timestamp events
- E2. Dynamic window expansion algorithm
- E3. HashMap<CpuId, Vec<Event>> bucketing

### F. Worker Thread Lifecycle (4 violations)
- F1. 6 functions use std::thread::scope per round
- F2. DispatchPool halfway (persistent threads but per-round EngineRing/TLS)
- F3. Preemptive pool disabled (sim-b431b6)
- F4. TLS that is per-round but should be per-simulation

## Implementation Plan

### Step 1: Persistent Workers with Split TLS Lifecycle

Create N persistent OS threads at simulation start, one per simulated CPU.
Each thread at creation:
- Opens PMU timer fd and measurement counter fd (once, via perf_event_open)
- Installs signal handler (once, via sigaction)
- Installs PREEMPT_CTX with stable pointers (once)
- Installs INTERLEAVE_CTX with stable EngineRing pointer (once)
- Parks until woken by engine

Per-event updates (cheap, no syscalls):
- Reset/re-arm PMU counter (ioctl only, no open/close)
- Update ops_context via CALLBACK_CTX
- enter_sim/exit_sim for CPU identity

Key changes:
- Split PreemptionBackend into: initial_setup / round_reconfigure / final_teardown
- Heap-allocate one EngineRing per simulation (not per round)
- Heap-allocate one PreemptRing per simulation with reset() method
- Move signal handler install to simulation start

Files: preempt/mod.rs, backend/mod.rs, backend/pmu.rs, dispatch_pool.rs,
       engine_ring.rs, engine.rs

### Step 2: Eliminate Event Batching

Change main event loop from:
  drain_at(t) -> group_events_by_cpu -> process_batch_concurrent
To:
  event = events.pop() -> process_event(event)

The engine pops one event at a time. If that event is on CPU X, the engine
wakes worker X's persistent thread. The worker runs one structop, yields
back. The engine picks the next worker by min-clock (which may be any CPU).

Interleaving still happens: multiple CPU workers have pending structops
and the engine picks between them by min-clock at every yield point. The
"batch" is implicit — defined by which CPUs have pending work.

Delete: drain_at, drain_concurrent_window, group_events_by_cpu,
process_dynamic_window, process_batch_concurrent

Files: engine.rs (EventQueue, main loop, process_dynamic_window)

Risk: Changes interleaving granularity from "batch of same-timestamp events"
to "one event at a time". Different interleavings explored. Existing seeds
produce different traces. This is intentional and correct.

### Step 3: Eliminate Phase 1/Phase 2

dispatch_concurrent disappears. The engine:
1. Pops next event
2. If it triggers dispatch (CPU goes idle): wakes worker, worker runs
   ops.dispatch(), yields back
3. Engine does DSQ fallback, start_running, update_idle — all sequentially
   on the engine thread

The "concurrent dispatch followed by sequential post-processing" pattern
is replaced by "engine processes events, sometimes waking workers for C code."

Delete: dispatch_concurrent, post_dispatch_run (merge into unified handler),
the Phase 1/Phase 2 comments

Files: engine.rs

### Step 4: Unify Dispatch Function Variants

16 functions collapse to ~2:
- engine_dispatch: engine side (wake worker, run engine loop)
- worker_body: worker side (wait for token, run C code, yield back)

The cooperative/preemptive distinction collapses because PMU preemption
is just another yield reason. The pooled/scoped distinction disappears
because all workers are persistent. The dispatch/batch distinction
disappears because the engine just hands work to workers event by event.

Delete: All 7 backend/mod.rs dispatch functions, all 4 dispatch_pool.rs
functions, all 5 engine.rs routing functions. Replace with unified path.

Files: backend/mod.rs, dispatch_pool.rs, engine.rs

### Step 5: ALL CPUs Always in Scope

Natural consequence of Step 2. No dispatch_cpus filtering. Engine just
processes whatever event is next on any CPU.

Delete: dispatch_cpus computation, the < 2 CPU fallbacks

Files: engine.rs

## Dependencies

Step 1 → Step 2 → Step 5 (consequence)
                → Step 3 → Step 4

Steps 1 and 2 are the heavy lifts. Steps 3-5 are mostly deletion.

## Agent Assignment (for parallel execution)

- **Agent A** (Step 1): Persistent workers + split TLS lifecycle.
  Heavy unsafe code, PMU fd management, signal handlers.
  Worktree: sched-test2

- **Agent B** (Step 2): Eliminate event batching + simplify main loop.
  Pure engine.rs refactor, event queue changes.
  Worktree: sched-test3
  Depends on Agent A's EngineRing changes.

- **Agent C** (Steps 3-5): Delete old dispatch machinery.
  Mostly deletion after Steps 1-2 land.
  Worktree: sched-test4
  Depends on both A and B.

## Verification

After each step:
- cargo test (all 590+ tests pass)
- cargo clippy (zero warnings)
- validate.sh
- Benchmark: dsq_contention/simple/interleave target < 100ms
- Stress: python3 bug_finding/stress.py --duration 5 --no-e9patch
- Determinism: python3 bug_finding/stress.py --duration 5 --determinism --no-e9patch

## See Also
- sim-c20fb9: original centralize-dispatch plan
- sim-0ef76a: EngineRing deadlock (fixed)
- sim-b431b6: preemptive pool TLS issue (subsumed by this plan)
