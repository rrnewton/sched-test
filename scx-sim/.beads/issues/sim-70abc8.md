---
title: Determinism failures persist across all schedulers and modes
status: open
priority: 1
issue_type: bug
depends_on:
  sim-e0791: related
created_at: 2026-03-14T17:15:02.657643074+00:00
updated_at: 2026-03-21T19:09:42.264176917+00:00
closed_at: 2026-03-21T01:00:53.545184437+00:00
---

# Description

RBC determinism failures — simulator bug, NOT hardware limitation.

CORRECTED UNDERSTANDING:
PMU RBC (Retired Branch Conditional) counters ARE deterministic for a given
sequential instruction stream. This is the foundational principle of Mozilla RR
and Hermit. If we observe nondeterministic RBC counts, it is OUR BUG — not
hardware drift or microarchitectural nondeterminism.

PMU SIGNAL delivery has skid (the signal arrives a few instructions late), but
the COUNTER VALUE itself is exact. These are two different things:
- Counter reads: DETERMINISTIC (same instruction stream → same count)
- Signal delivery point: NONDETERMINISTIC (skid, OS scheduling delays)

PREVIOUS (WRONG) FINDINGS:
The earlier investigation incorrectly attributed RBC mismatches to 'inherent
hardware nondeterminism.' The actual root cause is unknown and needs
investigation. Possible causes:
- Shared-library init code running different paths between runs
- Signal handler code contributing unexpected branches
- Kernel-injected code (vDSO, context switches) adding branches
- Counter not being reset/read at exactly the right point
- Thread scheduling differences causing different interleaving

EVIDENCE TO RE-EXAMINE:
- ~10% failure rate with RBC enabled, 0% with --no-rbc
- Small RBC deltas (1-8 branches) — consistent with a small amount of
  non-deterministic code running (e.g. signal handler paths, futex retries)
- All other fields match (RIP, memory hash, event type, CPU)

INVESTIGATION PLAN:
1. Run stress tests with PMU-measured RBC, capture failing cases
2. Compare exact RBC values at divergence points
3. Determine whether the extra branches come from signal handlers,
   kernel code, or simulator infrastructure
4. Fix the root cause — RBC MUST be deterministic
