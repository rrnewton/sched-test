# Trace Output

scxsim emits six categories of output, each controlled by a single
flag. The summary table is at [Reference → Output Formats](../reference/output-formats.md);
this page shows sample fragments and how each is intended to be
consumed.

## Summary table

| Sink | Flag(s) | Format |
|---|---|---|
| Brief stderr summary | (always on) | Stderr text. |
| Verbose summary | `--verbose-summary` | Stderr text, distribution stats per task / per CPU. |
| Perfetto trace (JSON) | `--perfetto PATH` (default `--trace-format json`) | Chrome Trace Event JSON. Load in <https://ui.perfetto.dev/>. |
| Perfetto trace (protobuf) | `--perfetto PATH --trace-format perfetto` | wprof-compatible Perfetto protobuf. |
| Structops JSONL | `--structops-jsonl PATH` | JSONL, diffable against live bpftrace captures. |
| Live trace dump | `--dump-trace` | Stderr, one line per simulator event. |
| Preemption trace | `--record-preemptions PATH` | Text; replayable via `scxsim replay`. |

## Brief summary (always on)

Printed to stderr at end of run. Example
(`examples/hello.json`, `-s lavd --cpus 4 --duration 100ms`):

```text
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

Stable, line-by-line — CI scripts and the `bug_finding/` harness
match these labels by literal string.

## Verbose summary (`--verbose-summary`)

Adds distribution-statistics blocks per task and per CPU, plus a
global DSQ section. Example tail (same hello.json):

```text
=== Trace Statistics ===

Duration: 9.678ms

--- Per-Task Statistics ---
  Task PID=1:
    Schedules:       5
    Run duration:    0.927ms mean, 0.206ms stddev, CV=22.2%
    Inter-arrival:   1.995ms mean, 0.190ms stddev
    Direct dispatch: 0
    Enqueue calls:   0
    Yields:          0
    Preemptions:     0
    Sleeps:          5

--- Per-CPU Statistics ---
  CPU 0:
    Ticks:         2
    Tick interval: 3.996ms mean, 0.000ms stddev
    Balance calls: 5
    Idle events:   5
    Idle duration: 5.030ms
    Utilization:   48.0%

--- Overall CPU Utilization: 48.0% (1 CPUs, 9.678ms) ---

--- Global Statistics ---
  DSQ inserts (FIFO):    5
  DSQ inserts (vtime):   0
  DSQ move_to_local:     10
  Kick CPU calls:        0
  DSQ routing:
    Local/direct:        5 (100.0%)
```

Use this when you need to characterise a workload's shape (mean run
length, inter-arrival distribution, CPU utilization).

## Perfetto trace (JSON)

Default `--trace-format`. Chrome Trace Event JSON, loadable in any
Perfetto-compatible viewer.

```bash
scxsim run -s lavd --cpus 4 --duration 100ms \
    --perfetto /tmp/hello.json examples/hello.json
```

Then drop `/tmp/hello.json` onto <https://ui.perfetto.dev/>.

Structure (sample event fragments):

```json
{"ph":"M","pid":0,"tid":0,"name":"process_name","args":{"name":"CPU 0"}}
{"ph":"i","pid":0,"tid":0,"ts":0,"name":"wake","s":"t","args":{"task":"hello","pid":1}}
{"ph":"i","pid":0,"tid":0,"ts":0,"name":"dsq_insert","cat":"kfunc","s":"t","args":{"pid":1,"dsq_id":"LOCAL","slice_ns":5000000}}
{"ph":"i","pid":0,"tid":0,"ts":0,"name":"select_task_rq","cat":"ops","s":"t","args":{"pid":1,"prev_cpu":0,"selected_cpu":0}}
{"ph":"B","pid":0,"tid":0,"ts":4,"name":"hello","cat":"sched","args":{"pid":1}}
{"ph":"E","pid":0,"tid":0,"ts":732,"cat":"sched","args":{"reason":"slept","pid":1}}
```

- `ph:"M"` — metadata (CPU process names).
- `ph:"i"` — instant marker (a single point-in-time event).
- `ph:"B"`/`ph:"E"` — begin/end of a duration slice (a task's
  on-CPU interval).
- `cat:"ops"` — sched_ext struct_ops callback.
- `cat:"kfunc"` — sched_ext helper (`scx_bpf_*`) call.
- `cat:"sched"` — engine-side scheduling events (slice begin/end).

## Perfetto trace (protobuf)

```bash
scxsim run -s lavd --cpus 4 --duration 200ms \
    --perfetto /tmp/run.pb --trace-format perfetto \
    examples/cpu_bound.json
```

wprof-compatible Perfetto protobuf. TrackEvent slices/instants with
`debug_annotations` matching wprof's vocabulary. Loadable by
scxtop's `load_perfetto_trace`; intended for side-by-side comparison
with wprof traces captured under `vm-run --wprof`. See
`experiments/wprof_trace_baseline_20260513/REPORT.md` §3 for the
wprof event schema.

## Structops JSONL

```bash
scxsim run -s lavd --cpus 4 --duration 100ms \
    --structops-jsonl /tmp/hello.jsonl examples/hello.json
```

One JSON object per line, schema-compatible with
`scripts/probes/structops_full.bt` + `scripts/probes/helpers_full.bt`.
Sample:

```json
{"ts_ns":600,"cpu":0,"pid":0,"kind":"helper","name":"dsq_insert","phase":"entry","args":{"task_pid":1,"dsq_id":9223372036854775810,"slice":5000000,"enq_flags":0},"ret":null}
{"ts_ns":0,"cpu":0,"pid":0,"kind":"structop","name":"select_cpu","phase":"entry","args":{"task_pid":1,"prev_cpu":0,"wake_flags":0},"ret":null}
{"ts_ns":0,"cpu":0,"pid":0,"kind":"structop","name":"select_cpu","phase":"exit","args":{},"ret":0}
{"ts_ns":4840,"cpu":0,"pid":0,"kind":"structop","name":"running","phase":"entry","args":{"task_pid":1},"ret":null}
{"ts_ns":732745,"cpu":0,"pid":0,"kind":"structop","name":"stopping","phase":"entry","args":{"task_pid":1,"still_runnable":0},"ret":null}
```

Per-event fields:

- `ts_ns` — simulated time (nanoseconds).
- `cpu` — CPU executing the event.
- `pid` — current task pid (0 = idle).
- `kind` — `structop` (callback) or `helper` (kfunc).
- `name` — `select_cpu`, `enqueue`, `dispatch`, `running`,
  `stopping`, `dsq_insert`, etc.
- `phase` — `entry` or `exit`.
- `args` — per-callback arguments.
- `ret` — return value on `exit` (else `null`).

This stream is **the** canonical fidelity check. Diff against a
bpftrace-captured stream from a live kernel via
`scripts/compare_live_vs_scxsim_calls.sh`; divergence is the unit
of "scxsim debt." See
[Twin Design Principles](../concepts/twin-design-principles.md).

## Live trace dump (`--dump-trace`)

Stderr-side per-event trace. One human-readable line per simulator
event. Sample:

```text
[              0:cpu0] cpu=0   WAKE     pid=1
[            600:cpu0] cpu=0   DSQ_INS  pid=1 dsq=0x8000000000000002 slice=5000000
[              0:cpu0] cpu=0   SELECT_CPU pid=1 prev=0 sel=0
[          3_300:cpu0] cpu=0   PICK     pid=1
[          4_840:cpu0] cpu=0   SET_NEXT pid=1
[          4_840:cpu0] cpu=0   SCHED    pid=1
[        732_745:cpu0] cpu=0   PUT_PREV pid=1 runnable=false
[        732_745:cpu0] cpu=0   SLEEP    pid=1
[        734_605:cpu0] cpu=0   BALANCE  prev_pid=1
[        734_605:cpu0] cpu=0   IDLE
```

Format: `[<sim_ns>:<emitting cpu>] cpu=<acting cpu> <EVENT> <args>`.
Useful for line-by-line debugging without needing to load a viewer.
Combine with `2>&1 | grep PUT_PREV` (etc.) for tight inspection
loops.

## Preemption trace (`--record-preemptions`)

```bash
scxsim run -s lavd --cpus 4 --duration 30ms \
    --record-preemptions /tmp/p.txt examples/hello.json
```

Text file with a metadata header followed by one line per recorded
preemption point. Header of a sample file (when nothing actually
preempted):

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

Replay it with `scxsim replay /tmp/p.txt`. See
[Replaying Preemption Traces](./replay.md).

## Which sink for which task?

| You want to … | Use |
|---|---|
| Eyeball "did it work?" | Brief summary (default). |
| Characterize a workload's run-length / inter-arrival shape | `--verbose-summary`. |
| Visualize a schedule | `--perfetto file.json` → ui.perfetto.dev. |
| Compare against a wprof capture | `--perfetto file.pb --trace-format perfetto`. |
| Compare scheduler decisions against a live kernel | `--structops-jsonl` + `scripts/compare_live_vs_scxsim_calls.sh`. |
| Step through events in a terminal | `--dump-trace`. |
| Make a deterministic, replayable trace | `--record-preemptions`. |
