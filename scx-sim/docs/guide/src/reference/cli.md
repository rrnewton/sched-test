# CLI Reference

> **Status — stub.** This page will mirror the output of
> `scxsim --help`, `scxsim run --help`, `scxsim vm-run --help`, and
> `scxsim replay --help`.

The canonical source is the binary's own `--help`. A planned follow-up
generates this page from `--help` at build time so it cannot drift.

## Top-level

```text
scxsim [OPTIONS] <COMMAND>

Commands:
  run     Run a simulation from an rt-app workload
  vm-run  Run workload in a virtme-ng VM with a real scheduler
  replay  Replay a recorded preemption trace
  help    Print this message or the help of the given subcommand(s)

Options:
      --no-disable-aslr  Do not disable ASLR (default disables it +
                         re-execs for stable .so base addresses)
```

## `scxsim run` (~30 options)

Grouped by intent:

- **Topology:** `-s/--scheduler`, `-c/--cpus`, `--smt`,
  `--list-schedulers`, `--scheduler-file`.
- **Time:** `--end-time` (alias `--duration`), `--warmup-ms`.
- **Determinism:** `--seed`, `--fixed-priority`, `--no-noise`,
  `--no-overhead`, `--rbc-ns`, `--no-rbc`, `--determinism-check`.
- **Stall detection:** `--watchdog-timeout` (alias `--watchdog`).
- **Scheduler-config sidecar:** `--config <PATH>`.
- **Trace output:** `--perfetto PATH`, `--trace-format {json|perfetto}`,
  `--structops-jsonl PATH`, `--dump-trace`, `--verbose-summary`,
  `--record-preemptions PATH`.
- **Concurrency / interleaving:** `--interleave`,
  `--stochastic-timer-interleave[-window|-one-in]`, `--preemptive`,
  `--timeslice-min`, `--timeslice-max`, `--break-on {rbc|insn}`,
  `--preempt-mode {pmu|e9patch}`, `--native-concurrent`, `--window-ns`.
- **Debugger:** `--wait-debugger`.

## `scxsim vm-run` (~7 options)

`-s/--scheduler`, `-c/--cpus`, `--wprof`, `--bpf-trace`,
`--scheduler-args=<RAW>`, `--pre-hook <PATH>`, `--post-hook <PATH>`.

## `scxsim replay` (~5 options)

`--scheduler-file`, `--verbose-summary`, `--record-preemptions PATH`,
`--no-pmu-signal`, `--preempt-mode {pmu|e9patch}`, `--wait-debugger`.
