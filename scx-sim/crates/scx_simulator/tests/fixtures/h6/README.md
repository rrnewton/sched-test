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
| `bug1_realistic_watchdog.json`       | `10000 5000000`  | true                 | Bug-1 stall under multi-second watchdogs (1s / 2s / 4s) |

The simulator-only knobs that don't fit in rt-app workload JSON
(`enable_cpu_bw`, watchdog timeout, scheduler selection, CPU count,
duration) come from either the scxsim CLI or the `--config` TOML sidecar.
For `bug1_canonical`, the sidecar `bug1_canonical.toml` declares
`enable_cpu_bw = true`; everything else comes from CLI flags. See
`SCXSIM_CGROUP_BW_DESIGN.md` ("Standalone-binary reproducer") for the
verbatim invocation.

## Watchdog-tier guidance

The `bug1_canonical.json` workload only sustains a contiguous-runnable
window of ~150ms before the cgroup quota refills at the 100ms period
boundary, pid=1 finally gets dispatched, and `runnable_for_ns` resets.
That is enough to trip the 80ms canonical trip-wire (chosen as the
documented minimum) but NOT to trip a 1s+ watchdog. To exercise the
bug under realistic-magnitude watchdogs (production `sched_ext`
default is 30s), use the `bug1_realistic_watchdog.json` variant which
stretches the cgroup period to 5s (`cpu.max=10000 5000000`) so the
contiguous throttle window grows to ~4990ms.

Verified watchdog tiers (3-rep byte-identical determinism each):

| Fixture                          | `--watchdog` | `--duration` | `runnable_for_ns` | Wall time |
|----------------------------------|--------------|--------------|-------------------|-----------|
| `bug1_canonical.json`            | 80ms         | 500ms        | 80000793          | ~10ms     |
| `bug1_realistic_watchdog.json`   | 1s           | 2000ms       | 1000003099        | ~20ms     |
| `bug1_realistic_watchdog.json`   | 2s           | 3000ms       | 2000013593        | ~30ms     |
| `bug1_realistic_watchdog.json`   | 4s           | 5000ms       | 4000035190        | ~60ms     |
