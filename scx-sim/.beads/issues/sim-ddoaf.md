---
title: 'FutexPingPong lowering ignores the declared worker count: 8 workers become 2 tasks'
status: open
priority: 2
issue_type: task
created_at: 2026-08-14T06:56:22.964514559+00:00
updated_at: 2026-08-14T06:56:22.964514559+00:00
---

# Description

lower.rs W::FutexPingPong builds Plan { tasks: 2, kind: PingPong { work } } with the task count HARDCODED to 2. The declared WorkSpec worker count (n) is not consulted.

Observed on ktstr cross_affinity_churn_runs_in_vm (branch feat/ktstr-scenario-dsl-v2, 22cde3bb), which declares .workers(8) for its FutexPingPong WorkSpec. The exported record carries workers: 8. The compiled Scenario has TWO tasks for that spec. The VM runs 8. Nothing anywhere reports the reduction -- 0 export gaps, and the fidelity report's FutexPingPong entries mention the blocking mechanism and the iteration conversion but not the worker count.

A ping-pong needs an even number of participants, so 2 is a defensible MINIMUM, but silently discarding a declared 8 is not the same thing. Either honour n (4 pairs for n=8), or refuse an n the arm cannot represent, or at minimum record an approximation naming the discarded count -- silence is the one option that is wrong, because it makes a 4x smaller workload look like the declared one.

Same question applies to FutexFanOut and any other arm with a hardcoded Plan.tasks.

Evidence: exported record works[0].workers=8; compiled Scenario tasks = 4 total (2 futexpp + 2 affchurn) on a scenario declaring 10 workers.
