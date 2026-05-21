# Running Simulations

How to drive each of scxsim's subcommands.

- [The `run` Subcommand](./running-simulations/run.md) — the main
  entry point: rt-app JSON in, trace + summary out.
- [Scheduler Config Sidecar (TOML)](./running-simulations/scheduler-config.md) —
  set per-symbol BPF globals (`bool`, `u8`, `u32`, `u64`) without
  rebuilding the scheduler.
- [Trace Output](./running-simulations/trace-output.md) — Perfetto JSON
  vs protobuf, structops JSONL, stderr dump, summaries.
- [Replaying Preemption Traces](./running-simulations/replay.md) — the
  `replay` subcommand and the `--record-preemptions PATH` workflow.
- [VM Runs (`vm-run`)](./running-simulations/vm-run.md) — drive a real
  kernel under virtme-ng for ground-truth comparison.
