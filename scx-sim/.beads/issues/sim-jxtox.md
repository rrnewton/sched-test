---
title: 'scxsim lavd: scx_clock_task() and scx_clock_pelt() are both the simulator''s per-CPU clock — model rq->clock_task and rq->clock_pelt, and a tickless idle'
status: open
priority: 2
issue_type: task
labels:
- no-stub
- kernel-fidelity
- lavd
depends_on:
  sim-439319: related
created_at: 2026-09-25T05:18:55.258008537+00:00
updated_at: 2026-09-25T05:21:26.470056675+00:00
---

# Description

DEFECT

`schedulers/lavd/wrapper.c` maps both `scx_clock_task(cpu)` and `scx_clock_pelt(cpu)` to `sim_scx_clock_task(cpu)`. That function (`crates/scx_simulator/src/unsafe_impl/kfuncs.rs`) returns that CPU's own simulated clock, `local_clock`, minus its cumulative IRQ time. So lavd's wall-clock runtime and its capacity- and frequency-invariant runtime are the same number, and the production inlines that read the runqueue never run.

PRODUCTION (scx/scheds/include/scx/common.bpf.h, kernel/sched/pelt.h)

- `scx_clock_task(cpu)` returns `rq->clock_task` through `get_current_rq(cpu)`, which is `bpf_per_cpu_ptr(&runqueues, cpu)`.
  - The value is the clock as of that rq's last `update_rq_clock()`, excluding IRQ and steal time.
  - For a remote CPU that is idle under NO_HZ_IDLE, no tick updates it, so it can be stale by the whole idle period.
- `scx_clock_pelt(cpu)` returns `rq->clock_pelt - rq->lost_idle_time`.
  - While the CPU runs, `update_rq_clock_pelt()` advances `clock_pelt` by the `clock_task` delta scaled by the CPU's capacity and current frequency.
  - While it is idle, each update sets `clock_pelt` back to `clock_task` (`_update_idle_rq_clock_pelt()`). When a fully busy rq goes idle, `update_idle_rq_clock_pelt()` first adds the gap to `lost_idle_time`, which `scx_clock_pelt` subtracts.
  - At capacity 1024 and maximum frequency the scaling is 1, so `scx_clock_pelt` equals `scx_clock_task`. They differ only on a lower-capacity CPU or below maximum frequency.
- lavd reads both clocks together:
  - `update_stat_for_running` reads them only when it runs on the task's own CPU. Its comment explains that a remote `rq->clock_task` read may be stale and would charge the idle gap as phantom runtime.
  - `account_task_runtime` turns them into `task_time_wall` (from `clock_task`) and `task_time_invr` (from `clock_pelt`).
  - `collect_sys_stat` derives each CPU's performance factor from `delta_pelt` and `delta_task`. Its block comment assumes NO_HZ_IDLE: `delta_task` includes completed idle periods but "NOT the current in-progress one". When `CONFIG_NO_HZ_IDLE` is set and the CPU is idle, it adds `cur_idle_pelt` (`cur_idle_wall * scx_bpf_cpuperf_cap(cpu) >> LAVD_SHIFT`) to `now_pelt` to make up for the stale read, then clamps `delta_pelt` to `compute_wall`.
  - `cpu_ctx_init_online` and `init_per_cpu_ctx` seed the per-CPU baselines.

SIMULATOR (sched-test 24d864c6, scx 413031d44)

- Both clocks are `local_clock - irq_cumulative_ns` for the named CPU.
  - `local_clock` advances whenever that CPU processes an event: the engine calls `advance_cpu_clock` (`src/unsafe_impl/kfuncs.rs`) for every per-CPU event.
  - Every CPU is ticked, idle or not: `handle_tick` in `src/safe/engine.rs` says "ticks are unconditional per-CPU timers".
  - So a read of a remote idle CPU lags by at most about one tick. That is a kernel without NO_HZ_IDLE, which is what the wrapper declares: `CONFIG_NO_HZ_IDLE` defaults to false ("simulator doesn't model NO_HZ_IDLE").
- Every CPU is capacity 1024 (`scx_bpf_cpuperf_cap` returns 1024). `scx_bpf_cpuperf_set` records the level lavd asks for, and `scx_bpf_cpuperf_cur` reads it back, but nothing else reads it: simulated execution does not slow down.
- `-DSIM_CONFIG_NO_HZ_IDLE=1` (scxsim-build `KernelConfig::no_hz_idle`) sets lavd's `CONFIG_NO_HZ_IDLE` global and nothing else. The engine still ticks idle CPUs.
- `get_current_rq` is never compiled. `crates/scx_simulator/csrc/sim_wrapper.h` stubs `bpf_per_cpu_ptr` to NULL, so it could not work anyway (verdict K03).
- The simulator has no steal time.

CONSEQUENCE

- In the simulator's own model (capacity 1024, no frequency effect, periodic tick) the macros return what the production inlines would: `scx_clock_pelt` equals `scx_clock_task`, and `lost_idle_time` would stay 0. The defect is what that model cannot represent.
  - `task_time_invr` always equals `task_time_wall`. Heterogeneous CPU capacities, and the frequency changes lavd itself requests through `scx_bpf_cpuperf_set`, where production's two clocks differ, cannot be simulated.
  - The NO_HZ_IDLE staleness that `update_stat_for_running` avoids and `collect_sys_stat` corrects for cannot occur, so neither defence is tested against the input it exists for.
- With `-DSIM_CONFIG_NO_HZ_IDLE=1`, lavd's correction runs on a clock that never went stale. The idle CPU kept ticking, so `now_pelt` already includes the current idle period up to the last tick, and adding `cur_idle_pelt` counts that period a second time. The `delta_pelt > compute_wall` clamp then bounds the result, which hides the double count. The flag runs lavd's branch; it does not model a tickless kernel.
- Impact on lavd's decisions: UNVERIFIED.

FIX DIRECTION

- Expose the per-CPU clock the simulator already keeps as `rq->clock_task`, through a `runqueues` per-CPU ksym, so that `get_current_rq()` and the unmodified `scx_clock_*` inlines run. This needs a real `bpf_per_cpu_ptr`, which sim-439319 (`sd_llc_id`) needs too.
- Keep `rq->clock_pelt` and `rq->lost_idle_time` as the kernel does: advance by the delta scaled by capacity and the current performance level while running, sync to `clock_task` while idle, and accumulate `lost_idle_time` when a fully busy rq goes idle.
- When NO_HZ_IDLE is selected, stop ticking idle CPUs, so a remote idle CPU's clock stays at its last update, as it does in production.
- Then delete the lavd macros.

ACCEPTANCE

- The lavd wrapper no longer defines either macro, and the coverage re-measure shows `get_current_rq`, `scx_clock_task` and `scx_clock_pelt` as compiled header code.
- On a CPU with a capacity or performance level below 1024, `scx_clock_pelt` advances at that fraction of `scx_clock_task` while running. While idle it behaves as `_update_idle_rq_clock_pelt()` and `update_idle_rq_clock_pelt()` specify, including `lost_idle_time`.
- With NO_HZ_IDLE selected, a read of a remote idle CPU's `scx_clock_task` returns its value at idle entry, and a lavd test runs `collect_sys_stat`'s NO_HZ_IDLE branch against such a CPU.
- Until this is fixed, both macros carry a DANGER TODO naming this issue. sim-io5ng tracks that.

Found by the 2026-09-24 scx pin bump coverage re-measure (sched-test 24d864c6, scx 413031d44). See verdict rows S04 and S05 in coverage/scx_pin_bump_20260924/verdicts.tsv in the dev harness (rrnewton/dev-sched-test).
