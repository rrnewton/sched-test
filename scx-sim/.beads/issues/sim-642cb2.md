---
title: 'scxsim: derive per-CPU utilization automatically for cosmos cpu_util_map (deadline-mode substrate)'
status: open
priority: 2
issue_type: task
created_at: 2026-07-23T03:25:23.635818561+00:00
updated_at: 2026-07-23T03:25:23.635818561+00:00
---

# Description

COSMOS reads cpu_util_map[cpu] (per-CPU user utilization, [0..1024]) in is_cpu_busy() to decide when to switch from per-CPU round-robin queues to the global deadline DSQ (task_dl path). In production, cosmos userspace (main.rs) polls per-CPU utilization every --polling-ms and writes cpu_util_map. scxsim does not model this: the sim engine tracks per-CPU busy time but never populates cpu_util_map, so is_cpu_busy() is always false and the deadline-mode / task_dl path is unreachable by default.

Interim: cosmos/wrapper.c now backs cpu_util_map and exposes cosmos_set_cpu_util(nr_cpus, util) so tests can play userspace's role explicitly (test_shared_dsq_contention sets util=1024 for a saturated oversubscribed run, matching real utilization for that workload).

TODO: have the sim engine compute per-CPU utilization from actual busy time and refresh cpu_util_map periodically (mirroring the userspace poll loop), so is_cpu_busy() reflects real load without the test knob. Filed during tg write-cosmos-tests. Relates to cosmos coverage (COVERAGE_AUDIT_20260722.md sec 4.4).
