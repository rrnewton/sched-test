---
title: 'scxsim: select_cpu wake_flags omit SCX_WAKE_TTWU (cosmos is_cpu_faster/cpus_share_cache unreachable)'
status: open
priority: 2
issue_type: bug
created_at: 2026-07-23T03:39:12.904055891+00:00
updated_at: 2026-07-23T03:39:12.904055891+00:00
---

# Description

engine.rs ~L3534 builds wake_flags for ops.select_cpu() as SCX_ENQ_WAKEUP(0x1)[|SCX_WAKE_SYNC(0x10)]. These are ENQ-namespace flags; select_cpu expects SCX_WAKE_*. SCX_WAKE_TTWU(0x8) is never set, so cosmos is_wakeup(wake_flags) is always false, making the wakeup migration branch in pick_idle_cpu() (main.bpf.c ~L858) dead. Result: is_cpu_faster()/cpus_share_cache() stay 0% coverage even with heterogeneous cpu_capacity (tg write-cosmos-tests; COVERAGE_AUDIT_20260722.md 4.2). Real kernel sets SCX_WAKE_TTWU on ttwu wakeups. Fix: deliver correct SCX_WAKE_* (incl TTWU) to select_cpu on genuine wakeups. NOTE: shared-engine change affecting all schedulers' select_cpu wake_flags; needs cross-scheduler validation. Deferred from cosmos-tests to avoid touching shared engine during concurrent work.
