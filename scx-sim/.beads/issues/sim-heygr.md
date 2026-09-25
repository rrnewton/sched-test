---
title: 'scxsim: model SCX_ENQ_CPU_SELECTED — __COMPAT_is_enq_cpu_selected() is shimmed to true, and ops.select_cpu runs where the kernel skips it'
status: open
priority: 1
issue_type: bug
labels:
- no-stub
- kernel-fidelity
- engine
depends_on:
  sim-010a1: related
created_at: 2026-09-25T05:18:55.251142991+00:00
updated_at: 2026-09-25T05:21:26.504680352+00:00
---

# Description

DEFECT

`__COMPAT_is_enq_cpu_selected(enq_flags)` is redefined to `(true)` in two places: `crates/scx_simulator/csrc/sim_wrapper.h` (it was moved there from the mitosis, cosmos and lavd wrappers, which all defined it the same way) and again in `schedulers/layered/wrapper.c`. Both comments give the same reason: "the simulator always runs select_cpu before enqueue". That is false. The shim also hides two more places where the engine's wake path departs from the kernel's.

PRODUCTION

- `scx/scheds/include/scx/compat.bpf.h` `__COMPAT_is_enq_cpu_selected()`:
  - On kernels that have `SCX_ENQ_CPU_SELECTED`, it returns `enq_flags & SCX_ENQ_CPU_SELECTED`.
  - On older kernels it returns true.
- `kernel/sched/core.c` `select_task_rq()` calls the class's `select_task_rq` only when `p->nr_cpus_allowed > 1 && !is_migration_disabled(p)`. For sched_ext that is `select_task_rq_scx()`, and so `ops.select_cpu`.
  - It sets `WF_RQ_SELECTED` only in that case.
  - `ttwu_do_activate()` turns `WF_RQ_SELECTED` into `ENQUEUE_RQ_SELECTED`.
  - `kernel/sched/ext.c` defines `SCX_ENQ_CPU_SELECTED` as `ENQUEUE_RQ_SELECTED`.
- A wakeup therefore runs three callbacks, in this order:
  1. `ops.select_cpu`, but only if the task can migrate.
  2. `ops.runnable`, from `enqueue_task_scx()`.
  3. `ops.enqueue`, with `SCX_ENQ_WAKEUP`, plus `SCX_ENQ_CPU_SELECTED` if select_cpu ran.
- Every other enqueue arrives without `SCX_ENQ_CPU_SELECTED`. That covers a re-enqueue after a slice or a yield, a CPU going offline, and a cgroup move.

SIMULATOR (sched-test 24d864c6, scx 413031d44)

1. The shim makes the inline return true for every enqueue.
2. Four engine paths call `enqueue(p, 0)` without running select_cpu first. These are exactly the enqueues where production answers false:
   - `crates/scx_simulator/src/safe/engine.rs` `stop_and_reenqueue`;
   - `handle_task_phase_complete`, both for the yield re-enqueue and for the "still runnable" re-enqueue after a wake phase;
   - `cgroup_migrate_enqueue`;
   - `handle_cpu_offline`.
3. `handle_task_wake` calls `select_cpu` unconditionally.
   - That includes a task pinned to one CPU or with migration disabled, where the kernel does not call it.
   - Its enq_flags are `SCX_ENQ_WAKEUP[|SCX_WAKE_SYNC]` and never include `SCX_ENQ_CPU_SELECTED`.
4. `handle_task_wake` calls `runnable` before `select_cpu`. The kernel calls `ops.select_cpu` first.

CONSEQUENCE

Enqueue-time CPU selection never runs on any re-enqueue:
- `____lavd_enqueue` never calls `pick_idle_cpu` on that path.
- `____layered_enqueue` never calls `maybe_refresh_task_layer_from_hint`.
- Mitosis's `!selected || SCX_ENQ_LAST` test collapses to `SCX_ENQ_LAST`.
- In cosmos, `task_should_migrate()` returns `!__COMPAT_is_enq_cpu_selected(enq_flags) && !scx_bpf_task_running(p)`. With the shim this is always false, so it never calls `scx_bpf_task_running`.

For pinned or migration-disabled tasks the simulator also runs a callback that production never runs. Whatever select_cpu does there has no production counterpart: a direct dispatch, an idle-CPU claim, a per-task statistics update. A callback-order-sensitive scheduler also sees runnable state before select_cpu, the reverse of production.

TRAP FOR THE FIX

Deleting the shims on their own is wrong.
- `crates/scx_simulator/scxtest/overrides.h` defines `__builtin_preserve_enum_value(x,y)` as 1.
- So `bpf_core_enum_value_exists()` and `bpf_core_enum_value(enum scx_enq_flags, SCX_ENQ_CPU_SELECTED)` both fold to 1, and the inline becomes `enq_flags & 1`.
- Bit 0 is `SCX_ENQ_WAKEUP`. In vmlinux.h at this pin, `SCX_ENQ_WAKEUP = 1` and `SCX_ENQ_CPU_SELECTED = 1048576`.

Testing `SCX_ENQ_WAKEUP` gives the right answer only while the engine runs select_cpu on every wakeup. Once select_cpu is gated the way the kernel gates it, it gives the wrong one: a pinned task's wakeup would still read as selected. The inline must see the real enum value. sim-bid6t (verdict K09) covers that.

FIX DIRECTION

- Gate the wake path's `select_cpu` the way `select_task_rq()` does:
  - call it only when `nr_cpus_allowed > 1` and migration is not disabled;
  - otherwise take a CPU from the task's cpumask, as `cpumask_any(p->cpus_ptr)` does;
  - call it before `runnable`.
- Set `SCX_ENQ_CPU_SELECTED` in the enqueue flags exactly when select_cpu ran, and never on the re-enqueue paths listed above.
- Make `bpf_core_enum_value()` return the vmlinux.h value (sim-bid6t). Then delete both shims so the unmodified `compat.bpf.h` inline runs.

ACCEPTANCE

- A test that fails today: a cosmos or lavd task re-enqueued after its slice expires reaches the enqueue-time branch.
  - For cosmos, `task_should_migrate()` calls `scx_bpf_task_running`.
  - For lavd, the enqueue path calls `pick_idle_cpu`.
  - Assert this through a probe or counter, not only "the run completed".
- A wakeup that ran select_cpu sees `__COMPAT_is_enq_cpu_selected()` as true.
- The wakeup of a task pinned to one CPU does not run select_cpu, and sees it as false.
- There is no simulator-side definition of `__COMPAT_is_enq_cpu_selected` left.
- Until this is fixed, both shim sites carry a DANGER TODO naming this issue. sim-io5ng tracks that.

Found by the 2026-09-24 scx pin bump coverage re-measure (sched-test 24d864c6, scx 413031d44). See verdict row S01 in coverage/scx_pin_bump_20260924/verdicts.tsv in the dev harness (rrnewton/dev-sched-test).
