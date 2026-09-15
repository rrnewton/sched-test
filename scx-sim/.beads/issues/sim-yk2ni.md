---
title: 'scxsim run: no CLI topology flag, so multi-node layer configs cannot be run from the command line'
status: open
priority: 2
issue_type: task
created_at: 2026-09-11T17:26:13.724178868+00:00
updated_at: 2026-09-11T17:26:13.724178868+00:00
---

# Description

#144 landed arbitrary virtual topologies in the RUST API only — MachineTopology plus DynamicScheduler::layered_for_topology. `scxsim run --help` lists --cpus and --smt and nothing else topological, so every CLI run is a flat 1-LLC / 1-node machine.

CONSEQUENCE, measured 2026-09-11 (tg exercise-layered-state-space): a real production layer config whose rules name NUMA node 1 is refused at load with 'the scheduler was given 1 node(s) (0-0)'. That refusal is CORRECT — upstream bails the same way — but it means a whole family of dual-socket production configs cannot be exercised from the CLI at all, even though the modelling they need exists and is tested.

This is a capability gap, not a defect. The pieces are all present:
  - MachineTopology::uniform(nr_cpus, cpus_per_llc, nr_nodes, threads_per_core) — safe/topology.rs
  - DynamicScheduler::layered_for_topology(&topo) — unsafe_impl/ffi.rs:1657
  - ScenarioBuilder::topology(topo) — the engine side

What is missing is CLI plumbing. The CLI builds its scheduler through the generic DynamicScheduler::load(path, prefix, nr_cpus) (bin/scxsim/main.rs::load_scheduler), which has no topology parameter, and builds its Scenario from the rt-app workload without calling .topology().

SUGGESTED SHAPE: --nodes N and --llcs-per-node M alongside the existing --cpus/--smt, feeding one MachineTopology into BOTH the engine and the scheduler (the point of #144's single-source-of-truth design). Refuse shapes MachineTopology::uniform refuses, rather than rounding them.

VERIFY: a config with a NumaNode(1) match loads and runs under 'scxsim run -s layered --cpus 16 --nodes 2 --layer-config <f>', and the layer report shows the NumaNode term being evaluated rather than the config being refused.
