---
title: PMU preemptive mode hangs with multi-worker workloads (default timeslice=1)
status: closed
priority: 2
issue_type: task
created_at: 2026-03-18T14:48:46.254573551+00:00
updated_at: 2026-03-18T14:48:50.194808189+00:00
closed_at: 2026-03-18T14:48:50.194808109+00:00
---

# Description

Default timeslice_min=1, timeslice_max=1 causes massive signal overhead with multi-worker workloads (e.g. dsq_contention: 8 tasks on 4 CPUs). The PMU fires after every ~30 branches (1 + skid), generating ~40,000 signals/ms with 4 workers. A 200ms sim hangs for minutes. Fix: raise defaults to min=100, max=500.
