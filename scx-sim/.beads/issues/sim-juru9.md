---
title: 'scx_layered Tier 3: integrate real layer_core_growth policy beyond flat Linear'
status: open
priority: 2
issue_type: task
depends_on:
  sim-lqyu9: discovered-from
created_at: 2026-08-12T20:24:41.992489961+00:00
updated_at: 2026-08-12T20:27:56.661925157+00:00
---

# Description

The first Tier-3 increment supports only one LLC, one harness node, no SMT, and Linear growth. It specializes the upstream flat Linear ordering with a source drift guard and rejects every other growth algorithm. Full Tier 3 must execute scheduler-owned layer_core_growth policy rather than add more local approximations. NUMA behavioural modelling is explicitly out of scope here and belongs to S692395.

# Acceptance Criteria

Integrate upstream layer_core_growth.rs or land an upstream pure-policy refactor that scxsim can compile directly; remove the local flat-Linear specialization where the real module supersedes it; support and behaviorally prove at least one non-Linear growth mode with a negative control/sabotage; preserve fail-loud behavior for any still-unsupported modes; do not add NUMA substrate or synthetic PMU inputs.
