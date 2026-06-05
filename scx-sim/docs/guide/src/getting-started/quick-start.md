# Quick Start

The shortest path from a built `scxsim` binary to a Perfetto-loadable
trace.

## Run it

From the [`scx-sim/`][scx-sim] directory of a built checkout:

```bash
scxsim run \
    --scheduler lavd \
    --cpus 4 \
    --duration 100ms \
    --perfetto /tmp/hello.json \
    examples/hello.json
```

[scx-sim]: https://github.com/facebookexperimental/sched-test/tree/simulator.v6/scx-sim

## What gets printed

```text
scxsim: disabling ASLR and re-executing...

Simulation complete:
  Logical time elapsed:   100ms
  Total tasks:            1
  Max concurrent running: 1
  Total time slices:      5
  Tasks at end:           0 alive, 0 runnable
  All tasks completed:    9.6ms (9.7% of simulation)

Sched_ext structop summary:
     cpu   structops         rbc      kfuncs
  ------  ----------  ----------  ----------
       0          57       11954         388
  ------  ----------  ----------  ----------
   total          57       11954         388

Preemption stats:
  longest_structop_rbc:    812
  longest_rbc_interval:    371  (between kfuncs)
  REPLAY_MARGIN:           200

Trace Summary:
  total_events:          64
  total_ticks:           2
  total_yields:          0
  total_preempts:        0
  total_sleeps:          5
  total_wakes:           5
  total_idle_periods:    5
  total_idle_duration:   5.030ms
  global_dsq_dispatches: 0
  local_dsq_dispatches:  5
```

Exit code: `0` (`ExitKind::Normal`).

## Read it back

Drop `/tmp/hello.json` onto <https://ui.perfetto.dev/>. You will see
four CPU tracks (`CPU 0` .. `CPU 3`); the `hello` task fires on
CPU 0, runs five times for ~1 ms each, sleeps ~1 ms between
iterations, and completes after ~9.7 ms of simulated time.

## What each line means

| Line | Meaning |
|---|---|
| `Logical time elapsed: 100ms` | Simulated (not wallclock) end-of-run time. Set by `--duration`. |
| `Total tasks: 1` | rt-app tasks created. `examples/hello.json` has one. |
| `Max concurrent running: 1` | High-water mark for parallel-running tasks. |
| `Total time slices: 5` | Distinct on-CPU intervals; matches the five-iteration `loop`. |
| `All tasks completed: 9.6ms (9.7% of simulation)` | When the last task finished; the remaining 90% is idle. |
| `structops`, `rbc`, `kfuncs` | sched_ext callbacks invoked, **RBC** (Retired Branch Count — retired conditional branches) inside them, and helper-function (kfunc) calls. The PMU-overhead (Performance Monitoring Unit) model uses `rbc`; see [Determinism](../concepts/determinism.md) and the [Glossary](../glossary.md). |
| `longest_structop_rbc: 812` | Worst-case RBC count for a single callback in this run. |
| `total_events: 64` | Simulator trace events emitted. |
| `total_ticks: 2` | Scheduling-tick fires (the `tick` kfunc, fired every ~4 ms). |
| `total_sleeps / total_wakes: 5 / 5` | Matches the `loop: 5` `sleep: 1000us` cadence. |
| `total_idle_duration: 5.030ms` | CPU 0 was idle in the 1-ms windows between iterations. |
| `local_dsq_dispatches: 5` | All five wake-ups dispatched into the local (per-CPU) **DSQ** (Dispatch Queue). |

## Variations

- **Different scheduler.** Replace `--scheduler lavd` with `simple`,
  `cosmos`, `mitosis`, or `tickless`. See
  [Concepts → Schedulers](../concepts/schedulers.md).
- **Different CPU count.** `--cpus 2` halves the parallelism. With
  one task this changes very little; try
  `examples/cpu_bound.json` for a multi-CPU workload.
- **Different duration.** `--duration 500ms` extends the simulation;
  with `loop: 5` the task still finishes after the same ~9.7 ms and
  the simulator goes idle for the rest of the window.
- **Detailed per-task / per-CPU stats.** Append `--verbose-summary`.
  See [Your First Simulation](./first-simulation.md) for the
  full annotated output.
- **Capture the structops stream.** `--structops-jsonl /tmp/h.jsonl`
  writes a JSONL trace diffable against bpftrace captures from a
  live kernel. See [Trace Output](../running-simulations/trace-output.md).
- **Perfetto protobuf instead of JSON.** Add
  `--trace-format perfetto` (and use a `.pb` path) to emit a
  wprof-compatible Perfetto protobuf trace — smaller and faster to
  load than JSON for long runs, though not human-readable. Loadable
  by scxtop's `load_perfetto_trace` and side-by-side with wprof
  captures. See [Trace Output → Perfetto protobuf](../running-simulations/trace-output.md#perfetto-trace-protobuf).

## Next

- [Your First Simulation](./first-simulation.md) — annotated end-to-end
  walk-through using the same `examples/hello.json` workload.
- [Concepts → rt-app Workloads](../concepts/workloads.md) — the JSON
  schema scxsim accepts.
- [The `run` Subcommand](../running-simulations/run.md) — every flag,
  by purpose.
