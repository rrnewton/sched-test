---
title: 'TimerFired: assign to specific CPU and migrate off global path'
status: closed
priority: 2
issue_type: task
depends_on:
  sim-f936c: parent-child
created_at: 2026-02-24T10:32:08.328358960+00:00
updated_at: 2026-03-02T20:06:22.050632915+00:00
closed_at: 2026-03-02T20:06:22.050632825+00:00
---

# Description

BPF timers fire in softirq on the CPU where bpf_timer_start() was called
(with BPF_F_TIMER_CPU_PIN). No scheduler locks are held. The kernel comment
says: "runs in hrtimer_run_softirq. It doesn't migrate and cannot be
preempted by another bpf_timer_cb() on the same cpu."

The simulator currently processes TimerFired with no CPU context
(set_sim_clock(state.clock, None)), which means bpf_get_smp_processor_id()
returns no meaningful value during the callback.

Checklist:
[ ] Track which CPU armed the timer (bpf_timer_start kfunc)
[ ] Assign TimerFired event to that CPU
[ ] Set correct CPU context so bpf_get_smp_processor_id() works
[ ] Move TimerFired from global path to per-CPU concurrent batching
[ ] Handle the case where timer callbacks touch global state (DSQs, etc.)
