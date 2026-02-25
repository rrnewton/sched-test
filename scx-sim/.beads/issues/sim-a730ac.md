---
title: Native concurrency backend for external determinism tools
status: in_progress
priority: 0
issue_type: feature
created_at: 2026-02-25T18:18:30.570337895+00:00
updated_at: 2026-02-25T18:18:37.709678880+00:00
---

# Description

Add an alternative backend where worker threads run truly concurrently,
enabling use of external determinism/replay tools (hermit, rr).

Plan: ai_docs/concurrent_backend_plan.md

Phase 1: Refactoring (7 steps, tests pass throughout)
Phase 2: New backend implementation (7 steps)

Key design: window-based clock throttling limits how far CPUs can race
ahead of each other. ThreadOrchestrator trait decouples synchronization
strategy from PreemptionBackend trait.
