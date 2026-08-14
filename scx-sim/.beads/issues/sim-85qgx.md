---
title: Attribute-level oracles are dropped from the export with ZERO gaps
status: open
priority: 2
issue_type: task
created_at: 2026-08-14T06:56:39.403052063+00:00
updated_at: 2026-08-14T06:56:39.403052063+00:00
---

# Description

The exporter emits an ExportGap when a ScenarioDef carries an Assert override ('Assert override at scenario -- the record carries the workload, not the oracle', seen on sched_perf_positive). It emits NOTHING when the same oracle is declared on the #[ktstr_scenario] ATTRIBUTE instead.

Measured on cross_affinity_churn_runs_in_vm (22cde3bb), whose attribute declares
sustained_samples = 25, max_keep_last_rate = 1e9, max_fallback_rate = 1e9 and
watchdog_timeout_s = 15. Export reports:

  exported cross_affinity_churn_runs_in_vm -> ... (0 gap(s))

and the record's top-level keys are exactly
[default_workers_per_cgroup, duration, name, steps, topology] -- no assert, no
checks, no rate bounds, nothing.

So the oracle is dropped either way; only ONE of the two paths says so. The
inconsistency matters because it inverts the natural reading: 'the oracle lives
in the attribute rather than in a with_checks override, so it carries through'
is exactly backwards -- living in the attribute is precisely why the exporter
never sees it, since the exporter is handed a ScenarioDef.

CONSEQUENCE FOR CROSS-BACKEND CLAIMS: a scenario's declared invariant is
evaluated on the VM and NOT evaluated on the simulator. 'It passed on both
backends' is therefore not a statement anyone can currently make about an
attribute-declared oracle -- the simulator side never ran the check. The replay
harness compares per-cgroup CPU time and nothing else.

Fix: emit the same gap for an attribute-declared oracle as for a with_checks
one, so the drop is visible at export time. That is a one-sided change (more
gaps, no behaviour change) and it makes the limitation legible instead of
folklore.
