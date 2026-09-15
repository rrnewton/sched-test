---
title: 'scxsim: model userspace scheduler control loops (scx_layered CPU reallocation)'
status: closed
priority: 2
issue_type: task
created_at: 2026-08-12T15:27:39.388546458+00:00
updated_at: 2026-08-12T20:32:28.507524377+00:00
closed_at: 2026-08-12T20:32:28.507524266+00:00
---

# Description

scx_layered support (tg layered-support-implement) shipped at Tier 2 with a
STATIC CPU allocation, because scxsim has no model for a userspace control
loop.

In production, scx_layered's Rust userspace (alloc.rs ~2500 lines +
layer_core_growth.rs ~1300 lines) runs a periodic loop that re-computes each
layer's CPU set from live utilisation, writes layer->cpus / nr_cpus /
nr_llc_cpus / node[].nr_cpus, sets layer->refresh_cpus, and triggers the BPF
side via BPF_PROG_RUN on refresh_layer_cpumasks, followed by one
refresh_node_ctx per node.

scxsim computes that allocation ONCE, before ops.init, and holds it fixed.
Consequence: every layer growth/shrink path is dark, including
GROWTH_ALGO_* (only the published value is exercised, never the algorithm),
layer_llc_drain_enable/disable transitions driven by CPUs appearing or
disappearing, and the empty-layer bookkeeping refresh_node_ctx maintains.

This is the same class of gap mitosis has with its userspace cell-control
path (apply_cell_config is never invoked), so there is precedent for
shipping without it. But a general "periodic userspace agent" substrate
would unblock both at once.

Sketch: a Scenario-level hook that runs a Rust closure at a configured
period during the simulation, with access to the loaded DynamicScheduler,
so a test can play userspace. The closure would be the ONLY place allowed
to write scheduler config, keeping the No-Stub boundary intact — it plays
userspace, it does not make scheduling decisions.

See scx-sim/ai_docs/LAYERED_SUPPORT.md ("Documented divergences" #1).
