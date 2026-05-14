---
title: 'bug1_canonical CI failure root cause (V4-C-confirmed: NOT clang ABI; NEW: engine→library disconnect on Ubuntu 24.04 runner)'
status: open
priority: 1
issue_type: bug
labels:
- ci
- cgroup-bw
- cpu-bw-stall-bug
created_at: 2026-05-14T02:38:40.620846939+00:00
updated_at: 2026-05-14T02:38:40.620846939+00:00
---

# Description

Phase 2 root-cause investigation. See tg `fix_bug1_canonical_test_2` for full context. KEY FINDING from PR #45 V4-C-cascade attempt: V4-C does NOT subsume the issue. CI still shows `is_throttled=0, runtime_total_sloppy=0` BUT engine instrumentation shows it IS calling consume with ~100M ns/period. So the engine→library accounting handshake is broken on the CI runner (clang/llvm/libelf codegen suspect). Tests gated under `#[ignore]`: bug1_canonical_subprocess (PR #40), bug1_canonical_consume_ns_bound + _zero (PR #45). Phase 2 work: Ubuntu 24.04 container repro + clang/libelf bisect, with specific focus on the consume_ns vs runtime_total disconnect.
