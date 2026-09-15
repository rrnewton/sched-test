# Cgroup Bandwidth

scxsim models cgroup-v2 `cpu.max` bandwidth control end-to-end:
per-cgroup quota and period, runtime budget consumption, throttle
transitions, and a replenishment timer. This is what makes the H6 /
Bug-1 cgroup-bw stall reproducible inside the simulator.

The implementation lives in the [`scx_cgroup_tree`][cgroup-tree]
crate plus [`safe/cgroup.rs`][safe-cgroup] and
[`unsafe_impl/cgroup_ffi.rs`][unsafe-cgroup].

[cgroup-tree]: https://github.com/rrnewton/sched-test/tree/simulator.v6/scx-sim/crates/scx_cgroup_tree
[safe-cgroup]: https://github.com/rrnewton/sched-test/blob/simulator.v6/scx-sim/crates/scx_simulator/src/safe/cgroup.rs
[unsafe-cgroup]: https://github.com/rrnewton/sched-test/blob/simulator.v6/scx-sim/crates/scx_simulator/src/unsafe_impl/cgroup_ffi.rs

## Concept refresher

cgroup-v2 `cpu.max` is a file with two values:

```text
<quota_us> <period_us>
```

Within each `<period_us>` window, the cgroup may consume up to
`<quota_us>` microseconds of CPU time, summed across all CPUs and
all tasks in that cgroup. When the cgroup exhausts its quota, it is
**throttled**: tasks remain runnable but cannot run until the
**replenishment** event at the end of the current period restores
the budget.

`<quota>` may be the literal `max`, which means "no limit"
(equivalent to no `cpu.max` at all).

## How scxsim represents it

Each cgroup is a node in the tree maintained by
`scx_cgroup_tree`. A node carries:

- `path` — slash-separated path, e.g. `/background/io`.
- `cpu.max` — `(quota_ns, period_ns)` or `(MAX, period_ns)`.
- `cpu.weight` — optional weight some schedulers honor.
- `runtime_ns` — remaining budget in the current period. Decremented
  as tasks consume CPU; refilled to `quota_ns` on the replenishment
  event.
- `is_throttled` — boolean; set when `runtime_ns` would go negative,
  cleared at replenishment.
- Per-cgroup counters: `nr_throttled_periods`, etc.

When a task wakes and is dispatched, the engine charges its
forthcoming run slice against its cgroup's `runtime_ns`. If the
cgroup is throttled, the task stays runnable but is not picked.

## The accounting timer

scxsim simulates the cgroup-bandwidth accounting timer as a kernel
timer event. Two arming modes:

- **Initial period.** Set to `period_ns` after the first quota
  consumption. Replenishes the budget on fire.
- **MIN-bound re-arm.** When the current period ends mid-throttle,
  the timer re-arms at the next `MIN(period_remaining,
  refill_window)`. Under continuous throttling this can fire as
  often as 1 ms.

The associated trace event is `TraceKind::CgroupBwReplenish`,
emitted on every replenishment.

## The LAVD-side switch: `enable_cpu_bw`

The LAVD scheduler hard-codes `enable_cpu_bw = false` in
`lavd_setup()` — that is, **cgroup bandwidth code paths are
disabled by default in the simulator's initial state.** Without an
override, LAVD's `lavd_enqueue` short-circuits the
`cgroup_throttled()` check at
`scx/scheds/rust/scx_lavd/src/bpf/main.bpf.c:817`
(`if (enable_cpu_bw && …)`) and the entire cgroup-bw machinery is
bypassed.

This is intentional: it keeps small/quick reproducers fast and
avoids dragging the cgroup-bw model into investigations that don't
need it.

To enable cgroup bandwidth in LAVD, pass a TOML sidecar:

```toml
[scheduler.bool_globals]
enable_cpu_bw = true
```

```bash
scxsim run -s lavd --config my-config.toml ... <workload>
```

See [Running Simulations → Scheduler Config Sidecar](../running-simulations/scheduler-config.md)
for the full mechanism. **Forgetting `--config` is the most common
"why doesn't my cgroup-bw reproducer reproduce?" bug.**

## Worked example: throttled vs unconstrained cgroups

`examples/cgroup_hierarchy.json` declares two cgroups: `/background`
throttled at 20% of one CPU (`"20000 100000"`) and `/interactive`
unconstrained (`"max 100000"`). Each cgroup has two tasks.

Run it:

```bash
scxsim run -s lavd --cpus 4 --duration 100ms --verbose-summary \
    examples/cgroup_hierarchy.json
```

(Note: this exercises the cgroup-tree code path even without
`enable_cpu_bw` in LAVD, because cgroup membership and the trace
events are scheduler-independent. To see LAVD's *throttling decisions*
attributable to cgroup quotas, also pass
`--config <toml-with-enable_cpu_bw=true>`.)

Expected summary excerpt (preempt count comes from LAVD's
quota-aware preemption when `enable_cpu_bw` is on; without it, this
number is much lower):

```text
Simulation complete:
  Logical time elapsed:   100ms
  Total tasks:            4
  Max concurrent running: 4
  Total time slices:      70
  ...
  total_preempts:        15
```

## Worked example: the Bug-1 stall

The canonical reproducer is
[`tests/fixtures/h6/bug1_canonical.{json,toml}`][bug1]. The fixture
combines:

- Cgroups with tight `cpu.max` quotas (`"10000 100000"` = 10% of one
  CPU per cgroup).
- Long-running CPU-bound tasks that continuously hit the throttle.
- A TOML sidecar enabling LAVD's `enable_cpu_bw`.

Invocation:

```bash
scxsim run \
    -s lavd \
    --cpus 4 \
    --duration 500ms \
    --watchdog 80ms \
    --config crates/scx_simulator/tests/fixtures/h6/bug1_canonical.toml \
    crates/scx_simulator/tests/fixtures/h6/bug1_canonical.json
```

Without `--config`, this runs cleanly (the cgroup-bw code path never
executes). With `--config`, the dual-controller stall fires and the
watchdog trips at 80 ms with `ExitKind::ErrorStall` (exit code 42).

See [Recipes → Reproducing a Stall Bug](../recipes/repro-stall.md)
for the full walkthrough.

[bug1]: https://github.com/rrnewton/sched-test/tree/simulator.v6/scx-sim/crates/scx_simulator/tests/fixtures/h6

## Diagnostics

Useful flags / outputs for cgroup-bw investigations:

- `--verbose-summary` — adds per-cgroup throttle counters
  (`nr_throttled_periods`, `is_throttled` at end-of-run).
- `--structops-jsonl PATH` — emits `cgroup_bw_replenish` events
  per replenishment.
- `--dump-trace` — stderr-side per-event trace including
  `CGROUP_REPLENISH`.
- lldb attach (`--wait-debugger`) — inspect cgroup tree state via
  the helpers under
  [`lldb_debug/`](https://github.com/rrnewton/sched-test/tree/simulator.v6/scx-sim/lldb_debug);
  `bug1_diagnose` is the canonical cgroup-bw helper.
