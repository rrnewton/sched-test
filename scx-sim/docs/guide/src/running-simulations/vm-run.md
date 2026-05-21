# VM Runs (`vm-run`)

> **Status — stub.** This page will document the `scxsim vm-run`
> subcommand, which drives a real kernel under [virtme-ng][vng] for
> ground-truth comparison against the simulator.

`scxsim vm-run [OPTIONS] <WORKLOAD>`:

| Flag | Purpose |
|---|---|
| `-s/--scheduler` | Scheduler to load inside the VM. |
| `-c/--cpus` | VM CPU count. |
| `--wprof` | Capture a Perfetto trace via wprof on an isolated VM CPU. |
| `--bpf-trace` | Capture a bpftrace ops+kfuncs trace via `trace_scx_ops.bt`. Writes `bpf_trace.log` in CWD. |
| `--scheduler-args=<RAW>` | Raw args forwarded to the in-VM scheduler. |
| `--pre-hook <PATH>` | Run a script before the workload starts. |
| `--post-hook <PATH>` | Run a script after the workload exits. |

Hooks see `SCXSIM_*` env vars including `SCXSIM_SCHED_PID`.

The simulator-vs-VM comparison is the canonical fidelity check for the
[Twin Design Principles](../concepts/twin-design-principles.md);
divergence between a `scxsim run` trace and a `scxsim vm-run` trace
of the same workload is the unit of "debt to file."

[vng]: https://github.com/arighi/virtme-ng
