# Introduction

**scxsim** is a deterministic, time-accelerated simulator for
[sched_ext][scx] BPF schedulers. It runs the *real* BPF scheduler `.so`
files against [rt-app][rtapp]-style JSON workloads, in seconds, without
booting a kernel.

This guide explains how to install scxsim, write workloads, drive
simulations, capture traces, and reproduce bugs.

> **Status — scaffold.** Most chapters in this guide are stubs. They
> exist so the table of contents and the build pipeline are wired up;
> content is being filled in incrementally. See the
> [Contributing](./contributing.md) chapter for the page-by-page plan.

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
