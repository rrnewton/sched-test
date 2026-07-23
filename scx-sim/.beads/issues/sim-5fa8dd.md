---
title: 'engine: SCX_WAKE_FORK not modeled on initial/fork task enqueue'
status: open
priority: 3
issue_type: bug
created_at: 2026-07-23T04:35:37.135970868+00:00
updated_at: 2026-07-23T04:35:37.135970868+00:00
---

# Description

Discovered while writing enqueue_flags.rs (tg test-enqueue-flags-handling).

The engine schedules every task's first activation via EventKind::TaskWake with waker=None (engine.rs ~1922). That goes through the normal wakeup path, so the initial enqueue is delivered as a plain SCX_ENQ_WAKEUP (0x1) with no wake-flag distinguishing it as a fork.

In the kernel, a newly-forked task's first select_task_rq/enqueue carries SCX_WAKE_FORK (=4) in the wake flags (and ops.init_task receives args->fork=true). Schedulers can special-case fork placement (e.g. spread new tasks, skip cache-affinity). scxsim currently cannot exercise those code paths because no fork/SCX_WAKE_FORK signal is ever delivered.

Note: there is NO SCX_ENQ_FORK enqueue flag in the kernel enum (SCX_ENQ_WAKEUP=1, SCX_ENQ_HEAD=0x10000, SCX_ENQ_PREEMPT, SCX_ENQ_REENQ, SCX_ENQ_LAST, ...). Fork is a WAKE flag (SCX_WAKE_FORK=4 alongside SCX_WAKE_TTWU=8, SCX_WAKE_SYNC=16). The task title's 'SCX_ENQ_FORK' is a misnomer.

What IS modeled today (verified across simple/lavd/cosmos):
- SCX_ENQ_WAKEUP (0x1) on every wake (Runnable/EnqueueTask).
- SCX_WAKE_SYNC (0x10) added for waker-driven (Phase::Wake) wakes -> Runnable enq_flags=0x11.
- enq_flags=0 on slice-expiry/yield re-enqueue.
Not modeled: SCX_WAKE_FORK, SCX_WAKE_TTWU, and enqueue flags SCX_ENQ_HEAD/PREEMPT/REENQ/LAST/NESTED (engine never emits them).

Minor related quirk: the wakeup-path EnqueueTask trace hardcodes enq_flags=SCX_ENQ_WAKEUP (engine.rs ~3643) even when the enqueue callback received WAKEUP|SYNC; SYNC is a wake flag not an enqueue flag, so hardcoding WAKEUP in the *enqueue* trace is defensible, but the callback vs trace values differ in the sync case.

Fix sketch: thread a fork/wake-flag through EventKind::TaskWake for a task's first activation; deliver SCX_WAKE_FORK to select_cpu and set args->fork in init_task. Then add fork-flag assertions to enqueue_flags.rs.
