# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**schtest** is a scheduler testing and benchmarking framework written in Rust. It tests Linux scheduler functionality, particularly for sched_ext (extensible scheduler) implementations.

This project consists of different pieces related to Linux scheduler microbenchmarking:
 - ./src: Rust targeted scheduler test cases / microbenchmarks
 - ./scripts: Python scripts providing more microbenchmarks and utilities

We will be using beads (`bd quickstart`) for local issue tracking.

## Running schedulers and recording scheduler

Schedulers are mostly stored in the sched_ext/scx repository. They are binaries such as `scx_lavd`, which, when run, modify the global scheduler on the Linux system. Whenever you run a benchmark, you should make a note of which scheduler is running on the system:

* `cat /sys/kernel/sched_ext/state`: to see there's any sched_ext scheduler loaded, and then
* `cat /sys/kernel/sched_ext/root/ops` to see which scheduler is being used, if any.

Ideally, check on this both before and after you run a benchmark (and as a sanity check make sure they are the same).

## Build Commands

```bash
# Build the project
cargo build --release

# Run tests (requires root)
sudo ./target/release/schtest

# Run benchmarks (requires root)
sudo ./target/release/schtest --benchmarks

# List available tests
sudo ./target/release/schtest --list

# Run with a custom scheduler
sudo ./target/release/schtest -- /path/to/scheduler [scheduler args...]

# Run Rust unit tests
cargo test
```

## Project Structure

- `src/main.rs` - CLI entry point using clap for argument parsing
- `src/lib.rs` - Library root exposing `cases`, `util`, and `workloads` modules
- `src/cases/` - Test cases and benchmarks (basic, fairness, latency, topology, irq, cgroup_tree)
- `src/workloads/` - Workload implementations (spinner, semaphore, process, cgroup_tree, benchmark)
- `src/util/` - Utilities for cgroups, scheduling, statistics, system info, etc.

## Key Patterns

- Tests are registered using the `inventory` crate with `cases::Test` and `cases::Benchmark` types
- Tests run single-threaded and require root privileges for cgroup/scheduler operations
- The framework can optionally spawn and manage an external scheduler binary
