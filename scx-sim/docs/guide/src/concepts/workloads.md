# rt-app Workloads

scxsim accepts [rt-app][rtapp]-compatible JSON workloads. A workload
describes a set of named tasks, what they do in their inner loop
(`run` / `sleep` / `suspend` / `resume` actions), an optional
priority, and optional cgroup membership.

The parser lives in [`safe/rtapp.rs`][rtapp-rs]; this page documents
the subset and the scxsim-specific extensions.

[rtapp]: https://github.com/scheduler-tools/rt-app
[rtapp-rs]: https://github.com/rrnewton/sched-test/blob/simulator.v6/scx-sim/crates/scx_simulator/src/safe/rtapp.rs

## Minimal shape

```json
{{#include ../../../../examples/hello.json}}
```

The two required top-level keys are `global` and `tasks`. Everything
else is optional.

## Global section

| Key | Type | Meaning |
|---|---|---|
| `duration` | `float` (seconds) | Workload's intended runtime. The simulator stops at `min(global.duration, --duration)`. Fractional values allowed: `0.05` = 50 ms. |
| `default_policy` | `string` | rt-app POSIX scheduling policy (`SCHED_OTHER`, `SCHED_FIFO`, ...). Accepted but not used by sched_ext; included for round-trip compatibility with rt-app captures. |
| `calibration` | `int` | rt-app calibration value. Accepted; scxsim uses virtual time instead of calibration loops. |

Any other `global` key is preserved without effect.

## Task section

```json
"tasks": {
  "<name>": {
    "priority": 0,
    "loop": 5,
    "run": 1000,
    "sleep": 1000
  }
}
```

| Key | Units / type | Meaning |
|---|---|---|
| `priority` | int | rt-app `nice`-equivalent. Forwarded to the scheduler as a hint. |
| `loop` | int | Iteration count. `-1` means "loop until `global.duration` elapses." |
| `run` | int **µs** | Time to spend on-CPU each iteration. Aliases: `runtime`. |
| `sleep` | int **µs** | Time to sleep between iterations. |
| `suspend` | int **µs** | Long-form sleep that pauses the task (used in `phases`). |
| `resume` | int **µs** | Wake the task after this many µs. |
| `timer` | int **µs** | Periodic-timer mode: `run` length, period = `timer` (next deadline). |
| `cpus` | array of int | CPU affinity mask (which CPU IDs this task may run on). |
| `taskgroup` | string OR object | Cgroup membership (see below). |
| `phases` | array of phase objects | Replace the simple `run`/`sleep` loop with a sequence of named phases. Each phase has its own `run` / `sleep` / `loop`. |
| `instance` | int | Replicate this task definition N times. |

> ⚠️ **Time units.** `global.duration` is **seconds**; per-task `run`
> and `sleep` are **microseconds**. This matches rt-app's wire
> format; mixing them up is the most common workload-authoring bug.

### Actions that are accepted but silently skipped

These are accepted by the parser (for round-trip compatibility with
rt-app workloads captured from a kernel) but have no effect in
scxsim, because the underlying subsystem is not modeled. A warning is
emitted on parse:

`lock`, `unlock`, `wait`, `signal`, `broad`, `sync`, `mem`,
`barrier`, `fork`.

`yield` and `iorun` are **not** in that list, though earlier revisions
of this page said they were. `yield` maps to `Phase::Yield`, and
`iorun` is described below.

See [Concepts → What scxsim Simulates](./what-scxsim-simulates.md)
for what is and isn't modeled and why.

### `iorun` — supported through one calibrated profile, or refused

`"iorun": <bytes>` is declared in bytes, exactly as I/O model v1's
input is. Its physics are the opposite: rt-app's `ioload()` is a
*buffered, unsynced* `write(2)` loop whose `io_device` defaults to
`/dev/null`, so it never blocks. Cost is driven by the **number of
`write` calls**, `ceil(bytes / global.mem_buffer_size)` — not by the
byte count, because `/dev/null` consumes a write without copying it.

Two consequences that surprise people:

- **`global.mem_buffer_size` is required, not decorative.** The same
  `"iorun"` value with a different buffer costs proportionally
  differently. Holding declared bytes fixed and sweeping the buffer
  moved measured cost 215×.
- **rt-app's own default buffer is 4 MiB**, so a default-configured
  `"iorun": 8388608` is *two* syscalls costing about a microsecond —
  not "8 MiB of I/O".

An `iorun` outside the calibrated regime is a **parse error**, not a
skipped event. Refused cases: any `io_device` other than `/dev/null`;
declared bytes above 2,147,483,647 (rt-app's own 32-bit `int`
saturates there and would silently shrink the workload);
`mem_buffer_size` outside 4 KiB–1 MiB; a resolved call count outside
512–65,536. A dropped event fails visibly; a wrongly-modelled one does
not.

Evidence, including the blind held-out evaluation:
`experiments/io_model_rtapp_iorun_20260814/` in the development
harness.

## Cgroup hierarchy

Place a task in a cgroup by appending a `taskgroup`:

### String form (membership only)

```json
"interactive_1": {
    "loop": -1, "run": 1000, "sleep": 9000,
    "taskgroup": "/interactive"
}
```

The path must start with `/`. The cgroup is created if it doesn't
exist; its `cpu.max` defaults to unlimited.

### Object form (membership + `cpu.max`)

```json
"background_0": {
    "loop": -1, "run": 5000, "sleep": 1000,
    "taskgroup": {
        "path": "/background",
        "cpu.max": "20000 100000"
    }
}
```

| Field | Meaning |
|---|---|
| `path` | Cgroup path (must start with `/`). Nested paths are allowed: `/a/b`. |
| `cpu.max` | `"<quota_us> <period_us>"` (cgroup-v2 convention). E.g. `"20000 100000"` = 20% of one CPU. `"max <period>"` removes the quota cap. |
| `cpu.weight` | Optional weight; some schedulers honor it. |

Multiple tasks in the same cgroup share that cgroup's quota — exactly
as with real cgroup-v2.

See [Concepts → Cgroup Bandwidth](./cgroup-bw.md) for what the
simulator does with `cpu.max`.

## Worked example: cgroup hierarchy

The bundled `examples/cgroup_hierarchy.json` exercises both
above-default and at-default cgroups:

```json
{{#include ../../../../examples/cgroup_hierarchy.json}}
```

`background_0` declares `cpu.max = "20000 100000"` (20% of one CPU);
`background_1` joins the same `/background` cgroup and inherits that
quota; `interactive_0` declares `cpu.max = "max 100000"` (unlimited);
`interactive_1` joins `/interactive` and inherits unlimited.

Run it:

```bash
scxsim run -s lavd --cpus 4 --duration 100ms examples/cgroup_hierarchy.json
```

Excerpt of expected output (the `total_preempts: 15` line reflects
LAVD throttle-driven preemption of the background tasks once their
20% quota is exhausted):

```text
Simulation complete:
  Logical time elapsed:   100ms
  Total tasks:            4
  Max concurrent running: 4
  Total time slices:      70
  ...
  total_preempts:        15
  ...
```

## Sources

- Parser: [`safe/rtapp.rs`][rtapp-rs] (canonical schema).
- Examples: [`scx-sim/examples/`][examples-tree].
- Fixtures: [`scx-sim/crates/scx_simulator/workloads/`][workloads-tree]
  and [`tests/fixtures/`][fixtures-tree].

[examples-tree]: https://github.com/rrnewton/sched-test/tree/simulator.v6/scx-sim/examples
[workloads-tree]: https://github.com/rrnewton/sched-test/tree/simulator.v6/scx-sim/crates/scx_simulator/workloads
[fixtures-tree]: https://github.com/rrnewton/sched-test/tree/simulator.v6/scx-sim/crates/scx_simulator/tests/fixtures
