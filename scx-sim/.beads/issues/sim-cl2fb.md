---
title: 'rt-app++ thread groups are simulation-only: no backend can run them on real hardware'
status: open
priority: 3
issue_type: task
created_at: 2026-09-11T14:23:11.923456728+00:00
updated_at: 2026-09-11T14:23:11.923456728+00:00
---

# Description

Recorded so the limit is visible rather than discovered.

The rt-app++ 'thread_of' key (see scx-sim/ai_docs/RTAPP_PP_WORKLOAD_LANGUAGE.md) lets a workload express real thread groups, which is what makes scx_layered's MATCH_PCOMM_PREFIX reproducible in simulation. There is currently NO backend that can run such a spec for real:

- rt-app (C, checkouts/rt-app@9eedd75) uses pthread_create for every task, so all tasks are threads of ONE process whose comm is 'rt-app'. MEASURED: every thread reported the same Tgid, leader comm 'rt-app'. Its 'fork' event spawns another THREAD of the same process, not a process.
- rt-app-rs uses thread::Builder::name, same shape.

Consequence: pcomm is 'rt-app' for every task in every stock spec, so no production pcomm rule can fire on a real rt-app run — with or without our extension.

scenario_to_rtapp_json() therefore REFUSES to export a scenario carrying a thread group, rather than emitting a spec that would run as a different workload. That refusal is correct and should stay until a backend exists.

TO CLOSE: teach rt-app-rs (which we own) to launch a spec as MULTIPLE PROCESSES, one per declared thread group, each with its threads. It already shells out for cgroup setup (taskgroups.rs), so process orchestration is not foreign to it. The leader process must set its own comm to the leader task's name, since that is the string pcomm rules read.

Until then, any comparison of a thread-group workload between scxsim and hardware is not available, and a report that claims one is wrong.
