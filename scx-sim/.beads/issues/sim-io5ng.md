---
title: 'scx-sim No-Stub: DANGER TODO markers are missing at every stub/shim site the 2026-09-24 coverage re-measure found'
status: open
priority: 1
issue_type: task
labels:
- no-stub
depends_on:
  sim-qwk14: related
  sim-jxtox: related
  sim-s05j1: related
  sim-heygr: related
  sim-98z2g: related
  sim-yganw: related
  sim-fhvmc: related
  sim-439319: related
  sim-bid6t: related
  sim-6f5g3: related
  sim-d9988: related
  sim-z7swq: related
created_at: 2026-09-25T05:19:19.851422786+00:00
updated_at: 2026-09-25T05:19:19.851422786+00:00
---

# Description

DEFECT

scx-sim/CLAUDE.md asks for a marker at every departure from production that remains:
- the No-Stub rule: "When a temporary deviation from real scheduler logic is unavoidable during development, mark it with `DANGER TODO(<issue>)` in the code ... and treat the scheduler as unsupported until the TODO is resolved."
- Kernel Fidelity: "When a shortcut is unavoidable (e.g. because we haven't yet modeled the requisite state), mark it with `DANGER TODO(<issue>)` in the code."
- the Reviewer Rule refuses changes that "introduce silent divergence from production behavior, with no `DANGER TODO(<issue>)` marker and no tg task tracking the debt".

The 2026-09-24 re-measure gave every departure from production a verdict. 18 of its 36 rows owe a marker, and none of the 18 has one at its sites. The only rows with a marker are S07 (sim-0z6u0) and K05 (sim-6mheb).

The list below is by row, site and issue. Unless noted, paths are relative to scx-sim/.

- S01, `__COMPAT_is_enq_cpu_selected` shim, in `crates/scx_simulator/csrc/sim_wrapper.h` and its duplicate in `schedulers/layered/wrapper.c`. Issue: sim-heygr.
- S02, S03 and S06. Issue: sim-yganw.
  - `sim_wrapper.h`: `__COMPAT_scx_bpf_dsq_peek` and `is_migration_disabled`;
  - the layered wrapper's duplicate `is_migration_disabled`;
  - `schedulers/lavd/wrapper.c`: `get_preempt_count` and `bpf_in_hardirq`/`bpf_in_serving_softirq`/`bpf_in_nmi`.
- S04 and S05, `scx_clock_task` and `scx_clock_pelt` in the lavd wrapper. Issue: sim-jxtox.
- S08, `crates/scx_simulator/csrc/sim_atq.c` and the lavd wrapper's `scx_atq_lock`/`scx_atq_unlock` and `arena_spin_lock`/`arena_spin_unlock`, with the same lock no-ops in `crates/scx_simulator/scxtest/overrides.h`. Issue: sim-98z2g.
- S09, S10, S11 and S14, the arena and sdt substitutions: `sim_sdt_stubs.c` (the task-storage hash and an empty `scx_arena_subprog_init`), and the lavd wrapper's `scx_static_alloc` → `sim_arena_calloc`. Issue: sim-d9988.
- S12, the `__free` and `no_free_ptr` overrides in `schedulers/mitosis/wrapper.c`. Issue: sim-fhvmc.
- K01, `bpf_get_prandom_u32` in `crates/scx_simulator/scxtest/overrides.h`. Issue: sim-z7swq.
- K03, `bpf_per_cpu_ptr` in `sim_wrapper.h`. Issue: sim-439319.
- K06, `bpf_ringbuf_reserve`/`bpf_ringbuf_submit` in the lavd wrapper. Issue: sim-qwk14.
- K09, the CO-RE overrides:
  - `overrides.h`: `__builtin_preserve_field_info` and `__builtin_preserve_enum_value`;
  - `sim_wrapper.h`: `__builtin_preserve_field_info` and `__builtin_preserve_type_info`;
  - the lavd wrapper's `bpf_core_field_exists`.
  Issue: sim-bid6t.
- K10, `scx_bpf_error` in the lavd wrapper's cgroup_bw region. Issue: sim-s05j1.
- E02, `lavd_set_power_mode`, `lavd_set_autopilot` and `lavd_setup_multi_domain`'s `no_core_compaction` write in the lavd wrapper. Issue: sim-6f5g3.

FIX DIRECTION

Add `DANGER TODO(<issue id>)` at each site above, naming the issue listed for it. No behaviour changes.

A marker is a promise, not a fix. Each named issue stays open until its site is gone.

ACCEPTANCE

- Re-running `verdicts.py` from coverage/scx_pin_bump_20260924 against the tree reports no row as owing a marker: every "required; none" becomes "DANGER TODO(sim-…)".

Found by the 2026-09-24 scx pin bump coverage re-measure (sched-test 24d864c6, scx 413031d44). See the danger_todo column of coverage/scx_pin_bump_20260924/verdicts.tsv in the dev harness (rrnewton/dev-sched-test).
