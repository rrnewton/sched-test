---
title: ktstr exporter maps only 4 of 45 work types; the rest become ExportGaps
status: open
priority: 2
issue_type: task
created_at: 2026-08-14T04:53:02.572988055+00:00
updated_at: 2026-08-14T04:53:02.572988055+00:00
---

# Description

work_type() in ktstr src/scenario/export.rs maps SpinWait, YieldHeavy, Mixed and (as of feat/ktstr-scenario-io-compute-imbalance) IoSyncWrite. The other 41 fall to '_ => None' and become an ExportGap, which DROPS that workspec from the exported record.

Found while getting cover_cgroup_io_compute_imbalance to run: IoSyncWrite was refused at this gate even though scxsim-workload-ir's SourceWorkType has all 45 variants and lower.rs handles 42. So the ktstr exporter, not the IR, is the narrow point in the chain.

Most of the remaining mappings are verbatim variant-to-variant with no fields, i.e. one line each. The ones carrying tuning knobs need the fields carried across too. Each should be added WITH a scenario that exercises it end to end -- an unexercised mapping is untested, and IoSyncWrite is the proof that this path had never been run.

Note the ordering constraint: adding a mapping is only useful once the scenario using it can reach the exporter at all (see the per-binary registry issue).
