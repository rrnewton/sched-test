---
title: 'repm: cannot find rt-app when invoked under sudo'
status: closed
priority: 2
issue_type: bug
labels:
- post-1.0
- repm
created_at: 2026-04-27T10:46:39.301590778+00:00
updated_at: 2026-04-27T10:54:08.373114752+00:00
closed_at: 2026-04-27T10:54:07.335758105+00:00
---

# Description

Source: rcmode (2026-04-27).
Reference: ~/work/multi_sched-test/ai_docs/RC_TEST_CROSS_MODE_20260427.md

When repm needs to invoke rt-app under sudo, it fails to find the binary because sudo strips PATH.

Action: either (a) absolute-path the rt-app invocation, (b) use 'sudo -E' or 'sudo env "PATH=\$PATH"', or (c) probe a conventional install path. Choose the cleanest fix.

Verify: 'sudo repm run ...' (or whatever the failing flow is) finds rt-app.

CLOSED: Migrated to repm/.beads/ as repm-cf7a.
