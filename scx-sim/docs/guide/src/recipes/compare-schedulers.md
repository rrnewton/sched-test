# Comparing Two Schedulers

The recurring question: "given workload W, how does scheduler A
compare to scheduler B?" This recipe shows the canonical loop —
same workload, vary `--scheduler`, three concrete comparison
axes (Perfetto, summary stats, structops stream).

## Setup

Pick a workload and the schedulers under test. For this recipe:

```bash
WORKLOAD=examples/cpu_bound.json
SCHEDS=(simple lavd cosmos)
```

(See [Concepts → Schedulers](../concepts/schedulers.md) for the five
bundled options.)

## 1. Capture each run

Pin the seed so per-scheduler differences are scheduler-attributable,
not noise:

```bash
for s in "${SCHEDS[@]}"; do
    scxsim run \
        -s "$s" \
        --cpus 4 \
        --duration 200ms \
        --seed 42 \
        --perfetto "/tmp/$s.json" \
        --structops-jsonl "/tmp/$s.jsonl" \
        --verbose-summary \
        "$WORKLOAD" \
        > "/tmp/$s.summary.txt" 2>&1
done
```

Three artefacts per scheduler:

- `/tmp/$s.json` — Chrome JSON Perfetto trace.
- `/tmp/$s.jsonl` — structops/helpers JSONL.
- `/tmp/$s.summary.txt` — full summary including per-task / per-CPU
  distributions.

## 2. Axis 1: visualize

Drop each `/tmp/$s.json` into <https://ui.perfetto.dev/> in
sequence. Compare:

- **Per-CPU utilization** — which CPUs are busy when.
- **Slice length distribution** — long slices vs many short slices.
- **Idle gaps** — where the scheduler decided no task should run.
- **Migrations** — same task showing up on different CPUs over time.

Perfetto's "diff view" doesn't compare two traces directly, but
keeping two browser tabs open side-by-side is the de-facto
comparison UI.

For wprof-side comparison (where you also have a `vm-run --wprof`
trace from the same workload), use the protobuf format:
`--perfetto file.pb --trace-format perfetto`.

## 3. Axis 2: summary stats

The summary blocks at the end of each `/tmp/$s.summary.txt` give
quick numeric comparisons. Useful greps:

```bash
for s in "${SCHEDS[@]}"; do
    echo "=== $s ==="
    grep -E "^(Logical|Total time slices|All tasks completed|Overall CPU Utilization|total|longest_)" \
        "/tmp/$s.summary.txt"
done
```

Sample comparison (4-burner cpu_bound, 200 ms, seed 42, LAVD vs
simple):

| Metric | `simple` | `lavd` |
|---|---|---|
| Total time slices | (lower) | (higher; LAVD ranks more often) |
| `longest_structop_rbc` | tens | hundreds-to-low-thousands |
| `total rbc` | ~600 | ~50,000+ |
| `local_dsq_dispatches` | (direct) | (mostly local) |

The RBC (Retired Branch Count — a PMU / Performance Monitoring Unit
event counter; see [Glossary](../glossary.md)) ratio is the headline
"cost-of-scheduler" difference — see [Concepts →
Determinism](../concepts/determinism.md) for the PMU overhead model.

## 4. Axis 3: structops stream

The JSONL stream is the most precise comparison axis. Diff entire
streams or just count event kinds:

```bash
for s in "${SCHEDS[@]}"; do
    echo "=== $s ==="
    jq -r '.name' "/tmp/$s.jsonl" | sort | uniq -c | sort -rn | head
done
```

Sample output:

```text
=== simple ===
   161 select_cpu
   161 running
   161 stopping
   161 enqueue
    ...
=== lavd ===
   161 select_cpu
   161 running
   161 stopping
   161 enqueue
   161 update_idle
    ...
```

The same workload produces the same structural calls (5×161 across
the rt-app iterations); per-scheduler differences appear in
optional callbacks (`init_task`, `update_idle`, `cgroup_*`, etc.)
and in the per-event `args` payloads.

For decision-level comparison (`select_cpu` returned values, dispatch
slice lengths, etc.):

```bash
jq -c 'select(.name == "select_cpu" and .phase == "exit") | {ts: .ts_ns, ret: .ret}' \
    /tmp/lavd.jsonl > /tmp/lavd.select_cpu.txt
jq -c 'select(.name == "select_cpu" and .phase == "exit") | {ts: .ts_ns, ret: .ret}' \
    /tmp/cosmos.jsonl > /tmp/cosmos.select_cpu.txt
diff /tmp/lavd.select_cpu.txt /tmp/cosmos.select_cpu.txt | head
```

## 5. A scripted version

Pulling it together:

```bash
#!/usr/bin/env bash
set -euo pipefail
WORKLOAD=${1:?usage: $0 <workload.json> [sched1 sched2 ...]}
shift
SCHEDS=("${@:-simple lavd cosmos}")

for s in "${SCHEDS[@]}"; do
    scxsim run -s "$s" --cpus 4 --duration 200ms --seed 42 \
        --perfetto "/tmp/$s.json" \
        --structops-jsonl "/tmp/$s.jsonl" \
        --verbose-summary \
        "$WORKLOAD" > "/tmp/$s.summary.txt" 2>&1
done

# Summary table.
printf '%-12s %12s %12s %14s\n' SCHED SLICES TOTAL_RBC LONGEST_RBC
for s in "${SCHEDS[@]}"; do
    slices=$(grep 'Total time slices' "/tmp/$s.summary.txt" | awk '{print $NF}')
    rbc=$(awk '/^   total/{print $3}' "/tmp/$s.summary.txt")
    longest=$(awk '/longest_structop_rbc/{print $2}' "/tmp/$s.summary.txt")
    printf '%-12s %12s %12s %14s\n' "$s" "$slices" "$rbc" "$longest"
done
```

## Pitfalls

- **Forgetting `--seed`.** Without a pinned seed, two runs of the
  *same* scheduler differ; the comparison loses statistical power.
- **Mixing CPU counts.** A 4-CPU LAVD run is not comparable to a
  2-CPU `simple` run.
- **Comparing under stress mode.** If one run has `--preemptive` or
  `--stochastic-timer-interleave` and the other doesn't, the
  differences are dominated by the stress knob, not the scheduler.
  See [Concepts → Twin Design Principles](../concepts/twin-design-principles.md).
- **Forgetting feature flags.** LAVD with `enable_cpu_bw = false`
  (the default) is a *different scheduler* than LAVD with
  `enable_cpu_bw = true`. Pass the same `--config` to both sides if
  the workload exercises cgroup bandwidth.
