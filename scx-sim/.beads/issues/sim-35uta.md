---
title: 'scxsim: model thread groups (p->signal->thread_head) for group-wide cgroup moves'
status: open
priority: 2
issue_type: task
created_at: 2026-08-12T15:28:03.771364237+00:00
updated_at: 2026-08-12T15:28:03.771364237+00:00
---

# Description

The simulated task_struct has no signal_struct, so p->signal is NULL and
p->thread_node is never linked.

Consequence for scx_layered (tg layered-support-implement): the engine
delivers tp_btf/cgroup_attach_task with threadgroup=false only. That is the
faithful value today (scxsim migrates one task at a time, never a whole
thread group), but it also means layered's threadgroup branch is dark, and
delivering true would dereference near-NULL:

    thread_head = &leader->signal->thread_head;         // NULL + offset
    p = container_of(next->thread_node.next, ...);      // near-NULL
    pid = BPF_CORE_READ(p, pid);                        // SIGSEGV

To model group moves we would need:
1. A signal_struct per thread group on the C side, with thread_head, and
   p->thread_node linked into it for every task sharing an MmId (the
   simulator already groups threads by MmId for wake-affine tests).
2. A Scenario event for a group-wide cgroup move, delivering the tracepoint
   with threadgroup=true.

Until then, see scx-sim/ai_docs/LAYERED_SUPPORT.md ("Documented
divergences" #4). Also blocks MATCH_PCOMM_PREFIX from being meaningfully
distinct from MATCH_COMM_PREFIX, since the group leader's comm is currently
the task's own.
