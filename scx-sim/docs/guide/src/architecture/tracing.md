# Trace Pipeline

Every simulator event — struct_ops entries and exits, dispatches,
ticks, watchdog checks, cgroup replenishes — flows through a single
emitter in [`safe/trace.rs`][trace-rs] and fans out to multiple
downstream sinks. This is the machinery behind every artefact
described in [Trace Output Sinks](../running-simulations/trace-output.md).

[trace-rs]: https://github.com/rrnewton/sched-test/blob/simulator.v6/scx-sim/crates/scx_simulator/src/safe/trace.rs

## The event type

`TraceKind` (in `safe/types.rs`) is a tagged enum covering every event
the engine cares to publish. Selected variants:

| Variant | Emitted when |
|---|---|
| `StructOpEntry { name, cpu, args }` | Just before invoking a scheduler callback. |
| `StructOpExit { name, cpu, ret, rbc }` | Just after the callback returns. |
| `Dispatch { task, from_cpu, to_cpu, dsq }` | A task moves between DSQs. |
| `Wake { task, cpu }` | A `Wake` event is processed. |
| `Sleep { task }` | A `Sleep` event is processed. |
| `Tick { cpu }` | A `Tick` event fires. |
| `CgroupBwReplenish { cgroup }` | Quota replenished. |
| `CgroupThrottle { cgroup, runtime_ns }` | A cgroup transitions into `is_throttled = true`. |
| `WatchdogCheck { max_wait_ns }` | The watchdog inspects runqueues. |
| `Exit { kind }` | The simulation terminates. |

Every variant carries the simulated timestamp (`ts_ns`) and the CPU it
originated on, plus variant-specific payload.

## The emitter

`safe/trace.rs` exposes a single `Tracer` struct that the engine holds
behind a `&mut`. `Tracer::emit(TraceKind)` does three things:

1. Stamps `ts_ns` from the engine's virtual clock.
2. Writes a brief representation into the in-memory ring (used by the
   default end-of-run summary).
3. Fans out to every enabled sink.

There is exactly one emit path. There is no separate "fast path" or
"slow path." This is deliberate: it keeps the trace stream
deterministic and means every sink sees identical event order.

## The three downstream sinks

| Sink | File | Enabled by |
|---|---|---|
| Perfetto JSON | `safe/perfetto.rs` (JSON writer half) | `--perfetto FILE` (default JSON format) |
| Perfetto protobuf | `safe/perfetto.rs` (protobuf writer half) | `--perfetto FILE --trace-format perfetto` |
| structops JSONL | `safe/bpf_trace.rs` | `--structops-jsonl FILE` |

Plus three lower-volume sinks:

- **Brief summary** — the default stderr at end of run. Always on.
- **Verbose summary** — `--verbose-summary` adds per-task / per-CPU /
  per-cgroup distributions to the same stderr block.
- **Dump trace** — `--dump-trace` writes the in-memory ring to stderr
  as text. Mostly useful for short runs and debugging.

Plus the preemption-trace sink, which is structurally similar but
records only the events needed for deterministic record/replay (see
[Replaying Preemption Traces](../running-simulations/replay.md)).

## Why one emitter, not per-sink

Earlier prototypes had per-sink callbacks (the scheduler would call
"trace this" once per interested sink). That design accumulated
divergence bugs: the Perfetto sink would record an event the JSONL
sink missed, then `--determinism-check` would pass on one stream and
fail on the other. The unified emitter eliminates that whole class.

The corollary: if you add a new sink, add it to `Tracer::emit`'s
fan-out, not to ad-hoc call sites scattered through the engine. The
unified emitter is the *only* place that knows the engine has produced
an event.

## Performance notes

- Sinks are gated by an enable flag, so when (e.g.) `--structops-jsonl`
  is not requested, the JSONL writer is a no-op.
- The structops JSONL writer is the densest sink: ~10× more events than
  Perfetto JSON for the same run. It is also the most diff-friendly,
  which is why the determinism recipes use it.
- The Perfetto protobuf sink is significantly more compact than the
  JSON sink for long runs and is preferred for the `vm-run --wprof`
  side-by-side comparison workflow.

## Sources

- [`safe/trace.rs`][trace-rs] — the central emitter.
- [`safe/perfetto.rs`](https://github.com/rrnewton/sched-test/blob/simulator.v6/scx-sim/crates/scx_simulator/src/safe/perfetto.rs) —
  Perfetto JSON and protobuf writers.
- [`safe/bpf_trace.rs`](https://github.com/rrnewton/sched-test/blob/simulator.v6/scx-sim/crates/scx_simulator/src/safe/bpf_trace.rs) —
  structops JSONL writer.
- `ai_docs/record_replay_architecture.md` — design of the preemption
  trace format and replay loop.
