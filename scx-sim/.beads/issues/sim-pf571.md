---
title: 'scxsim: migrate cosmos wrapper off hand-written scx_pmu_* onto the real scx/lib/pmu.bpf.c'
status: open
priority: 2
issue_type: task
created_at: 2026-08-12T15:27:52.236083327+00:00
updated_at: 2026-08-12T15:27:52.236083327+00:00
---

# Description

schedulers/cosmos/wrapper.c hand-writes five scx_pmu_* function bodies
(scx_pmu_install / _uninstall / _task_init / _task_fini / _event_start /
_event_stop / _read, roughly wrapper.c:155-240) that simulate PMU counters
with scx_bpf_now() deltas.

That is the "elided library" antipattern scx-sim/CLAUDE.md's No-Stub Rule
forbids: the real scx/lib/pmu.bpf.c is not linked, and a Rust/C-side
approximation stands in for it with the same interface. The counters cosmos
reads are a guess at what the library would produce, not the library.

The layered wrapper (tg layered-support-implement) established the correct
pattern: compile scx/lib/pmu.bpf.c into the .so and supply only the genuine
hardware primitive underneath it, bpf_perf_event_read_value(). See
schedulers/layered/wrapper.c (the "#define _license _scx_pmu_license"
include block, and layered_perf_event_read_value).

Work:
1. Include lib/pmu.bpf.c in cosmos's wrapper the same way (the _license
   rename is needed because SEC() is a no-op in the sim build, so two
   license arrays collide in one translation unit).
2. Route scx_pmu_tasks (TASK_STORAGE) through cosmos's map layer.
3. Delete the hand-written bodies.
4. Decide what bpf_perf_event_read_value() should report for cosmos. The
   layered wrapper returns -ENOENT (honest: scxsim models CPU time, not
   microarchitecture). Cosmos's existing tests depend on the fake counter
   producing runtime-proportional values, so this needs care — either those
   tests change, or scxsim grows a real simulated PMU (a separate, larger
   piece of substrate).
