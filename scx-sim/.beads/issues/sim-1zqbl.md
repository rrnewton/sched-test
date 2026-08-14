---
title: IoSyncWrite runs on the simulator but the io/compute split is a fabricated constant
status: open
priority: 2
issue_type: task
created_at: 2026-08-14T04:53:21.008674354+00:00
updated_at: 2026-08-14T04:53:21.008674354+00:00
---

# Description

cover_cgroup_io_compute_imbalance now runs end to end on the simulator. Its DECLARED INVARIANT does not survive the trip, and this is a scenario-selection warning, not a bug in the lowering.

IoSyncWrite lowers to Run(500us) Sleep(500us) -- a 50 percent duty cycle in which BOTH durations are DEFAULT_SLICE. The simulator has no IO model; it models a task that alternates run and sleep.

What survives: the SHAPE. A blocking task competing with 4 saturating spinners is a real scheduling question, and the measured result is scheduling-relevant -- cg_0 got 730,961,186 ns (1.52 percent) against cg_1's 47,229,221,503 ns (98.48 percent) on 4 CPUs over 12s. A 50 percent duty-cycle task could in principle have taken 12.5 percent of machine capacity; it got 1.52, because it queues behind spinners on every wake.

What does NOT survive: the QUANTITY. How much CPU cg_0 deserves is fixed by DEFAULT_SLICE, not by anything about writes. A real IoSyncWrite duty cycle depends on device latency and could be anywhere.

CONSEQUENCE: do NOT add this scenario to cross_backend.rs CASES with a per-cgroup share bound. Comparing the simulator's fabricated 50 percent duty cycle against a VM's real disk duty cycle and reporting agreement would be manufacturing fidelity. If a share bound is ever wanted here, the fabricated quantum has to be replaced by a declared one first (ktstr would need to state the compute-between-writes, cf. the UnspecifiedWorkQuantum work).

The disclosure half is fixed on integration by PR #127 -- the report now records UnspecifiedWorkQuantum alongside IoMechanism, so this scenario is correctly visible to the port gate as not-ported-in-the-meaningful-sense.
