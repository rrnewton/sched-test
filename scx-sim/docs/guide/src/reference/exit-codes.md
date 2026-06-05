# Exit Codes and Stderr Markers

scxsim has stable exit codes and stable single-line stderr markers
of the form `scxsim: ExitKind::<Variant> ...`, intended to be matched
by automation (CI, bisect scripts, the `bug_finding/` harness).

| Exit code | `ExitKind` | Meaning |
|----------:|------------|---------|
| 0  | `Normal`                        | Workload completed without a stall, scheduler-side error, or resource exhaustion. |
| 1  | (generic)                       | Pre-simulation failure (bad CLI flag, workload parse error, scheduler-load failure, ...). |
| 42 | `ErrorStall`                    | Watchdog tripped before workload completion (`--watchdog-timeout`). |
| 43 | `ErrorBpf`                      | The loaded scheduler `.so` returned an error from a struct_ops callback. (The variant name preserves the kernel-side terminology where the scheduler runs as BPF bytecode; under scxsim the same code path is native, but the error semantics are identical.) |
| 44 | `ErrorDispatchLoopExhausted`    | Dispatch loop exceeded its budget (typically a busy-loop in the scheduler). |
| 45 | `ErrorCgroupExhausted`          | Cgroup-registry exhausted (`CBW_NR_CGRP_MAX`). |

Every non-zero ExitKind also emits a single stable stderr line:

```text
scxsim: ExitKind::ErrorStall ...details...
```

automation should pattern-match this line rather than parsing surrounding
log noise.

This table mirrors
[`scx-sim/README.md`](https://github.com/facebookexperimental/sched-test/blob/simulator.v6/scx-sim/README.md);
a future improvement is to generate it directly from the `ExitKind`
enum in `safe/types.rs` so the two cannot drift.
