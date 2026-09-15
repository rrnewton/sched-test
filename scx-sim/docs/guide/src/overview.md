# Overview

scxsim sits between a real sched_ext scheduler and the kernel: it
loads the scheduler's compiled `.so` (the *same* C source the kernel
build uses, but compiled to **native** code instead of BPF
bytecode — see [Introduction](./introduction.md) for the full
pipeline distinction), fakes the kernel-side `struct sched_ext_ops`
callbacks, fakes time, fakes CPU topology, and runs an rt-app workload
against the result.

```text
            ┌─────────────────────────────────────────────────┐
            │  scxsim                                         │
            │                                                 │
   workload │  ┌──────────┐    ┌──────────┐    ┌──────────┐   │   trace
  ──────────┼─▶│ scenario │───▶│ engine   │───▶│ trace    │───┼─────────▶
   (JSON)   │  └──────────┘    │ (virtual │    │ pipeline │   │  (perfetto,
            │                  │  time +  │    └──────────┘   │   JSONL,
            │                  │  CPUs)   │                   │   stderr,
            │                  └────┬─────┘                   │   summary)
            │                       │                         │
            │                       │ struct_ops callbacks    │
            │                       ▼                         │
            │             ┌─────────────────┐                 │
            │             │ libscx_<sched>  │  ← scheduler C  │
            │             │ .so (lavd, ...) │    compiled to  │
            │             │                 │    NATIVE code  │
            │             └─────────────────┘                 │
            └─────────────────────────────────────────────────┘
```

## What you get

- **The same scheduler C source that runs on the kernel.** No
  re-implementation; no shims. The kernel build compiles that C to
  BPF bytecode; scxsim compiles the same C to a native shared
  library. Bugs in the scheduler reproduce here because it is
  literally the same source code. (See [Twin Design
  Principles](./concepts/twin-design-principles.md).)
- **Determinism.** Same workload + same seed + same scheduler revision
  ⇒ same trace, bit-for-bit. (See [Determinism](./concepts/determinism.md).)
- **Speed.** Hundreds of simulated milliseconds in well under a second
  for typical 4-CPU workloads.
- **Rich observability.** Per-event stderr trace, Perfetto JSON or
  protobuf for `ui.perfetto.dev`, a structops JSONL stream
  diff-comparable against bpftrace captures from a live kernel,
  per-task and per-CPU summaries, and replayable preemption traces.

## What it is *not*

- **Not a model.** scxsim does not implement the LAVD ranking
  function or the cosmos cgroup logic; it loads the real (native-
  compiled) scheduler `.so` and *invokes* it. Bugs in the scheduler
  reproduce here because it is the same C source — just compiled to
  native rather than to BPF bytecode.
- **Not a full kernel.** Page faults, networking, real I/O, and
  hardware interrupts are not modeled. scxsim targets CPU-scheduling
  behaviour and cgroup-bandwidth accounting. Workloads that are
  fundamentally I/O-driven won't be faithful.
- **Not a Linux Test Project replacement.** For ABI- or syscall-level
  testing, run in a VM (`vm-run` subcommand) or on bare metal.
