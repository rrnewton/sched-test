---
title: lower.rs models IoSyncWrite as 50% off-CPU; measured is ~21%
status: open
priority: 2
issue_type: task
created_at: 2026-08-14T05:37:33.749062666+00:00
updated_at: 2026-08-14T05:37:33.749062666+00:00
---

# Description

The IO arm of lower.rs lowers IoSyncWrite/IoRandRead/IoConvoy to Run(DEFAULT_SLICE) Sleep(DEFAULT_SLICE) -- a 50/50 duty cycle. A VM run of cover_cgroup_io_compute_imbalance on 6.14.11 measures the real worker off-CPU 21.39% and 22.22% on two runs, not 50%.

Consequence, same scenario, 4 CPUs / 12s:
  cg_0 CPU time   VM 9,476,392,819 ns (19.72%)  SIM 730,961,186 ns (1.52%)   12.96x
  second run      VM 9,392,820,188 ns (19.48%)                               12.85x

The direction matters and is counter-intuitive: buffered sync writes mostly do
NOT block -- they complete into page cache -- so the worker is largely ON-CPU.
The lowering assumes the opposite.

This is the disclosed-but-wrong case, not the undisclosed one. PR #127 made the
report record UnspecifiedWorkQuantum for this arm, so a consumer is now TOLD the
quantum was invented. This issue is about the invented value also being far from
what the hardware does.

DO NOT fix by hard-coding 21.4%. That is one measurement, one host, one
page-cache state, and substituting it would be the same fabrication with better
provenance -- and it would then read as measured rather than invented. Options,
in order of preference:

1. ktstr declares the quantum (compute between writes, and/or expected block
   time), so the IR carries a source-derived number and the arm stops inventing.
2. If the arm must keep a default, keep it disclosed AND make the disclosure
   carry the measured discrepancy, so nobody reads Run/Sleep 500us as physical.
3. Refuse IoSyncWrite at the lowering rather than approximate it, consistent
   with how Schbench/Taobench/Custom are refused. This is the most honest option
   and the most disruptive: it would un-port the scenario.

Evidence: sidecar
scratch/ktstr-vm-target/ktstr/6.14.11-c1be982/cover_cgroup_io_compute_imbalance-*.ktstr.json
fields stats.cgroups[].avg_off_cpu_pct and total_cpu_time_ns. Blocks any
cross-backend share bound for IO scenarios (see sim-1zqbl).
