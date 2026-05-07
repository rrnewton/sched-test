---
title: 'scx-sim: degenerate latency distributions — avg=p50=p99 identical, no tail modeling'
status: open
priority: 2
issue_type: bug
labels:
- post-1.0
- fidelity
created_at: 2026-04-27T10:46:36.757648793+00:00
updated_at: 2026-04-27T10:46:36.757648793+00:00
---

# Description

Source: rcmode cross-mode consistency test (2026-04-27).
Reference: ~/work/multi_sched-test/ai_docs/RC_TEST_CROSS_MODE_20260427.md

All sim latency percentiles (avg / p50 / p99) come back IDENTICAL (e.g. all 1087µs). The pinned baseline shows real distribution: avg 1109 / p50 1343 / p99 1845. The sim is not modeling tail latency at all — it appears to emit a single deterministic value per-task.

Action:
1. Reproduce with rcmode's rt-app spec.
2. Trace where the latency samples are collected. Are they actually being sampled per scheduling event, or is some aggregation flattening the distribution?
3. If the sim's task model is too deterministic to produce a distribution, identify what missing source of variance to add (cache effects, scheduler nondeterminism, etc.). NOTE: this must NOT break cooperative-mode determinism per our determinism contract.
4. Fix.

Verify: Sim percentiles span a real distribution; ratio to pinned within reasonable bounds (at minimum p99 != p50).
