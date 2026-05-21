# Replaying Preemption Traces

The `replay` subcommand is the second half of the **record → replay**
loop. It re-runs a previously captured preemption trace under the
same scheduler `.so`, reproducing the exact preemption points seen at
record time. This is how non-deterministic preemption sites become
debuggable, reproducible artefacts.

## Why this exists

The default `scxsim run` is deterministic given a `--seed`. Once you
add `--preemptive`, however, preemption fires on PMU retired-branch
counts, and PMU delivery has **skid** (~30–100 branches between
"counter overflows" and "signal handler runs"). This makes runs
divergent on each invocation — until you pin the preemption points
into a trace and replay from it.

The recorded trace captures **where** each preemption fired (RBC
count + dynamic instance + worker). Replay re-injects each
preemption at exactly the same place by setting hardware
breakpoints — eliminating skid.

## Record

Add `--record-preemptions PATH` to any `scxsim run`:

```bash
scxsim run -s lavd --cpus 4 --duration 200ms \
    --preemptive \
    --record-preemptions /tmp/preempts.txt \
    examples/cpu_bound.json
```

(`--preemptive` is optional — without it, the recording captures
co-operative yield points only, which is still useful for debugger
stepping.)

The resulting file has a metadata header and one line per recorded
preemption point. Header sample:

```text
# scxsim preemption trace
# workers: 4
# break_on: rbc
# total: 0
# nr_cpus: 4
# nr_tasks: 1
# seed: 42
# duration_ns: 30000000
# scheduler: lavd
# so_hash: 0xa4f5c653d5aed108
# so_path: /.../target/release/build/scx_simulator-.../out/schedulers/libscx_lavd.so
```

Header fields:

| Field | Meaning |
|---|---|
| `workers` | Number of dispatch worker threads. |
| `break_on` | PMU event used (`rbc` or `insn`). |
| `total` | Total preemption points recorded. |
| `nr_cpus`, `nr_tasks`, `seed` | Topology and seed. |
| `duration_ns` | Simulated duration. |
| `scheduler` | Scheduler name. |
| `so_hash`, `so_path` | Hash + path of the scheduler `.so` used at record time. |

`so_hash` is checked at replay-load time; mismatch is a hard error.

## Replay

```bash
scxsim replay /tmp/preempts.txt
```

The `.so` path is taken from the trace header; if the file has moved
since recording, override with `--scheduler-file`:

```bash
scxsim replay --scheduler-file /new/path/libscx_lavd.so /tmp/preempts.txt
```

## `scxsim replay` options

| Flag | Purpose |
|---|---|
| `--scheduler-file <PATH>` | Override the `.so` path stored in the trace (use after moving the binary). |
| `--verbose-summary` | Same per-task / per-CPU distribution stats as `run --verbose-summary`. |
| `--record-preemptions <PATH>` | Re-record during replay (write a new trace). |
| `--no-pmu-signal` | Use only hardware breakpoint stepping. Slower but eliminates PMU skid. |
| `--preempt-mode {pmu, e9patch}` | Preemption mechanism. `e9patch` requires the `_e9.so` variant; deterministic and debugger-compatible. |
| `--wait-debugger` | Pause before `ops.init()` for lldb attach. |
| `--no-disable-aslr` | Skip ASLR-disable + re-exec (loses stable `.so` base addresses). |

## When to use which mechanism

| Scenario | Mechanism | Flags |
|---|---|---|
| Fast inner loop | PMU + breakpoint | `--preempt-mode pmu` (default) |
| Deterministic replay (no PMU hardware) | e9patch | `--preempt-mode e9patch` |
| Debugger-attached step-through | e9patch | `--preempt-mode e9patch --wait-debugger` |
| Maximum determinism (slow) | PMU off | `--no-pmu-signal` |

The e9patch variant requires a pre-built `_e9.so` next to the regular
`.so`; install via `scripts/install_e9patch.sh`.

## The full record → replay → re-record loop

```bash
# 1. Record under stress mode.
scxsim run -s lavd --cpus 4 --duration 500ms \
    --preemptive --stochastic-timer-interleave \
    --seed entropy \
    --record-preemptions /tmp/p1.txt \
    examples/cpu_bound.json

# 2. Replay (deterministic re-run).
scxsim replay /tmp/p1.txt

# 3. Re-record while replaying (sometimes captures additional
#    preemption points stabilized by replay's breakpoint stepping).
scxsim replay --record-preemptions /tmp/p2.txt /tmp/p1.txt
```

The "re-record on replay" step is rarely needed but matters when
chasing a preemption point that is itself near the PMU-skid window.

## Cross-machine replay

If `so_hash` matches, a replay trace can be replayed on a different
machine. This is how a Bug-1 reproducer captured on one developer's
workstation moves to CI: ship the `.so`, the workload JSON, the TOML
sidecar, and the preemption-trace file, and any machine can
`scxsim replay` it.

See [Concepts → Determinism](../concepts/determinism.md) for the
broader determinism contract.
