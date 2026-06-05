# Introduction

**scxsim** is a deterministic, time-accelerated simulator for
[sched_ext][scx] schedulers. It takes the *real* scheduler C source —
the same files that, in production, are compiled to BPF bytecode and
loaded into the kernel — and instead compiles them with vanilla
clang/llvm targeting **native code**. The result is a set of ordinary
shared libraries (`libscx_lavd.so`, `libscx_cosmos.so`, …) that scxsim
`dlopen`s and drives against [rt-app][rtapp]-style JSON workloads, in
seconds, without booting a kernel.

> **The `.so` files are NOT BPF bytecode.** sched_ext schedulers are
> written in C with BPF-style annotations. The *kernel* build target
> compiles that C to BPF bytecode (`clang --target=bpf`); the
> **scxsim** build target compiles the *exact same C source* with
> vanilla clang to a native-code shared library. Same scheduler
> source, different compilation target. This is the central trick
> that makes everything below possible.

## Why scxsim — two headline advantages

The native-`.so` approach unlocks two things that are **impossible**
when a scheduler is running as BPF bytecode inside the kernel:

### 1. Debuggability — pause it, attach a debugger

Because the scheduler runs as ordinary userspace machine code inside
a single-process Rust binary, you can use **gdb or lldb** on it
exactly as you would on any other native program:

- Set breakpoints in scheduler functions (`lavd_select_cpu`,
  `cgroup_throttled`, …) and **single-step** through scheduler
  decisions.
- Inspect variables, walk the cgroup tree, dump DSQs from inside a
  breakpoint.
- Pause the simulator mid-decision and **think** without the kernel
  watchdog killing you.

Kernel BPF has none of this. There is no `gdb` for BPF bytecode
running inside the kernel — you get printk-style telemetry and that
is it. scxsim turns scheduler debugging into a normal native-code
debugging workflow. See [Recipes → Debugging with
LLDB](./recipes/lldb.md).

### 2. Deterministic reproducibility — replay the exact same scenario

scxsim's engine drives virtual time, fake CPUs, and a controlled
PRNG. Given **same workload + same seed + same scheduler revision**,
you get **bit-identical traces** every run. That means:

- A bug that reproduced **once** can be reproduced **on demand**, by
  you, by a teammate, by CI.
- You can bisect a regression across scheduler revisions with the
  bin_cache + `--scheduler-file` workflow and trust that a divergent
  trace genuinely points at the offending commit.
- The `--determinism-check` mode runs the simulation twice and
  checkpoint-diffs them, so non-determinism becomes a hard CI gate.

A live kernel — VM or bare metal — cannot offer this. Interrupt
timing, IPI delivery, cache effects, scheduler-tick jitter, and other
non-deterministic factors mean that even the most carefully scripted
kernel reproducer is intermittent. The cpu-bw-stall-bug (H6 / Bug-1)
was a 1-in-many-runs flake on a real kernel; under scxsim it
reproduces every single time. See [Concepts →
Determinism](./concepts/determinism.md).

---

This guide explains how to install scxsim, write workloads, drive
simulations, capture traces, and reproduce bugs.

## Who should read this

- **Scheduler developers** writing or debugging sched_ext schedulers
  (`lavd`, `cosmos`, `mitosis`, `tickless`, `simple`, ...) who want a
  fast inner loop without VMs.
- **Workload designers** crafting rt-app JSON to probe specific
  scheduler behaviour (priority handling, cgroup bandwidth, dispatch
  contention, ...).
- **Bug reproducers** narrowing failures from production captures down
  to a small, replayable, deterministic minimal example.

## How this guide is organized

The guide follows the [Diátaxis][diataxis] structure:

| Section | Style | Purpose |
|---|---|---|
| **Getting Started** | Tutorial | Stand up scxsim and run something end-to-end. |
| **Concepts** | Explanation | The vocabulary: workloads, schedulers, determinism, cgroups. |
| **Running Simulations** | How-to | Drive each subcommand of the CLI. |
| **Recipes** | How-to | Solve common task-shaped problems. |
| **Reference** | Reference | CLI flags, output formats, exit codes, fixtures. |
| **Architecture** | Explanation | How the simulator is put together. |

[scx]: https://github.com/sched-ext/scx
[rtapp]: https://github.com/scheduler-tools/rt-app
[diataxis]: https://diataxis.fr/
