# The `run` Subcommand

> **Status — stub.** This page will walk through the most common
> `scxsim run` flag clusters.

Categories (from `scxsim run --help`):

- **Topology:** `-s/--scheduler`, `-c/--cpus`, `--smt`,
  `--list-schedulers`, `--scheduler-file`.
- **Time:** `--end-time` (alias `--duration`), `--warmup-ms`.
- **Determinism:** `--seed`, `--fixed-priority`, `--no-noise`,
  `--no-overhead`, `--rbc-ns`, `--no-rbc`, `--determinism-check`.
- **Stall detection:** `--watchdog-timeout` (alias `--watchdog`,
  default 30s, `0`/`off` disables).
- **Scheduler-config sidecar:** `--config <PATH>` (see
  [Scheduler Config](./scheduler-config.md)).
- **Trace output:** `--perfetto PATH`, `--trace-format {json|perfetto}`,
  `--structops-jsonl PATH`, `--dump-trace`, `--verbose-summary`,
  `--record-preemptions PATH`.
- **Concurrency / interleaving:** `--interleave`,
  `--stochastic-timer-interleave` (+ window and one-in flags),
  `--preemptive`, `--timeslice-min`/`--timeslice-max`, `--break-on`,
  `--preempt-mode {pmu|e9patch}`, `--native-concurrent`, `--window-ns`.
- **Debugger:** `--wait-debugger`.

The full canonical reference is the output of `scxsim run --help`; the
[CLI Reference](../reference/cli.md) page mirrors it.
