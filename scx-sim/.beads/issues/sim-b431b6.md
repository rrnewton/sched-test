---
title: Wire WorkerPool into preemptive dispatch paths
status: open
priority: 1
issue_type: feature
created_at: 2026-03-26T21:28:42.183946131+00:00
updated_at: 2026-03-26T21:28:42.183946131+00:00
---

# Description

## Problem

The cooperative dispatch paths use the persistent WorkerPool (DispatchPool),
eliminating per-round std::thread::scope overhead. But preemptive dispatch
still falls back to std::thread::scope because of TLS lifecycle issues.

The comment in engine.rs:3969-3972 says:
"Preemptive pool disabled: signal handler + perf event TLS requires per-round
thread identity, which persistent threads don't guarantee across
backend.worker_setup/worker_teardown cycles."

## Root Cause

The PreemptionBackend trait has worker_setup/worker_teardown methods that are
called once per dispatch round. They:
1. Create per-thread PMU timer fds (perf_event_open)
2. Create per-thread measurement counter fds
3. Install PREEMPT_CTX TLS with the timer/measurement fds
4. On teardown: uninstall TLS, close fds

With persistent threads, worker_setup/worker_teardown would be called on the
same OS thread across rounds. The issue is that PMU perf events are tied to
the thread that created them. If a persistent thread calls worker_teardown
(closing fds) then worker_setup (opening new fds), the perf events must be
re-created on the same thread.

The agent that wired cooperative dispatch found that when running multiple
simulations sequentially with the preemptive pool, an infinite event loop
occurred, suggesting the PMU counter state was corrupted across rounds.

## Proposed Fix

Split PreemptionBackend lifecycle into:
- worker_setup / worker_teardown: once at pool create/destroy (install TLS,
  create perf fds)
- round_begin / round_end: per dispatch round (reset counters, re-arm timer,
  update EngineRing pointer in TLS)

This keeps the perf fds alive across rounds (same thread, same fds) while
allowing per-round state reset.

## Impact

dsq_contention/simple/preemptive-pmu is still 2x (down from 11x baseline)
because it uses std::thread::scope. With the pool, cooperative interleave
recovered to ~9x (matching baseline). The same recovery is expected for
preemptive once this is fixed.

## See Also
- sim-c20fb9: centralize-dispatch plan
- sim-0ef76a: EngineRing deadlock (fixed)
