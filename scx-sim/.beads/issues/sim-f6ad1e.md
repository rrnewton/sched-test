---
title: 'CgroupCpusetChange: model per-task rq->lock for set_cpumask'
status: closed
priority: 3
issue_type: task
depends_on:
  sim-f936c: parent-child
created_at: 2026-02-24T10:32:08.343778899+00:00
updated_at: 2026-03-02T20:06:22.058889436+00:00
closed_at: 2026-03-02T20:06:22.058889356+00:00
---

# Description

CgroupCpusetChange is serialized by cpuset_mutex globally, but contains
per-task operations that each hold task_rq_lock (pi_lock + rq->lock):

- ops.dequeue (if task is queued)
- ops.set_cpumask (SCX_KF_REST, rq=NULL but rq->lock IS held)
- ops.runnable + ops.enqueue (re-enqueue after cpumask update)

Similar pattern to CgroupMigrate: the global event is bookkeeping, but
the per-task structop calls should ideally run on CPU timelines.

Checklist:
[ ] Model per-task rq->lock acquisition
[ ] Per-task dequeue/set_cpumask/enqueue on per-CPU timelines
[ ] Verify set_cpumask kfunc context matches SCX_KF_REST
