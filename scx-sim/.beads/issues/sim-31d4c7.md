---
title: 'Mitosis: 7 config-gated functions coverable-but-untested (slice-shrinking + multi-CPU-pinned DSQ path)'
status: open
priority: 2
issue_type: task
depends_on:
  sim-010a1: related
created_at: 2026-07-23T21:11:58.138553997+00:00
updated_at: 2026-07-23T21:11:58.138553997+00:00
---

# Description

Mitosis function coverage is 54/93=58.1% (integration@7d2d7f2, see ai_docs/MITOSIS_COVERAGE_20260723.md). Of 39 dark functions, 32 are substrate-blocked (LLC subsystem, apply_cell_config path, fentry/tp hooks) but 7 are CONFIG-GATED and coverable TODAY with a knob + workload:

B1 slice_shrinking.bpf.h (4): slice_shrink_apply, slice_shrink_limit, slice_shrink_on_enqueue, slice_shrink_on_running. Gated by enable_slice_shrinking global (default false, slice_shrinking.bpf.h:71) + a partially-pinned task (!all_cell_cpus_allowed, mitosis.bpf.c:877). Cover by setting the global via get_symbol + a task with allowed_cpus = strict subset.

B2 multi-CPU-pinned DSQ path (3): select_pinned_cpu, enqueue_pinned_cpu, update_pinned_dsq. select_pinned_cpu fires only when task is multi-CPU pinned (!all_cell_cpus_allowed && cpumask_weight>1, mitosis.bpf.c:670-679) AND dynamic_affinity_cpu_selection=true (default false, mitosis.bpf.c:51). Existing pinned tests use single-CPU pins (weight==1). Cover with a 2+ CPU allowed_cpus subset + dynamic_affinity_cpu_selection enabled.

Closing these raises mitosis coverage 58.1% -> 61/93=65.6% (the substrate ceiling). Test opportunity, not a substrate block.
