---
title: bug1_canonical_subprocess test fails on CI but passes locally — root cause unknown
status: open
priority: 1
issue_type: bug
labels:
- ci
- cgroup-bw
- cpu-bw-stall-bug
created_at: 2026-05-14T00:18:08.519419047+00:00
updated_at: 2026-05-14T00:18:08.519419047+00:00
---

# Description

Phase 2 of tg `fix_bug1_canonical_test`: investigate why `test_bug1_canonical_subprocess_reproduces_throttle` (and `_deterministic_10_reps`) fail on the GitHub Actions `scx_simulator` workflow but pass 100% locally.

CI fingerprint: `{exit_code:0, is_throttled:0, nr_throttled_periods:'0/6', nr_throttled_tasks:0}` — runtime_total_sloppy=0 means engine charges ZERO ns of work to the cgroup over 600ms simulated time. Library is loaded + reports normally.

Local: 100% pass, fingerprint `{is_throttled:1, nr_throttled_periods:'5/6', nr_throttled_tasks:16}`.

Phase 1 (in-flight): the two tests are gated behind `#[ignore]` (this issue's TODO marker) so the cascade can land. Phase 1 PR will reference this issue ID.

Phase 2 plan (this issue):
1. Reproduce locally in an Ubuntu 24.04 container with apt-installed `clang llvm libelf-dev build-essential xxd markdown zlib1g-dev`.
2. Bisect along: clang/llvm version, libelf-dev version, kernel-headers version, cargo features.
3. Once root-caused, either pin the affected toolchain in `.github/workflows/simulator.yml` or add a workaround in `build.rs` / `schedulers/Makefile`.

Reference CI run: https://github.com/facebookexperimental/sched-test/actions/runs/25832704071
Reference investigation: tg `investigate-test-bug1-canonical-subprocess-reproduces-throttle-regression` (closed)
