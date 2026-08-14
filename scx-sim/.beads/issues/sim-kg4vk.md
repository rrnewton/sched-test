---
title: mitosis tests reference debug_events, deleted upstream by df131b98
status: open
priority: 2
issue_type: task
created_at: 2026-08-12T13:53:03.180542803+00:00
updated_at: 2026-08-12T13:53:03.180542803+00:00
---

# Description

Upstream commit df131b98 ('scx_mitosis: Remove debug events', 2026-08-02) deleted the debug_events ARRAY map, struct debug_event, DEBUG_EVENTS_BUF_SIZE, and the debug_events_enabled global from scx_mitosis (intf.h -27, mitosis.bpf.c -125, main.rs -6). Zero references remain upstream.

Found during the scx submodule sync 59c30ba -> upstream c630d994 (tg catchup-sync-upstream-scx).

Wrapper side (FIXED in that sync): scx-sim/schedulers/mitosis/wrapper.c mirrored the map with a static debug_events_arr[DEBUG_EVENTS_BUF_SIZE], routed lookups to it, memset it in mitosis_setup(), and set debug_events_enabled=false. All four removed, matching the existing precedent comment in that file for the 0f579b78/b62f1bae map removals.

Test side (needs an owner decision): crates/scx_simulator/tests/mitosis.rs had 3 ACTIVE tests panicking with 'symbol debug_events_enabled not found':
  - test_debug_events_enabled  -> subject deleted upstream; marked #[ignore] pointing here
  - test_many_debug_events     -> subject deleted upstream; marked #[ignore] pointing here
  - test_dump_cpumask_many_cpus -> NOT a debug-events test. Its subject is dump_cpumask's >32-CPU comma-separator branch, which SURVIVES upstream untouched. The debug_events_enabled=true line was incidental setup, so it was simply deleted and the test stays ACTIVE.
(4 other tests referencing debug_events_enabled were already #[ignore]d under mb sim-c923d6 for the earlier cell-allocator removal, so they did not fail.)

DECISION NEEDED: unlike sim-c923d6 ('re-enable after rewrite'), debug events are gone from upstream permanently -- there is nothing to re-enable. The two #[ignore]d tests should most likely be DELETED rather than rewritten, or re-pointed at whatever coverage they were really buying. Left as #[ignore] rather than deleted so the call stays with the project owner.
