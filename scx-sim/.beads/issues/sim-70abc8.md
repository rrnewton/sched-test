---
title: Determinism failures persist across all schedulers and modes
status: closed
priority: 1
issue_type: bug
depends_on:
  sim-e0791: related
created_at: 2026-03-14T17:15:02.657643074+00:00
updated_at: 2026-03-21T01:00:53.545184527+00:00
closed_at: 2026-03-21T01:00:53.545184437+00:00
---

# Description

PMU hardware nondeterminism — NOT a simulator logic bug.

INVESTIGATION FINDINGS:

1. Root cause: The determinism check records PMU RBC (Retired Branch Conditional) hardware counter values at checkpoint events. These hardware counters have inherent nondeterminism — identical code paths can produce slightly different counts between runs due to CPU microarchitectural effects (context switches, interrupts, pipeline state).

2. Evidence:
   - With --no-rbc: 30/30 determinism checks PASS (100%)
   - With RBC enabled: ~90% pass, ~10% fail with small RBC deltas (1-8 branches)
   - With --interleave --no-rbc: 20/20 PASS
   - Failure checkpoints are random (different each time)
   - Only RBC field diverges; RIP, memory hash, event type, CPU all match
   - The code itself documents this: 'PMU: Nondeterministic due to PMU skid and real CPU scheduling' (engine.rs:929)

3. NOT related to sim-e0791: That issue had CPU mismatches, memory hash divergences, and event type mismatches — genuine simulator logic bugs. This issue is exclusively RBC count mismatches with all other fields matching perfectly.

4. The original stress test finding of 2162 failures across 30 minutes is consistent with ~10% PMU failure rate on thousands of runs.

5. Resolution options:
   a) EXPECTED BEHAVIOR: Exclude RBC from determinism comparison when using PMU engine (recommended)
   b) Use --preempt-engine e9patch for deterministic RBC (already available)
   c) Add a tolerance window for RBC comparison (e.g. allow delta <= 10)
   d) Document as known limitation of PMU mode

RECOMMENDATION: Close as 'expected behavior' or downgrade to cosmetic. The determinism check should either skip RBC comparison in PMU mode, or use a tolerance. The simulator logic itself is fully deterministic (proven by --no-rbc passing 100%).
