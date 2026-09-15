---
title: IoSyncWrite runs on the simulator but the io/compute split is a fabricated constant
status: open
priority: 2
issue_type: task
created_at: 2026-08-14T04:53:21.008674354+00:00
updated_at: 2026-08-14T05:37:14.635324968+00:00
---

# Description

cover_cgroup_io_compute_imbalance runs end to end on BOTH backends now. Its declared invariant does not survive the trip, and the VM measurement changes WHY.

MEASURED, both backends, 4 CPUs, 12s, scheduler simple (sim) / scx-ktstr (VM):

                              VM                    SIM              ratio
  cg_0 IoSyncWrite   9,476,392,819 (19.72%)    730,961,186 (1.52%)   12.96x
  cg_1 SpinWait x4  38,575,309,627 (80.28%) 47,229,221,503 (98.48%)   0.82x
  second VM run: cg_0 9,392,820,188 (19.48%) -- 12.85x. Reproducible.

THE CLEANEST STATEMENT OF THE DEFECT is off-CPU time, which the VM measures
directly and which is exactly what the lowering invents:

  cg_0 avg_off_cpu_pct   MEASURED 21.39% / 22.22%    MODELLED 50%

IoSyncWrite lowers to Run(500us) Sleep(500us) -- DEFAULT_SLICE on both phases,
a 50/50 duty cycle. The real worker is off-CPU only ~21-22%: buffered writes
land in page cache and mostly do NOT block. So the invented quantum is not
merely undeclared, it is WRONG, and wrong in the opposite direction to the
intuition that 'IO means blocking'.

CORRECTION TO THE ORIGINAL RATIONALE ON THIS ISSUE. It said a share bound here
would be 'manufactured agreement'. That was a prediction made before the VM
ran, and it is WRONG. A 0.05 bound would not pass spuriously -- it FAILS, hard:
92.2% relative difference on cg_0, 17.8% on cg_1.

The conclusion is unchanged but the reason is different, and stronger:

  DO NOT add this scenario to cross_backend.rs CASES.

Not because the check would be vacuous, but because it would be RED and the red
would be MISATTRIBUTED. cross_backend is a scheduler-fidelity check. This
divergence is a WORKLOAD-MODELLING defect in the lowering. Wiring it in would
stand up a permanent red pointing future readers at the scheduler when the
fault is the invented duty cycle.

DO NOT 'FIX' THIS BY HARD-CODING 21.4%. Substituting a second invented constant
sourced from one run, on one host, with one page-cache state is the same defect
with better provenance. The real fix is for the source to declare the quantum
(ktstr stating compute-between-writes), after which the share comparison
becomes meaningful and this scenario can be reconsidered for CASES.

WHAT THIS SCENARIO CAN VERIFY ACROSS BACKENDS TODAY: nothing scheduling-
meaningful. Machine saturation holds on both (VM 48.05e9 / 48.21e9 vs 4x12s =
48e9 capacity; sim 47.96e9) and both run the declared 12s, but for 4 spinners
on 4 CPUs that is near-tautological and is already covered sim-side by the
duration assertion from #126. It counts toward conversion and pipeline coverage
-- it is the first non-SpinWait work type carried end to end -- but NOT toward
the cross-backend-verification checklist item.

Disclosure half fixed on integration by PR #127 (e492a01): the report now
records UnspecifiedWorkQuantum alongside IoMechanism, so this scenario is
correctly visible to the port gate.
