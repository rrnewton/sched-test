---
title: layered MATCH_IS_KTHREAD ignores match->is_kthread, so IsKthread(false) matches kthreads (upstream)
status: open
priority: 3
issue_type: task
created_at: 2026-09-11T14:22:53.780263530+00:00
updated_at: 2026-09-11T14:22:53.780263530+00:00
---

# Description

UPSTREAM BUG, at scx pin 59c30bae. scheds/rust/scx_layered/src/bpf/main.bpf.c:

    case MATCH_IS_KTHREAD:
        return p->flags & PF_KTHREAD;

It returns the flag and never reads match->is_kthread — unlike MATCH_IS_GROUP_LEADER directly above it, which does '(p->tgid == p->pid) == match->is_group_leader'. So a layer whose rule is IsKthread(false) matches kernel threads, exactly the tasks it was written to exclude.

OURS: LayerMatch::IsKthread(bool) mirrors the upstream schema and therefore advertises a knob the BPF does not read.

DELIBERATELY NOT WORKED AROUND on our side. Making to_ffi() emit the Not/exclude flag for IsKthread(false) would make our API mean something the scheduler does not, which is precisely the failure class tests/layered_rtapp_naming.rs exists to catch. Negation already works correctly through LayerMatch::Not, which sets the separate exclude flag.

PINNED by tests/layered_rtapp_naming.rs::known_gap_is_kthread_ignores_its_own_argument, which asserts BOTH halves: a kthread lands in the IsKthread(false) layer (the bug) and a user task does not (which is why it is easy to miss — the rule is only wrong on the tasks it was meant to exclude).

TO CLOSE: either upstream honours the argument (then invert the known-gap test and drop the caveat from rtapp_kthread_drives_is_kthread_match), or we decide this is worth a PR to sched-ext/scx. Note the harness push policy: scx personal/experimental branches go to rrnewton, never to origin.
