---
title: PpidEquals(X) also matches task X, because parentless tasks are their own real_parent
status: open
priority: 3
issue_type: task
created_at: 2026-09-11T14:22:53.783293140+00:00
updated_at: 2026-09-11T14:22:53.783293140+00:00
---

# Description

sim_task_alloc() sets p->real_parent = p — 'self-referencing; simulates init as parent'. That is exactly right for pid 1 and a simplification for every other task.

layered's arm is 'p->real_parent->pid == match->ppid', so a task with no declared parent satisfies a rule naming its OWN pid. A configuration written as 'everything under the supervisor' silently picks up the supervisor itself.

Found while adding the rt-app++ 'parent' key (which drives real_parent and makes PpidEquals reachable from a workload spec for the first time). PINNED by tests/layered_rtapp_naming.rs::known_gap_ppid_equals_also_matches_the_named_parent_itself.

NOT CASUALLY REMOVABLE, and this is the part to read before touching it. Engine::new deliberately defers task_pid_to_raw registration until after init_task completes, specifically so that a self-referencing real_parent makes bpf_task_from_pid() return NULL during init_task — which is what drives LAVD down its correct initialisation path (avg_runtime_wall = sys_stat.slice_wall). See the comment at that site. Closing this means giving parentless tasks a real synthetic init parent, and re-verifying that path.

DO NOT close it by synthesising a 'parent' for every task in the rt-app front end. That hides the artifact rather than fixing it, and it would make every task's ppid a number no spec named.
