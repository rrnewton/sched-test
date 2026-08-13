---
title: 'ktstr<->sim divergence: step teardown in sched_dynamic_add'
status: open
priority: 2
issue_type: task
created_at: 2026-08-13T15:19:30.264182961+00:00
updated_at: 2026-08-13T15:19:30.264182961+00:00
---

# Description

Cross-backend re-confirmation at integration 7811372 found sched_dynamic_add is the one scenario whose two backends disagree materially. PRE-EXISTING, not caused by the 2026-08-12 engine work: the same numbers appear at 382e069, the commit before PRs #115/#117/#119/#121 landed.

THE SCENARIO. Two steps, each HoldSpec::frac(0.5): step 0 declares cg_0, step 1 declares cg_1.

WHAT EACH BACKEND DOES, per-cgroup CPU time over a 12s run on 2 CPUs:
  cg_0   VM 5.980s   sim 11.996s   ratio 0.498
  cg_1   VM 5.962s   sim  5.989s   ratio 0.995

cg_1 agrees to 0.5%. cg_0 is off by 2x, and in the direction that says the simulator KEEPS cg_0 running through step 1 while the VM stops it at the phase boundary.

WHY THIS IS TEARDOWN SEMANTICS AND NOT AN ACCOUNTING WINDOW, which was my first hypothesis and is wrong: total CPU across the whole run is VM 11.942s vs sim 17.986s. If cg_0 ran the full 12s and cg_1 the last 6s, the total would be ~18s — which is exactly what the simulator reports. The VM did two thirds of that work, so cg_0 really did stop. The VM sidecar corroborates it structurally: phases[0].per_cgroup contains only cg_0, phases[1].per_cgroup only cg_1.

WHY NOTHING CAUGHT IT. The replay test asserts that each scenario EXECUTES and that declared cpusets are honoured. Nothing compares per-cgroup CPU time across the two backends, so a 2x disagreement on one cgroup passes both suites. This is the same shape as the cpuset gap (sim-4qlh5): the lowering reports 'all declared fields carried' and is correct about the fields, while a behaviour downstream of the IR diverges.

WHICH SIDE IS RIGHT IS NOT ESTABLISHED. The kernel-side behaviour is whatever ktstr's execute_steps does with a prior step's cgroups, and I did not read that path. Do not assume the VM is the oracle here without checking; it is possible the intended semantics are cumulative and the VM is the one dropping cg_0.

THE FIX IS NOT OBVIOUSLY IN THE ENGINE. It may be in the lowering (whether SourceStep setup is cumulative or replacing), which is the ktstr-owned half. Worth settling before anyone adds a cross-backend CPU-time oracle, because that oracle would go red on this immediately.

Evidence: VM sidecars at /ktstr-samefs/target/ktstr/6.14.11-f5d0fce/ (ktstr f5d0fce, kernel 6.14.11, 5/5 PASS); sim via cargo nextest run -p ktstr-scenario-replay --no-capture.
