---
title: export_registered_scenarios only sees scenarios in its own test binary
status: open
priority: 2
issue_type: task
created_at: 2026-08-14T04:53:02.575656724+00:00
updated_at: 2026-08-14T04:53:02.575656724+00:00
---

# Description

KTSTR_SCENARIOS is a per-test-binary registry. export_registered_scenarios lives in ktstr tests/ktstr_sched_tests.rs and walks the registry as linked into THAT binary. A #[ktstr_scenario] declared in tests/scenario_coverage.rs is a different link unit and is never exported -- so it never reaches the simulator.

It refuses nothing and errors nowhere. The scenario simply does not appear downstream, which is the same silent-narrowing shape that every_record_has_a_baseline_and_a_declared_expectation exists to catch, one level further out.

This has never bitten because all five originally-ported scenarios happen to live in ktstr_sched_tests.rs. It bites immediately for the ~90 remaining conversion candidates, the bulk of which are in scenario_coverage.rs.

Worked around for cover_cgroup_io_compute_imbalance by MOVING it into ktstr_sched_tests.rs (see rrnewton/ktstr feat/ktstr-scenario-io-compute-imbalance). That does not scale.

Fix: factor the export loop into a shared helper in ktstr's src/scenario/export.rs and call it from each test binary that declares scenarios, or add a check that fails when a registered scenario is not covered by any export step.
