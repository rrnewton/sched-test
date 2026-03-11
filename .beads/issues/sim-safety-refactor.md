---
title: 'Safety refactor: Arc<Mutex<SimState>> + fold all shared state'
status: closed
priority: 1
issue_type: task
created_at: 2026-03-09T18:28:04.468392808+00:00
updated_at: 2026-03-11T14:20:45.903665991+00:00
closed_at: 2026-03-11T14:20:45.903665841+00:00
---

# Description

Move all shared simulator state behind a single Arc<Mutex<SimState>>. Eliminates SendPtr, raw pointer thread-locals, and CGROUP_REGISTRY AtomicPtr. Phases 1a/1b/3a/2a/2b complete and ported to safety-refactor-v2. All 150+ tests pass.
