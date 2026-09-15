---
title: 'CgroupMigrate: model per-task rq->lock and correct dequeue flags'
status: closed
priority: 2
issue_type: task
depends_on:
  sim-f936c: parent-child
created_at: 2026-02-24T10:32:08.332295135+00:00
updated_at: 2026-03-02T20:06:22.052849313+00:00
closed_at: 2026-03-02T20:06:22.052849172+00:00
---

# Description

CgroupMigrate is genuinely serialized by cgroup_mutex (global), but the
per-task structop callbacks within it are per-CPU operations:

- ops.dequeue holds the task's current CPU rq->lock
- ops.cgroup_move runs with SCX_KF_UNLOCKED (rq=NULL), but rq->lock IS
  held by the caller
- ops.runnable + ops.enqueue hold rq->lock during re-enqueue

The global engine event should become a bookkeeping event that schedules
per-CPU work. The structop calls themselves should run on CPU timelines.

Also fix kernel fidelity issues found in the analysis:
[ ] Use DEQUEUE_SAVE | DEQUEUE_MOVE flags (not flags=0) for dequeue
[ ] Skip ops.runnable during re-enqueue (kernel skips it when
    task_on_rq_migrating is set)
[ ] Verify ops.quiescent behavior matches kernel
[ ] Model the cgroup_mutex serialization for the global coordination
[ ] Per-task dequeue/cgroup_move/enqueue should run on per-CPU timelines
