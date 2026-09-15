# CLI Reference

The canonical source of truth is `scxsim --help` and each
subcommand's `--help`. The text below is the literal output from
`scxsim 0.1.0` (`simulator.v6` tag).

> **Note.** A future improvement generates this page from `--help`
> at build time so it cannot drift. Until then, treat the binary's
> own `--help` as authoritative if any discrepancy appears.

## Top-level: `scxsim --help`

```text
sched_ext simulator

Usage: scxsim [OPTIONS] <COMMAND>

Commands:
  run     Run a simulation from an rt-app workload
  vm-run  Run workload in a virtme-ng VM with a real scheduler
  replay  Replay a recorded preemption trace
  help    Print this message or the help of the given subcommand(s)

Options:
      --no-disable-aslr
          Do not disable ASLR. By default scxsim disables ASLR via
          personality(ADDR_NO_RANDOMIZE) and re-execs so that .so base
          addresses are stable across runs (important for deterministic
          replay).

  -h, --help
          Print help
```

## `scxsim run --help`

```text
Run a simulation from an rt-app workload

Usage: scxsim run [OPTIONS] [WORKLOAD]

Arguments:
  [WORKLOAD]
          Path to an rt-app JSON workload file

Options:
      --no-disable-aslr
          Do not disable ASLR (see top-level help).

  -s, --scheduler <SCHEDULER>
          Scheduler name. [default: simple]

  -c, --cpus <CPUS>
          Number of simulated CPUs (minimum 1). [default: 4]

      --smt <SMT>
          SMT threads per core (minimum 1). [default: 1]

      --seed <SEED>
          PRNG seed (u32 integer or "entropy" for OS randomness).
          Falls back to SCX_SIM_SEED env var, then default (42).
          [env: SCX_SIM_SEED=]

      --fixed-priority
          Use insertion-order event tiebreaking instead of randomized.

      --end-time <DURATION>
          Simulation end time (overrides workload duration). Accepts
          "1s", "0.5s", "500ms", "100us", "1000ns"; bare number = ns.
          Alias: --duration.

      --warmup-ms <MS>
          Warmup period in milliseconds. Trace stats exclude pre-warmup
          events. Simulation still runs from time 0.

      --perfetto <PATH>
          Write Perfetto trace to file. Default format is Chrome Trace
          Event JSON (load in https://ui.perfetto.dev). Use
          --trace-format perfetto for wprof-compatible protobuf.

      --trace-format <FMT>
          Output format for --perfetto. [default: json]
          - json     : Chrome Trace Event JSON.
          - perfetto : wprof-compatible Perfetto protobuf.

      --structops-jsonl <PATH>
          Write a structops/helpers JSONL trace, schema-compatible with
          scripts/probes/structops_full.bt + helpers_full.bt.

      --dump-trace
          Print trace events to stderr.

      --no-noise
          Disable tick jitter noise.

      --no-overhead
          Disable context-switch overhead.

      --rbc-ns <NS>
          Nanoseconds of logical time per retired conditional branch in
          scheduler code (PMU-based overhead). [default: 10]

      --no-rbc
          Disable PMU-based RBC scheduler overhead (= --rbc-ns 0).

      --watchdog-timeout <DURATION>
          Watchdog timeout for stall detection. "0" or "off" disables.
          [default: 30s]  Alias: --watchdog.

      --config <PATH>
          Path to a TOML scheduler-config file. Declares per-symbol BPF
          globals (bool/u8/u32/u64) written to the loaded .so after
          load but before ops.init().

      --interleave
          Enable concurrent callback interleaving at kfunc yield points.

      --stochastic-timer-interleave
          Pull cgroup_bw BPF timer events into cgroup_bw yield sites.

      --stochastic-timer-interleave-window <DURATION>
          Fire-ahead window for --stochastic-timer-interleave.
          [default: 20ms]

      --stochastic-timer-interleave-one-in <N>
          Approximate rate: one eligible timer per N sites. [default: 4]

      --preemptive
          Enable preemptive interleaving via PMU retired-branch signals.
          Implies --interleave.

      --timeslice-min <RBC>
          Minimum preemptive timeslice in retired conditional branches.
          [default: 300]  Below 200 can livelock LAVD.

      --timeslice-max <RBC>
          Maximum preemptive timeslice in retired conditional branches.
          [default: 1500]

      --break-on <BREAK_ON>
          PMU event for preemptive interleaving. [default: rbc]
          - rbc  : Retired conditional branches (lower frequency).
          - insn : Instructions retired (higher frequency).

      --preempt-mode <PREEMPT_MODE>
          Preemption mechanism. [default: pmu]
          - pmu     : PMU hardware timer (skid).
          - e9patch : e9patch software RBC (deterministic).

      --native-concurrent
          Native concurrent dispatch via OS threads with clock-window
          synchronisation. Implies --interleave.

      --window-ns <WINDOW_NS>
          Clock window size in ns (only with --native-concurrent).
          [default: 10_000_000 (10 ms)]

      --list-schedulers
          List available schedulers and exit.

      --determinism-check
          Enable strict determinism checking (runs twice, compares
          checkpoint sequences, exits 1 on divergence).

      --record-preemptions <PATH>
          Record preemption points to a file for later replay.

      --verbose-summary
          Print detailed per-task and per-CPU statistics after simulation.

      --wait-debugger
          Pause before ops.init() so a debugger can attach. Writes an
          lldb breakpoint script next to the .so, prints attach command.

      --scheduler-file <PATH>
          Override the scheduler .so path used by --scheduler. The
          basename must still match libscx_<name>.so. Use case:
          per-revision bin_cache regression bisect.

  -h, --help
          Print help (see a summary with '-h')
```

## `scxsim vm-run --help`

```text
Run workload in a virtme-ng VM with a real scheduler

Usage: scxsim vm-run [OPTIONS] <WORKLOAD>

Arguments:
  <WORKLOAD>
          Path to an rt-app JSON workload file

Options:
      --no-disable-aslr
          Do not disable ASLR (see top-level help).

  -s, --scheduler <SCHEDULER>
          Scheduler name. [default: simple]

  -c, --cpus <CPUS>
          Number of workload CPUs to use in the VM. Tracing modes add
          one extra VM CPU for the tracer. [default: 4]

      --wprof
          Record a Perfetto trace using wprof during VM execution.
          Adds an extra VM CPU isolated via isolcpus.

      --bpf-trace
          Trace ops + kfuncs via bpftrace (trace_scx_ops.bt). Writes
          bpf_trace.log in CWD.

      --scheduler-args <ARGS>
          Raw shell arguments appended to the scheduler command. Use
          --scheduler-args=--enable-cpu-bw when the first arg starts
          with `-`.

      --pre-hook <PATH>
          Executable hook run inside the VM after the scheduler starts
          and before rt-app starts. Sees SCXSIM_* env vars including
          SCXSIM_SCHED_PID.

      --post-hook <PATH>
          Executable hook run inside the VM after rt-app exits and
          before the scheduler/tracer are stopped. Same env.

  -h, --help
          Print help (see a summary with '-h')
```

## `scxsim replay --help`

```text
Replay a recorded preemption trace

Usage: scxsim replay [OPTIONS] <TRACE_FILE>

Arguments:
  <TRACE_FILE>
          Path to a preemption trace file (produced by `run
          --record-preemptions`)

Options:
      --no-disable-aslr
          Do not disable ASLR (see top-level help).

      --scheduler-file <SCHEDULER_FILE>
          Override the scheduler .so file path stored in the trace.
          Only needed if the .so has moved since recording.

      --verbose-summary
          Print detailed per-task and per-CPU statistics after
          simulation.

      --record-preemptions <PATH>
          Re-record preemption points during replay to a new trace file.

      --no-pmu-signal
          Skip PMU timer; use hardware breakpoint stepping only. Slower
          but guarantees deterministic replay (avoids PMU skid).

      --preempt-mode <PREEMPT_MODE>
          Preemption mechanism for replay. [default: pmu]
          - pmu     : PMU + hardware breakpoint replay.
          - e9patch : e9patch software RBC.

      --wait-debugger
          Pause before ops.init() so a debugger can attach.

  -h, --help
          Print help (see a summary with '-h')
```

## See also

- [Running Simulations → The `run` Subcommand](../running-simulations/run.md) — same
  flags grouped by intent, with examples.
- [Running Simulations → VM Runs](../running-simulations/vm-run.md) — `vm-run` walk-through.
- [Running Simulations → Replaying Preemption Traces](../running-simulations/replay.md) — record / replay loop.
- [Reference → Output Formats](./output-formats.md) — every sink one table.
- [Reference → Exit Codes](./exit-codes.md) — the stable `ExitKind` enum.
