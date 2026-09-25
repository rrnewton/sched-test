---
title: 'scxsim: CO-RE queries answer a mix of constants no kernel produces — make field/type/enum queries answer from the simulated vmlinux.h'
status: open
priority: 2
issue_type: task
labels:
- kernel-fidelity
depends_on:
  sim-0qzq6: related
created_at: 2026-09-25T05:18:55.272537505+00:00
updated_at: 2026-09-25T05:21:26.492997052+00:00
---

# Description

DEFECT

Three layers of overrides answer CO-RE queries with constants, and they disagree with each other.

1. `crates/scx_simulator/scxtest/overrides.h` is included first, through scx_test.h. It defines `__builtin_preserve_field_info(x,y)` and `__builtin_preserve_enum_value(x,y)` as 1.
2. `crates/scx_simulator/csrc/sim_wrapper.h` then redefines `__builtin_preserve_field_info` and `__builtin_preserve_type_info` as 0. So `bpf_core_field_exists()` and `bpf_core_type_exists()` report absent. It also defines `bpf_core_type_matches` as 1 and `bpf_core_type_size` as `sizeof`.
3. From its cgroup_bw region onwards, `schedulers/lavd/wrapper.c` defines `bpf_core_field_exists(...)` as 1 again.

`bpf_core_enum_value()` therefore returns 1 for every enumerator, and `SCX_ENQ_IMMED` reads 0 (sim-0qzq6).

PRODUCTION

CO-RE builtins and load-time feature probes answer per field, per type and per enumerator, from the running kernel's BTF.

SIMULATOR (sched-test 24d864c6, scx 413031d44)

No real kernel produces this mix. The answer also depends on where in a translation unit the query sits.

CONSEQUENCE

Today this:
- selects the `___compat` fallbacks (verdict K12);
- makes `UEI_RECORD` skip copying `exit_code` and `exit_cpu`, which nothing in the simulator reads yet;
- hides the IMMED bounce-back (sim-0qzq6).

It also blocks two other fixes.
- `__COMPAT_is_enq_cpu_selected()` cannot be un-shimmed. With enum values folded to 1 it would test `SCX_ENQ_WAKEUP` (bit 0) instead of `SCX_ENQ_CPU_SELECTED` (1 << 20). See sim-heygr.
- `is_migration_disabled()` cannot be un-shimmed while field existence reads 0. See sim-yganw.

EQUIVALENT-TODAY only because each wrong answer happens to be patched around somewhere else.

FIX DIRECTION

Answer from the vmlinux.h the simulator compiles against. Every field, type and enumerator declared there exists, with its declared value.
- Field and type existence: 1 for anything that compiles. Keep a denylist for anything the simulator deliberately does not model.
- Enum value: the enumerator's own value. `bpf_core_enum_value(T, V)` expands to `__builtin_preserve_enum_value(*(typeof(T) *)V, kind)`. For the value query, `(unsigned long)&(x)` recovers V without dereferencing anything.
- Then drop the per-wrapper overrides, so one definition answers everywhere.

ACCEPTANCE

- One definition of each CO-RE builtin is used by every scheduler build.
- A unit test checks `bpf_core_enum_value(enum scx_enq_flags, SCX_ENQ_CPU_SELECTED) == 1 << 20`, and that field existence is true for `task_struct.migration_disabled`.
- `UEI_RECORD` copies `exit_code`.
- The scheduler suites still pass.
- Until this is fixed, the override sites carry a DANGER TODO naming this issue. sim-io5ng tracks that.

Found by the 2026-09-24 scx pin bump coverage re-measure (sched-test 24d864c6, scx 413031d44). See verdict row K09 in coverage/scx_pin_bump_20260924/verdicts.tsv in the dev harness (rrnewton/dev-sched-test).
