# Glossary

Single-line definitions of recurring terms.

| Term | Definition |
|---|---|
| **BPF** | Berkeley Packet Filter — the kernel-internal sandboxed bytecode that sched_ext schedulers compile to. |
| **bin_cache** | Per-revision cache of built scheduler `.so` files used for bisecting regressions; see `scripts/`. |
| **bpftrace** | Tracing tool used to capture struct_ops + helper-call sequences from a real kernel for diff against scxsim's `--structops-jsonl` output. |
| **cgroup-v2** | Linux process-group hierarchy controlling resource allocation; scxsim models the `cpu.max` (quota / period) controller. |
| **DSQ** | Dispatch Queue — sched_ext's per-CPU and global runnable-task queues. |
| **e9patch** | Binary rewriter used for `--preempt-mode e9patch` to inject preemption sites; see `scripts/install_e9patch.sh`. |
| **ExitKind** | Stable enum at simulator exit (`Normal`, `ErrorStall`, ...); see [Exit Codes](./reference/exit-codes.md). |
| **kfunc** | "Kernel function" called from BPF code; scxsim provides an emulation shim in `unsafe_impl/kfuncs.rs`. |
| **LAVD** | Latency-Aware Virtual Deadline — the primary production target scheduler under scxsim study. |
| **Perfetto** | Trace viewer at <https://ui.perfetto.dev/>; loads both JSON and protobuf traces emitted by scxsim. |
| **rt-app** | Real-time application emulator; scxsim consumes a JSON dialect of its workload format. |
| **sched_ext** | Linux kernel framework for loading custom schedulers as BPF programs; see <https://github.com/sched-ext/scx>. |
| **struct_ops** | sched_ext's mechanism for a BPF program to override per-callback scheduler entry points. |
| **TraceKind** | Variant of an internal simulator event (TaskScheduled, CgroupBwReplenish, ...); the unit of trace output. |
| **virtme-ng** (`vng`) | Lightweight VM wrapper used by `scxsim vm-run` to drive a real kernel. |
| **watchdog** | Stall detector; `--watchdog-timeout` (default 30s) trips `ExitKind::ErrorStall`. |
| **wprof** | Whole-system profiler whose Perfetto-protobuf output scxsim's protobuf trace is designed to mirror for side-by-side comparison. |
