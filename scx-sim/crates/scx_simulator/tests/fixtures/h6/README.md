# H6 fixtures — reference recipes for the H6 dual-controller stall matrix

Reference rt-app-rs JSON describing the H6 matrix cells (A/B/C) and the
Bug-1 canonical reproducer that drive `tests/h6_matrix.rs` and
`tests/bug1_canonical_repro.rs`. They are **reference documentation
only** — the actual tests build their scenarios via `Scenario::builder`
in Rust to avoid coupling Diff 5 to the in-flight scxsim rt-app JSON
parser work (Phase 1 in `SCXSIM_CGROUP_BW_DESIGN.md`).

When the scxsim rt-app JSON parser supports the `taskgroup`/`cpu.max`
constructs end-to-end, these fixtures can be wired up by a future
test that loads the JSON and asserts the same trace shapes.

## Cell mapping

| Fixture                       | Kernel `cpu.max` | LAVD `enable_cpu_bw` | Expected     |
|-------------------------------|-----------------:|---------------------:|--------------|
| `cell_a_kernel_max_lavd_on.json` | `max max`     | true                 | no stall     |
| `cell_b_kernel_finite_lavd_off.json` | `10000 100000` | false           | engine throttles only |
| `cell_c_dual_controller.json` | `10000 100000`   | true                 | Bug-1 stall  |
| `bug1_canonical.json`         | `10000 100000`   | true                 | Bug-1 stall (David Dai R1, oversubscribed) |

The simulator-only knobs (`enable_cpu_bw`, watchdog timeout, scheduler
selection) come from the test driver, not the workload JSON — see
`SCXSIM_CGROUP_BW_DESIGN.md` ("Scenario and CLI surface").
