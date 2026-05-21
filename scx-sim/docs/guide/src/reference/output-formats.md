# Output Formats

> **Status — stub.** This page will fully document each output sink
> with sample fragments.

| Sink | Flag / mechanism | Format |
|---|---|---|
| Perfetto trace (JSON) | `--perfetto PATH --trace-format json` (default) | Chrome Trace Event JSON, loadable in <https://ui.perfetto.dev/>. |
| Perfetto trace (protobuf) | `--perfetto PATH --trace-format perfetto` | wprof-compatible Perfetto protobuf TrackEvent slices/instants; readable by scxtop's `load_perfetto_trace`; side-by-side with wprof. |
| Structops JSONL | `--structops-jsonl PATH` | JSONL, schema-compatible with `scripts/probes/structops_full.bt` + `helpers_full.bt`; diffable via `scripts/compare_live_vs_scxsim_calls.sh`. |
| Live trace dump | `--dump-trace` | Stderr text. |
| Summary stats | (default; `--verbose-summary` for per-task / per-CPU dists) | Stdout text. |
| Preemption trace | `--record-preemptions PATH` | Text file grouped by worker; replayable via `scxsim replay`. |
| VM wprof | `vm-run --wprof` | Perfetto trace in CWD. |
| VM bpftrace | `vm-run --bpf-trace` | `bpf_trace.log` in CWD. |
| LLDB attach point | `--wait-debugger` | Writes lldb breakpoint script next to `.so`; prints attach command. |
