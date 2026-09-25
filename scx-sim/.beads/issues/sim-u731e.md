---
title: The simulated CSS iterator silently drops cgroups beyond 2048, while a scenario may create 10000
status: open
priority: 2
issue_type: bug
labels:
- cratesio
created_at: 2026-09-25T03:43:12.622789836+00:00
updated_at: 2026-09-25T03:43:12.622789836+00:00
---

# Description

A scenario may create up to 10000 cgroups, but the simulated CSS iterator holds 2048 and silently drops the rest. A BPF scheduler that walks the hierarchy with `bpf_for_each(css, ...)` then never sees the dropped cgroups. Nothing reports it.

From reading the code on the release-candidate branch (integration 24d864c6). This was not run.

- `Scenario` defaults `max_cgroups` to `DEFAULT_MAX_CGROUPS`, which is 10000 (safe/cgroup.rs). The engine passes it to `CgroupRegistry::new`, so a scenario with more than 2048 cgroups is accepted.
- Before each walk, the engine calls `CgroupRegistry::prepare_css_iter_from_root`. That collects every descendant in pre-order and post-order and hands both lists to the C side.
- csrc/sim_cgroup.c keeps those lists in two static arrays of `MAX_CSS_ITER_CGROUPS` (2048) entries. `sim_css_iter_add` and `sim_css_iter_add_post` store an entry only while the count is under 2048. Beyond that they return without storing anything, and there is no error path.
- The doc comment on the pub `prepare_css_iter` already says cgroups beyond that capacity 'are silently dropped' and suggests bumping the constant.
- Upstream's lib/cgroup_bw.bpf.c caps its own managed set at `CBW_NR_CGRP_MAX` = 2048, so schedulers built on cgroup_bw hit their own limit at the same point. Schedulers that walk css without cgroup_bw have no such cap, so the simulator drops cgroups the real kernel would visit.

For an external consumer the result is silently wrong: the run ends normally, and the scheduler walked a truncated tree.

Fix: either refuse a scenario whose cgroup count exceeds the iterator capacity, at `Scenario` build time or in `CgroupRegistry::create`, or size the arrays from the registry's `max_cgroups`. Add a test that creates 2049 cgroups and checks that a css walk visits all of them, or that the scenario is refused.

Found while preparing the crates.io release candidate (tg scxsim-cratesio-release-candidate).
