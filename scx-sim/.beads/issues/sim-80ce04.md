---
title: 'Refactor PreemptionBackend: target-count arm(), is_precise(), read_count()'
status: open
priority: 2
issue_type: feature
labels:
- refactor
- backend
created_at: 2026-02-24T14:33:33.297176563+00:00
updated_at: 2026-02-24T15:05:58.398058276+00:00
---

# Description

## Motivation

The PreemptionBackend trait was designed around PMU-based recording where arm() rolls a random timeslice internally and programs the PMU timer. The ReplayBackend works around this interface with dummy PRNG calls, opposite counter semantics, and separate signal handler state.

## Amended Proposed Trait Design

### PreemptTarget with newtypes for RBC counting

Use newtypes to distinguish relative vs absolute RBC counts:

```rust
/// Relative RBC count — branches to execute from the current counter position.
/// Used by PmuBackend for random timeslices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelativeRbc(pub u64);

/// Absolute RBC count — cumulative branches from the start of the current structop.
/// Used by ReplayBackend for precise targeting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AbsoluteRbc(pub u64);

/// RBC target: either relative (from current position) or absolute (from structop start).
pub enum RbcTarget {
    Relative(RelativeRbc),
    Absolute(AbsoluteRbc),
}

/// Target for preemption.
pub struct PreemptTarget {
    /// RBC count to preempt at (relative or absolute).
    pub count_rbc: RbcTarget,
    /// For precise backends with a known target RIP (replay).
    /// None for first-time recording where we don't know where we'll stop.
    pub target_rip: Option<u64>,
}
```

### Updated trait methods

```rust
pub(crate) trait PreemptionBackend: Sync {
    type WorkerCtx: Send;

    // Lifecycle (unchanged)
    fn global_setup(&self) {}
    fn global_teardown(&self) {}
    fn worker_setup(&self, ring: &PreemptRing, worker_id: WorkerId) -> Self::WorkerCtx;
    fn worker_teardown(&self, ctx: Self::WorkerCtx);
    fn log_completion(&self, ring: &PreemptRing);

    // Arm with explicit RBC target
    fn arm(&self, ctx: &mut Self::WorkerCtx, target: PreemptTarget);
    fn disarm(&self, ctx: &mut Self::WorkerCtx) -> StructopDelta;

    // Precision query
    fn is_precise(&self) -> bool;

    // Counter operations
    fn read_count(&self, ctx: &Self::WorkerCtx) -> u64;
    fn reset_count(&self, ctx: &mut Self::WorkerCtx);
}
```

Key additions vs original plan:
- count_rbc is specific to RBC (not generic "events")
- target_rip is Option<u64> (None for recording, Some for replay)
- RelativeRbc vs AbsoluteRbc newtypes prevent mixing up counting modes
- reset_count() added — called at the start of each structop execution to reset the per-structop RBC counter

### Migration path (3 phases, unchanged)

Phase 1 (non-breaking): Add is_precise(), read_count(), reset_count() with defaults
Phase 2 (breaking): Change arm() signature to accept PreemptTarget with RbcTarget
Phase 3: Clean up cooperative yield rearm path, investigate rearm_timer strategy

### Open investigation: rearm_timer

The rearm_timer strategy (reset counter + set new period at kfunc boundaries) may not be working correctly in all modes. This needs separate investigation — the refactor should not depend on resolving it, but Phase 3 should address it.
