---
title: 'scxsim: the layered control loop under-allocates — a third of the machine is owned by no layer'
status: open
priority: 1
issue_type: bug
created_at: 2026-09-11T14:34:49.760144446+00:00
updated_at: 2026-09-11T14:34:49.760144446+00:00
---

# Description

OBSERVED, MECHANISM NOT CHARACTERISED. Filed so the measurement is not lost; whoever picks this up should treat the cause as open.

With scx_layered's userspace control loop enabled, both Grouped layers end up owning far fewer CPUs than their own measured utilization warrants, and roughly a third of the machine is owned by NO layer.

MEASURED 2026-09-11 on the branch feat/scxsim-arbitrary-virtual-topologies (integration@426d85e + PR #144). 64 CPUs, 8 per LLC, 128 CPU-bound hogs, two Grouped layers both with util_range (0.8, 0.9) — 'hot' matching 32 of the tasks by comm prefix, 'rest' catching the other 96. Control loop at production's 100ms interval. Owned counts read from layered_probe_layer_has_cpu, CPU time from layered_probe_layer_usage (OWNED + OPEN):

  nodes=1 ms=200   owned=[14, 25]  unowned=25  cputime_ns=[2383886085, 10309443480]
  nodes=1 ms=1000  owned=[2, 39]   unowned=23  cputime_ns=[11556169811, 52345592458]
  nodes=2 ms=200   owned=[16, 25]  unowned=23  cputime_ns=[3483885106, 9199758014]
  nodes=2 ms=1000  owned=[3, 38]   unowned=23  cputime_ns=[25114429956, 79260339306]

Take the 1-node / 1000ms row. Layer 0 accumulated 11.56 CPU-seconds over 1s of simulated time, i.e. a steady-state utilization of ~11.6 CPUs. At util_high = 0.9 its target should be about 13 CPUs. IT OWNS 2. Layer 1 accumulated 52.3 CPU-seconds (~52 CPUs), target about 58, and owns 39. Meanwhile 23 CPUs belong to neither layer.

Layer 0 also SHRINKS over time — 14 CPUs at 200ms, 2 at 1000ms — while its utilization is rising, which is the opposite of what the sizing loop is supposed to do.

NOT A NUMA DEFECT. The 1-node and 2-node rows are the same shape, so this is unrelated to mb sim-dox34 (fixed in PR #144) and is not caused by the node partition.

WHY IT HAS NEVER SHOWN UP: work still reaches every CPU. All four rows above report ran_on=64/64, because a Grouped layer's tasks spill onto unowned idle CPUs through pick_idle_cpu's open path (LSTAT_OPEN_IDLE is non-zero throughout). So every coverage assertion in the tree passes while the allocation itself is wrong. Any test that asserts on WHICH CPUs a layer owns, on layer isolation, or on anything downstream of nr_llc_cpus / layer->nr_cpus would be reading a bogus allocation.

WHERE TO START: LayeredControl::step in crates/scx_simulator/src/safe/layered_control.rs — specifically raw_target(), the shrink dampening (current - (current - target).div_ceil(2), which halves toward the target each cycle and could ratchet down if target is computed too low), and calc_raw_demands, which the layered wrapper header already names as 'the one reimplemented piece' of the otherwise-upstream allocator. unified_alloc itself is upstream's real code compiled in, so suspect the demand computation feeding it before suspecting the allocator.

REPRO: build the four-row table above with a throwaway #[ignore] test that runs DynamicScheduler::layered_for_topology + layered_enable_control_loop(100_000_000) and prints layer_has_cpu counts alongside layer_usage. Took about ten minutes.
