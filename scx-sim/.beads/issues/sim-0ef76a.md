---
title: EngineRing futex handoff causes dsq_contention regression and hangs
status: open
priority: 0
issue_type: bug
created_at: 2026-03-24T20:20:15.314389489+00:00
updated_at: 2026-03-24T20:20:15.314389489+00:00
---

# Description

## Problem

The centralize-dispatch refactoring (Phases A-C) replaced PRNG-based worker
selection with engine-mediated min-local-clock selection via EngineRing with
futex handoff. Benchmarks show severe regression on contention-heavy workloads:

- dsq_contention/simple/preemptive-pmu: 11x -> 2x (-82%)
- dsq_contention/lavd/preemptive-pmu: 2.6x -> 0.6x (-79%, slower than real-time)
- dsq_contention/lavd/interleave: intermittent hangs (2/3 runs timed out at 30s)

Sequential mode is unaffected (-1% to -3%, within noise).
two_runners workload is neutral (-4% overall).

## Root Cause Hypothesis

The futex round-trip to the engine thread adds latency under heavy contention.
With many workers yielding frequently (dsq_contention has 8 tasks), the
serialized engine_loop becomes a bottleneck. The intermittent hangs suggest
a possible deadlock or livelock in the futex handoff path.

## Possible Fixes

1. Batch engine decisions: accumulate multiple yields before waking engine
2. Fast-path: if only one non-finished worker, skip the engine round-trip
3. Profile the futex wake/wait overhead in dsq_contention scenarios
4. Check for subtle race in the park-before-wake ordering under high contention

## Benchmark Data

Branch: centralize-dispatch
Baseline: simulator.v4 (cd9b73f)
See sim-c20fb9 for the full plan.
