---
title: Sim arena reset wipes cpumasks allocated during scheduler *_setup() (tickless + cosmos lose their primary CPU)
status: closed
priority: 1
issue_type: bug
created_at: 2026-08-12T14:50:17.195371176+00:00
updated_at: 2026-08-12T22:30:34.875411536+00:00
closed_at: 2026-08-12T22:30:34.875411425+00:00
---

# Description

Schedulers that populate a BPF cpumask during their load-time `*_setup()` hook silently lose it on the first simulation run.

Sequence (tickless):
1. `DynamicScheduler::tickless(n)` dlopens the .so and calls `tickless_setup(n)`, which calls `enable_primary_cpu({cpu_id=0})`. That does `bpf_cpumask_create()` (allocates from the sim bump arena) then `bpf_cpumask_set_cpu(0, mask)`.
2. `Simulator::run()` calls `ffi::reset_task_state()` -> `sim_sdt_reset()` -> `sim_arena_reset()`, which memsets the used arena to zero (csrc/sim_arena.h:98-103).
3. `primary_cpumask` still points at that now-zeroed object, so `is_primary_cpu()` returns false for every CPU from then on.

Proven with lldb on the instrumented tickless test binary, breaking on sim_sdt_reset:
  before: ((unsigned long*)&sim_arena_buf)[0] = 1
  after:  ((unsigned long*)&sim_arena_buf)[0] = 0
and stepping into the call inside tickless_dispatch:
  bpf_cpumask_test_cpu(cpu=0, cpumask=sim_arena_buf) -> false

Impact on tickless (fidelity, not just coverage): `tickless_init` skips `init_timer()` for every CPU, and `tickless_dispatch` never enters its `if (is_primary_cpu(cpu))` branch, so `dispatch_all_cpus` / `dispatch_cpu` / `is_pcpu_task` never execute. ops.dispatch degenerates to a bare `scx_bpf_dsq_move_to_local(SHARED_DSQ)`. The scheduler's real dispatch policy is not running -- a Principle 1 / No-Stub-Rule divergence.

COSMOS IS ALSO AFFECTED: `cosmos_setup()` calls `enable_primary_cpu()` at load time (schedulers/cosmos/wrapper.c:456) and cosmos_main_patched.c:1005 uses the same arena-allocated mask. Same lldb check on the cosmos test binary shows the same 1 -> 0 wipe. Cosmos still measures 54/54 functions because its call sites read `(!primary || bpf_cpumask_test_cpu(...))` -- a NULL check, not an emptiness check -- so it degrades silently and function coverage cannot see it.

Latent memory-safety hazard beyond the zeroing: sim_arena_reset also rewinds the bump pointer to 0, so a subsequent allocation can hand the same bytes to a different object while primary_cpumask still points there.

Found while investigating why tickless measures 20/28 functions (tg task tickless-timer-coverage-gap). Six of its eight uncovered functions are this bug.

# Design

Three candidate fixes, each with fidelity implications -- the arena reset exists for run-to-run determinism, so this needs an owner decision rather than a unilateral pick:

A1. Re-arm primary CPUs after the reset (re-run `*_setup()`, or add an explicit post-reset hook). Requires handling `init_cpumask()`'s early return on the stale non-NULL `primary_cpumask`, which otherwise refuses to re-create the mask.
A2. Null out scheduler kptrs as part of sim_arena_reset, so `init_cpumask()` re-creates naturally on next use.
A3. Stop arena-allocating scheduler cpumasks, or exclude them from the reset.

Whichever is chosen should be applied uniformly -- audit every `*_setup()` for load-time allocation that the reset can invalidate.

# Acceptance Criteria

- is_primary_cpu() returns true for the configured primary CPU during a simulation run, for both tickless and cosmos.
- A regression test asserts the primary mask survives Simulator::run() (e.g. two consecutive runs on the same loaded scheduler).
- An audit note covering every scheduler's *_setup() for the same load-time-allocation hazard.
