---
title: 'Mitosis: rewrite timer/reconfig tests for userspace-only cell-control (upstream removed BPF cell allocator)'
status: open
priority: 1
issue_type: task
created_at: 2026-07-23T00:58:27.562106119+00:00
updated_at: 2026-07-23T00:58:27.562106119+00:00
---

# Description

Upstream scx commits 0f579b78 'delete legacy BPF cell allocator' and b62f1bae 'make userspace the only cell-control path' removed the BPF-side reconfiguration mechanism: the update_timer ARRAY map, cgrp_init_percpu_cpumask PERCPU_ARRAY map, and the userspace-writable configuration_seq global (only applied_configuration_seq remains). mitosis no longer uses bpf_timer. 9 scxsim tests drive the deleted path (write configuration_seq via FFI + fire mitosis timer) and were #[ignore]'d during the July 2026 scx upstream sync (tg update-scx-submod-july): test_timer_reconfiguration_path, test_debug_events_with_timer_reconfig, test_smt_pinned_timer_reconfig, test_overloaded_all_features, test_cpu_controller_enabled_timer_reconfig, test_all_features_cpu_controller_enabled, test_staggered_many_tasks_timer, test_timer_many_reconfigurations, test_large_cpu_count_all_features. TODO: rewrite for userspace-only cell-control model. Relates to sim-010a1.
