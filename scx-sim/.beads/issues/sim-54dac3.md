---
title: Replay output differs from recording — 267 mismatches found by expanded stress tests
status: closed
priority: 1
issue_type: bug
created_at: 2026-03-18T02:52:48.814893033+00:00
updated_at: 2026-03-21T01:15:17.612842383+00:00
closed_at: 2026-03-21T01:15:17.612842293+00:00
---

# Description

Expanded stress.py determinism coverage (replay output comparison) found 267 cases where PMU record + HW breakpoint replay succeeds (exit 0) but the simulation metrics differ between recording and replay. Examples: total_events, total_ticks, total_sleeps differ. This means replay is not faithfully reproducing the recording's execution, even though it completes without error. Found by: stress.py --duration 2 --determinism --no-e9patch --random-workloads (1650 runs total).
