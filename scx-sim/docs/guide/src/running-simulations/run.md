# The `run` Subcommand

`scxsim run` is the main entry point: an rt-app JSON workload goes
in; trace and summary outputs come out.

> **Acronyms used on this page** (full definitions in the
> [Glossary](../glossary.md)):
> **CPU** = Central Processing Unit;
> **SMT** = Simultaneous Multi-Threading;
> **DSQ** = Dispatch Queue;
> **PRNG** = Pseudo-Random Number Generator (seeded for determinism);
> **PMU** = Performance Monitoring Unit (hardware perf counters);
> **RBC** = Retired Branch Count (PMU event used for the scheduler
> overhead model);
> **BPF** = Berkeley Packet Filter (the kernel-side compile target;
> under scxsim the scheduler `.so` is native, not BPF bytecode —
> see [Introduction](../introduction.md)).

```text
scxsim run [OPTIONS] [WORKLOAD]
```

If `WORKLOAD` is omitted, scxsim uses a small built-in default — but
the common case is to pass the path to a workload file from
[`examples/`](https://github.com/rrnewton/sched-test/tree/simulator.v6/scx-sim/examples) or
[`crates/scx_simulator/workloads/`](https://github.com/rrnewton/sched-test/tree/simulator.v6/scx-sim/crates/scx_simulator/workloads).

The full canonical option list is `scxsim run --help` (also mirrored
at [Reference → CLI](../reference/cli.md)). This page groups the
options by intent and shows the common combinations.

## Topology

| Flag | Default | Purpose |
|---|---|---|
| `-s, --scheduler <NAME>` | `simple` | One of `simple`, `lavd`, `cosmos`, `mitosis`, `tickless`. See [Concepts → Schedulers](../concepts/schedulers.md). |
| `-c, --cpus <N>` | `4` | Number of simulated CPUs (minimum 1). |
| `--smt <N>` | `1` | SMT threads per core (minimum 1). |
| `--list-schedulers` | — | Print the loadable schedulers and their `.so` paths, then exit. |
| `--scheduler-file <PATH>` | — | Override the default `libscx_<name>.so` lookup. The basename must still match `libscx_<name>.so`. |

```bash
scxsim run --list-schedulers
scxsim run -s lavd --cpus 8 --smt 2 examples/cpu_bound.json
```

## Time

| Flag | Default | Purpose |
|---|---|---|
| `--duration <DUR>` (alias `--end-time`) | from workload `global.duration` | Simulation end time. Accepts `1s`, `0.5s`, `500ms`, `100us`, `1000ns`. Bare numbers = nanoseconds. |
| `--warmup-ms <MS>` | `0` | Trace statistics exclude events before this simulated time. Run still starts at 0. |

```bash
scxsim run -s lavd --duration 200ms --warmup-ms 50 examples/cpu_bound.json
```

The 50 ms warmup excludes startup transients from the per-CPU
utilization / inter-arrival distributions printed by
`--verbose-summary`.

## Determinism

| Flag | Default | Purpose |
|---|---|---|
| `--seed <SEED>` | `42` (or `$SCX_SIM_SEED`) | u32 or `entropy`. See [Determinism](../concepts/determinism.md). |
| `--fixed-priority` | off | Insertion-order tiebreaking instead of PRNG. |
| `--no-noise` | noise on | Disable tick jitter. |
| `--no-overhead` | overhead on | Disable context-switch overhead. |
| `--rbc-ns <NS>` | `10` | Ns per retired conditional branch (PMU overhead model). |
| `--no-rbc` | RBC charging on | Equivalent to `--rbc-ns 0`. |
| `--determinism-check` | off | Run twice, compare checkpoint sequences, exit non-zero on divergence. |

```bash
# Strict-determinism upper bound:
scxsim run -s lavd --seed 42 --no-noise --no-overhead --rbc-ns 0 \
    examples/hello.json

# CI gate:
scxsim run -s lavd --seed 42 --determinism-check examples/hello.json
# => "Determinism check PASSED: 22 checkpoints matched"
```

## Stall detection

| Flag | Default | Purpose |
|---|---|---|
| `--watchdog <DUR>` (alias `--watchdog-timeout`) | `30s` | If a runnable task is not scheduled within this simulated duration, exit `ExitKind::ErrorStall` (exit code 42). `0` or `off` disables. |

```bash
scxsim run -s lavd --watchdog 80ms ... <workload>
```

A short watchdog is the canonical way to catch a stall fast in CI:
the Bug-1 reproducer uses `--watchdog 80ms` to trip within a few
hundred milliseconds of wallclock.

## Scheduler config sidecar

| Flag | Default | Purpose |
|---|---|---|
| `--config <PATH>` | — | TOML file setting per-symbol BPF globals in the loaded `.so` after load but before `ops.init()`. |

The TOML file uses typed sub-tables: `[scheduler.bool_globals]`,
`[scheduler.u8_globals]`, `[scheduler.u32_globals]`,
`[scheduler.u64_globals]`. See
[Scheduler Config Sidecar (TOML)](./scheduler-config.md) for the full
mechanism, the loudness guarantees, and the worked Bug-1 example.

```bash
scxsim run -s lavd --config my-config.toml ... <workload>
```

## Trace output

| Flag | Default | Purpose |
|---|---|---|
| `--perfetto <PATH>` | — | Write Perfetto trace. |
| `--trace-format <FMT>` | `json` | `json` (Chrome Trace Event JSON, default; loadable in <https://ui.perfetto.dev/>) or `perfetto` (wprof-compatible protobuf). |
| `--structops-jsonl <PATH>` | — | Schema-compatible with `scripts/probes/structops_full.bt` + `helpers_full.bt`; diffable via `scripts/compare_live_vs_scxsim_calls.sh`. |
| `--dump-trace` | off | Print per-event trace lines to stderr. |
| `--verbose-summary` | off | Print per-task / per-CPU distribution stats after the brief summary. |
| `--record-preemptions <PATH>` | — | Write a preemption trace replayable via `scxsim replay`. |

See [Trace Output](./trace-output.md) for sample fragments of each.

## Concurrency / interleaving (stress mode)

These are opt-in exaggeration knobs; the default trace is serial.

| Flag | Default | Purpose |
|---|---|---|
| `--interleave` | off | Concurrent dispatch callbacks across CPUs with PRNG-driven token passing at kfunc yield points. |
| `--stochastic-timer-interleave` | off | Pull `cgroup_bw` BPF-timer events into cgroup_bw yield sites. Deterministic per seed. |
| `--stochastic-timer-interleave-window <DUR>` | `20ms` | Fire-ahead window for the above. |
| `--stochastic-timer-interleave-one-in <N>` | `4` | One eligible timer per N sites. |
| `--preemptive` | off | Preempt mid-C-code at random RBC intervals. Implies `--interleave`. |
| `--timeslice-min <RBC>` | `300` | Lower bound of preemptive timeslice. Below 200 can livelock LAVD. |
| `--timeslice-max <RBC>` | `1500` | Upper bound. |
| `--break-on {rbc, insn}` | `rbc` | PMU event for preemptive breaks. `insn` is higher-frequency. |
| `--preempt-mode {pmu, e9patch}` | `pmu` | Preemption mechanism. `e9patch` is deterministic and debugger-compatible but requires the `_e9.so` variant. |
| `--native-concurrent` | off | True OS-thread parallelism for dispatch (clock-window sync). |
| `--window-ns <NS>` | `10_000_000` (10 ms) | Clock-window size with `--native-concurrent`. |

See [Concepts → Twin Design Principles](../concepts/twin-design-principles.md)
for when to reach for these.

## Debugger

| Flag | Default | Purpose |
|---|---|---|
| `--wait-debugger` | off | Pause before `ops.init()`. Writes an lldb breakpoint script next to the `.so` and prints an attach command. |

See [Recipes → Debugging with LLDB](../recipes/lldb.md) for the full
attach workflow.

## A few canonical recipes

**Smallest useful invocation:**

```bash
scxsim run -s lavd --cpus 4 --duration 100ms examples/hello.json
```

**Capture everything for an offline investigation:**

```bash
scxsim run -s lavd --cpus 4 --duration 200ms --seed 42 \
    --verbose-summary \
    --perfetto /tmp/run.json \
    --structops-jsonl /tmp/run.jsonl \
    --record-preemptions /tmp/run.preempts \
    examples/cpu_bound.json \
    2>/tmp/run.stderr
```

**Bug-1 stall reproducer (uses TOML sidecar + short watchdog):**

```bash
scxsim run -s lavd --cpus 4 --duration 500ms --watchdog 80ms \
    --config crates/scx_simulator/tests/fixtures/h6/bug1_canonical.toml \
    crates/scx_simulator/tests/fixtures/h6/bug1_canonical.json
```

**Determinism gate:**

```bash
scxsim run -s lavd --seed 42 --determinism-check examples/cpu_bound.json
```
