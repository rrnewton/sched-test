---
title: SIGSTKFLT crash during preemptive recording with mitosis
status: open
priority: 2
issue_type: task
created_at: 2026-03-03T18:46:22.377742544+00:00
updated_at: 2026-03-03T18:46:22.377742544+00:00
---

# Description

stress.py --determinism finds reproducible SIGSTKFLT (signal 16) crashes when running mitosis scheduler with --preemptive --record-preemptions. Affects multiple workloads (two_runners, dsq_contention) and CPU counts (2, 4, 8). Also occasionally affects lavd scheduler. SIGSTKFLT is used internally by the preemptive interleaving mechanism but should be caught, not crash the process.
