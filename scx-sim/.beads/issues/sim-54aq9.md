---
title: 'record_replay_determinism #[ignore] is stale on PMU-capable hardware — passes in 0.3s doing real work'
status: open
priority: 2
issue_type: task
created_at: 2026-08-12T14:13:12.345489179+00:00
updated_at: 2026-08-12T14:13:12.345489179+00:00
---

# Description

Found during `investigate-14-skipped-tests`.

crates/scx_simulator/tests/determinism.rs:242
  #[ignore = "requires PMU hardware + HW breakpoints (unavailable in CI); see mb sim-70abc8"]

Empirically it PASSES on this bare-metal box and does real work — verified with --no-capture: two full simulations, byte-identical results (354 structops / 115 kfuncs both runs). It does NOT silently no-op, and the replay backend did not panic.

So the blanket #[ignore] means PMU-capable dev machines get zero coverage of record/replay determinism, even though the test works there. The gate is a static attribute where it should be a runtime capability check.

Not enabled by this investigation because unlike the stress trio the CI environment genuinely may lack PMU/HW-breakpoints, and per scx-sim CLAUDE.md 'No Silent Failures' the replay backend PANICS by design when breakpoints are unavailable — so naively removing #[ignore] would turn CI red rather than skip.

Proposed: detect PMU + HW-breakpoint capability at runtime; run and assert when capable, emit a REAL skip (counted as skipped, not passed) when not. Coordinate with sim-hdsgn, which proposes the same capability-gate mechanism for the scx_perf / compare / perfetto silent skips. Related: sim-70abc8.
