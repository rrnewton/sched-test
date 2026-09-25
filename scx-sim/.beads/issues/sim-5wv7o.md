---
title: 'scxsim cosmos: GPU affinity went dead at the scx 81738161 bump (upstream keys gpu_pid_map by tgid; the sim''s tgid is 0) and test_gpu_node_affinity cannot tell'
status: open
priority: 1
issue_type: bug
labels:
- cosmos
- kernel-fidelity
depends_on:
  sim-c63e46: related
  sim-6mheb: related
created_at: 2026-09-25T05:18:55.290429219+00:00
updated_at: 2026-09-25T05:21:47.614561946+00:00
---

# Description

DEFECT

At the new pin, cosmos's GPU-affinity path no longer runs in the simulator, and the test meant to cover it still passes.

PRODUCTION (scx 413031d44)

- `scx_cosmos` `main.bpf.c` `pick_cpu_on_gpu_node()` calls `gpu_node_by_tgid(p->tgid)`, which looks the task up in `gpu_pid_map` by thread-group id.
- `cosmos_select_cpu` and `cosmos_enqueue` consult it: "If the task's TGID is in gpu_pid_map ...".
- Userspace (`main.rs`, through NVML) fills the map with GPU processes' tgids.

SIMULATOR (sched-test 24d864c6)

- `schedulers/cosmos/wrapper.c` `cosmos_add_gpu_task(pid, node)` inserts `pid` as the key. `crates/scx_simulator/src/unsafe_impl/ffi.rs` exposes it as `cosmos_add_gpu_task`.
- The simulator never sets `p->tgid`; it stays 0 (sim-6mheb).
- So `gpu_node_by_tgid(0)` always misses, and `pick_cpu_on_gpu_node` returns before `can_use_node()`.

The coverage re-measure shows the result: `can_use_node` moved from COVERED at the old pin (scx 59c30bae, which keyed by pid) to UNCOVERED at the new one. It is the only COVERED → UNCOVERED test-gap transition in the bump.

THE TEST CANNOT SEE IT

In `crates/scx_simulator/tests/cosmos.rs`, `test_gpu_node_affinity`:
- registers pid 1 as a GPU task for node 1 ({2,3});
- pins it to node 0's CPUs {0,1};
- asserts only `total_runtime(Pid(1)) > 0` and `total_runtime(Pid(2)) > 0`.

A lookup miss behaves exactly like `can_use_node()` returning false, so the test passes whether or not the GPU path runs. It asserts nothing about GPU affinity.

Five comments still describe the old key, `gpu_node_by_pid`:
- `ffi.rs`, in the `cosmos_add_gpu_task` doc;
- `tests/cosmos.rs`, in two doc comments;
- `schedulers/cosmos/wrapper.c`, in two comments.

FIX DIRECTION

1. Set `p->tgid` in the simulated task_struct (sim-6mheb). This blocks the rest.
2. Key the wrapper's insert by tgid, and rename the FFI argument to match.
3. Rewrite the test so the outcome depends on the GPU path. Let the task run on node 1, and assert it is placed there, for example most of its runtime on CPUs {2,3}. A control without the map entry should place it differently.
4. Update the five comments.

sim-c63e46, which asks for the GPU-affinity subsystem to be modelled, should note this regression.

ACCEPTANCE

- The rewritten test fails at 24d864c6 and passes after the fix.
- A coverage re-measure shows `can_use_node` and `pick_cpu_on_gpu_node` COVERED again.
- `grep -rn gpu_node_by_pid` in scx-sim finds nothing.

Found by the 2026-09-24 scx pin bump coverage re-measure (sched-test 24d864c6, scx 413031d44), as the COVERED → UNCOVERED transition of cosmos can_use_node in coverage/scx_pin_bump_20260924 in the dev harness (rrnewton/dev-sched-test).
