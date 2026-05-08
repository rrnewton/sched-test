# H6 fixtures — reference recipes for the H6 dual-controller stall matrix

Reference rt-app-rs JSON describing the H6 matrix cells (A/B/C) and the
Bug-1 canonical reproducer.

* `cell_*.json` — H6 matrix recipes used as **reference documentation**
  by `tests/h6_matrix.rs`, which still builds its scenarios via
  `Scenario::builder` in Rust.
* `bug1_canonical.json` + `bug1_canonical.toml` — the Bug-1 canonical
  reproducer pair that drives `tests/bug1_canonical_repro.rs` end-to-end.
  As of the `cgroup_bw` 5-diff stack capstone, that test is **subprocess
  form**: it spawns `target/release/scxsim` against these files and
  asserts exit code 42 + the stable
  `scxsim: ExitKind::ErrorStall pid=<N> runnable_for_ns=<N>` stderr
  marker. No `Scenario::builder` duplication remains in the test code.

## Cell mapping

| Fixture                              | Kernel `cpu.max` | LAVD `enable_cpu_bw` | Expected              |
|--------------------------------------|-----------------:|---------------------:|-----------------------|
| `cell_a_kernel_max_lavd_on.json`     | `max max`        | true                 | no stall              |
| `cell_b_kernel_finite_lavd_off.json` | `10000 100000`   | false                | engine throttles only |
| `cell_c_dual_controller.json`        | `10000 100000`   | true                 | Bug-1 stall           |
| `bug1_canonical.json`                | `10000 100000`   | true                 | Bug-1 stall (David Dai R1, oversubscribed) |

The simulator-only knobs that don't fit in rt-app workload JSON
(`enable_cpu_bw`, watchdog timeout, scheduler selection, CPU count,
duration) come from either the scxsim CLI or the `--config` TOML sidecar.
For `bug1_canonical`, the sidecar `bug1_canonical.toml` declares
`enable_cpu_bw = true`; everything else comes from CLI flags. See
`SCXSIM_CGROUP_BW_DESIGN.md` ("Standalone-binary reproducer") for the
verbatim invocation.
