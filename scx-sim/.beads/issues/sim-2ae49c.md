---
title: Audit and mitigate all sources of nondeterminism in BPF scheduler code
status: closed
priority: 0
issue_type: feature
created_at: 2026-02-24T15:31:36.969441703+00:00
updated_at: 2026-02-24T19:09:49.868685016+00:00
closed_at: 2026-02-24T19:09:49.868684926+00:00
---

# Description

Track all potential sources of nondeterminism in BPF sched_ext schedulers that
the simulator must intercept or control for deterministic replay.

Each source should be checked off as we either (a) confirm it is already
mitigated, (b) implement mitigation, or (c) rule it out as not relevant.

## Already Controlled
- [x] `bpf_get_prandom_u32()` — FIXED: was hardcoded to 0 by overrides.h macro;
      now routed to deterministic Rust PRNG via sim_wrapper.h redirect (6b06a99)

## Controlled by Serialized Execution (confirmed)
- [x] `bpf_get_smp_processor_id()` — intercepted in kfuncs.rs:1171, returns sim CPU
- [x] `scx_bpf_task_cpu()` — intercepted in kfuncs.rs:1232, returns modeled CPU
- [x] Idle CPU state queries (`scx_bpf_get_idle_cpumask`, `scx_bpf_get_idle_smtmask`,
      `scx_bpf_test_and_clear_cpu_idle`, `scx_bpf_pick_idle_cpu`,
      `scx_bpf_select_cpu_dfl`, `scx_bpf_select_cpu_and`) — all intercepted in
      kfuncs.rs; deterministic under serialized execution
- [x] `scx_bpf_dsq_nr_queued()` — intercepted in kfuncs.rs:1132
- [x] `__sync_val_compare_and_swap` (CAS operations) — no races under serialization
      (LAVD uses heavily for logical clock, idle tracking, preemption coordination)
- [x] `scx_bpf_dsq_move_to_local`, `scx_bpf_dsq_move` — intercepted in kfuncs.rs
- [x] `scx_bpf_cpu_curr()` — intercepted in kfuncs.rs:1358
- [x] `scx_bpf_task_running()` — intercepted in kfuncs.rs:1651

## Time Sources (CRITICAL)
- [x] `scx_bpf_now()` — returns per-CPU logical clock (kfuncs.rs:1160)
- [x] `bpf_ktime_get_ns()` — returns per-CPU logical clock (kfuncs.rs:1290)
- [x] `bpf_timer_start()` / `bpf_timer_init()` — LAVD wired via sim_timer_start
      → TimerFired event (kfuncs.rs:1706); generic stubs no-op (acceptable for
      currently simulated schedulers)
- [x] `p->se.sum_exec_runtime` — modeled via sum_exec_base + elapsed
      (engine.rs:437, task.rs:181)

## Implicit Randomness (NOT via bpf_get_prandom_u32)
- [x] `bpf_cpumask_any_distribute()` — deterministic first-fit in sim_bpf_stubs.c:160
      (not realistic distribution, but deterministic)
- [x] `bpf_cpumask_any_and_distribute()` — deterministic first-fit in sim_bpf_stubs.c:170

## Hardware / Environment State
- [x] `scx_bpf_cpuperf_cur()` — intercepted in kfuncs.rs:1685
- [x] `scx_bpf_cpuperf_cap()` — intercepted in kfuncs.rs:1696
- [x] `cpufreq_cpu_data` — neutralized: LAVD wrapper stubs bpf_probe_read_kernel
      to always return -EFAULT with zeroed output. Code path is dead.
- [x] `hw_pressure` — neutralized: LAVD wrapper stubs bpf_per_cpu_ptr to return
      NULL. Thermal pressure code path is dead.
- [x] PMU counters — layered only, not currently simulated. N/A.
- [x] `CONFIG_HZ` — layered/flash only, not currently simulated. Tickless uses
      `tick_freq` which is set by wrapper (line 55). N/A for current schedulers.

## IRQ / Execution Context
- [x] `bpf_in_hardirq()` — intercepted in kfuncs.rs:1188
- [x] `bpf_in_serving_softirq()` — intercepted in kfuncs.rs:1203
- [x] `bpf_in_nmi()` — intercepted in kfuncs.rs:1197 (always returns 0)

## Hash Map Iteration Order
- [x] `BPF_MAP_TYPE_HASH` iteration order — N/A for current simulators.
      None of the 5 simulated schedulers (simple, lavd, cosmos, tickless,
      mitosis) use hash maps. They use arrays, per-CPU arrays, and task storage.
      Re-evaluate if rusty/layered/chaos are added.
- [x] rusty `task_masks` pointer-as-key — N/A, rusty is not simulated.

## Kernel State Reads
- [x] `scx_bpf_get_online_cpumask()` — LAVD only; simulator controls CPU topology
- [x] `bpf_get_current_task_btf()` — intercepted in kfuncs.rs:1325 (with waker override)
- [x] `bpf_cgroup_from_id()` / `scx_bpf_task_cgroup()` — intercepted in kfuncs.rs:1576/1592
- [x] `scx_bpf_dsq_peek()` — intercepted in kfuncs.rs:1386; deterministic under serialization

## Notes
- Schedulers analyzed: LAVD, rusty, bpfland, layered, flash, chaos, p2dq
- LAVD is by far the most complex and has the most nondeterminism surface area
- ALL items resolved as of 2026-02-24
- One bug found and fixed: bpf_get_prandom_u32 was hardcoded to 0 (6b06a99)
- All other sources either already intercepted, neutralized by stubs, or N/A
  for currently simulated schedulers
