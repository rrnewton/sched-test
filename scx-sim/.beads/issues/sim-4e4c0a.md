---
title: 'scxsim: nested cgroup CPU-bw throttle not enforced (bpf_cgroup_ancestor NULL stub -> parent_id=0)'
status: open
priority: 2
issue_type: task
created_at: 2026-07-23T04:25:44.610397571+00:00
updated_at: 2026-07-23T04:25:44.610397571+00:00
---

# Description

Under scxsim, cgroup CPU-bandwidth THROTTLE is enforced only for ROOT-level (level==1) cgroups. Nested cgroups (level>1) are never throttled: the sim substrate stubs bpf_cgroup_ancestor() to return NULL (csrc/sim_bpf_stubs.c:323). In scx/lib/cgroup_bw.bpf.c scx_cgroup_bw_init (~1146-1152): cgx->parent_id is set from bpf_cgroup_ancestor(); since the stub returns NULL, EVERY cgroup gets parent_id=0. Then cbw_update_nquota_ub (~1089) for any level>1 cgroup does cbw_get_cgroup_ctx_with_id(0) -> NULL -> logs 'Fail to lookup parent ctx: 0' -> -ESRCH -> nquota_ub stays = own nquota (INF for unlimited nested child), so an ancestor's quota never propagates down and the nested cgroup never throttles. IMPACT (tg test-cgroup-bw-throttle-unthrottle): genuine nested-hierarchy throttle enforcement cannot be tested. Root-level throttle/unthrottle IS fully testable (crates/scx_simulator/tests/cgroup_bw_throttle_cycle.rs items 1-4,6). Item 5 delivered as (a) a passing throttle-ISOLATION test (level-1 limited cgroup coexisting with a nested unlimited hierarchy) and (b) an #[ignore]d executable spec (test_nested_ancestor_limit_throttles_child) that flips green once fixed. FIX: model bpf_cgroup_ancestor() in the sim substrate so cgroup_bw can cache real parent_id and propagate nquota_ub down. Filed during tg test-cgroup-bw-throttle-unthrottle.
