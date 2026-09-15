---
title: 'REGRESSION: 25 clippy errors in safe/engine.rs - unused variables and dead assignments'
status: closed
priority: 1
issue_type: bug
created_at: 2026-03-14T11:14:48.994942890+00:00
updated_at: 2026-03-14T11:16:38.397771362+00:00
closed_at: 2026-03-14T11:16:38.397771272+00:00
---

# Description

The safety refactor introduced 25 clippy errors (treated as errors via -D warnings) in crates/scx_simulator/src/safe/engine.rs. All are 'unused variable s' or 'value assigned to guard is never read' errors. These are all in the new lock-drop-relock pattern where a MutexGuard is dropped, SIM_ARC is installed, C code is called, and then the guard is re-acquired. The pattern has dead code in the re-acquisition where s or guard is assigned but never read. This blocks validate.sh from passing. The baseline (sched-test3) has clean clippy.
