---
title: 'layered: p->tgid is never set, so is_scheduler_task() is TRUE for every task and IsGroupLeader is inverted'
status: open
priority: 2
issue_type: task
created_at: 2026-09-10T20:52:52.858243152+00:00
updated_at: 2026-09-10T20:52:52.858243152+00:00
---

# Description

Two facts, both measured at integration@f418aa7 with scx pin 59c30bae.

1. sim_task_set_pid() never assigns p->tgid, so tgid is 0 for every simulated task while pids start at 1.
   - scx_layered MATCH_IS_GROUP_LEADER is `(p->tgid == p->pid) == want` (main.bpf.c:3077). IsGroupLeader(true) can therefore NEVER fire and IsGroupLeader(false) ALWAYS fires. Silently wrong, no error. Every deployed AI-training layered config carries such a rule.
   - MATCH_TGID_EQUALS (main.bpf.c:3033) only ever compares against 0.
   - Worse: layered's is_scheduler_task(p) is `(u32)p->tgid == layered_root_tgid` (main.bpf.c:205) and layered_root_tgid is 0 because the wrapper never runs initialize_pid_namespace. So EVERY simulated task is currently classified as one of scx_layered's own userspace daemon threads, and takes the daemon fast path at main.bpf.c:2044 (hi_fb DSQ / SCX_DSQ_LOCAL, 'run before preempting layers') instead of the ordinary layer-DSQ vtime path below it.

2. THE OBVIOUS FIX IS NOT LANDABLE ON ITS OWN. Setting p->tgid = pid is correct, and it turns tests/layered.rs::default_config_loads_and_runs RED, because it unmasks the stall in sim-zwypg. Measured, 4 CPUs, three 20ms run-once tasks:
     tgid unset (today):  3/3 complete
     tgid = pid:          2/3 complete, third stuck forever (still 2/3 at 3000ms)
   Held back for that reason rather than landed red. The companion group_leader fix DID land (it fixes a NULL deref and is green).

To land this: fix sim-zwypg (the idle-CPU dispatch stall) first, then set p->tgid = pid in sim_task_set_pid(), then invert known_gap_is_group_leader_answers_backwards_for_every_task in tests/layered_rtapp_naming.rs. Consider also driving initialize_pid_namespace from the wrapper with a tgid no simulated task can have, so is_scheduler_task() is false for everyone — which is the truthful answer, since scxsim runs no scx_layered userspace daemon inside the simulation.
