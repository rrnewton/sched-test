---
title: Cosmos scheduler stalls under interleaving modes
status: closed
priority: 1
issue_type: bug
created_at: 2026-03-14T17:14:33.710225849+00:00
updated_at: 2026-03-15T21:01:16.391084735+00:00
closed_at: 2026-03-15T21:01:16.391084614+00:00
---

# Description

30-minute stress test found 218+ stalls with the cosmos scheduler under cooperative and preemptive interleaving. Affects dsq_contention and two_runners workloads on 2, 4, and 8 CPUs. Error: ErrorStall, tasks are runnable but never scheduled within 2s watchdog timeout. Repro: scxsim run workloads/dsq_contention.json -s cosmos -c 4 --seed 3600722315 --watchdog-timeout 2s --end-time 4s --interleave
