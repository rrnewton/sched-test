# Glossary

Single-line definitions of recurring terms.

| Term | Definition |
|---|---|
| **BPF** | Berkeley Packet Filter — the kernel-internal sandboxed bytecode format that sched_ext schedulers' C source is compiled to **when targeting the kernel**. Under scxsim the *same* C source is instead compiled with vanilla clang to **native** code (`.so`), so the scheduler runs as ordinary userspace machine code. The shorthand "BPF scheduler" in this guide refers to a scheduler *written for* the sched_ext BPF interface, not to BPF bytecode per se. |
| **bin_cache** | Per-revision cache of built scheduler `.so` files used for bisecting regressions; see `scripts/`. |
| **bpftrace** | Tracing tool used to capture struct_ops + helper-call sequences from a real kernel for diff against scxsim's `--structops-jsonl` output. |
| **CFS** | Completely Fair Scheduler — Linux's default process scheduler class. scxsim does **not** model CFS; it simulates only sched_ext. |
| **cgroup-v2** | Linux process-group hierarchy controlling resource allocation; scxsim models the `cpu.max` (quota / period) controller. |
| **CI** | Continuous Integration — automated test runs on each commit / pull request. |
| **CLI** | Command-Line Interface. |
| **DSQ** | Dispatch Queue — sched_ext's per-CPU and global runnable-task queues. |
| **DVFS** | Dynamic Voltage and Frequency Scaling — CPU power-management; **not** modeled by scxsim. |
| **e9patch** | Binary rewriter used for `--preempt-mode e9patch` to inject preemption sites; see `scripts/install_e9patch.sh`. |
| **ExitKind** | Stable enum at simulator exit (`Normal`, `ErrorStall`, ...); see [Exit Codes](./reference/exit-codes.md). |
| **FFI** | Foreign Function Interface — the calling convention used when Rust (the engine) invokes the scheduler's native-compiled C code through function pointers. |
| **FIFO** | First-In, First-Out queue ordering. |
| **kfunc** | "Kernel function" — a function the sched_ext scheduler's C source calls into that, in a real kernel, lives inside the kernel itself; scxsim provides an emulation shim in `unsafe_impl/kfuncs.rs`. |
| **LAVD** | Latency-Aware Virtual Deadline — the primary production target scheduler under scxsim study. |
| **NUMA** | Non-Uniform Memory Access — multi-socket memory-topology effects; **not** modeled by scxsim. |
| **Perfetto** | Trace viewer at <https://ui.perfetto.dev/>; loads both JSON and protobuf traces emitted by scxsim. |
| **PMU** | Performance Monitoring Unit — hardware performance counters; scxsim's PMU-overhead model uses Retired Branch Counts (RBC) sampled from the PMU to estimate scheduler-side CPU cost. |
| **PRNG** | Pseudo-Random Number Generator — scxsim seeds a single deterministic PRNG so identical seed → identical trace. |
| **RBC** | Retired Branch Count (sometimes "Retired Conditional Branches") — a hardware-PMU event counter that scxsim multiplies by `--rbc-ns` (default 10 ns) to estimate the simulated wallclock cost of each scheduler callback. |
| **rt-app** | Real-time application emulator; scxsim consumes a JSON dialect of its workload format. |
| **sched_ext** | Linux kernel framework for loading custom schedulers as BPF programs; see <https://github.com/sched-ext/scx>. scxsim mirrors the sched_ext interface but runs the scheduler as native code. |
| **struct_ops** | sched_ext's mechanism for a scheduler to override per-callback scheduler entry points by registering a struct of function pointers. |
| **TraceKind** | Variant of an internal simulator event (TaskScheduled, CgroupBwReplenish, ...); the unit of trace output. |
| **virtme-ng** (`vng`) | Lightweight Virtual Machine (VM) wrapper used by `scxsim vm-run` to drive a real kernel. |
| **VM** | Virtual Machine — used by `scxsim vm-run` to run the *kernel-side* BPF-compiled scheduler for ground-truth comparison against the simulator. |
| **watchdog** | Stall detector; `--watchdog-timeout` (default 30s) trips `ExitKind::ErrorStall`. |
| **wprof** | Whole-system profiler whose Perfetto-protobuf output scxsim's protobuf trace is designed to mirror for side-by-side comparison. |
