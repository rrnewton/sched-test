---
title: Simple scheduler stalls under cooperative interleaving
status: open
priority: 2
issue_type: bug
created_at: 2026-03-14T17:14:39.248206127+00:00
updated_at: 2026-03-14T17:14:39.248206127+00:00
---

# Description

30-minute stress test found 66 stalls with the simple scheduler under cooperative interleaving. Affects lavd_dsq_stress (4,8 CPUs) and dsq_contention (2 CPUs) workloads. Also some under preemptive mode. Error: ErrorStall. Repro: scxsim run workloads/lavd_dsq_stress.json -s simple -c 4 --seed <see findings> --watchdog-timeout 2s --end-time 4s --interleave
