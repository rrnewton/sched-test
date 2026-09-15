---
title: 'scxsim: runtime cpuset changes and cgroup migrations do not re-narrow task cpumasks (residue of sim-4qlh5)'
status: open
priority: 2
issue_type: task
created_at: 2026-08-12T23:20:38.520897502+00:00
updated_at: 2026-08-12T23:20:38.520897502+00:00
---

# Description

Follow-up to sim-4qlh5, which fixed the STATIC case only. Filing the residue explicitly so the fix does not read as broader than it is.

`Scenario::effective_cpuset` narrows a task's cpumask by its cgroup's cpuset at scenario setup (`engine.rs:1499`). Two runtime paths still bypass it:

## 1. `handle_cgroup_cpuset_change` (engine.rs:3426)

Calls `cgroup_registry.update_cpuset(...)` and then `cgroup_init` to notify the scheduler. It never re-narrows the cpumasks of tasks already in that cgroup. A scenario that starts a cgroup on all CPUs and later confines it to {2,3} via `cgroup_cpuset_change_events` will keep running its tasks anywhere.

In the kernel, writing `cpuset.cpus` updates every member task's effective cpumask and MIGRATES tasks that are no longer allowed where they are running.

## 2. `cgroup_migrate_events`

A task moved into a different cgroup at runtime keeps the cpumask it was constructed with. In the kernel, joining a cpuset-confined cgroup re-narrows the task immediately.

## Why this is filed rather than fixed with sim-4qlh5

The static fix is verified by measurement (placement went from `{cg_0:{1}, cg_1:{0}}` to `{cg_0:{0}, cg_1:{2}}` on a 4-CPU disjoint-halves scenario) and has a negative control. Extending it to the runtime paths needs task migration on cpuset shrink — genuinely more work than a mask recompute, since a task currently running on a now-forbidden CPU has to be moved the way the kernel moves it. Bundling a half-done version of that into the static fix would repeat the mistake sim-4qlh5 was about: a change that appears to cover a property it only partly covers.

## No scenario exercises this today

Neither ktstr scenario nor any in-tree scx-sim test uses `cgroup_cpuset_change_events` with member tasks whose placement is then checked, which is precisely why the gap is invisible. sim-4qlh5 was caught only because `sched_cpuset_split` existed. A scenario that shrinks a cpuset mid-run and asserts placement afterwards is the acceptance test for this issue and should be written with it.
