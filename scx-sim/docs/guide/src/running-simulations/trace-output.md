# Trace Output

> **Status — stub.** This page will document each trace sink.

| Sink | Flag(s) | Format |
|---|---|---|
| Perfetto trace (JSON) | `--perfetto PATH --trace-format json` (default) | Chrome Trace Event JSON; load directly in <https://ui.perfetto.dev/>. |
| Perfetto trace (protobuf) | `--perfetto PATH --trace-format perfetto` | wprof-compatible Perfetto protobuf TrackEvent stream; readable by scxtop's `load_perfetto_trace`. |
| Structops JSONL | `--structops-jsonl PATH` | JSONL, schema-compatible with [`scripts/probes/structops_full.bt`](https://github.com/facebookexperimental/sched-test/blob/simulator.v6/scx-sim/scripts/probes/structops_full.bt); diffable against live captures via `scripts/compare_live_vs_scxsim_calls.sh`. |
| Live trace dump | `--dump-trace` | Stderr text (one line per simulator event). |
| Summary stats | (default at end of run); `--verbose-summary` for per-task / per-CPU distributions | Stdout text. |
| Preemption trace | `--record-preemptions PATH` | Text file grouped by worker; replayable via `scxsim replay`. |

See also [Output Formats](../reference/output-formats.md) for the
full table including exit codes and stderr markers.
