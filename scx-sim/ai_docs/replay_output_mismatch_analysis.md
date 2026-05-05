# Replay Output Mismatch Analysis

**Issue**: sim-54dac3 — 267 `replay_output_mismatch` findings from expanded stress tests
**Date**: 2026-03-18

## Executive Summary

The replay output divergence is NOT caused by the preemption backend (PMU skid,
breakpoint timing, etc). It is caused by a **fundamental architectural gap**:
replay reconstructs synthetic compute-only tasks instead of the original workload.
The trace file does not store workload behavior (sleep/wake phases), so replay
creates N pure-compute tasks, producing entirely different simulation metrics.

## Key Findings

### 1. Backend comparison is moot: both backends produce identical replay output

Tested 80 configurations (20 seeds x 2 schedulers x 2 CPU counts):

| Backend            | Match | Mismatch | Fail |
|--------------------|-------|----------|------|
| PMU + HW breakpoint| 0/80  | 80/80    | 0/80 |
| Breakpoint-only    | 0/80  | 80/80    | 0/80 |

In every single case, the breakpoint-only mode (`--no-pmu-signal`) produces
**byte-identical output** to the default PMU+BP mode. The divergence is 100%
between recording and replay, but 0% between backends.

### 2. Replay-to-replay is perfectly deterministic

Two replays of the same trace produce byte-identical output every time. The
replay engine itself is deterministic. The problem is solely in what it replays.

### 3. Root cause: synthetic workload in replay

The replay code (`replay_simulation()` in `main.rs`, lines 736-756) builds a
synthetic scenario with pure-compute tasks:

```rust
let compute_phase = Phase::Run(10_000_000);   // 10ms run, no sleep
let behavior = TaskBehavior {
    phases: vec![compute_phase],
    repeat: RepeatMode::Forever,
};
```

The trace file stores metadata (`nr_cpus`, `nr_tasks`, `seed`, `duration_ns`,
`scheduler`, `timeslice_min/max`, `so_hash`) but NOT the workload definition
(task phases, sleep durations, wake targets).

### 4. Divergence pattern is systematic and predictable

For a `two_runners.json` workload (tasks with run+sleep phases):

| Metric              | Recording | Replay |
|---------------------|-----------|--------|
| total_events        | ~625      | ~1146  |
| total_ticks         | ~75       | ~250   |
| total_yields        | 0         | ~98    |
| total_preempts      | 0         | 0      |
| total_sleeps        | 50        | 0      |
| total_wakes         | 50        | 2      |
| total_idle_periods  | 50        | 0      |
| global_dsq_dispatch | 0         | ~98    |

Recording tasks sleep/wake (producing idle periods), while replay tasks only
run (producing yields). CPUs in replay never go idle, so they tick ~3x more
often (4ms intervals vs 10-20ms).

Even with a pure-compute workload, metrics diverge:

| Metric              | Recording | Replay |
|---------------------|-----------|--------|
| total_time_slices   | 50        | 100    |
| total_preempts      | 48        | 0      |
| total_yields        | 0         | 98     |

Recording generates engine-level preemptions (timeslice expiry), while replay
tasks complete their 10ms phases and yield normally.

### 5. What the trace actually records

The preemption trace records **mid-structop preemptions** (where the scheduler's
C code was interrupted by the PMU timer) — NOT engine-level preemptions (where a
task's timeslice expired). These are different concepts:

- Trace preemption: scheduler callback interrupted at a specific RIP/RBC count
- Engine preemption: simulation task was preempted due to timeslice expiry

A typical trace has very few points (e.g., 4 entries for a 500ms simulation with
`timeslice_min=timeslice_max=1`), because `timeslice=1` retired conditional
branch + PMU skid means preemptions happen rarely.

## Why the stress test sees 267 mismatches

The stress test (`run_determinism_preemptive`) compares `_extract_simulation_metrics`
between the recording stdout and the replay stdout. Since replay always uses
synthetic compute-only tasks, ANY workload with sleep/wake phases will produce
different metrics. The 267 out of 1650 runs reflects the proportion of test
configurations that use workloads with non-trivial behavior.

Note: `run_only` random workloads might also mismatch because the recording's
preemptive mode causes engine-level preemptions (total_preempts > 0) that don't
reproduce in replay (which produces yields instead).

## Recommendations

### Option A: Store workload in trace (full fidelity replay)
Add the workload JSON path and/or serialized task behaviors to the trace file
header. Replay would reconstruct the exact same workload. This is the ideal
fix but requires trace format changes.

### Option B: Fix the metric comparison in stress.py
Since replay intentionally uses synthetic tasks, the `_check_replay_vs_record`
comparison should either:
1. Only compare structop-level metrics (structops, kfuncs) which are controlled
   by the preemption trace, not workload-level metrics
2. Be removed entirely, keeping only `_check_replay_replay` (replay-to-replay
   determinism)

### Option C: Both
Store workload in the trace AND fix the stress test comparison. The trace-stored
workload enables true record-replay fidelity, while the stress test fix prevents
false positives for existing traces.

## e9patch Backend Status

The e9patch backend (`--preempt-mode e9patch`) requires `_e9.so` scheduler
variants built with `make install-e9patch && make -C schedulers e9`. These are
not currently built in this worktree. Since the breakpoint backend comparison
shows the divergence is backend-independent, testing e9patch would not change
the conclusion. However, e9patch replay should produce identical replay output
to breakpoint replay for the same trace (same synthetic workload, same
preemption points).
