---
title: 'scxsim: no cgroup cpu.weight substrate — proportional-by-cgroup-weight not runnable'
status: open
priority: 2
issue_type: task
labels:
- scxsim
- cgroup
- substrate-gap
created_at: 2026-07-23T04:39:31.473740677+00:00
updated_at: 2026-07-23T04:39:31.473740677+00:00
---

# Description

Cgroup-v2 cpu.weight proportional CPU allocation is NOT modeled or runnable in scxsim; only cgroup BANDWIDTH (cpu.max quota/period/burst throttling via cgroup_bw.bpf.c) is a real executed path. Discovered while attempting tg test-cgroup-weight-allocation. Verified evidence: (1) CgroupDef has only name/parent_name/cpuset/bandwidth — no weight field (safe/scenario.rs:130-143). (2) No ScenarioBuilder API to set a cgroup weight; cgroup builders only take cpuset/CgroupBandwidth (scenario.rs:982-1057). (3) rt-app cpu.weight is parsed + range-validated (1..=10000) then DISCARDED, never stored (safe/rtapp.rs:366-377). (4) The Scheduler FFI trait exposes cgroup_init/cgroup_exit/cgroup_move/cgroup_set_bandwidth but NO cgroup_set_weight (unsafe_impl/ffi.rs:471-502); the resolved SchedOps table has no weight entry. (5) LAVD SCX_OPS_DEFINE registers only .cgroup_init/.cgroup_exit/.cgroup_move/.cgroup_set_bandwidth (scx/scheds/rust/scx_lavd/src/bpf/main.bpf.c:2762-2789) — no .cgroup_set_weight; its cgroup logic is entirely bandwidth (scx_cgroup_bw_*). (6) COSMOS has no cgroup ops at all. The real proportional path is per-TASK weight (nice -> p->scx.weight vtime), already tested in tests/simple.rs (test_weighted_fairness, test_three_way_weighted_fairness) and tests/lavd.rs (test_lavd_varied_nice_values). Consequence: a faithful cgroup-weight proportional-allocation test cannot be written today without violating the No-Stub Rule (would assert on behavior no BPF code produces).

# Design

To make cgroup cpu.weight runnable + testable, build the substrate (do NOT stub): (a) add a weight field to CgroupDef (+ sibling of CgroupBandwidth or scalar) and ScenarioBuilder methods (e.g. cgroup_with_weight / nested variants); (b) store rt-app taskgroup cpu.weight into RtTaskgroupSpec and synthesize it into CgroupDef (rtapp.rs:366-377,291-295,434-468); (c) add a cgroup_set_weight FFI op type + Scheduler trait method + SchedOps fn-pointer entry + engine dispatch after cgroup_init (ffi.rs); (d) a scheduler that registers .cgroup_set_weight and factors per-cgroup weight into vtime/DSQ selection (upstream lavd/cosmos do not currently). Only then write proportional tests (equal weights->equal CPU, 2:1->~2:1 split via summing total_runtime across each cgroup's PIDs, mid-sim weight change, zero-task cgroup, nested inheritance).

# Acceptance Criteria

cpu.weight plumbs Rust engine -> C scheduler; a scheduler consumes it for proportional per-cgroup CPU distribution; integration tests assert ~ratio splits by summing per-cgroup total_runtime — all running real BPF logic (No-Stub compliant).
