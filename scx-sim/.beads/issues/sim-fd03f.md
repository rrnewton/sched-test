---
title: 'TaskWake: model per-CPU execution and migrate off global path'
status: closed
priority: 1
issue_type: task
depends_on:
  sim-f936c: parent-child
created_at: 2026-02-24T10:32:08.324797271+00:00
updated_at: 2026-03-02T20:06:22.048741448+00:00
closed_at: 2026-03-02T20:06:22.048741348+00:00
---

# Description

TaskWake is the highest-impact item. In the kernel, try_to_wake_up() is
fundamentally per-CPU:

- ops.select_cpu() runs on the WAKER's CPU with only p->pi_lock (no rq lock)
- ops.runnable() and ops.enqueue() run holding the TARGET CPU's rq->lock
- Two wakes for different tasks on different CPUs run fully in parallel

The simulator already sets wake_cpu and waker_raw internally (engine.rs
handle_task_wake), but classifies the event as global (cpu() returns None),
preventing concurrent processing.

Checklist:
[ ] Assign TaskWake to the waker's CPU (or prev_cpu if no waker)
[ ] Model the rq->lock: select_cpu runs without rq lock, runnable/enqueue
    hold the target CPU's rq lock
[ ] Handle cross-CPU dispatch: select_cpu on waker's CPU may dispatch to a
    different CPU's local DSQ
[ ] Move TaskWake from the global event path to per-CPU concurrent batching
[ ] Verify determinism and replay compatibility
