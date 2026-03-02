---
title: 'CgroupCreate: keep as global bookkeeping, verify CPU context'
status: closed
priority: 3
issue_type: task
depends_on:
  sim-f936c: parent-child
created_at: 2026-02-24T10:32:08.336242237+00:00
updated_at: 2026-03-02T20:06:22.055065771+00:00
closed_at: 2026-03-02T20:06:22.055065700+00:00
---

# Description

ops.cgroup_init runs with SCX_KF_UNLOCKED, rq=NULL, under cgroup_mutex.
In the kernel it runs on whatever CPU the mkdir(2) caller is on.

This is genuinely global (serialized by cgroup_mutex, no per-CPU state).
The structop runs in sleepable context with no rq lock. It can remain a
global engine event that runs on an arbitrary CPU timeline.

Checklist:
[ ] Assign to a specific CPU (writer's CPU or any available)
[ ] Ensure the CPU context is set so bpf_get_smp_processor_id() returns
    a valid value during the callback
[ ] Verify kfunc restrictions match SCX_KF_UNLOCKED
