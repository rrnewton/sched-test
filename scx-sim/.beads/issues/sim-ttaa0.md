---
title: scxsim has no thread groups, so MATCH_PCOMM_PREFIX degenerates to MATCH_COMM_PREFIX
status: open
priority: 2
issue_type: task
created_at: 2026-09-10T20:36:05.914373206+00:00
updated_at: 2026-09-10T21:02:39.879331658+00:00
---

# Description

Every simulated task is a single-threaded process: sim_task_alloc() now sets p->group_leader = p and sim_task_set_pid() sets p->tgid = pid, which is the correct kernel state for such a task and fixes a NULL deref (see the commit that added this issue reference).

What is STILL missing: real thread groups. Real deployed scx_layered configurations use MATCH_PCOMM_PREFIX heavily — it is the second most common match kind overall, and for several edge-family configs it is the ONLY kind used — precisely to catch WORKER THREADS by their PROCESS name rather than their own. (Corpus location and exact counts are internal; see tg layered-recon-and-hodges-configs notes.) With one task per thread group, pcomm == comm and that distinction cannot be reproduced.

Needed:
- a thread-group concept on TaskDef (a tgid, or a 'threads of' relation), with the engine pointing p->group_leader at the leader's task_struct;
- an rt-app surface to express it (rt-app itself has no thread-group syntax, so this is an extension);
- then invert known_gap_pcomm_prefix_cannot_distinguish_a_thread_from_its_leader in tests/layered_rtapp_naming.rs.

Also unblocks MATCH_TGID_EQUALS as something other than 'equals my own pid'.
