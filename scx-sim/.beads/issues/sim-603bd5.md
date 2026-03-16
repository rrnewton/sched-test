---
title: Replay SIGABRT crashes during preemptive determinism testing
status: open
priority: 2
issue_type: bug
created_at: 2026-03-14T17:14:47.731384547+00:00
updated_at: 2026-03-15T20:06:32.771099009+00:00
---

# Description

30-minute stress test found 45 SIGABRT crashes during preemptive replay (record+replay determinism path). Affects mitosis (most: dsq_contention, lavd_dsq_stress, simple_wake, two_runners on 2-8 CPUs) and lavd (simple_wake, 4 CPUs). The REPLAY MISMATCH shows ops context divergence (trace=none, replay=select_cpu or running). Related to but distinct from sim-c3fd09 (SIGSTKFLT crashes). Repro: scxsim run workloads/simple_wake.json -s lavd -c 4 --seed 754214214 --watchdog-timeout 2s --end-time 4s --preemptive --record-preemptions /tmp/repro.preempt && scxsim replay /tmp/repro.preempt

# Notes

## Deep Dive: Record/Replay Architecture Analysis (2026-03-15)

### What a PreemptionRecord contains
rbc_count (raw PMU timeslice), instruction_pointer (RIP), structop_rbc (cumulative per-worker RBC), cpu_id, worker_id, sequence, structop_local/global, ops_context, kfunc_name/count, insn_bytes (5 bytes for .so mismatch detection). Trace file stores rip_offset for ASLR resilience.

### Record with PMU, replay with breakpoints: ALREADY IMPLEMENTED
Two-tier retry in replay_dispatch_with_retry: PMU signal approach (3 retries) then breakpoint-only fallback (2 retries). Both modes need PMU COUNTER (read_rbc_count) but bp-only skips PMU SIGNAL. Breakpoint-only is deterministic (no skid).

### E9patch replay: architecturally feasible, not yet implemented
E9patch trampoline gives deterministic branch counting with no PMU hardware. Would work in VMs/containers. Needs E9PatchReplayBackend that sets e9 counter to (target.structop_rbc - current_rbc) and verifies IP at yield.

### On this bug
REPLAY MISMATCH ops context divergence is about PRNG sync between recording/replay, not the preemption mechanism itself. If a preemption point is missed or misplaced, a different worker runs, PRNG diverges, and all subsequent scheduling decisions differ.
