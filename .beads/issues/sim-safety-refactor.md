---
title: 'Safety refactor: Arc<Mutex<SimState>> + fold all shared state'
status: closed
priority: 1
issue_type: task
created_at: 2026-03-09T18:28:04.468392808+00:00
updated_at: 2026-03-10T12:22:11.021861520+00:00
closed_at: 2026-03-10T12:22:11.021861430+00:00
---

# Description

Complete. Eliminated CGROUP_REGISTRY AtomicPtr, 3 of 4 SendPtrs, raw pointer context save/restore in signal handler and yield paths. 31 handler methods converted to SimState. Phase 3c (Arc<Mutex<>>) deferred as marginal benefit over current architecture.
