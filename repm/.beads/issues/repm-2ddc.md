---
title: 'repm: gen-config emits invalid JSON'
status: open
priority: 2
issue_type: bug
created_at: 2026-04-27T10:53:59.105403162+00:00
updated_at: 2026-04-27T10:53:59.105403162+00:00
---

# Description

Source: rcmode (2026-04-27).
Reference: ~/work/multi_sched-test/ai_docs/RC_TEST_CROSS_MODE_20260427.md

'repm gen-config' produces a file that downstream tools reject as invalid JSON.

Action: locate gen-config, identify what's malformed (trailing comma, missing quotes, comments?), fix to emit strict JSON.

Verify: 'repm gen-config' output passes 'jq .' without error.

Migrated from scx-sim/.beads/ sim-30f921 (was misfiled under scx-sim).
