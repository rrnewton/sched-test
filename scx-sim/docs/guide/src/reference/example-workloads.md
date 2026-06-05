# Example Workloads

scxsim ships two parallel sets of workloads:

1. **Demo / guide examples** under
   [`scx-sim/examples/`](https://github.com/rrnewton/sched-test/tree/simulator.v6/scx-sim/examples) —
   short, well-commented rt-app JSON files referenced from the guide.
   These are designed to be the first thing a new user runs.
2. **Test fixtures** under
   [`scx-sim/crates/scx_simulator/workloads/`](https://github.com/rrnewton/sched-test/tree/simulator.v6/scx-sim/crates/scx_simulator/workloads) and
   [`scx-sim/crates/scx_simulator/tests/fixtures/`](https://github.com/rrnewton/sched-test/tree/simulator.v6/scx-sim/crates/scx_simulator/tests/fixtures) —
   integration-test fixtures (Bug-1 canonical, H6 cells, dsq stress).
   These are precise and not always easy to read; the demos are
   curated subsets of the same shape.

## Guide examples

| File | Demonstrates |
|---|---|
| [`examples/hello.json`](https://github.com/rrnewton/sched-test/blob/simulator.v6/scx-sim/examples/hello.json) | Minimal single-task workload — a five-iteration "hello world." |
| [`examples/cpu_bound.json`](https://github.com/rrnewton/sched-test/blob/simulator.v6/scx-sim/examples/cpu_bound.json) | Four CPU-bound workers showing how the scheduler distributes load. |
| [`examples/cgroup_hierarchy.json`](https://github.com/rrnewton/sched-test/blob/simulator.v6/scx-sim/examples/cgroup_hierarchy.json) | Two cgroups with different `cpu.max` quotas (`/background` throttled, `/interactive` unconstrained). |

See [`examples/README.md`](https://github.com/rrnewton/sched-test/blob/simulator.v6/scx-sim/examples/README.md)
for one-liners on how to run each.

## Production fixtures (selected)

| Fixture | Used by |
|---|---|
| `workloads/simple_wake.json` | `tests/simple.rs`, basic wake-up integration test. |
| `workloads/two_runners.json` | LAVD ranking smoke test. |
| `workloads/dsq_contention.json` | DSQ contention smoke test. |
| `workloads/lavd_dsq_stress.json` | Multi-worker LAVD stress. |
| `tests/fixtures/h6/bug1_canonical.json` + `.toml` | The cpu-bw-stall-bug R1 canonical reproducer. |
| `tests/fixtures/h6/cell_a_kernel_max_lavd_on.json` | H6 matrix cell A (no-stall control). |
| `tests/fixtures/h6/cell_b_kernel_finite_lavd_off.json` | H6 matrix cell B (engine-only throttle). |
| `tests/fixtures/h6/cell_c_dual_controller.json` | H6 matrix cell C (dual-controller stall). |
