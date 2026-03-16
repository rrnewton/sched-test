---
title: 'Pre-existing: test_preemptive_custom_timeslice hangs indefinitely'
status: open
priority: 1
issue_type: bug
created_at: 2026-03-14T11:14:59.890940194+00:00
updated_at: 2026-03-14T11:14:59.890940194+00:00
---

# Description

The test test_preemptive_custom_timeslice in crates/scx_simulator/tests/interleave.rs hangs indefinitely (>30s timeout). It uses PreemptiveConfig with custom timeslice_min=50, timeslice_max=200 and cooperative_only=false (PMU mode). The simulation never completes. This is a pre-existing issue: the test also hangs on the sched-test3 baseline. The test should either have a timeout or be marked #[ignore].
