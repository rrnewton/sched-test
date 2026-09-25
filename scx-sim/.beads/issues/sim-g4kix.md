---
title: 'The engine never consumes a running task''s slice: all 128 of scx_tickless''s set_slice preemptions are dropped and the run exits Normal'
status: open
priority: 1
issue_type: bug
labels:
- cratesio
created_at: 2026-09-25T03:43:12.595434499+00:00
updated_at: 2026-09-25T03:43:12.595434499+00:00
---

# Description

The engine does not consume a running task's slice, and does not re-arm slice expiry when a scheduler changes the slice mid-run. A task started with SCX_SLICE_INF therefore keeps its CPU until its phase ends, even after the scheduler has given it a finite slice precisely so that it can be preempted.

In the kernel, task_tick_scx runs update_curr_scx, which subtracts the elapsed runtime from p->scx.slice unless it is SCX_SLICE_INF, calls ops.tick, and then reschedules whenever the slice is 0. Preemption follows the slice value, however the slice got there.

The engine does two narrower things:
- At run start (the run-start path that pushes SliceExpired), it pushes one SliceExpired at local_t + slice if the slice ends before the phase. It pushes nothing for SCX_SLICE_INF, whose value exceeds any phase.
- In handle_tick it preempts only on a self-kick with SCX_KICK_PREEMPT, or when ops.tick itself moved the slice from non-zero to 0 (slice_zeroed = pre_tick_slice > 0 && post_tick_slice == 0). Nothing decrements the slice between ticks.

A later write to p->scx.slice (scx_bpf_task_set_slice, or the compat fallback that stores the field directly) is invisible to both.

Measured with scx_tickless at integration 24d864c6 (release-candidate scratch consumer, 4 CPUs, 3 SpinWait tasks and 3 burst/sleep tasks, 1 s). tickless enqueues with SCX_SLICE_INF. Its sched_timerfn, on finding a CPU whose current task has an infinite slice while tasks are queued, calls scx_bpf_task_set_slice(p, slice_ns) and counts it in nr_preemptions:
- tickless's own counters: nr_preemptions = 128, nr_ticks = 997.
- The engine's summary: total_preempts = 0, total_ticks = 997.
- pid 1 (a spinner) was scheduled once and never descheduled in the whole run. Trace::total_runtime reports 0 for it, because the only interval is still open at the end. 5 of the 6 tasks were runnable at exit.

So all 128 preemptions the scheduler asked for were dropped, and the run still exits Normal.

cosmos, lavd, layered and mitosis also call scx_bpf_task_set_slice. Any call that shortens or extends a running task's slice is subject to the same gap. Not measured for those four.

A related point, found by reading the code but not measured: handle_slice_expired deducts task.get_slice() (the slice value when the event fires) from run_remaining_ns, not the time since task_started_at. When the slice changed after the event was armed, the two differ.

sim-5b567 (closed) covered the missing tick callback. This is the accounting that task_tick_scx does around the callback.

Fix: model update_curr_scx. At each tick, and at any point where the engine stops the task, subtract the elapsed runtime from p->scx.slice unless it is SCX_SLICE_INF. After ops.tick, preempt if the slice is 0. Either re-arm SliceExpired from the live slice after every callback that can write it, or drop the pre-armed event and let the tick decide, as the kernel does. Then re-measure tickless's preempt count and every scheduler's fingerprints, which will move.

Found while preparing the crates.io release candidate (tg scxsim-cratesio-release-candidate).
