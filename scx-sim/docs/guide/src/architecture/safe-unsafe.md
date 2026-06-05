# Safe vs Unsafe Layers

scxsim is organized around a deliberate split: a large pure-Rust
[`safe/`][safe-tree] module that owns deterministic simulator state,
and a small [`unsafe_impl/`][unsafe-tree] module that owns the **FFI**
(Foreign Function Interface) trampolines into the `dlopen`'d
scheduler `.so` and the **kfunc** (kernel-function emulation) shim
that stands in for the kernel. (Both terms in the
[Glossary](../glossary.md).)

[safe-tree]: https://github.com/rrnewton/sched-test/tree/simulator.v6/scx-sim/crates/scx_simulator/src/safe
[unsafe-tree]: https://github.com/rrnewton/sched-test/tree/simulator.v6/scx-sim/crates/scx_simulator/src/unsafe_impl

The rule of thumb:

- If you are writing a test, a recipe, or asking "what does the
  simulator do?" — you are in `safe/`.
- If you are debugging an FFI calling-convention mismatch, a stale
  `.so` ABI, or a `#[no_mangle]` symbol resolution issue — you are in
  `unsafe_impl/`.

## What `safe/` owns

| File | Responsibility |
|---|---|
| `safe/engine.rs` | The event loop, virtual-time clock, per-CPU run loops. See [Architecture → Engine](./engine.md). |
| `safe/scenario.rs` | Translates a workload JSON into an initial event heap. |
| `safe/types.rs` | The `TraceKind`, `ExitKind`, `EventKind` enums — the engine's public state machine. |
| `safe/dsq.rs` | Per-CPU FIFO + vtime dispatch queues (the simulator-side model of sched_ext DSQs). |
| `safe/cgroup.rs` | The cgroup tree and cgroup-bandwidth accounting state. See [Architecture → Cgroup Modeling](./cgroup.md). |
| `safe/trace.rs` | Central trace event emitter; fans out to all sinks. See [Architecture → Trace Pipeline](./tracing.md). |
| `safe/perfetto.rs` | Perfetto JSON and protobuf writers. |
| `safe/bpf_trace.rs` | bpftrace-style structops JSONL writer. |
| `safe/preempt.rs` | Preemption-point scheduler, record/replay machinery. |

All of `safe/` is `#![forbid(unsafe_code)]` and is the substrate that
makes the [Determinism](../concepts/determinism.md) contract
provable: there are no raw pointers, no in-band time, no PRNG sources
that the engine doesn't own.

## What `unsafe_impl/` owns

| File | Responsibility |
|---|---|
| `unsafe_impl/struct_ops.rs` | The FFI trampoline that invokes scheduler callbacks (`ops.select_cpu`, `ops.enqueue`, …) loaded from the `.so`. |
| `unsafe_impl/kfunc_shim.rs` | Emulates the BPF kfuncs the scheduler calls into (`scx_bpf_dsq_insert`, `scx_bpf_dispatch_from_dsq`, etc.). |
| `unsafe_impl/dispatch_workers.rs` | Native worker pool for `--native-concurrent` mode. |
| `unsafe_impl/preempt_pmu.rs` | PMU-based preemption-point injection (RBC counter overflow → signal). |
| `unsafe_impl/preempt_e9patch.rs` | e9patch-based software RBC injection. |
| `unsafe_impl/cgroup_ffi.rs` | Bridge from `safe/cgroup.rs` to BPF map accesses the scheduler issues. |
| `unsafe_impl/loader.rs` | The dlopen + symbol-resolution path; resolves `ops.<name>` from `bpf_struct_ops_<name>`. |

This module is unavoidably `unsafe` because the scheduler `.so` is
native machine code (clang-compiled from the scheduler's C source
with a native target rather than the BPF target) that the simulator
must call directly through raw function pointers. Every `unsafe`
block is audited against `ai_docs/safety_audit.md`.

## Where the boundary lives

A typical engine tick crosses the safe/unsafe boundary once per
struct_ops call:

```text
safe/engine.rs        : pops Wake event from heap, advances vtime
        │
        ▼
unsafe_impl/struct_ops.rs : pre-call RBC read, marshal args
        │
        ▼
libscx_lavd.so        : lavd_select_cpu(p, prev_cpu, wake_flags)
        │            ─── may call kfuncs ───→  unsafe_impl/kfunc_shim.rs
        │                                          │
        ▼                                          ▼
unsafe_impl/struct_ops.rs : post-call RBC read     safe/dsq.rs etc.
        │
        ▼
safe/engine.rs        : record TraceKind::StructOpExit, re-arm timers
```

The trampoline emits a `TraceKind::StructOp{Entry, Exit}` event into
`safe/trace.rs` on every crossing, which is what makes the structops
JSONL stream the densest determinism-diffable artefact (see
[Trace Output Sinks](../running-simulations/trace-output.md)).

## Why the split matters

- **Determinism reasoning is local.** If you suspect a non-determinism
  bug, you can audit `safe/` exhaustively without touching the FFI.
  Most reproducible bugs end up being `safe/`-side state machine bugs,
  not FFI bugs.
- **`unsafe_impl/` is intentionally small.** The total `unsafe` line
  count is a measurable property; growing it requires explicit
  justification in `ai_docs/safety_audit.md`.
- **Tests live in `safe/`.** The integration tests under `tests/`
  exercise `safe/` directly with a stub or real `.so`; per-FFI tests
  live alongside the trampoline they exercise.

## Sources

- [`safe/`][safe-tree] — the deterministic state machine.
- [`unsafe_impl/`][unsafe-tree] — the FFI bridge.
- `ai_docs/safety_audit.md` — per-`unsafe`-block rationale.
- `ai_docs/concurrency_protocol.md` — interleaving rules across the
  boundary, including the `--native-concurrent` worker model.
