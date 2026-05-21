# Reproducing a Stall Bug

> **Status — stub.** This recipe will walk through Bug-1 canonical as
> the worked example: how the fixture is constructed, what
> `scxsim run -s lavd --config bug1_canonical.toml --watchdog 80ms
> --duration 500ms --cpus 4 bug1_canonical.json` actually proves, and
> how to use `--verbose-summary`, the structops JSONL diff, and the
> per-task `is_throttled` / `nr_throttled_periods` counters to
> diagnose.

Source for the worked example:

- Fixture: [`crates/scx_simulator/tests/fixtures/h6/bug1_canonical.json`](https://github.com/facebookexperimental/sched-test/blob/simulator.v6/scx-sim/crates/scx_simulator/tests/fixtures/h6/bug1_canonical.json)
- Sidecar TOML: [`crates/scx_simulator/tests/fixtures/h6/bug1_canonical.toml`](https://github.com/facebookexperimental/sched-test/blob/simulator.v6/scx-sim/crates/scx_simulator/tests/fixtures/h6/bug1_canonical.toml)
- Test harness: `tests/bug1_canonical_repro.rs`
- Background: `crates/scx_simulator/tests/fixtures/h6/README.md`
