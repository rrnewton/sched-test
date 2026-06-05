# What scxsim Simulates

scxsim is a discrete-event simulator that loads a real sched_ext
scheduler — built from the *same* C source that the kernel build
compiles to BPF bytecode, but here compiled with vanilla clang to a
**native** `.so` shared library — and drives it against an
rt-app-style workload, in virtual time, on a virtual CPU topology.
The scheduler source is unchanged from what would run on a real
kernel; only the compilation target differs. Everything around the
scheduler is modeled.

This page enumerates what the model covers, what it deliberately stubs
out, and where the boundaries live.

## In scope (modeled)

| Aspect | Where it lives | Notes |
|---|---|---|
| **CPU topology** | `-c/--cpus`, `--smt` | Flat array of CPUs; SMT lays them out in pairs. No NUMA or per-socket modeling beyond what the scheduler itself queries. |
| **Per-CPU run state** | `safe/engine.rs`, `safe/cpu.rs` | Idle / running task / which task is current, all per CPU. |
| **Context switches** | `safe/engine.rs` | Modeled as instantaneous in virtual time but charged with `--no-overhead`-gated noise. |
| **struct_ops callbacks** | `unsafe_impl/struct_ops.rs` | The real scheduler `.so` is dlopen'd; each `ops.*` callback is invoked through an FFI trampoline. |
| **Dispatch queues (DSQs)** | `safe/dsq.rs` | FIFO and vtime DSQs, both per-CPU local and global. |
| **Cgroup hierarchy** | `scx_cgroup_tree` crate, `safe/cgroup.rs` | Path-based tree with per-cgroup `cpu.max` (quota/period). |
| **Cgroup bandwidth** | `safe/cgroup.rs`, `unsafe_impl/cgroup_ffi.rs` | Quota consumption, throttle, replenishment via an accounting timer. See [Concepts → Cgroup Bandwidth](./cgroup-bw.md). |
| **Time advancement** | `safe/engine.rs` | Virtual time only; the simulator never sleeps. A 1-second simulation finishes in well under a second of wallclock. |
| **Scheduling tick** | `unsafe_impl/kfuncs.rs` (`tick`) | Fires per CPU at ~4 ms (default); `--no-noise` makes it strictly periodic. |
| **Tick jitter** | `--no-noise` to disable | PRNG-driven small offsets on tick timestamps. |
| **Context-switch overhead** | `--no-overhead` to disable | Per-switch noise on top of the structop RBC cost. |
| **PMU-modeled scheduler overhead** | `--rbc-ns N`, `--no-rbc` | Retired Conditional Branches inside callbacks, multiplied by `--rbc-ns` (default 10 ns), charged to dispatch latency. |

## Out of scope (not modeled)

| Aspect | Why it's omitted |
|---|---|
| **Page faults, memory pressure** | scxsim is a *scheduling* simulator. Faults would require a memory model and a page-cache model; both out of scope. |
| **Disk and network I/O** | Same: no I/O subsystem. Workloads are CPU and sleep only. |
| **Hardware interrupts** | Only the scheduling tick and the cgroup-accounting timer are simulated as interrupt-like events. |
| **CPU frequency / voltage** | No DVFS modeling. Tasks compute at a fixed virtual rate. |
| **CFS / fair-class scheduler** | scxsim simulates *sched_ext*, not the rest of Linux's scheduler classes. |
| **Real syscalls** | rt-app's `lock`/`unlock`/`signal`/`mem`/`iorun` and similar actions are accepted by the parser but skipped with a one-line warning. |
| **Kernel preemption inside scheduler code** | Mid-callback preemption is opt-in via `--preemptive` (uses PMU or e9patch); the default is run-to-yield. |

## Where the boundary actually lives

The boundary between "real" and "modeled" runs through two layers in
the scxsim crate:

- **`safe/`** — pure Rust. This is the simulator: the event loop, the
  workload-to-task translator, the DSQ representation, the cgroup
  tree, the trace emitter. Everything here is deterministic given a
  seed and is what tests reason about.
- **`unsafe_impl/`** — FFI bridge. This is where calls cross from
  simulator-Rust into the native machine code that clang produced
  from the scheduler's C source. The trampolines, the kfunc
  emulation shims, the dispatch worker pool, and (when enabled) the
  e9patch / PMU preemption injection live here.

If a behaviour you observe in a trace is wrong, the question
"`safe/` bug, `unsafe_impl/` bug, or scheduler bug?" determines who
fixes it. See [Architecture → Safe vs Unsafe Layers](../architecture/safe-unsafe.md).

## Implication for bugs

Because the scheduler C source is the same source the kernel build
uses, bugs in the scheduler reproduce inside scxsim *because they
are literally the same source code* — only the compilation target
differs (native `.so` here vs BPF bytecode in the kernel). The H6 /
Bug-1 canonical reproducer
([`tests/fixtures/h6/bug1_canonical.{json,toml}`][bug1]) is a
production cgroup-bw stall that triggers under scxsim against the
same `libscx_lavd.so` that runs on the kernel.

The converse: behaviours that depend on un-modeled subsystems (page
fault timing, real network jitter, real interrupt storms) will not
reproduce. Such cases need [`vm-run`](../running-simulations/vm-run.md)
or bare-metal.

[bug1]: https://github.com/facebookexperimental/sched-test/tree/simulator.v6/scx-sim/crates/scx_simulator/tests/fixtures/h6

See also [Concepts → Twin Design Principles](./twin-design-principles.md)
for the policy that determines which knobs are realistic by default
and which are opt-in stress-test exaggerations.
