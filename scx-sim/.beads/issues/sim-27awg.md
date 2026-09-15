---
title: 'scxsim: node-scoped idle kfuncs ignore their node argument'
status: open
priority: 2
issue_type: bug
created_at: 2026-09-10T20:42:02.779795474+00:00
updated_at: 2026-09-10T20:42:02.779795474+00:00
---

# Description

scxtest/scx_test_cpumask.c implements the node-scoped idle kfuncs with the node parameter marked unused:

  s32 scx_bpf_pick_idle_cpu_node(const struct cpumask *cpus_allowed, int node __attribute__((unused)), u64 flags)
  const struct cpumask *scx_bpf_get_idle_cpumask_node(int node __attribute__((unused)))
  const struct cpumask *scx_bpf_get_idle_smtmask_node(int node __attribute__((unused)))

They return the GLOBAL idle mask, and scx_bpf_pick_idle_cpu_node returns the lowest idle CPU in cpus_allowed regardless of which node was asked for. A scheduler that asks 'give me an idle CPU on node 3' gets a node-0 CPU and no indication anything was approximated.

This is a Twin-Design-Principle-1 divergence in the SUBSTRATE (these are kernel-side kfuncs, so they are legitimately scxsim's to implement -- this is not a No-Stub violation of a scheduler). But it is silent and carries no DANGER TODO marker, which the Kernel Fidelity rule requires for a known deviation.

scx_layered does NOT call these (it uses nodec->cpumask and lookup_layer_node_cpumask), so this is NOT the cause of sim-dox34. Filed separately so the two are not conflated: fixing this will not fix sim-dox34, and fixing sim-dox34 will not fix this.

Minimum acceptable interim step: a DANGER TODO(sim-<this>) at each of the three call sites naming the deviation, so the next agent to read them knows the node argument is discarded.

# Acceptance Criteria

Either the three kfuncs honour their node argument against a per-node idle mask, or each carries a DANGER TODO naming this issue.
