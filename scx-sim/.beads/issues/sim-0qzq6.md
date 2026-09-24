---
title: 'scxsim: SCX_ENQ_IMMED reads as 0 — model the kernel''s IMMED bounce-back'
status: open
priority: 2
issue_type: bug
created_at: 2026-09-24T21:30:09.462313587+00:00
updated_at: 2026-09-24T21:30:09.462313587+00:00
---

# Description

csrc/sim_wrapper.h #undefs the SCX_ENQ_* CO-RE enum macros so they take vmlinux.h values, but not SCX_ENQ_IMMED (vmlinux 0x200000000). It therefore resolves to the weak const volatile __SCX_ENQ_IMMED, which nobody sets: every scheduler sees SCX_ENQ_IMMED == 0, i.e. a kernel without IMMED. scx_cosmos (since sched-ext/scx 5c0be1ce7 et al.) then runs its documented fallback (direct_dispatch_local skips on shared_dsq_has_pinned_waiter) instead of the IMMED path. Fix: undef it AND model the kernel semantics — an IMMED insert into a local DSQ whose task cannot run immediately on that CPU is bounced back through ops.enqueue (SCX_ENQ_REENQ). Just un-defining it without the bounce would admit a behaviour the kernel does not produce (task parked behind others on a local DSQ despite IMMED). Found while fixing the cosmos-arm failures on the 413031d44 pin bump (task fix-cosmos-wrapper-cpu-util-map).
