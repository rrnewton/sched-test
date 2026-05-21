# Reproducing a Stall Bug

This recipe walks through the canonical Bug-1 reproducer end-to-end:
how the fixture is constructed, what the invocation actually proves,
and how to diagnose. Bug-1 is the production cgroup-bandwidth stall
that motivated much of the H6 investigation track.

**Inputs** (all in-tree):

- Workload: [`tests/fixtures/h6/bug1_canonical.json`][bug1-json]
- Sidecar:  [`tests/fixtures/h6/bug1_canonical.toml`][bug1-toml]
- Test harness: `tests/bug1_canonical_repro.rs`
- Background: `crates/scx_simulator/tests/fixtures/h6/README.md`

[bug1-json]: https://github.com/facebookexperimental/sched-test/blob/simulator.v6/scx-sim/crates/scx_simulator/tests/fixtures/h6/bug1_canonical.json
[bug1-toml]: https://github.com/facebookexperimental/sched-test/blob/simulator.v6/scx-sim/crates/scx_simulator/tests/fixtures/h6/bug1_canonical.toml

## TL;DR

```bash
scxsim run \
    -s lavd \
    --cpus 4 \
    --duration 500ms \
    --watchdog 80ms \
    --config crates/scx_simulator/tests/fixtures/h6/bug1_canonical.toml \
    crates/scx_simulator/tests/fixtures/h6/bug1_canonical.json
```

**Expected outcome:** non-zero exit with `ExitKind::ErrorStall` (exit
code 42). The watchdog trips because runnable tasks remain
unscheduled past the 80 ms simulated-time threshold.

## What each piece does

### The workload

Bug-1's `bug1_canonical.json` declares cgroups with tight `cpu.max`
quotas (`"10000 100000"` = 10% of one CPU per cgroup) and long-running
CPU-bound tasks that hit the throttle continuously. The exact mix —
number of cgroups, number of tasks per cgroup, run-to-sleep ratio —
was reduced from a captured production trace via a delta-debugging
loop until removing any one element broke the reproduction.

### The TOML sidecar

```toml
[scheduler.bool_globals]
enable_cpu_bw = true
```

This is the **single most important line of the reproducer.** Without
it, LAVD's `lavd_setup()` hard-codes `enable_cpu_bw = false` for the
simulator's initial state, `lavd_enqueue` short-circuits the
`cgroup_throttled()` check at
`scx/scheds/rust/scx_lavd/src/bpf/main.bpf.c:817`
(`if (enable_cpu_bw && …)`), and the cgroup-bandwidth code path
**never runs.** The workload then completes cleanly and the bug
appears to be fixed — but only because its trigger was bypassed.

This is the "Forgetting `--config`" pitfall documented at
[Scheduler Config Sidecar → Common pitfalls](../running-simulations/scheduler-config.md#common-pitfalls).

### The short watchdog

`--watchdog 80ms` is what makes the reproducer fast. The default
30 s watchdog would catch the same stall but would simulate ~30 s of
virtual time first — slow even in scxsim. 80 ms is well above the
quota-replenishment period of 100 ms but short enough that a true
stall is caught quickly.

### The 4 CPUs

The dual-controller stall (LAVD's vtime ranking competing with
cgroup-bw throttling) requires enough CPUs to expose the race
window. With `--cpus 1` or `--cpus 2` the reproducer is unreliable;
with `--cpus 4` it is deterministic given the default seed.

## What the invocation proves

**Clean repro (current `simulator.v6`):**

```text
scxsim: disabling ASLR and re-executing...
scxsim: ExitKind::ErrorStall: watchdog tripped at 80ms ...
```

Exit code: 42.

**Pre-fix (a regressed scheduler revision):** same exit kind, same
watchdog timestamp, same trace shape. Determinism is the whole
point: the exit *kind* AND the exit *time* match across runs of the
same `.so`, and a regression that changes the time is itself signal.

## Diagnosing once it reproduces

Once you have a reliable stall, the productive next steps:

### 1. Verbose summary

```bash
scxsim run -s lavd --cpus 4 --duration 500ms --watchdog 80ms \
    --config crates/scx_simulator/tests/fixtures/h6/bug1_canonical.toml \
    --verbose-summary \
    crates/scx_simulator/tests/fixtures/h6/bug1_canonical.json
```

Look for per-task lines with `Schedules: 0` or near-zero — those are
the throttled tasks the watchdog tripped on. Cross-reference against
the per-task cgroup membership to identify which cgroup's quota was
the bottleneck.

### 2. Perfetto trace

```bash
scxsim run -s lavd --cpus 4 --duration 500ms --watchdog 80ms \
    --config crates/scx_simulator/tests/fixtures/h6/bug1_canonical.toml \
    --perfetto /tmp/bug1.json \
    crates/scx_simulator/tests/fixtures/h6/bug1_canonical.json
```

Drop `/tmp/bug1.json` onto <https://ui.perfetto.dev/>. The
diagnostic shape: long idle gaps on CPU tracks while runnable-task
markers accumulate. Click any `dsq_insert` instant to see the task
that wanted to run; click the next `pick_task` (if any) to see how
long the stall lasted.

### 3. Structops JSONL diff against bpftrace

```bash
# Simulator side.
scxsim run -s lavd --cpus 4 --duration 500ms --watchdog 80ms \
    --config crates/scx_simulator/tests/fixtures/h6/bug1_canonical.toml \
    --structops-jsonl /tmp/bug1-sim.jsonl \
    crates/scx_simulator/tests/fixtures/h6/bug1_canonical.json

# Kernel side (under vm-run with bpftrace probes).
scxsim vm-run -s lavd --cpus 4 --bpf-trace \
    crates/scx_simulator/tests/fixtures/h6/bug1_canonical.json
# => writes bpf_trace.log

scripts/compare_live_vs_scxsim_calls.sh /tmp/bug1-sim.jsonl bpf_trace.log
```

A clean diff confirms the simulator is faithfully reproducing the
kernel-side behaviour. A divergent diff identifies a specific
struct_ops / helper call that scxsim and the kernel handle
differently — file as `twin-design-principles` debt.

### 4. lldb attach

```bash
scxsim run -s lavd --cpus 4 --duration 500ms --watchdog 80ms \
    --config crates/scx_simulator/tests/fixtures/h6/bug1_canonical.toml \
    --wait-debugger \
    crates/scx_simulator/tests/fixtures/h6/bug1_canonical.json
```

scxsim prints an attach command. From lldb, `bug1_diagnose` walks the
cgroup tree, prints per-cgroup `runtime_ns` and `is_throttled`, and
identifies which cgroup is the deadlock pivot. See
[Recipes → Debugging with LLDB](./lldb.md).

## Hardening a new reproducer

Adapting this pattern for a different bug:

1. **Capture live.** Bpftrace probes on the failing production trace.
2. **Strip down.** Delta-debug the workload JSON until removing any
   one element breaks reproduction. Result is a minimal fixture.
3. **Identify the gating BPF feature flag.** Use `--list-schedulers`
   + skim the scheduler source for `enable_*` / `disable_*` `const
   volatile` globals. Write a TOML sidecar.
4. **Bound the watchdog.** Choose the shortest watchdog that catches
   the stall reliably; below that, you get false negatives. The Bug-1
   80 ms is "comfortably above the 100 ms replenishment period
   minus some margin."
5. **Pin determinism.** `--seed 42` (or any specific u32). Confirm
   with `--determinism-check` that re-runs are stable. Confirm with
   `--record-preemptions /tmp/p.txt` + `scxsim replay /tmp/p.txt`
   that the trace is replayable.
6. **Write a `tests/bug<N>_canonical_repro.rs`** that wires the
   fixture into Rust integration tests, asserting on the expected
   `ExitKind`.

## See also

- [Concepts → Cgroup Bandwidth](../concepts/cgroup-bw.md) — the
  cgroup-bw machinery that this reproducer exercises.
- [Scheduler Config Sidecar (TOML)](../running-simulations/scheduler-config.md) —
  why `--config` is required and the four sub-tables.
- [Reference → Exit Codes](../reference/exit-codes.md) — the full
  ExitKind enum and what each non-zero exit means.
