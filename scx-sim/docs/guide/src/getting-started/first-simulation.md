# Your First Simulation

This page walks through the bundled `examples/hello.json` workload
end-to-end: the JSON, the simulator's response, the Perfetto trace,
and the four follow-on experiments you can run with one flag.

## 1. Inspect the workload

[`examples/hello.json`][hello-json]:

```json
{{#include ../../../../examples/hello.json}}
```

[hello-json]: https://github.com/facebookexperimental/sched-test/blob/simulator.v6/scx-sim/examples/hello.json

Each field:

- `global.duration: 0.05` — workload-side end time in **seconds**
  (50 ms). The `--duration` CLI flag overrides this. Without an
  override, the simulation stops at the smaller of `global.duration`
  and any active CLI duration.
- `global.default_policy: "SCHED_OTHER"` — sched_ext-only mode; rt-app
  POSIX scheduling-policy enum, not used by scxsim's scheduling
  decisions (those come from the loaded scheduler `.so` — the
  scheduler's C source compiled to native code, see
  [Introduction](../introduction.md)).
- `tasks.hello.priority: 0` — nice-equivalent; passed to the
  scheduler as a hint.
- `tasks.hello.loop: 5` — iteration count. `-1` means "loop until
  `global.duration` elapses."
- `tasks.hello.run: 1000` — microseconds of simulated CPU work per
  iteration.
- `tasks.hello.sleep: 1000` — microseconds of sleep between
  iterations.

The expected total task lifetime is therefore
`5 * (1000us run + 1000us sleep) = 10 ms`, of which the trailing
sleep is truncated when the workload ends — so the task completes at
~9 ms. The simulator runs to the 100 ms duration we pass on the CLI;
the remaining 91 ms is idle time.

See [Concepts → rt-app Workloads](../concepts/workloads.md) for the
full schema.

## 2. Run it with LAVD

```bash
scxsim run \
    --scheduler lavd \
    --cpus 4 \
    --duration 100ms \
    --seed 42 \
    --perfetto /tmp/hello.json \
    examples/hello.json
```

Output is reproduced in full at [Quick Start](./quick-start.md). The
two anchor lines are:

```text
  All tasks completed:    9.6ms (9.7% of simulation)
  local_dsq_dispatches:  5
```

That is: five dispatches (one per `loop` iteration), workload finished
at ~9.6 ms simulated.

## 3. Compare against `simple`

The `simple` scheduler is the minimal-viable baseline; it does
direct-dispatch to the local CPU's DSQ and nothing else. Use it to
verify that observed differences are scheduler-attributable.

```bash
scxsim run -s simple --cpus 1 --duration 50ms examples/hello.json
```

Last lines:

```text
Sched_ext structop summary:
     cpu   structops         rbc      kfuncs
  ------  ----------  ----------  ----------
       0          52         654          16
  ------  ----------  ----------  ----------
   total          52         654          16

Preemption stats:
  longest_structop_rbc:    41
  longest_rbc_interval:    21  (between kfuncs)
  REPLAY_MARGIN:           200

Trace Summary:
  total_events:          59
  ...
  local_dsq_dispatches:  5
```

Same five dispatches, very different RBC count: `simple` retires
`654` conditional branches inside scheduler callbacks vs LAVD's
`11_954`. (**RBC** = Retired Branch Count, a Performance Monitoring
Unit / PMU event counter — see the [Glossary](../glossary.md).) That
~20x ratio is the cost of LAVD's vtime / cgroup-bw / selection
machinery vs `simple`'s "put it on this CPU, done."

The PMU-overhead model (`--rbc-ns 10`, the default) translates that
RBC count into simulated wallclock charged against the dispatch path.
See [Concepts → Determinism](../concepts/determinism.md) for the
math and the knobs.

## 4. Prove the run is deterministic

```bash
scxsim run --scheduler lavd --cpus 4 --duration 50ms --seed 42 \
    --determinism-check examples/hello.json
```

Last line:

```text
Determinism check PASSED: 22 checkpoints matched
```

`--determinism-check` runs the simulation twice internally,
collects an aggressive checkpoint sequence on each run, and exits
non-zero if any checkpoint diverges. This is the canonical CI
gate for "my new scheduler change did not introduce non-determinism."

For the explicit "two runs, byte-compare the JSONL trace" version of
the same check, see
[Recipes → Verifying Determinism](../recipes/verify-determinism.md).

## 5. Open the Perfetto trace

```bash
scxsim run -s lavd --cpus 4 --duration 100ms \
    --perfetto /tmp/hello.json examples/hello.json
```

Then open <https://ui.perfetto.dev/> and drop `/tmp/hello.json` onto
the page. You will see:

- Four CPU tracks (`CPU 0` .. `CPU 3`); only `CPU 0` is busy in this
  workload.
- A blue `hello` slice block on `CPU 0` repeated five times. Each
  block is the body of one `run: 1000` iteration.
- Instant markers (`i` events) for the scheduler callbacks bracketing
  each slice: `wake`, `select_task_rq`, `dsq_insert`, `pick_task`,
  `set_next_task`, then on the trailing edge `put_prev_task`,
  `dsq_move_to_local`, `balance`, `idle`.
- A final `completed` instant marker at ~9.7 ms, after which CPU 0
  stays idle.

The same trace stream emitted as wprof-compatible Perfetto protobuf is
available via `--trace-format perfetto`; see
[Trace Output](../running-simulations/trace-output.md).

## 6. Toggle realism

Drop the engine's two realism dimensions to see the effect:

```bash
scxsim run -s lavd --cpus 4 --duration 100ms \
    --no-noise --no-overhead --rbc-ns 0 examples/hello.json
```

`--no-noise` removes tick-jitter, `--no-overhead` removes the random
per-context-switch overhead noise, and `--rbc-ns 0` (or the alias
`--no-rbc`) zeros the PMU-derived scheduler overhead. The resulting
trace is the "purely deterministic, zero-cost scheduler" upper bound.
Useful as a baseline when you suspect realism noise is masking a
small structural change.

Conversely, `--rbc-ns 100` (10× default) exaggerates the scheduler's
own CPU footprint; combined with `--preemptive`, this stress-tests
preemption behaviour on long callbacks. See
[Concepts → Twin Design Principles](../concepts/twin-design-principles.md)
for when to reach for these knobs.

## Where to go next

- [Concepts → rt-app Workloads](../concepts/workloads.md) — write your
  own JSON.
- [Concepts → Schedulers](../concepts/schedulers.md) — pick the right
  `.so` for your investigation.
- [The `run` Subcommand](../running-simulations/run.md) — every flag,
  by purpose.
- [Recipes](../recipes.md) — task-shaped how-tos: stalls, comparisons,
  determinism, lldb attach.
