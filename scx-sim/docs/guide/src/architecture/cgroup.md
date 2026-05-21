# Cgroup Modeling

scxsim models cgroup-v2 CPU bandwidth (`cpu.max`) faithfully enough
to reproduce production-side cgroup-bw stalls like
[Bug-1](../recipes/repro-stall.md). The model spans three
crates/modules.

## The three layers

| Layer | What it owns |
|---|---|
| [`crates/scx_cgroup_tree/`][cgroup-tree-crate] | Pure data structure: the cgroup hierarchy, parent/child links, `cpu.max` quota and period per cgroup. |
| [`safe/cgroup.rs`][safe-cgroup] | Simulator state: per-cgroup `runtime_ns`, `is_throttled`, the periodic `CgroupBwReplenish` event, and the throttle-pivot bookkeeping. |
| [`unsafe_impl/cgroup_ffi.rs`][cgroup-ffi] / `cgroup_wrapper.rs` | BPF-side view: when the scheduler reads `cgroup->cpu.max` or calls `bpf_cgroup_throttled()`, these shim the access into the safe-side state. |

[cgroup-tree-crate]: https://github.com/facebookexperimental/sched-test/tree/simulator.v6/scx-sim/crates/scx_cgroup_tree
[safe-cgroup]: https://github.com/facebookexperimental/sched-test/blob/simulator.v6/scx-sim/crates/scx_simulator/src/safe/cgroup.rs
[cgroup-ffi]: https://github.com/facebookexperimental/sched-test/blob/simulator.v6/scx-sim/crates/scx_simulator/src/unsafe_impl/cgroup_ffi.rs

## Where cgroups come from

Cgroups are declared in the workload JSON via the `taskgroup` field of
each task. See [Concepts → Workloads](../concepts/workloads.md) for the
inline-vs-shared forms. At scenario load:

1. `safe/scenario.rs` walks every task's `taskgroup` and unions them
   into a tree (root `/` plus every named ancestor).
2. Each leaf is given its declared `cpu.max` (quota + period). Inner
   nodes inherit `"max <period>"` unless explicitly set.
3. The tree is handed to the engine as initial state and exposed to
   the scheduler via the cgroup-FFI shim.

## Accounting timer: replenish + throttle

scxsim models the kernel's cfs-bandwidth controller as a top-half /
bottom-half split scheduled by the engine:

- **Top half — `CgroupBwReplenish` event.** Fires every `period`
  (default 100 ms). For each cgroup it tops up `runtime_ns` to
  `min(runtime_ns + quota, quota)` and, if the cgroup is currently
  throttled and the new `runtime_ns > 0`, clears `is_throttled`.
- **Bottom half — `ops.running` / `ops.stopping` charge-and-check.**
  When a task is dispatched on a CPU, the engine charges the task's
  current cgroup `runtime_ns -= slice_consumed_ns` on every
  `ops.stopping` (slice end) and during long slices on `ops.tick`. If
  `runtime_ns` goes negative, the cgroup is marked `is_throttled =
  true` and any task in it is removed from the dispatchable pool.

A scheduler that respects cgroup bandwidth (LAVD with
`enable_cpu_bw = true`, for example) consults `cgroup_throttled(cg)`
in its `ops.enqueue` / `ops.dispatch` and skips throttled cgroups. A
scheduler that does not consult it can still enqueue throttled
tasks — they will simply never run, which is exactly the stall mode
Bug-1 demonstrates.

## Worked example

The bundled [`cgroup_hierarchy.json`](../concepts/workloads.md#example-cgroup-hierarchy)
declares two siblings:

```text
/
├── interactive  cpu.max = "20000 100000"  (20% of one CPU)
└── background   cpu.max = "10000 100000"  (10% of one CPU)
```

Run it under LAVD with cgroup-bw enabled:

```bash
scxsim run -s lavd --cpus 4 --duration 500ms \
    --config crates/scx_simulator/tests/fixtures/h6/bug1_canonical.toml \
    --verbose-summary \
    examples/cgroup_hierarchy.json
```

The verbose summary breaks out per-cgroup runtime consumed and number
of throttle entries. Tight quotas combined with long-running tasks
quickly mark cgroups as throttled; loose quotas never do.

## The `enable_cpu_bw` gate

LAVD's cgroup-bw integration is **off by default** in the BPF source
(`scx/scheds/rust/scx_lavd/src/bpf/main.bpf.c:817`, the
`if (enable_cpu_bw && …)` check at `lavd_enqueue`). Without the gate
flipped on via a TOML sidecar:

```toml
[scheduler.bool_globals]
enable_cpu_bw = true
```

the scheduler never consults cgroup throttle state at all and even the
canonical Bug-1 reproducer comes up clean. This is the single highest
-impact configuration knob in the entire reproducer — see
[Concepts → Cgroup Bandwidth](../concepts/cgroup-bw.md) and
[Scheduler Config Sidecar](../running-simulations/scheduler-config.md).

## Determinism considerations

The accounting timer is event-driven and uses only virtual time; it
introduces no wall-clock variability. The `CgroupBwReplenish` event is
scheduled at scenario load and re-armed deterministically. Per-cgroup
state mutations all flow through `safe/cgroup.rs`, so cgroup behaviour
is part of the [Determinism](../concepts/determinism.md) checkpoint
set and is verified byte-for-byte by `--determinism-check`.

## Sources

- [`crates/scx_cgroup_tree/`][cgroup-tree-crate] — the tree data
  structure shared with `scx_simulator`.
- [`safe/cgroup.rs`][safe-cgroup] — simulator-side state, replenish
  event handler, throttle bookkeeping.
- [`unsafe_impl/cgroup_ffi.rs`][cgroup-ffi] — BPF-side accessors.
- [`unsafe_impl/cgroup_wrapper.rs`](https://github.com/facebookexperimental/sched-test/blob/simulator.v6/scx-sim/crates/scx_simulator/src/unsafe_impl/cgroup_wrapper.rs) —
  thin layer mediating the safe / unsafe view.
