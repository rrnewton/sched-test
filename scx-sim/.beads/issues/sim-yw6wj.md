---
title: 'scxsim: deliver exec events so lavd''s sys_enter_execve hooks (aggressive migration at exec) can run'
status: open
priority: 3
issue_type: feature
labels:
- substrate-gap
- lavd
created_at: 2026-09-25T05:18:55.281788913+00:00
updated_at: 2026-09-25T05:18:55.281788913+00:00
---

# Description

GAP

The simulator has no exec event, so two lavd programs never fire: `cond_hook_sys_enter_execve` and `cond_hook_sys_enter_execveat`.

PRODUCTION

- `main.bpf.c` attaches these programs to `?tracepoint/syscalls/sys_enter_execve` and `?tracepoint/syscalls/sys_enter_execveat`.
- `main.rs` loads them only when there is more than one compute domain (`order.cpdom_map.len() > 1`).
- On an exec, if the task's CPU is in an overloaded compute domain (`is_stealee`), `set_aggressive_migration`:
  - sets `LAVD_FLAG_MIGRATION_AGGRESSIVE` on the task, which the CPU selection in `idle.bpf.c` reads;
  - kicks that CPU with `SCX_KICK_PREEMPT`, so the task migrates at once.

SIMULATOR (sched-test 24d864c6, scx 413031d44)

Simulated workloads have no exec phase, so neither program runs.

CONSEQUENCE

Tasks never get the aggressive-migration flag at exec. Multi-domain lavd scenarios therefore never exercise that migration path. `set_aggressive_migration` sits behind these programs and is not delivered (LATENT).

FIX DIRECTION

- Add an exec phase to the workload model.
- When lavd is loaded with more than one compute domain, deliver it as the `sys_enter_execve` tracepoint program, running on the task's current CPU in the task's context.

ACCEPTANCE

A multi-domain lavd test execs a task on an overloaded domain. It asserts that the task gets `LAVD_FLAG_MIGRATION_AGGRESSIVE`, that the CPU is kicked, and that the task migrates.

Found by the 2026-09-24 scx pin bump coverage re-measure (sched-test 24d864c6, scx 413031d44). See verdict row E06 in coverage/scx_pin_bump_20260924/verdicts.tsv in the dev harness (rrnewton/dev-sched-test).
