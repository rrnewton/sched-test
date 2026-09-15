# Architecture

How scxsim is put together internally.

- [Engine](./architecture/engine.md) — the virtual-time event loop and
  per-CPU run loops.
- [Safe vs Unsafe Layers](./architecture/safe-unsafe.md) — what lives
  in `safe/` (pure Rust modeling) vs `unsafe_impl/` (FFI bridge to
  the scheduler `.so`).
- [Cgroup Modeling](./architecture/cgroup.md) — the `scx_cgroup_tree`
  crate, the cgroup-bandwidth accounting timer, and replenishment.
- [Trace Pipeline](./architecture/tracing.md) — `TraceKind` events,
  the emitter, and the three downstream sinks (Perfetto JSON,
  Perfetto protobuf, structops JSONL).
