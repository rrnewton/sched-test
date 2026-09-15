# Architecture Violations Analysis

## Target Architecture

1. **Persistent worker threads**: One OS thread per simulated CPU, stable for
   the ENTIRE simulation. TLS installed once, torn down once.
2. **No concurrent dispatch rounds**: Engine just processes the next event.
3. **No Phase 1/Phase 2**: Engine handles everything uniformly.
4. **ALL CPUs always in scope**: No `dispatch_cpus` subset filtering.
5. **Split TLS lifecycle**: Setup once at pool creation, reset/re-arm per event.

## Current Violations

### A. Concurrent Dispatch Remnants

- **A1. `process_dynamic_window`** (engine.rs:4062-4196): Multi-step batching
  algorithm with window expansion. Should be: pop one event, process it.
- **A2. `dispatch_concurrent` Phase 1/Phase 2** (engine.rs:3739-3921): Explicit
  concurrent dispatch phase followed by sequential DSQ fallback / start_running.
- **A3. `process_batch_concurrent`** (engine.rs:4234-4432): Creates per-round
  EngineRing, PreemptRing, saves/restores RBC counters.
- **A4. `group_events_by_cpu`** (engine.rs:732-745): Partitions events into
  global/per-CPU buckets for batch treatment.
- **A5. `drain_concurrent_window`** (engine.rs:544-573): Special EventQueue
  method for window expansion.
- **A6. `drain_at`** (engine.rs:528-537): Drains all events at one timestamp
  into Vec for batch processing.

### B. Per-Round Lifecycle

- **B1. EngineRing created per round** (5 locations): Heap-allocates per-worker
  atomic state every dispatch/batch call.
- **B2. PreemptRing created per round** (2 locations): Allocates 426KB
  PreemptionRecordStore per round.
- **B3. Preempt TLS installed/uninstalled per round** (pmu.rs:89-97, 164-166).
- **B4. Interleave TLS installed/uninstalled per round** (dispatch_pool.rs:133, 146).
- **B5. PMU fds opened/closed per round** (pmu.rs:56-61, 164-167):
  perf_event_open + close per dispatch round.
- **B6. Signal handler installed/uninstalled per round** (pmu.rs:41-45).
- **B7. SIM_ARC reinstalled per round** (7 locations).
- **B8. Structop counters seeded per round**.

### C. Subset Dispatch

- **C1. `dispatch_cpus` filtered** (engine.rs:3753-3760): Only idle CPUs with
  empty local DSQs included.
- **C2. Reactive dispatch on idle CPUs** (engine.rs:3153-3177).
- **C3. `dispatch_cpus.len() < 2` fallback** to sequential.
- **C4. `per_cpu.len() >= 2` threshold** for concurrent batching.

### D. 16 Dispatch Function Variants

backend/mod.rs: 7 functions. dispatch_pool.rs: 4 functions. engine.rs: 5
routing/wrapper functions. Target: ~2 functions (engine side + worker side).

### E. Event Batching

- **E1. Same-timestamp grouping** via `drain_at`.
- **E2. Dynamic window expansion** algorithm (engine.rs:4144-4192).
- **E3. `HashMap<CpuId, Vec<Event>>` bucketing** (engine.rs:4246-4248).

### F. Worker Thread Lifecycle

- **F1. 6 functions use `std::thread::scope`** per round.
- **F2. DispatchPool halfway**: Persistent threads but per-round EngineRing/TLS.
- **F3. Preemptive pool disabled** (TODO sim-b431b6).
- **F4. TLS that is per-round but should be per-simulation**: INTERLEAVE_CTX,
  PREEMPT_CTX, STRUCTOP counters, SIM_ARC.

## Unified Fix Plan

### Step 1: Eliminate event batching (E, A4-A6)

Change main loop to: pop one event, process it. Delete drain_at,
drain_concurrent_window, group_events_by_cpu, process_dynamic_window,
process_batch_concurrent, process_events_sequential.

Interleaving still happens naturally: worker runs one structop, yields,
engine picks next worker by min-clock (which may be a different CPU).

### Step 2: Persistent workers with split TLS lifecycle (B, F)

N persistent OS threads, one per CPU. At creation: open PMU fds, install
all TLS, create one persistent EngineRing. Per event: reset/re-arm PMU
counters, update ops_context. No per-round teardown/reinstall.

### Step 3: Eliminate Phase 1/Phase 2 (A1-A3)

Engine handles DSQ fallback, start_running, update_idle sequentially on the
engine thread after each worker yield. No concurrent phase distinction.

### Step 4: Unify dispatch variants (D)

16 functions collapse to ~2: engine_dispatch (wake worker, run engine loop)
and worker_body (wait, run C code, yield).

### Step 5: ALL CPUs in scope (C)

Natural consequence of Step 1 — no dispatch_cpus filtering needed.

### Dependencies

Step 1 → Step 5 (consequence)
Step 1 → Step 2 → Step 3 → Step 4
