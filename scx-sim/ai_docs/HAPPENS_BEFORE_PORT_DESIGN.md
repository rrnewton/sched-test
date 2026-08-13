# Design: porting hermit's happens-before to scx-sim

**Status: HELD — NOT APPROVED, DO NOT IMPLEMENT FROM THIS.**

Written before the owner narrowed the ask to "just study how we did it in
hermit". The account that supersedes it as the current deliverable is
`HERMIT_HAPPENS_BEFORE_STUDY.md`; read that first. This file is retained only
because its §2 refutation and §3 what-scx-sim-already-has survey are still
accurate and were separately verified. Its §4 design and §7 recommendation are
premature and have not been reviewed.
**Task:** tg `port-hermit-happens-before-to-scxsim`
**Date:** 2026-08-13
**Baseline:** scx-sim at `origin/integration@706ea87`; hermit at the dev-hermit
workspace checkout.

---

## 1. What hermit's happens-before actually is

Read from source, not from the name. The feature spans three layers:

| Layer | File | Role |
|---|---|---|
| Model | `detcore-model/src/happens_before.rs` (1275 lines) | Parse (JSON + terse DSL), normalize, statically validate — name resolution, exactly-one-position, cycle detection. |
| Resolution | `hermit-cli/src/happens_before.rs` (464 lines) | Resolve a `CodeLocation` (function / source line) to a concrete address via debug info. |
| Enforcement | `detcore/src/scheduler.rs` (`HbRuntime`, ~line 295) | Park threads at anchors until gating anchors fire. |

Its own module doc states the purpose precisely:

> Happens-before edges: a sparse, authored partial order over dynamic events.
> Where `--replay-schedule-from` replays a *complete* total order captured from
> a prior run, a happens-before specification pins down only the *few* events
> that matter for a race and lets the deterministic scheduler fill in the rest.
> An agent (or human) that already knows a target race can therefore construct
> it deterministically instead of blind seed-search.

**The data model.** A `HappensBeforeSpec` is `{version, threads, events, edges}`.
An `Anchor` is `{name, thread, position, location}`. A `Position` is one of:

```rust
SyscallCount(u64)                                  // after N syscalls
Rcb(u64)                                           // when the RCB clock hits N
Syscall { sysno, phase: Option<SyscallPhase>, nth } // nth occurrence of a syscall
Rip { addr: Option<u64>, nth }                     // nth execution of an address
Marker { name, nth }                               // cooperative guest marker
```

A `HappensBeforeEdge` says the BEFORE anchor must fire before the AFTER
anchor's thread may proceed. `Strength` is `Hard` (default — a true gate; park
the sink thread) or `Soft` (a priority nudge; the sink may still run if it is
the only runnable thread).

**The enforcement mechanism.** `HbRuntime` holds `fired` (anchors that have
fired, monotonic), `parked` (threads removed from the run queue awaiting their
gate), `spawn_order` (deterministic thread naming), and `wake_pending`.
Re-admission is deferred to a `step3` boundary because pushing to the run queue
mid-`tentative_pop` is illegal.

**Cost and maturity, stated honestly.** Only `Position::SyscallCount` is
actually enforced. Every other position kind parses, validates, and then *never
fires* — `HbRuntime::new` walks `unenforced_positions()` and emits a warning per
anchor so a run never silently drops an ordering constraint. So the upstream
feature is roughly: a complete model + validator, a debug-info resolver, and a
one-position-kind enforcer.

---

## 2. The stated value of the port does not survive contact with the source

The brief's hypothesis, offered explicitly to confirm or refute:

> a happens-before relation would let us reason about whether two events COULD
> have been reordered, which is exactly what is needed to find concurrency bugs
> by exploration rather than by luck … turns random search into systematic
> search.

**Refuted.** That describes an *inferred* happens-before relation — one computed
from observed operations and their conflicts, of the kind dynamic partial-order
reduction (DPOR) uses to decide which interleavings are worth exploring and
which are equivalent. Hermit's feature is the opposite direction: it is
*prescriptive*, not descriptive. You write down an order you already want; the
scheduler enforces it. It contains no conflict detection, no independence
relation, no equivalence-class reasoning, and nothing that could tell you
whether two events *could* have been reordered.

Its actual value is the sentence in its own doc comment: **an agent that already
knows a target race can construct it deterministically instead of blind
seed-search.** That is worth having, and it is a different thing from systematic
exploration.

The distinction matters for the LAVD repro work the brief connects this to:

- **Grinding scx-sim with concurrency randomisation** — already supported (§3).
  Hermit-style HB does not make that grinding smarter.
- **Turning random search into systematic search** — would need DPOR, i.e. an
  inferred conflict relation over kfunc-level state accesses. That is a
  substantially larger and different project. It is not this port.
- **Pinning a suspected interleaving once you can describe it** — this is what
  the port buys, and it is the natural *successor* to a grind that has found
  something, not a replacement for the grind.

---

## 3. What scx-sim already has (checked, not assumed)

The brief was emphatic about this, citing the noise-parameter task whose premise
collapsed because the simulator already had a noise model on by default. So:

| Capability | Status in scx-sim | Where |
|---|---|---|
| Deterministic concurrent callback interleaving | **EXISTS** | `unsafe_impl/interleave.rs` (756 lines) — one worker per CPU, token-passing via `EngineRing`, one thread active at a time |
| A per-worker logical clock, and scheduling by it | **EXISTS** | `pick_by_min_clock()` in `unsafe_impl/engine_ring.rs:357` — picks the non-finished worker with the smallest local clock, tie-broken by CPU id |
| A single choke point where a worker yields control | **EXISTS** | `maybe_yield()` in `interleave.rs:374`, called at every kfunc entry |
| Seed-based exploration + reproduction | **EXISTS** | "same seed → same interleaving → same trace"; the documented methodology is explore-many-seeds then reproduce-with-seed |
| RBC (retired conditional branch) counting | **EXISTS** | e9patch `jcc` instrumentation, `sim_rbc_trampoline.c`, `scx_perf::try_create_rbc_counter` |
| **Firing at an absolute RBC target** | **EXISTS** | `arm_replay_timer(timer_fd, target_rbc)` in `unsafe_impl/preempt/mod.rs:2154` — arms the PMU timer to fire at `target_rbc - REPLAY_MARGIN` |
| Record/replay of a preemption schedule | **EXISTS** | replay mode with recorded `rbc_count` per preemption point |
| Opt-in interleaving stress knob | **EXISTS** | `--stochastic-timer-interleave` |
| An authored ordering constraint / gate | **ABSENT** | — |
| A spec model: parse, validate, cycle-detect | **ABSENT** | — |
| Any inferred causality / vector clocks / conflict relation | **ABSENT** | verified with `find … -print0 \| xargs -0 grep -ril` for `happens.before`, `vector.clock`, `causal`, `lamport`; the only hits are unrelated uses in `perfetto_pb.rs`, `engine.rs`, `interleave.rs`, `worker_pool.rs` |

**The single most important line in that table:** hermit's `Position::Rcb` — the
position kind hermit models but does *not* enforce — is the one scx-sim already
implements and uses in anger for replay. scx-sim's RBC addressing is *ahead* of
hermit's enforcement here.

So the port is much smaller than "port a 1700-line feature". Most of what
hermit's HB needs from its host, scx-sim already has.

---

## 4. What the port would actually be

**Concept mapping.**

| hermit | scx-sim equivalent |
|---|---|
| Guest thread (`DetTid`) | Worker / CPU (`WorkerId`, `CpuId`) — already the unit of interleaving |
| Syscall (scheduling decision point) | kfunc entry, where `maybe_yield()` is already called |
| `Position::SyscallCount(n)` | "after N kfunc yields on this worker" — the counter does not exist but the counting point does |
| `Position::Rcb(n)` | **already exists** — `arm_replay_timer` |
| `Position::Rip{addr,nth}` | e9patch RIP trampoline exists (`e9_rip_trampoline.c`), so plausible later |
| Run queue + `tentative_pop` | `pick_by_min_clock()` candidate set |
| Park / re-admit at a `step3` boundary | Park a worker inside `maybe_yield()`; re-admit by returning it to the candidate set |
| `Strength::Hard` / `Soft` | Hard = remove from candidate set; Soft = clock penalty (min-clock selection makes a "nudge" natural) |

**Proposed shape, three pieces:**

1. **Model** (`crates/scx_simulator/src/safe/happens_before.rs`, new, safe Rust).
   Spec types, JSON parse, normalization, static validation including cycle
   detection. Port hermit's structure closely — this is the part worth copying
   rather than reinventing, and it has no unsafe or engine coupling.
2. **Enforcement** (~2 touch points, deliberately small):
   - `pick_by_min_clock()` — filter out parked workers before selection.
   - `maybe_yield()` — on reaching an anchor, fire it; if the worker is the sink
     of an unsatisfied edge, park instead of yielding normally.
3. **Addressing**, in order of cost:
   - `KfuncCount(n)` — a per-worker counter at the existing yield point. Cheap.
   - `Rcb(n)` — wire to the existing `arm_replay_timer` path. Medium; the
     mechanism exists, the plumbing does not.
   - `Rip`, `Marker` — later, or never.

**Deliberately excluded from v1:** debug-info `CodeLocation` resolution (hermit
needs it for whole-program guests; scx-sim's "threads" are CPUs running known
scheduler callbacks, so symbolic naming buys much less), and the terse DSL.

---

## 5. Risks

- **Deadlock by authoring.** A spec can park every worker. Hermit handles the
  static case with cycle detection; the dynamic case (an anchor that never
  fires because its thread took another path) still hangs. scx-sim needs a
  watchdog that fails loudly with the unfired-anchor set — silently hanging a
  simulation would violate No Silent Failures.
- **Determinism interaction.** Parking changes which worker runs next, so an HB
  run is not comparable to a non-HB run at the same seed. Must be surfaced in
  the trace header like other exaggerated-mode knobs (Twin Design Principle 2:
  opt-in, never default, self-documenting).
- **RBC coupling.** RBC counts shift with build configuration — the UBSan mode
  comment in `schedulers/Makefile` already warns that a trace recorded under
  UBSan will not replay against a non-UBSan build. `Position::Rcb` anchors
  inherit that fragility and must be documented with it.
- **Engine churn.** The brief notes other agents are changing the engine
  tonight; both touch points are in `unsafe_impl/`, which is active.

## 6. Open questions for the owner

1. **Is the value proposition still wanted, given §2?** The port buys
   *constructing a known interleaving*, not systematic search. If the actual
   goal is systematic exploration for the LAVD repro, this is the wrong feature
   and DPOR is the right conversation.
2. **v1 addressing:** `KfuncCount` only, or `KfuncCount` + `Rcb`? `Rcb` is the
   more powerful and the mechanism already exists, but it is the fragile one.
3. **Authoring surface:** JSON spec file only, or also a builder API for tests?
   A builder would make HB usable from the existing integration tests without
   any file I/O, which may be where most of the value actually lands.

## 7. Recommendation

Proceed, but scoped to what §2 establishes it is for: **piece 1 (model) plus
piece 2 (enforcement) with `KfuncCount` addressing only**, exposed through a
test-facing builder API before any file format. That is a genuinely small change
against a substrate that already has the hard parts, and it is directly useful
the moment a grind produces an interleaving worth pinning.

Do **not** bill it as turning random search into systematic search. It does not.
