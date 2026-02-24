---
title: Audit and mitigate all sources of nondeterminism in BPF scheduler code
status: open
priority: 0
issue_type: feature
created_at: 2026-02-24T15:31:36.969441703+00:00
updated_at: 2026-02-24T15:31:36.969441703+00:00
---

# Description

Track all potential sources of nondeterminism in BPF sched_ext schedulers that
the simulator must intercept or control for deterministic replay.

Each source should be checked off as we either (a) confirm it is already
mitigated, (b) implement mitigation, or (c) rule it out as not relevant.

## Already Controlled
- [ ] `bpf_get_prandom_u32()` — intercepted via known kfunc interface

## Controlled by Serialized Execution (confirm)
- [ ] `bpf_get_smp_processor_id()` — deterministic if CPU assignment is controlled
- [ ] `scx_bpf_task_cpu()` — deterministic if CPU assignment is controlled
- [ ] Idle CPU state queries (`scx_bpf_get_idle_cpumask`, `scx_bpf_get_idle_smtmask`,
      `scx_bpf_test_and_clear_cpu_idle`, `scx_bpf_pick_idle_cpu`,
      `scx_bpf_select_cpu_dfl`, `scx_bpf_select_cpu_and`) — deterministic if
      simulator models idle state
- [ ] `scx_bpf_dsq_nr_queued()` — deterministic under serialized execution
- [ ] `__sync_val_compare_and_swap` (CAS operations) — no races under serialization
      (LAVD uses heavily for logical clock, idle tracking, preemption coordination)
- [ ] `scx_bpf_dsq_move_to_local`, `scx_bpf_dsq_move` — race-free under serialization
- [ ] `scx_bpf_cpu_curr()` — deterministic if CPU state is modeled
- [ ] `scx_bpf_task_running()` — deterministic if task state is modeled

## Time Sources (CRITICAL)
- [ ] `scx_bpf_now()` — used pervasively in LAVD (vtime, stats, preemption) and rusty
- [ ] `bpf_ktime_get_ns()` — used in LAVD (DSQ consume latency) and chaos scheduler
- [ ] `bpf_timer_start()` / `bpf_timer_init()` — LAVD periodic stats, chaos delay
      timer, layered antistall timer. Timers fire on wall-clock, inherently nondeterministic.
- [ ] `p->se.sum_exec_runtime` — kernel-maintained task runtime counter, read by LAVD
      via `task_exec_time()`

## Implicit Randomness (NOT via bpf_get_prandom_u32)
- [ ] `bpf_cpumask_any_distribute()` — uses internal rotating counter, NOT prandom.
      Used by LAVD for CPU selection (idle.bpf.c, preempt.bpf.c).
- [ ] `bpf_cpumask_any_and_distribute()` — same rotating-counter mechanism.
      Used by LAVD in `find_sticky_cpu_at_cpdom()`.

## Hardware / Environment State
- [ ] `scx_bpf_cpuperf_cur()` — reads current CPU frequency/perf level (DVFS).
      Used by LAVD.
- [ ] `scx_bpf_cpuperf_cap()` — CPU performance capacity. Used by LAVD.
- [ ] `cpufreq_cpu_data` — raw kernel cpufreq policy read via BPF_CORE_READ (LAVD power.bpf.c)
- [ ] `hw_pressure` — thermal pressure kernel variable (LAVD power.bpf.c)
- [ ] PMU counters — hardware perf counters for memory bandwidth (layered via lib/pmu.h)
- [ ] `CONFIG_HZ` — kernel tick rate, read by layered and flash

## IRQ / Execution Context
- [ ] `bpf_in_hardirq()` — LAVD uses for wakeup priority boosting (main.bpf.c:595-603)
- [ ] `bpf_in_serving_softirq()` — same LAVD wakeup path
- [ ] `bpf_in_nmi()` — same category

## Hash Map Iteration Order
- [ ] `BPF_MAP_TYPE_HASH` iteration order — nondeterministic by design.
      Used in rusty (task_masks), layered (layer_match_dbg, gpu_tgid, gpu_tid,
      cgroup_match_bitmap). Only matters if scheduler iterates over these maps
      and the iteration order affects scheduling decisions.
- [ ] rusty `task_masks` uses `struct task_struct *` as hash key — pointer values
      differ across runs, producing different bucket distribution.

## Kernel State Reads
- [ ] `scx_bpf_get_online_cpumask()` — varies with CPU hotplug (LAVD)
- [ ] `bpf_get_current_task_btf()` — returns waker task; deterministic if wakeup
      source is modeled (LAVD, rusty, chaos)
- [ ] `bpf_cgroup_from_id()` / `scx_bpf_task_cgroup()` — cgroup state (LAVD)
- [ ] `scx_bpf_dsq_peek()` — race-dependent under concurrency, deterministic
      under serialization (LAVD)

## Notes
- Schedulers analyzed: LAVD, rusty, bpfland, layered, flash, chaos, p2dq
- LAVD is by far the most complex and has the most nondeterminism surface area
- Many "nondeterministic" kfuncs become deterministic under the simulator's
  serialized execution model — but this needs to be confirmed for each one
