# Summary

[Introduction](./introduction.md)
[Overview](./overview.md)

# Getting Started

- [Getting Started](./getting-started.md)
  - [Installation](./getting-started/installation.md)
  - [Quick Start](./getting-started/quick-start.md)
  - [Your First Simulation](./getting-started/first-simulation.md)

# Concepts

- [Concepts](./concepts.md)
  - [What scxsim Simulates](./concepts/what-scxsim-simulates.md)
  - [Twin Design Principles](./concepts/twin-design-principles.md)
  - [rt-app Workloads](./concepts/workloads.md)
  - [Schedulers](./concepts/schedulers.md)
  - [Determinism and Seeds](./concepts/determinism.md)
  - [Cgroup Bandwidth](./concepts/cgroup-bw.md)

# Running Simulations

- [Running Simulations](./running-simulations.md)
  - [The `run` Subcommand](./running-simulations/run.md)
  - [Scheduler Config Sidecar (TOML)](./running-simulations/scheduler-config.md)
  - [Trace Output](./running-simulations/trace-output.md)
  - [Replaying Preemption Traces](./running-simulations/replay.md)
  - [VM Runs (`vm-run`)](./running-simulations/vm-run.md)

# Recipes

- [Recipes](./recipes.md)
  - [Reproducing a Stall Bug](./recipes/repro-stall.md)
  - [Comparing Two Schedulers](./recipes/compare-schedulers.md)
  - [Verifying Determinism](./recipes/verify-determinism.md)
  - [Debugging with LLDB](./recipes/lldb.md)

# Reference

- [CLI Reference](./reference/cli.md)
- [Output Formats](./reference/output-formats.md)
- [Exit Codes and Stderr Markers](./reference/exit-codes.md)
- [Example Workloads](./reference/example-workloads.md)

# Architecture

- [Architecture](./architecture.md)
  - [Engine](./architecture/engine.md)
  - [Safe vs Unsafe Layers](./architecture/safe-unsafe.md)
  - [Cgroup Modeling](./architecture/cgroup.md)
  - [Trace Pipeline](./architecture/tracing.md)

---

[Contributing](./contributing.md)
[Glossary](./glossary.md)
