---
title: 'scxsim: multi-NUMA-node topologies confine all execution to node 0'
status: open
priority: 1
issue_type: bug
created_at: 2026-09-10T20:41:51.862563613+00:00
updated_at: 2026-09-10T20:41:51.862563613+00:00
---

# Description

With nr_nodes > 1, scx_layered under scxsim runs work on node 0 ONLY. cpus_that_ran is exactly nr_cpus/nr_nodes, always the lowest CPU ids. Measured on integration@f418aa7 (2026-09-10) at 2, 4 and 8 nodes, at CPU counts 32..384, with a single catch-all layer AND with two Grouped layers, at every LLC size tried. It is a NUMA defect, not a large-machine defect: it appears the moment nr_nodes goes 1 -> 2 at 32 CPUs.

EVIDENCE (crates/scx_simulator/tests/layered_large_topology.rs):
- known_gap_multi_node_confines_all_work_to_node_zero pins the behaviour.
- diag_node_confinement_matrix isolates the cause to nr_nodes alone: 64 CPUs / 8 LLCs / 1 node -> ran_on=64/64; same shape with 2 nodes -> ran_on=32/64. Layer config and LLC size make no difference.
- diag_where_does_node1_work_go shows it is a PLACEMENT-side failure. With 1 node, tasks are inserted into all 8 per-LLC hi-fallback DSQs (0x40000000..0x40000007) and all 64 CPUs consume. With 2 nodes, ops.enqueue is only ever invoked from CPUs 0-31 and tasks only land in 0x40000000..0x40000003 (node 0's LLCs). Node-1 CPUs DO run ops.dispatch and DO call scx_bpf_dsq_move_to_local -- they are asking for work that was never placed where they can see it.
- The topology publication itself is correct: every_cpu_is_visible_to_the_scheduler_at_384 verifies the wrapper publishes the right LLC, node and SMT sibling for all 384 CPUs, and the layer BPF kptr cpumasks are correct across the full range (layer 0 = 0-191, layer 1 = 192-383).

COST: the multi-node arms churn. At 384 CPUs / 200ms simulated, 1 node produces 101k trace events and 2 nodes produce 3.39M -- a 33x blow-up with half the machine idle. So this also dominates the wall-clock cost of any multi-node simulation.

LIKELY AREA: scxsim has no NUMA model at all. layered_with_topology's own doc comment says the node count is 'a harness-supplied grouping over LLCs' with 'no engine counterpart' and no inter-node distance cost. A separate, independently-verified instance of the same gap: the node-scoped idle kfuncs in scxtest/scx_test_cpumask.c all take 'int node __attribute__((unused))' -- scx_bpf_pick_idle_cpu_node, scx_bpf_get_idle_cpumask_node, scx_bpf_get_idle_smtmask_node answer node-blind, and scx_bpf_pick_idle_cpu_node returns the lowest matching CPU globally. scx_layered does NOT call those (it uses nodec->cpumask and lookup_layer_node_cpumask), so they are not this mechanism, but any scheduler that does call them gets a wrong answer with no marker and no DANGER TODO.

# Acceptance Criteria

Work reaches every NUMA node on a multi-node topology; known_gap_multi_node_confines_all_work_to_node_zero goes red and is inverted to assert the positive property.
