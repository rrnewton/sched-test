---
title: Only one simulation may run per process, and nothing in the library enforces it (SIM_LOCK is a caller contract)
status: open
priority: 1
issue_type: bug
labels:
- cratesio
created_at: 2026-09-25T00:19:31.017698933+00:00
updated_at: 2026-09-25T03:41:10.602802082+00:00
---

# Description

SIM_LOCK's own doc: the compiled C scheduler has global mutable state, so only one simulation can run at a time within a process, and callers must hold SIM_LOCK. The library never takes it:
- DynamicScheduler::try_load_with_definition does not;
- Simulator::run does not;
- ArenaTenant is a live-scheduler counter (ARENA_TENANTS; the first tenant releases the arena), not a lock.

The host static libs (the arena, the map registry, the kfunc substrate) are process-global and shared by every loaded .so. So two concurrent runs share them even when the .so differ.

An in-process embedder that runs two scenarios on two threads, as ktstr's test harness would under cargo test, gets whatever interleaving the globals produce, with no error. Not measured here. This rests on SIM_LOCK's doc and the shared-globals structure.

Fix: turn the silent race into a wait or an error. Either take a process-wide lock inside run (or for the DynamicScheduler's lifetime), or refuse a second concurrent load with a LoadError.

Until then, ai_docs/ktstr_scxsim_embed_contract.md section 8 documents it as a caller obligation.
