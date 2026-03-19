---
title: PMU preemptive livelock with small timeslices (< longest_structop_rbc / 8)
status: open
priority: 1
issue_type: bug
created_at: 2026-03-19T19:42:17.503280966+00:00
updated_at: 2026-03-19T19:42:17.503280966+00:00
---

# Description

When timeslice_min is smaller than ~1/8 of the scheduler's longest structop RBC,
the preemptive PMU mode livelocks probabilistically (0-70% completion across seeds).

Data from LAVD timeslice overhead study (debug/lavd_timeslice_overhead.csv):
- LAVD longest_structop_rbc = 2026
- TS < 250: 0-70% completion rate (livelock)
- TS >= 300: 100% completion, ~25% overhead

The livelock is NOT signal overhead (the signal handler is <1% of CPU time).
It appears to be a token ring thrashing interaction where the PREEMPT_INHIBIT
mechanism + SIM_ARC mutex + futex yield/wake create a live-lock when signals
fire too frequently within a structop.

The default timeslice_min should be raised to max(300, longest_structop_rbc / 5)
for reliability. An adaptive approach based on the longest_structop_rbc stat
would be ideal.

Workaround: use --timeslice-min 300 --timeslice-max 1500 for LAVD.
