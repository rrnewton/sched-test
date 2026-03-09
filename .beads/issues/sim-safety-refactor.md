---
title: 'Safety refactor: Arc<Mutex<SimState>> + fold all shared state'
status: open
priority: 1
issue_type: task
created_at: 2026-03-09T18:28:04.468392808+00:00
updated_at: 2026-03-09T18:28:04.468392808+00:00
---

# Description

Move all shared simulator state behind a single Arc<Mutex<SimState>>. Eliminates SendPtr, raw pointer thread-locals, and CGROUP_REGISTRY AtomicPtr.
