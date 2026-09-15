---
title: Zero-duration phases make the engine run ~20x slower than the VM it simulates
status: open
priority: 2
issue_type: task
created_at: 2026-08-14T06:56:22.966919298+00:00
updated_at: 2026-08-14T06:56:22.966919298+00:00
---

# Description

A Scenario whose tasks carry only zero-length phases drives the engine into a degenerate reschedule loop.

Measured on ktstr cross_affinity_churn_runs_in_vm (22cde3bb), whose two work types both declare spin_iters: 0, so ctx.iters() yields 0 ns and every phase lowers to Run(0) / Sleep(0):

  4 tasks, 4 CPUs, 11.9 s logical
  35,058,057 time slices; 45M structops on cpu0 alone
  WALL CLOCK 370.5 / 371.8 / 373.7 / 376.8 s  (mean 373.2 s)

For comparison the IO scenario -- same 12 s declared, same 4 CPUs, real phase
durations -- runs in 86-92 ms. That is ~4000x. And it is 20x SLOWER THAN THE VM
IT IS SIMULATING (VM mean 18.384 s), which inverts the usual reason for having
a simulator.

The engine is not wrong to schedule a zero-length phase; it is that nothing
bounds the resulting churn. Options:

1. Refuse spin_iters = 0 at the lowering. ctx.iters(.., 0) producing a 0 ns
   phase is arguably malformed input the IR should reject rather than lower --
   compare ProducedInvalidIr for other invariant breaks. This is the cleanest
   and it would have surfaced the real problem (the scenario declares no work)
   at the gate instead of after six minutes.
2. Clamp a zero phase to a minimum quantum. Rejected as written: it invents a
   duration, which is the defect UnspecifiedWorkQuantum exists to name.
3. Leave the engine alone and treat it as a scenario-authoring bug.

Note the interaction: because a zero-work scenario still produces a plausible
per-cgroup CPU number (19,537,778,632 ns, 40.7% of capacity, from pure
reschedule churn), it does NOT look degenerate in the results -- only in the
clock and the slice count.
