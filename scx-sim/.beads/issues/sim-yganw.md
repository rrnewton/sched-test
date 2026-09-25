---
title: 'scxsim: compile the scx compat inlines as shipped — __COMPAT_scx_bpf_dsq_peek, is_migration_disabled and bpf_in_* are replaced by macros, and scx_lib_init_probe never runs'
status: open
priority: 3
issue_type: task
labels:
- no-stub
- kernel-fidelity
depends_on:
  sim-35f7d: related
  sim-fa170: related
  sim-7cc89: related
  sim-7115f: related
  sim-e4644: related
  sim-cd172: related
created_at: 2026-09-25T05:18:55.254721815+00:00
updated_at: 2026-09-25T05:21:26.498247980+00:00
---

# Description

DEFECT

Three inlines from the scx headers are replaced by simulator macros instead of being compiled as shipped. The registration probe their migration answer depends on never runs. So the code production runs is not the code the simulator runs.

Each one gives production's answer in the cases the simulator models by default. Each one still breaks the No-Stub rule (it reimplements an interface), and each one hides a case production has.

SITES (sched-test 24d864c6, scx 413031d44)

S02: `__COMPAT_scx_bpf_dsq_peek(dsq_id)`, from `scx/scheds/include/scx/compat.bpf.h`.
- Production: calls the `scx_bpf_dsq_peek` kfunc where the kernel has it. Otherwise it walks the DSQ with `bpf_iter_scx_dsq` and returns the first task.
- Simulator: `crates/scx_simulator/csrc/sim_wrapper.h` routes the macro straight to the simulator's `scx_bpf_dsq_peek`. Neither the inline nor its fallback is compiled.
- Effect: same answer on a kernel that has the kfunc. The fallback that older kernels take is never exercised.

S03: `is_migration_disabled(p)`, from `scx/scheds/include/scx/common.bpf.h`.
- Production:
  - Where `task_struct` has no `migration_disabled` field, it returns false.
  - A count of 1 may be the non-sleepable BPF prolog's own `migrate_disable()` on `current`. With `CONFIG_PREEMPT_RCU` the inline returns `bpf_get_current_task_btf() != p`. On kernel 6.18 or later it returns true. Otherwise it asks `__scx_prolog_disables_migration`: if set, `bpf_get_current_task_btf() != p`, else true.
  - Any other count returns true if it is non-zero.
- Simulator: `sim_wrapper.h` defines it as `sim_task_get_migration_disabled(p) > 0`, and `schedulers/layered/wrapper.c` defines it again.
  - The reason: `sim_wrapper.h`'s `__builtin_preserve_field_info` override makes `bpf_core_field_exists` fold to 0.
  - With that override, the real inline would return false for every task.
- Effect:
  - At the default kconfig in `csrc/sim_kconfig_defaults.h` (`SIM_LINUX_KERNEL_VERSION` 0x061200, that is 6.18.0, and `SIM_CONFIG_PREEMPT_RCU` 0), a count of 1 takes the 6.18 branch and returns true. There the real inline gives the macro's answer.
  - An embedder can select `preempt_rcu` or an older `kernel_version` through scxsim-build `KernelConfig`, and the macro ignores both. The `KernelConfig` doc says the pair "affects only the cosmetic dump banner until that override is unwound".
  - Under `CONFIG_PREEMPT_RCU` the real inline assumes, without asking the probe, that the prolog has added 1 to `current`'s count. The simulator runs no prolog. So deleting the macro alone would report a `current` that has disabled migration once as free to migrate.

K02: `scx_lib_init_probe`, from `common.bpf.h`.
- Production: a `__weak` `SEC("fentry/bpf_scx_reg")` program. It fires while the scheduler registers, before `ops.init`, under the same non-sleepable prolog. Where the field exists, it sets `__scx_prolog_disables_migration = md > 0` from `current->migration_disabled`, and warns through `bpf_printk` when `md > 1`. A cid-form scheduler repoints it at `bpf_scx_reg_cid()`.
- Simulator: never runs, so the flag keeps its default of false.
- Effect: doubly inert today. The flag's only reader is the inline that S03 replaces, and even the real inline reads it only on kernels before 6.18 without `CONFIG_PREEMPT_RCU`, which is not the default kconfig.

S06: `bpf_in_hardirq()`, `bpf_in_serving_softirq()` and `bpf_in_nmi()`, from `bpf_experimental.h`.
- Production: decode `preempt_count`, read through `get_preempt_count()`.
- Simulator: `schedulers/lavd/wrapper.c` maps them to `sim_bpf_in_*` and defines `get_preempt_count()` as `sim_bpf_in_hardirq() ? 0x10000 : 0`. `bpf_in_nmi` is always 0.
- Effect: correct for the IRQ contexts the simulator models. NMI context is not represented, and neither are the other preempt_count fields.

FIX DIRECTION

- S02: delete the macro. The simulator already exports `scx_bpf_dsq_peek`, so the inline's `bpf_ksym_exists()` check picks the kfunc path. A test build that hides the kfunc could also exercise the iterator fallback against the simulator's `bpf_iter_scx_dsq`.
- S03 and K02:
  - mirror the simulator's migration-disabled count into `p->migration_disabled`;
  - make CO-RE field existence answer truthfully for fields the simulated `task_struct` has (sim-bid6t, verdict K09);
  - while a non-sleepable program runs, add the prolog's 1 to `current`'s count wherever the selected kconfig says the inline expects it, so the inline and the probe both read what production has;
  - run `scx_lib_init_probe` at scheduler registration, where the fentry fires;
  - then delete both macros.
- S06: keep a per-CPU preempt_count in the simulator, with the HARDIRQ, SOFTIRQ and NMI offset bits. Expose it where the production `get_preempt_count()` reads it, then delete the lavd overrides of `get_preempt_count` and `bpf_in_*`.

ACCEPTANCE

- No simulator-side definition of `__COMPAT_scx_bpf_dsq_peek`, `is_migration_disabled`, `get_preempt_count` or `bpf_in_*` remains.
- A coverage re-measure shows these as compiled header code, not as STUBBED rows, and shows `scx_lib_init_probe` as COVERED.
- The existing migration-disabled tests (sim-7cc89) and IRQ-context tests (sim-e4644) pass through the real inlines, at the default kconfig and with `KernelConfig { preempt_rcu: Some(true), .. }`.
- Until this is fixed, each site carries a DANGER TODO naming this issue. sim-io5ng tracks that.

Found by the 2026-09-24 scx pin bump coverage re-measure (sched-test 24d864c6, scx 413031d44). See verdict rows S02, S03, S06 and K02 in coverage/scx_pin_bump_20260924/verdicts.tsv in the dev harness (rrnewton/dev-sched-test).
