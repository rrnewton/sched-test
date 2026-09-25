---
title: 'scx-sim: stale comments after the scx 81738161 bump (layered bpf_ksym_exists NOTE, cosmos_llc.rs header, cosmos pick_idle_cpu_node NOTE)'
status: open
priority: 3
issue_type: task
labels:
- docs
depends_on:
  sim-439319: related
  sim-e10316: related
created_at: 2026-09-25T05:18:55.287623382+00:00
updated_at: 2026-09-25T05:21:26.495514683+00:00
---

# Description

DEFECT

At sched-test 24d864c6 these comments describe code that has since changed. Anyone who trusts them will reason from facts that are no longer true.

1. `schedulers/layered/wrapper.c` says: "NOTE: bpf_ksym_exists() is deliberately NOT overridden (mitosis forces 0, cosmos forces 1)". No wrapper overrides `bpf_ksym_exists` any more.
2. `crates/scx_simulator/tests/cosmos_llc.rs`, module header:
   - (a) It gives the stub's location as `scx-sim/csrc/sim_wrapper.h`. The file is `crates/scx_simulator/csrc/sim_wrapper.h`.
   - (b) Point 2 says the engine "never sets" `SCX_WAKE_TTWU` and "delivers `SCX_WAKE_SYNC` only". Since sim-e10316 (closed), `handle_task_wake` builds `SCX_WAKE_TTWU[|SCX_WAKE_SYNC]`.
   - (c) "Until both land" names sim-439319 and sim-e10316. One has landed, so only sim-439319 still blocks genuine LLC domains.
3. The doc comment above `test_gpu_node_affinity` in `crates/scx_simulator/tests/cosmos.rs` says `__COMPAT_scx_bpf_pick_idle_cpu_node()` "would hit a NULL weak ksym". But `scx_bpf_pick_idle_cpu_node` is defined in `crates/scx_simulator/scxtest/scx_test_cpumask.c` and exercised by `tests/numa_topology.rs`.

The `gpu_node_by_pid` comments, which upstream renamed to `gpu_node_by_tgid`, are tracked with the cosmos GPU-affinity regression, sim-5wv7o.

FIX DIRECTION

Correct or delete each comment at its site. Cite symbols, not line numbers.

ACCEPTANCE

- `grep -n 'mitosis forces 0' schedulers/layered/wrapper.c` finds nothing.
- `grep -n 'scx-sim/csrc/' crates/scx_simulator/tests/cosmos_llc.rs` finds nothing.
- `grep -n 'NULL weak ksym' crates/scx_simulator/tests/cosmos.rs` finds nothing.

Found by the 2026-09-24 scx pin bump coverage re-measure (sched-test 24d864c6, scx 413031d44), while checking the verdicts in coverage/scx_pin_bump_20260924/verdicts.tsv in the dev harness (rrnewton/dev-sched-test).
