---
title: 'cgroup_bw: replenish wakes LAVD-bailed tasks the lib BTQ drain also re-inserts (task queued twice)'
status: open
priority: 1
issue_type: bug
labels:
- cgroup-bw
- kernel-fidelity
created_at: 2026-09-24T21:10:15.426730870+00:00
updated_at: 2026-09-24T21:10:15.426730870+00:00
---

# Description

DEFECT: under LAVD with enable_cpu_bw=true, a task that LAVD puts aside on throttle is queued TWICE at the next replenish. The engine wakes it, and the cgroup_bw library's BTQ drain also re-inserts it. A task on two DSQ slots at once cannot happen in the kernel.

MECHANISM (code cited by symbol):
1. `lavd_enqueue` bails on a throttled cgroup. `scx_cgroup_bw_put_aside` puts the task in the library's BTQ. The wrapper macro then calls `scxsim_cgroup_bw_observe_put_aside` (unsafe_impl/kfuncs.rs), which records LavdBailOnCgroupThrottle AND pushes the pid onto `bw_blocked[cgid]`. The comment there says: "V2 does NOT push redundant TaskWake events on replenish — the lib already drives reenqueue via cbw_drain_btq_batch ... Adding an engine-side wake here would duplicate the lib's drain".
2. The engine's replenish handling in the fire-timer path (engine.rs, after `diff_snapshots`) drains ALL of `bw_blocked[cgid]` on keep_throttled == false. For every entry it sets task.state = Sleeping, pushes EventKind::TaskWake and records CgroupBwReenqueueOnReplenish. It cannot tell bail-path entries (which the library owns) from `eager_stash_throttled` entries (which the engine owns). So the redundant wake the comment rules out does happen.
3. LAVD then drains the same pid from the BTQ (`lavd_dispatch` -> `scx_cgroup_bw_reenqueue` -> `cbw_drain_btq_batch` -> `lavd_enqueue_cb` -> `scx_bpf_dsq_insert_vtime`). The sim DSQ accepts the second insert of a task that is already queued.

EVIDENCE. Scenario: `contended_throttled_scenario(3, 1000)` in tests/cgroup_bw_stall_completion.rs (1 CPU, seed 42, instant_timing, 10ms/100ms quota, three forever tasks). Dump trace.events(). At scx 413031d44, pid 1:
-  21.5ms  LavdBailOnCgroupThrottle pid 1 (into BTQ and bw_blocked)
- 100.0ms  replenish, keep_throttled=false -> CgroupBwReenqueueOnReplenish pid 1, TaskWoke, Runnable, SelectTaskRq, DsqInsertVtime dsq 4096 vtime 33168, EnqueueTask
- 105.0ms  DsqInsertVtime pid 1 dsq 4096 vtime 33168, then LavdReenqueueViaBtqDrain: the library's BTQ drain inserts the same task again
- 120.0ms  cgroup throttled
- 130.0ms  CgroupBwDequeueOnThrottle pid 1
- 140.0ms  CgroupBwDequeueOnThrottle pid 1 again: two DSQ entries for one task
- 300.0ms  CgroupBwReenqueueOnReplenish pid 1 TWICE (both bw_blocked entries drained)

Duplicate (time, pid) re-enqueues in that run:
- scx 59c30baee (old pin): 500ms pid 1 and pid 3; 800ms pid 3.
- scx 413031d44: 300ms pid 1; 500, 700, 800 and 900ms pid 3.
PRE-EXISTING at both pins; not caused by the 2026-09-24 pin bump. With 3 tasks, a single replenish records up to 5 CgroupBwReenqueueOnReplenish events.

PROBABLY RELATED, NOT VERIFIED: sim-a00345 (a finite RepeatMode::Once task emits multiple TaskCompleted under throttle). A task queued twice can be picked twice.

ALSO: the DANGER TODO on `eager_stash_throttled` (engine.rs) cites `sim-eager-throttle-v2`, which is not an mb issue id (`mb show` says not found). Point it at a real issue when this is fixed.

FIX DIRECTION (Model the Kernel): the BTQ belongs to the library, so the library alone re-enqueues bail-path tasks. The engine should wake only the tasks it stashed itself. Either keep bail-path pids out of `bw_blocked`, or tag entries by owner. Then check that no path can queue a task that is already queued. A debug assertion in the sim DSQ insert that the task is not already on a DSQ would catch any other path.

ACCEPTANCE:
- The scenario above records no duplicate (time, pid) CgroupBwReenqueueOnReplenish.
- No pid gets two CgroupBwDequeueOnThrottle without an intervening enqueue.
- A regression test asserts both.
- sim-a00345 is re-checked.
