---
title: 'repm: ''repm analyze --score'' cannot cross-match runs with different mode names'
status: open
priority: 2
issue_type: bug
labels:
- post-1.0
created_at: 2026-04-27T10:57:22.576465277+00:00
updated_at: 2026-04-27T10:57:22.576465277+00:00
---

# Description

Source: rcpipe rc-test-full-pipeline (2026-04-27).

Symptom: 'repm analyze --score' fails to cross-match runs from different modes when the canonical names differ slightly (e.g. purerust_pinned/LAVD vs rtapp_sim/lavd). Comparison must be done manually.

Action: normalize mode names (canonical-mode-name lookup table per project memory: rtapp_pinned, purerust_pinned, rtapp_vm, purerust_vm, rtapp_sim, production), and case-fold scheduler names. Or accept an explicit --pair flag.

Verify: 'repm analyze --score --left purerust_pinned/LAVD --right rtapp_sim/LAVD' completes and produces a comparison table.

Classification: post-1.0-candidate.
Migrated from tg bug-repm-analyze-score-cross-match.
