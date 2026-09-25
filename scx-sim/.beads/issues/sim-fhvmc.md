---
title: 'scxsim mitosis: __free() and no_free_ptr() are redefined, so cleanup.bpf.h''s scoped releases are never compiled'
status: open
priority: 3
issue_type: task
labels:
- no-stub
- mitosis
depends_on:
  sim-ed0d6: related
created_at: 2026-09-25T05:18:55.263129089+00:00
updated_at: 2026-09-25T05:21:26.460938177+00:00
---

# Description

DEFECT

`schedulers/mitosis/wrapper.c` replaces both halves of `scx/scheds/include/lib/cleanup.bpf.h`'s scoped-release pair:
- `#undef __free` / `#define __free(x)`, under the comment "Strip __free() cleanup attributes - simulator manages resources manually";
- `#undef no_free_ptr` / `#define no_free_ptr(p) (p)`, under "no_free_ptr just returns the pointer unchanged".

So no release that mitosis attaches to a local is ever compiled: `bpf_cpumask_release`, `bpf_cgroup_release` and `scx_bpf_put_idle_cpumask`.

PRODUCTION (scx 413031d44)

- `__free(name)` attaches `__attribute__((__cleanup__(__free_##name)))`. The `DEFINE_FREE` release runs when the local goes out of scope, on every return path, and skips NULL (for example `DEFINE_FREE(bpf_cpumask, ..., if (_T) bpf_cpumask_release(_T))`).
- `no_free_ptr(p)` returns the pointer and sets the variable to NULL, so the scoped release does nothing when ownership is handed on. mitosis uses it where it publishes a pointer: `publish_prepared_cpumask` and `init_cpumask_slot` in `cell_cpumask.bpf.h`, and the `root_cgrp` exchange in `mitosis.bpf.c`.

SIMULATOR (sched-test 24d864c6)

- The attribute is removed, so no release call is made.
- `no_free_ptr` leaves the variable set. No mitosis code reads a variable after `no_free_ptr`, so this is not observable today.
- The releases would do nothing anyway:
  - `bpf_cpumask_release` (`crates/scx_simulator/csrc/sim_bpf_stubs.c`) calls `sim_arena_free`, which `csrc/sim_arena.h` documents as "Free is a no-op — the arena is bulk-reset between simulation runs";
  - `bpf_cgroup_release` is an empty function in both `sim_bpf_stubs.c` (weak) and `src/unsafe_impl/kfuncs.rs`;
  - `scx_bpf_put_idle_cpumask` is empty in `kfuncs.rs`.

CONSEQUENCE

Nothing observable changes today (EQUIVALENT-TODAY). But the day a release gains a body, for example reference counting on cgroups or cpumasks, or leak checking at scheduler exit, mitosis's releases will be skipped silently. So the simulator can never catch a mitosis leak or a double release that production would have. This breaks the No-Stub rule, because it is a no-op shim.

FIX DIRECTION

Delete both overrides in the same change, so that cleanup.bpf.h's `DEFINE_FREE` releases and its `no_free_ptr` compile against the simulator's kfuncs. Deleting only `__free` would be worse than today: with `no_free_ptr` still a plain read, every pointer mitosis publishes would also be released at scope exit.

If something stopped the header compiling when the overrides were added (sim-ed0d6), fix that in the simulator, not by stripping the attribute.

ACCEPTANCE

- `grep -n '__free\|no_free_ptr' schedulers/mitosis/wrapper.c` finds no redefinition.
- A mitosis run balances its acquires against its releases: a counter test on `bpf_cgroup_release` and `bpf_cpumask_release`, which also shows that no published pointer is released.
- Until this is fixed, the overrides carry a DANGER TODO naming this issue. sim-io5ng tracks that.

Found by the 2026-09-24 scx pin bump coverage re-measure (sched-test 24d864c6, scx 413031d44). See verdict row S12 in coverage/scx_pin_bump_20260924/verdicts.tsv in the dev harness (rrnewton/dev-sched-test).
