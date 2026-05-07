---
title: 'scx-sim: run_duration fidelity — simulator reports 132µs for 500µs configured (4.4x error)'
status: open
priority: 2
issue_type: bug
labels:
- post-1.0
- fidelity
created_at: 2026-04-27T10:46:07.435804648+00:00
updated_at: 2026-04-27T10:46:07.435804648+00:00
---

# Description

Source: rcmode cross-mode consistency test (2026-04-27).
Reference: ~/work/multi_sched-test/ai_docs/RC_TEST_CROSS_MODE_20260427.md

When the rt-app spec configures run=500µs, the simulator reports run_duration ≈ 132µs (4.4x off). The pinned (real-system) run reports approximately 500µs as expected. This is a fundamental fidelity bug — the simulator is NOT accurately reproducing the workload it's claimed to simulate.

Action:
1. Reproduce: run the same rt-app spec in BOTH rtapp_pinned and rtapp_sim modes; confirm the 132µs vs 500µs delta.
2. Identify where the discrepancy enters: run-time accounting? Frame timing? Loop modeling? Inspect engine.rs run-duration accounting + how rt-app run loops are translated to sim work.
3. Fix the root cause (not a multiplier band-aid).
4. Verify: re-run the same rt-app spec; sim run_duration must match pinned within ±10%.
