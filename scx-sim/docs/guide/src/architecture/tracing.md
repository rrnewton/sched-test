# Trace Pipeline

> **Status — stub.** Will describe the trace pipeline: `TraceKind`
> events, the central `safe/trace.rs` emitter, and the three
> downstream sinks (Perfetto JSON, Perfetto protobuf, structops
> JSONL).

Key sources:

- [`safe/trace.rs`](https://github.com/facebookexperimental/sched-test/blob/simulator.v6/scx-sim/crates/scx_simulator/src/safe/trace.rs)
- [`safe/perfetto.rs`](https://github.com/facebookexperimental/sched-test/blob/simulator.v6/scx-sim/crates/scx_simulator/src/safe/perfetto.rs)
- [`safe/bpf_trace.rs`](https://github.com/facebookexperimental/sched-test/blob/simulator.v6/scx-sim/crates/scx_simulator/src/safe/bpf_trace.rs)
- `ai_docs/record_replay_architecture.md`
