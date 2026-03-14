---
title: Replay SIGABRT crashes during preemptive determinism testing
status: open
priority: 2
issue_type: bug
created_at: 2026-03-14T17:14:47.731384547+00:00
updated_at: 2026-03-14T17:14:47.731384547+00:00
---

# Description

30-minute stress test found 45 SIGABRT crashes during preemptive replay (record+replay determinism path). Affects mitosis (most: dsq_contention, lavd_dsq_stress, simple_wake, two_runners on 2-8 CPUs) and lavd (simple_wake, 4 CPUs). The REPLAY MISMATCH shows ops context divergence (trace=none, replay=select_cpu or running). Related to but distinct from sim-c3fd09 (SIGSTKFLT crashes). Repro: scxsim run workloads/simple_wake.json -s lavd -c 4 --seed 754214214 --watchdog-timeout 2s --end-time 4s --preemptive --record-preemptions /tmp/repro.preempt && scxsim replay /tmp/repro.preempt
