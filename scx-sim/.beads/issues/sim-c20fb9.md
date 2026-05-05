---
title: Centralize Dispatch/Context-Switch Logic
status: in_progress
priority: 2
issue_type: feature
created_at: 2026-03-24T18:03:56.275830929+00:00
updated_at: 2026-03-24T18:20:22.104728369+00:00
---

# Description

Centralize all dispatch and context-switch logic through a single EngineRing orchestrator.

## Context

The simulator currently has THREE separate thread-selection mechanisms:
1. **TokenRing** (cooperative): SmallRng PRNG, Mutex/Condvar
2. **PreemptRing** (preemptive): hand-rolled xorshift32 AtomicU32 PRNG, futex
3. **NativeOrchestrator**: Barrier, true parallel

The PMU signal handler (preempt/mod.rs:1976) calls ring.yield_token() directly,
picking the next thread via xorshift32 without engine involvement. The engine is
blocked on wait_all_done() during concurrent phases — it has zero scheduling
authority. Workers are spawned fresh each dispatch round via std::thread::scope.

**Problem**: Different PRNGs produce different interleavings that expose scheduler
bugs differently. The engine's min-logical-time event loop is bypassed inside
concurrent batches. Thread spawn/join overhead on every dispatch round.

**Goal**: One centralized dispatch loop. All yield paths (cooperative kfunc boundary,
PMU signal preemption, structop completion) return to the engine. Engine picks next
thread by min-local-clock. Persistent per-CPU threads.

**Completion criterion**: The xorshift32 PRNG in PreemptRing is deleted.

---

## Phase A: EngineRing — New Centralized Orchestrator

**Create** unsafe_impl/engine_ring.rs

An EngineRing that is 100% async-signal-safe (futex + atomics only):

- workers: Box<[AtomicU32]> — per-worker PARKED/RUNNING futex words
- engine_wake: AtomicU32 — workers wake engine here
- yielded_worker: AtomicU32 — which worker just yielded
- yield_reason: AtomicU32 — cooperative / preemption / finish
- finished_mask: AtomicU64
- worker_cpu_map: Box<[CpuId]> — WorkerId to CpuId

Protocol:
- Worker yields: stores yielded_worker + yield_reason, parks self, wakes engine
- Engine wakes: reads yielded info, charges sched time to yielded CPU,
  picks next worker by min-local-clock, wakes that worker, re-parks on engine_wake
- All async-signal-safe: signal handler uses the same yield_to_engine()

The decision logic lives on the engine side (not in the ring):
  pick_next_by_min_clock: Among non-finished workers, pick the one whose CPU
  has smallest local_clock. Ties broken by CpuId (deterministic).

Implements ThreadOrchestrator trait.

**Files**: new engine_ring.rs, modify backend/mod.rs (trait impl)

**Test**: Unit test with 3 workers, verify decision callback invoked, min-clock picker works.

---

## Phase B: Wire Cooperative Mode to EngineRing

Replace TokenRing usage in cooperative dispatch with EngineRing.

- run_cooperative_batch / run_cooperative_dispatch (backend/mod.rs):
  create EngineRing instead of TokenRing
- interleave::maybe_yield() (interleave.rs): the cooperative yield path
  calls engine_ring.yield_to_engine() instead of ring.yield_token()
- Unify INTERLEAVE_CTX TLS to point at EngineRing instead of TokenRing

At each yield, the engine:
1. Charges intermediate kfunc cost to the yielded CPU's local_clock
2. Picks next worker by min-local-clock
3. Wakes that worker

**Files**: interleave.rs, backend/mod.rs, engine.rs (cooperative dispatch paths)

**Test**: All existing cooperative interleave tests pass. New test verifying
min-clock ordering produces valid traces.

---

## Phase C: Wire Preemptive Mode to EngineRing (Delete xorshift32)

The critical phase. Modify the PMU signal handler to yield to the engine
instead of picking the next thread directly.

**Step C1**: Modify preempt_handler:
- Replace ring.yield_token(pctx.worker_id) with
  engine_ring.yield_to_engine(pctx.worker_id) with reason Preemption

**Step C2**: Modify cooperative_yield_impl:
- Replace ring.yield_token(ctx.worker_id) with
  engine_ring.yield_to_engine(ctx.worker_id) with reason Cooperative

**Step C3**: Unify TLS contexts:
- Add engine pointer to PreemptCtx and ReplayCtx

**Step C4**: Wire preemptive dispatch paths:
- run_preemptive_dispatch/batch: use EngineRing as orchestrator
  instead of PreemptRing

**Step C5**: Delete from PreemptRing:
- prng: AtomicU32 field (moved to TimeslicePrng in engine_ring.rs)
- next_prng() method
- pick_next() method
- yield_token() method
- start(), wait_for_token(), finish(), wait_all_done()
- workers, finished_mask, all_done fields

**Files**: preempt/mod.rs, backend/mod.rs, engine.rs, backends (pmu, replay, e9patch, native)

**Test**: All preemptive tests pass. Verify xorshift32 code is gone from preempt/mod.rs.

---

## Phase D: Persistent Worker Threads

Replace per-dispatch-round std::thread::scope with persistent threads.

**Step D1**: Create WorkerPool:
  handles: Vec<JoinHandle<()>>
  commands: Vec<AtomicU32> — IDLE/DISPATCH/BATCH/SHUTDOWN
  completions: Vec<AtomicU32> — PENDING/DONE
  work_descs: Vec<UnsafeCell<WorkDesc>>

Workers run a loop: park -> read command -> execute -> signal done -> park.

**Step D2**: Spawn pool at simulation start in run_internal (engine.rs).
Each worker installs TLS once. Pool lives until simulation end.

**Step D3**: Adapt PreemptionBackend lifecycle:
- worker_setup / worker_teardown: once at pool create/destroy
- New round_begin / round_end: per dispatch round (reset counters, re-arm timer)

**Step D4**: dispatch_concurrent / process_batch_concurrent become thin
wrappers: assign CPUs to pool workers, set command, wake workers, run
EngineRing decision loop.

**Files**: new worker_pool.rs, engine.rs, backend/mod.rs

**Test**: Assert thread IDs stable across dispatch rounds. All tests pass.

---

## Phase E: Delete Legacy Code

- Delete TokenRing (or reduce to WorkerId + maybe_yield redirect)
- Delete PreemptRing (or reduce to record store if needed)
- Collapse 6 dispatch driver functions into 2 (engine_dispatch, engine_batch)
- Collapse the massive if/else backend selection chains in engine.rs
- Delete NativeOrchestrator (use EngineRing with "resume-all" decision)

**Files**: interleave.rs, preempt/mod.rs, backend/mod.rs, engine.rs

**Test**: cargo clippy no dead code. All integration tests. Full stress test.

---

## Key Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Orchestrator primitive | Futex (not Mutex/Condvar) | Signal handler must be async-signal-safe |
| Thread selection | Min-local-clock | Logical time is the gold standard; fair by construction |
| Yield cost | Read RBC at each yield | Precise local_clock at every decision point |
| Compat flag | None | Clean break; replay uses recorded cursor anyway |
| Tie-breaking | CpuId (deterministic) | Future: add randomized delay as separate feature |
| Persistent threads | WorkerPool with futex parking | Eliminates spawn/join overhead; TLS installed once |
| Replay compatibility | Replay cursor overrides decision callback | Recorded preemption points are replayed exactly |

## Risks

1. **Determinism change**: min-local-clock produces different interleavings than PRNG.
   All existing seeds yield different traces. This is intentional and correct.
   Replay traces from old recordings won't match — replay mode bypasses the
   decision callback and uses the recorded cursor.

2. **Intermediate yield cost**: At kfunc yields, read the RBC counter to get
   a precise intermediate charge. This gives accurate local_clock values at
   every decision point.

3. **engine.rs size**: Already 4700 lines (sim-880544). This refactor should
   SHRINK it by deleting dispatch duplication, but we need to be careful
   not to add more. EngineRing and WorkerPool go in separate files.

## Verification

After each phase:
- ./validate.sh passes (557+ tests, clippy, fmt, mypy, no conflict markers)
- Stress test: python3 bug_finding/stress.py --duration 5 --no-e9patch
- Determinism: python3 bug_finding/stress.py --duration 5 --determinism --no-e9patch
- Benchmark: make benchmark to check for performance regression

After Phase C specifically:
- Verify grep -r xorshift preempt/mod.rs returns nothing
- Verify grep -r 'fn next_prng' preempt/mod.rs returns nothing

## Files Summary

| File | Changes |
|------|---------|
| unsafe_impl/engine_ring.rs | NEW — centralized orchestrator |
| unsafe_impl/worker_pool.rs | NEW — persistent thread pool |
| unsafe_impl/preempt/mod.rs | Signal handler yields to EngineRing; delete xorshift32 |
| unsafe_impl/interleave.rs | Cooperative yield -> EngineRing; eventually delete TokenRing |
| unsafe_impl/backend/mod.rs | Unify dispatch drivers; adapt PreemptionBackend lifecycle |
| safe/engine.rs | Use WorkerPool + EngineRing; simplify dispatch paths |

---

## Implementation Status (2026-03-24)

Branch: `centralize-dispatch` (4 commits ahead of `simulator.v4`)

| Phase | Status | Commit | Notes |
|-------|--------|--------|-------|
| A: EngineRing | DONE | 84f590a | 621-line module, 8 unit tests, TimeslicePrng |
| B: Cooperative wiring | DONE | d7df88e | InterleaveCtx uses fn-pointer indirection, engine_loop with min-clock |
| C: Preemptive wiring | DONE | 373e5a4 | All yield paths through EngineRing, xorshift32 gone from preempt/mod.rs |
| D: Persistent threads | TODO | — | WorkerPool not yet implemented |
| E: Delete legacy | TODO | — | TokenRing, old dispatch drivers still present |

### What changed vs the plan

- TimeslicePrng was placed in engine_ring.rs (not a separate file) since it's
  small and logically part of the new centralized infrastructure.
- InterleaveCtx uses a function-pointer indirection (YieldFn + data pointer)
  rather than a union/enum, keeping it Copy and async-signal-safe.
- run_dispatch_with_orchestrator / run_batch_with_orchestrator kept alive for
  NativeOrchestrator (native-concurrent mode) — they create a dummy EngineRing
  for the worker_setup signature.
- ReplayCtx also got an engine pointer (replay signal handlers need it too).
- The `seed` parameter is now unused in cooperative mode (min-clock is
  inherently deterministic without PRNG).

### All tests passing

200+ tests (unit + integration), 0 clippy warnings, clean build.
