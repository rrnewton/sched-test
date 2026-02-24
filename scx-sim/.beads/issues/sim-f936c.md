---
title: Eliminate special-case 'global event' treatment for structops
status: open
priority: 1
issue_type: epic
created_at: 2026-02-24T10:31:31.935670615+00:00
updated_at: 2026-02-24T10:31:31.935670615+00:00
---

# Description

All sched_ext structop callbacks (scheduler C code) should run on a CPU's
normal timeline via the per-CPU concurrent processing path. No structop
should receive special treatment as a "global event" processed sequentially
outside the interleaving system.

The invariant: ALL C scheduler code invoked via structops is called on a
per-CPU timeline, participating in normal concurrent batch processing.
The only remaining "global engine events" should be pure simulation
bookkeeping that schedules work onto CPU timelines (analogous to how a
hardware IRQ schedules a softirq onto a specific core).

See ai_docs/global_events_kernel_analysis.md for the detailed kernel
analysis motivating each sub-task. Key findings:
- TaskWake runs on the waker's CPU in the kernel (not global)
- TimerFired runs in softirq on a specific CPU (not global)
- Cgroup ops are serialized by cgroup_mutex but their per-task structop
  callbacks (dequeue, cgroup_move, enqueue) hold per-CPU rq->lock

Each child issue tracks one global event type through: accurate CPU
assignment, accurate locking model, and migration off the global path.
