# Concurrency Model Exploration: Beyond Same-Nanosecond Batching

This document explores design options for widening the simulator's definition
of "concurrent events" beyond the current strict same-nanosecond batching.

## 1. Current Model

### 1.1 The Event Loop

The simulator is a discrete-event simulator (DES) with a min-heap priority
queue (`EventQueue`). The main loop in `engine.rs` (line 1113) pops events
in timestamp order:

```rust
// engine.rs:1113-1119
'event_loop: while let Some(t) = events.peek_time() {
    if t > scenario.duration_ns {
        break;
    }
    state.clock = t;
    let batch = events.drain_at(t);
```

### 1.2 Batching: Strictly Same-Nanosecond

The `drain_at(t)` method (engine.rs:188-197) pops **all events at exactly
timestamp `t`**:

```rust
// engine.rs:188-197
fn drain_at(&mut self, t: TimeNs) -> Vec<Event> {
    let mut batch = Vec::new();
    while let Some(Reverse(e)) = self.heap.peek() {
        if e.time_ns != t {
            break;
        }
        batch.push(self.heap.pop().unwrap().0);
    }
    batch
}
```

This is the narrowest possible definition of "concurrent": only events
scheduled at **exactly the same nanosecond** are grouped together.

### 1.3 Partitioning: Global vs Per-CPU

Once a same-timestamp batch is collected, it is partitioned by
`group_events_by_cpu()` (engine.rs:284-297):

- **Global events** (TaskWake, TimerFired, cgroup ops): processed
  sequentially first (engine.rs:1125-1145).
- **Per-CPU events** (Tick, SliceExpired, TaskPhaseComplete, hotplug, IRQ):
  eligible for concurrent processing if 2+ CPUs have events
  (engine.rs:1147-1159).

```rust
// engine.rs:1121-1159
if interleave_enabled {
    let (global, per_cpu) = group_events_by_cpu(batch);
    // 1. Global events: always processed sequentially first
    for event in global { ... }
    // 2. Per-CPU events: concurrent if 2+ CPUs
    if per_cpu.len() >= 2 {
        self.process_batch_concurrent(per_cpu, ...);
    } else {
        // Single CPU: sequential
    }
}
```

### 1.4 What "Concurrent" Means Here

"Concurrent" does **not** mean truly parallel (multiple threads running
simultaneously). It means **interleaved execution on separate OS threads
with only one thread active at a time**, controlled by a token-passing
mechanism.

Two interleaving modes exist:

1. **Cooperative** (`interleave.rs`): Workers yield at kfunc boundaries
   via `maybe_yield()`. A `TokenRing` uses Mutex/Condvar and a PRNG to
   select the next worker. Only one worker is active at a time.

2. **Preemptive** (`preempt/mod.rs`): Workers are additionally preempted
   mid-C-code by PMU RBC timer signals. A `PreemptRing` uses atomics/futex
   for async-signal-safe token passing.

Both modes are deterministic for a given seed: the PRNG determines worker
selection order, so same seed produces same interleaving produces same trace.

### 1.5 The Cost Model

The simulator has a cost model for scheduler overhead (engine.rs:441-494):

- **RBC counter mode**: A PMU counter measures retired conditional branches
  in scheduler C code. `charge_sched_time()` converts branch counts to
  nanoseconds (`count * sched_overhead_rbc_ns`) and advances the per-CPU
  `local_clock`.

- **Fallback kfunc cost mode**: Without a PMU counter, each `with_sim()`
  call (kfuncs.rs:799) contributes a tiered nanosecond cost
  (`kfunc_cost::TRIVIAL=10`, `SIMPLE=50`, etc.) to `rbc_kfunc_ns`. A
  minimum of 50ns per callback ensures time always advances.

- **Per-CPU local clocks** (`cpu.rs:46`): Each CPU has its own
  `local_clock` that advances independently. `advance_cpu_clock()`
  (kfuncs.rs:408-411) ensures it never falls behind the global event
  queue time.

### 1.6 The Dispatch Path (A Second Concurrency Entry Point)

Besides the event-loop batching, there is a second place where concurrent
interleaving occurs: `dispatch_concurrent()` (engine.rs:2877-2995). When
a task wakes and multiple CPUs are idle, dispatch callbacks for all idle
CPUs run concurrently via the same token-passing mechanism. This path is
**not** constrained by same-nanosecond batching -- it fires whenever 2+
CPUs are idle at the point of a task wake.

### 1.7 Summary of Current Limitations

The same-nanosecond constraint means that events like:
- CPU0 tick at T=4,000,000ns
- CPU1 slice expiry at T=4,000,001ns

are processed **strictly sequentially**, even though in real hardware
these would overlap: the tick handler on CPU0 takes hundreds or thousands
of nanoseconds, during which CPU1 would independently start processing
its own slice expiry.

On a 3GHz CPU, 1ns corresponds to ~3 clock cycles. A `sched_ext`
`ops.dispatch()` callback typically executes thousands of instructions
(hundreds of nanoseconds). Events 1ns apart are, for all practical
purposes, simultaneous.

---

## 2. Literature Review

### 2.1 Classical PDES: Conservative and Optimistic Synchronization

The parallel discrete event simulation (PDES) literature provides the
foundational theory for this problem.

**Chandy-Misra-Bryant (CMB) Conservative Protocol**: Each logical process
(LP) sends events only to LPs it connects to. An LP can safely process
an event at time T if it can guarantee no future event will arrive with
timestamp < T. This guarantee comes from **lookahead**: a minimum time
increment that an LP adds to outgoing events. If LP-A sends to LP-B
with lookahead L, then LP-B knows that once it receives a message at
time T from LP-A, no future message from LP-A will have timestamp < T+L.

Sources:
- [Synchronization methods in parallel and distributed discrete-event simulation](https://www.sciencedirect.com/science/article/pii/S1569190X12001244)
- [An Introduction to Parallel Discrete Event Simulation](https://www.sne-journal.org/fileadmin/user_upload_sne/SNE_Issues_OA/SNE_29_2/articles/sne.29.2.10471.on.OA.pdf)

**Jefferson Time Warp (Optimistic Protocol)**: LPs process events
speculatively. If a "straggler" event arrives with an earlier timestamp
than already-processed events, the LP rolls back by restoring saved state
and re-processing. Anti-messages cancel incorrect downstream effects.

Sources:
- [Virtual Time III: Unification of Conservative and Optimistic](https://www.informs-sim.org/wsc17papers/includes/files/058.pdf)
- [Virtual Time III, Part 2](https://www.osti.gov/pages/servlets/purl/2005060)

**Relevance**: Our simulator is a sequential DES that models concurrent
kernel CPUs. The PDES literature's concepts (lookahead, causality,
synchronization windows) provide frameworks for reasoning about which
events can safely be processed concurrently.

### 2.2 Fujimoto's Temporal Uncertainty

Richard Fujimoto (Georgia Tech) introduced **temporal uncertainty** for
enabling parallelism in PDES. The key insight: in many simulations,
event timestamps are not precisely known -- there is an inherent range
of valid timestamps. Events within this uncertainty window can be
processed concurrently because their relative ordering does not affect
correctness.

Formally, an event has a timestamp range [T_min, T_max]. If two events
have overlapping ranges, they are **concurrent** (their relative order
is not determined). The simulation can process them in any order -- or
in parallel -- and remain correct.

This maps directly to our problem: on real hardware, a tick handler at
T=4,000,000ns actually executes over a range [4,000,000, 4,000,500]ns.
A slice expiry at T=4,000,050ns overlaps this range and is truly
concurrent.

Sources:
- [Exploiting Temporal Uncertainty in Parallel and Distributed Simulations](https://sites.cc.gatech.edu/computing/pads/PAPERS/Exploiting_Temporal_Uncertainty.pdf) (Fujimoto, 1999)
- [IEEE Xplore: Exploiting temporal uncertainty](https://ieeexplore.ieee.org/document/766160)

### 2.3 Lookahead in PDES

**Lookahead** is the minimum time advance that a logical process
guarantees between receiving an event and sending a new one. In our
context, it represents the minimum time a scheduler callback takes to
execute. If CPU0 starts processing an event at time T, and we know the
callback takes at least L nanoseconds, then CPU0 cannot generate any
new event before T+L. Any event on CPU1 scheduled before T+L is
causally independent of CPU0's current processing.

The concept of **lookahead accumulation** is also relevant: when events
chain through multiple stages, the lookahead compounds. In our model,
a tick -> dispatch -> enqueue chain has cumulative lookahead from each
stage's minimum execution time.

Sources:
- [Lookahead Accumulation in Conservative PDES](https://www.researchgate.net/profile/Jan-Lemeire/publication/228970039_Lookahead_Accumulation_in_Conservative_Parallel_Discrete_Event_Simulation/links/02e7e52259c22d0f37000000/Lookahead-Accumulation-in-Conservative-Parallel-Discrete-Event-Simulation.pdf)
- [Improving lookahead in parallel discrete event simulations](https://ieeexplore.ieee.org/document/924616)
- [A practical efficiency criterion for the null message algorithm](https://doc.omnetpp.org/publications/varga03criterion.pdf)

### 2.4 Simultaneous Events in DES

The problem of **simultaneous events** -- events at the same timestamp
-- is well-studied in DES. The standard approach uses a tiebreaker
(priority, insertion order, or random) to impose a total order. However,
research shows that the *choice* of tiebreaking can significantly affect
simulation results (the "simultaneous event problem").

The scx_simulator already addresses this with its `EventQueue::next_seq()`
mechanism (engine.rs:155-166): in randomized mode, PRNG-derived priorities
explore different orderings; in fixed-priority mode, insertion order is
preserved. This is sophisticated but only applies to events at the
**exact same** nanosecond.

Sources:
- [A Discrete-event Simulation Tool for the Analysis of Simultaneous Events](https://doc.omnetpp.org/publications/1345281.pdf)
- [The Effect of Modeling Simultaneous Events on Simulation Results](https://scholar.afit.edu/etd/2249/)
- [Efficient Analysis of Simultaneous Events in Distributed Simulation](https://ieeexplore.ieee.org/document/4384554)
- [Unbiased Deterministic Total Ordering of Parallel Events](https://arxiv.org/abs/2105.00069)

### 2.5 CPU/Architecture Simulators

Architecture simulators face a similar problem. **gem5** uses a
tick-based timing model where CPU components advance in lockstep.
**parti-gem5** parallelizes this by partitioning CPUs into groups
that can advance independently, with synchronization barriers at
shared-resource boundaries.

Sources:
- [parti-gem5: gem5's Timing Mode Parallelised](https://arxiv.org/abs/2308.09445)
- [gem5 CPU Models](https://gem5bootcamp.github.io/gem5-bootcamp-env/modules/using%20gem5/models-cpu/)

### 2.6 Lamport's Happened-Before

Leslie Lamport's **happened-before** relation provides the theoretical
foundation for reasoning about concurrent events. Two events are
concurrent (neither happened-before the other) if there is no causal
chain connecting them. In our model, events on different CPUs are
concurrent unless one produces an event consumed by the other (e.g.,
CPU0's dispatch kicks CPU1).

The key insight: concurrency is defined by **causal independence**, not
by temporal proximity. Two events 1000ns apart can be causally
independent (and thus concurrent), while two events at the same
nanosecond can be causally dependent (and must be ordered).

### 2.7 Kendo and Deterministic Logical Time

Kendo (2009) uses **retired instruction counting** as a logical clock
for deterministic multithreading. Lock acquisitions are ordered by
logical time (instruction count), not wall-clock time. This ensures
determinism while preserving concurrency for independent memory
accesses.

The scx_simulator's existing RBC-based cost model is architecturally
similar: RBC counts serve as a progress metric that defines logical
time for each CPU/worker.

Sources:
- Referenced in existing `ai_docs/controlled_concurrency_survey.md`

### 2.8 ns-3 and SimPy

**ns-3** handles simultaneous events via a global event scheduler with
configurable tiebreaking. It supports distributed simulation via MPI
with a CMB-style conservative protocol. **SimPy** processes events
strictly sequentially by timestamp; simultaneous events are processed
in FIFO insertion order. Neither has a concept of "near-simultaneous"
batching.

Sources:
- [UNISON for ns-3: parallel simulation](https://github.com/NASA-NJU/UNISON-for-ns-3)
- [SimPy Parallel Simulation](https://pythonhosted.org/SimPy/Manuals/Interfacing/ParallelSimPy/SimPyPP.html)

---

## 3. Design Space Exploration

### 3.1 Window-Based Batching

**Concept**: Replace `drain_at(t)` (exact timestamp match) with
`drain_within(t, t + W)` where W is a configurable window (e.g., 100ns).
All events within the window are grouped into a single batch and processed
concurrently.

**Mechanism**:
```
// Pseudocode:
let t = events.peek_time();
let batch = events.drain_within(t, t + window_ns);
// Process batch concurrently (same as current model)
```

**Window Size Selection**:
- **Physical argument**: On a 3GHz CPU, a scheduler structop callback
  takes ~100-1000ns (based on kfunc costs: trivial=10ns, simple=50ns,
  with 5-20 kfunc calls per callback). A window of 100-500ns would
  capture events that overlap in real hardware execution.
- **Configurable**: The window should be a scenario parameter, allowing
  users to tune it. A value of 0 recovers the current behavior.
- **Derived from cost model**: The window could be set to the minimum
  callback execution time from the overhead model (e.g.,
  `MIN_CALLBACK_COST_NS = 50` from engine.rs:487).

**Tradeoffs**:
- **Pro**: Simple to implement. Minimal change to the existing
  architecture -- just widen the `drain_at` predicate.
- **Pro**: Captures the physical intuition that "close enough" events
  overlap.
- **Con**: The window is arbitrary. Too small and you get the current
  behavior; too large and you group causally dependent events. There is
  no theoretical justification for any particular window size.
- **Con**: Events at the window boundary are treated differently from
  events just outside it. An event at T+W is concurrent; T+W+1 is not.
  This creates a discontinuity.
- **Con**: Does not address causal dependencies. Events within the
  window on the same CPU are not truly concurrent.

**Determinism**: Fully deterministic for a given window size and seed.
The window is part of the scenario config. The within-batch ordering
uses the existing PRNG-based tiebreaking.

### 3.2 Island Separation

**Concept**: Instead of a fixed window, identify "islands" of events
separated by quiescent intervals. An island is a maximal cluster of
events where no gap between consecutive events exceeds a threshold.
Events within an island are processed concurrently; events in different
islands are processed sequentially.

**Mechanism**:
```
// Pseudocode:
loop {
    let t = events.peek_time();
    let mut island = events.drain_at(t);
    // Keep pulling in events that are "close" to the current batch
    while let Some(next_t) = events.peek_time() {
        if next_t - t > gap_threshold {
            break;
        }
        t = next_t;
        island.extend(events.drain_at(next_t));
    }
    process_island_concurrently(island);
}
```

**Island Detection**:
- Look ahead in the event queue: if the next event is within the
  gap threshold, pull it into the current island.
- This naturally handles variable-density event regions: busy periods
  (many events per microsecond) form large islands; idle periods
  (millisecond gaps) create natural boundaries.

**Tradeoffs**:
- **Pro**: More natural than a fixed window. Islands adapt to the
  actual event density.
- **Pro**: Idle periods (no events for milliseconds) naturally
  separate islands, matching the intuition that concurrent processing
  only matters during busy periods.
- **Con**: Still requires a gap threshold parameter (same arbitrariness
  problem as window-based batching).
- **Con**: Islands can grow very large in dense event regions (e.g.,
  when many tasks wake simultaneously), potentially grouping hundreds
  of events. Processing such large batches concurrently with
  token-passing may be expensive (more workers, more context switches).
- **Con**: Events within an island at different timestamps need careful
  ordering. Current event processing assumes `state.clock = t` is
  constant within a batch.

**Determinism**: Deterministic for a given threshold and seed. However,
the island structure depends on the threshold, so changing it changes
which events are grouped -- potentially more disruptive than the
window approach.

### 3.3 Cost-Model-Driven Preemption (Lookahead-Based)

**Concept**: Use the existing cost model to determine when a CPU's
callback execution overlaps with another CPU's event. If CPU0 starts
processing at time T and its callback is estimated to take C
nanoseconds, then any event on CPU1 at time T' where T < T' < T+C
is concurrent with CPU0.

**Mechanism**: This is essentially the PDES conservative protocol
with the cost model providing the lookahead.

1. Pop the earliest event at time T for, say, CPU0.
2. Start processing it. The cost model predicts it will take C
   nanoseconds.
3. While processing, check: are there events on other CPUs at
   times [T, T+C)? If so, start those concurrently.
4. When CPU0's callback completes, its actual cost C_actual is known.
   Any event on any CPU at time T+C_actual or later must wait.

**Implementation Sketch**:
```
// Pseudocode:
let event = events.pop();
let t = event.time_ns;
let cpu = event.cpu();
let estimated_cost = estimate_callback_cost(&event.kind);

// Pull in concurrent events from other CPUs
let mut concurrent = vec![event];
while let Some(next_t) = events.peek_time() {
    if next_t >= t + estimated_cost { break; }
    if let Some(next_cpu) = events.peek().cpu() {
        if next_cpu != cpu {
            concurrent.push(events.pop());
        } else {
            break; // Same CPU: sequential
        }
    }
}
process_concurrently(concurrent);
```

**Tradeoffs**:
- **Pro**: Grounded in physical reality. The window is derived from the
  actual (modeled) execution time, not an arbitrary parameter.
- **Pro**: Self-adjusting: expensive callbacks create larger windows;
  cheap callbacks create smaller windows.
- **Con**: The cost model is approximate. The kfunc cost tiers
  (TRIVIAL=10ns, SIMPLE=50ns) are estimates. The RBC-based cost is
  measured but only available when the PMU counter is active.
- **Con**: The estimated cost is not known until the callback starts
  executing. We might need to use the minimum callback cost
  (MIN_CALLBACK_COST_NS = 50ns) as a conservative lookahead.
- **Con**: Significant complexity. The current model processes a batch
  atomically and then moves to the next timestamp. This approach
  requires incremental event consumption with dynamic concurrency
  decisions.
- **Con**: Breaks the clean separation between event selection and
  event processing. The current `drain_at` / `process_event` split is
  simple and elegant.

**Determinism**: Deterministic if the cost estimate is deterministic
(which it is, since it depends only on event kind and the kfunc cost
table).

### 3.4 Random Preemption with Time Checks

**Concept**: During single-CPU event processing, randomly preempt and
check if simulated time has advanced enough that other CPUs' events are
now "ready" for concurrent processing. This converts sequential processing
into concurrent processing mid-execution.

**Mechanism**:
1. Start processing CPU0's event at time T.
2. At each kfunc yield point (already happens via `maybe_yield`), check:
   has CPU0's `local_clock` advanced enough that events on other CPUs
   (in the event queue) are now within the "concurrent window"?
3. If so, spawn worker threads for those events and begin interleaving.

**Tradeoffs**:
- **Pro**: Naturally integrates with the existing `maybe_yield`
  mechanism. No new scheduling infrastructure needed.
- **Pro**: Cost-model-driven: the decision to add concurrent workers
  depends on how much simulated time has actually elapsed.
- **Con**: Mid-execution concurrency changes are complex. Starting new
  workers while one worker is already active requires careful
  synchronization.
- **Con**: The decision to spawn workers depends on `local_clock`
  advancement, which depends on the kfunc cost model. In instant-timing
  scenarios (zero overhead), `local_clock` never advances, so this
  approach would never trigger.
- **Con**: Nondeterminism risk: the decision to spawn depends on
  how much work was done before the yield point, which could vary.

**Determinism**: Problematic. The decision to add workers depends on
the accumulated kfunc cost at each yield point, which is deterministic
for a given seed, but the resulting interleaving is more complex to
reason about. Replay requires tracking exactly when workers were added.

### 3.5 Hybrid: Window Batching + Cost-Model Lookahead

**Concept**: Combine approaches 3.1 and 3.3. Use a fixed minimum window
(e.g., 50ns = MIN_CALLBACK_COST_NS) for initial batching, then extend
the batch dynamically based on the cost model as events are processed.

**Mechanism**:
1. Collect initial batch: all events in [T, T + W_min).
2. Start concurrent processing of per-CPU events in the batch.
3. As callbacks complete and their actual cost C is known, check if new
   events in the queue fall within [T, T + C). If so, add them to the
   concurrent batch dynamically (or process them in the next micro-batch).

**Tradeoffs**:
- **Pro**: Gets the best of both worlds: the simplicity of window
  batching for initial grouping, and the precision of cost-model-driven
  extension.
- **Con**: Highest implementation complexity. Dynamic batch extension
  requires the ability to add workers to an in-progress concurrent
  batch, or a multi-round approach with micro-batches.

### 3.6 Causal Independence Analysis (Graph-Based)

**Concept**: Challenge the time-window framing entirely. Instead of
asking "are these events close in time?", ask "are these events causally
independent?" Build a dependency graph of events and process independent
events concurrently regardless of timestamp.

**Mechanism**:
- Events on different CPUs are independent unless one produces a
  side effect consumed by the other (e.g., dispatching to another
  CPU's local DSQ, kicking another CPU, modifying a shared DSQ).
- Analyze the event handlers to determine read/write sets. Events
  with non-overlapping write sets can be processed concurrently.
- This is essentially **static analysis** of the event handlers to
  extract a causal dependency graph.

**Tradeoffs**:
- **Pro**: Theoretically optimal -- maximizes concurrency without
  sacrificing correctness.
- **Pro**: No arbitrary window parameter.
- **Con**: Extremely difficult in practice. The scheduler C code is
  opaque (loaded via dlopen). We cannot statically analyze what
  kfuncs it will call or what shared state it will access.
- **Con**: Even for events we can analyze (tick, slice expiry), the
  scheduler callbacks called within them are unknown at event-scheduling
  time.
- **Con**: Over-engineering for the actual use case. The goal is to
  test scheduler correctness under concurrent interleavings, not to
  extract maximum parallelism.

**Determinism**: Could be deterministic if the dependency analysis is
deterministic. But the analysis would need to be conservative (assume
dependencies when uncertain), which limits the concurrency benefit.

### 3.7 Epoch-Based Synchronization

**Concept**: Divide simulated time into fixed epochs (e.g., 1000ns
each). Within an epoch, all events are processed concurrently with
interleaving. At epoch boundaries, synchronize: all events in the
epoch must complete before the next epoch begins.

**Mechanism**:
```
// Pseudocode:
let epoch_size = 1000; // ns
let mut epoch_start = 0;
loop {
    let epoch_end = epoch_start + epoch_size;
    let batch = events.drain_before(epoch_end);
    if batch.is_empty() {
        epoch_start = events.peek_time().unwrap_or(MAX);
        continue;
    }
    process_epoch_concurrently(batch);
    epoch_start = epoch_end;
}
```

**Tradeoffs**:
- **Pro**: Simple and predictable. The epoch boundary provides a
  natural synchronization point.
- **Pro**: No need for fine-grained dependency analysis.
- **Con**: The epoch size is arbitrary (same problem as window-based
  batching, just at a larger scale).
- **Con**: Events at the beginning and end of an epoch may not actually
  overlap in real time. E.g., an event at T=0 and T=999ns may not
  overlap if callbacks take 100ns each.
- **Con**: Very different from the current architecture. The current
  model processes events at specific timestamps; epochs would require
  handling events at mixed timestamps within a batch.

**Determinism**: Deterministic for a given epoch size and seed.

---

## 4. Tradeoff Analysis

### 4.1 Comparison Matrix

| Approach | Precision | Efficiency | Determinism | Complexity | Kernel Fidelity |
|---|---|---|---|---|---|
| Current (same-ns) | Exact | No benefit (rare batches) | Perfect | Already done | High |
| Window batching | Approximate | Moderate (more batches) | Perfect | Low | Moderate |
| Island separation | Approximate | Moderate | Perfect | Low-Med | Moderate |
| Cost-model lookahead | Good | Moderate | Perfect | High | Good |
| Random preemption | Poor | Low | Difficult | Medium | Poor |
| Hybrid (window+cost) | Good | Moderate | Perfect | Very High | Good |
| Causal independence | Optimal | High | Perfect | Extreme | Perfect |
| Epoch-based | Approximate | Moderate | Perfect | Medium | Low |

### 4.2 Precision vs Complexity Frontier

The approaches form a Pareto frontier from simple-but-imprecise to
complex-but-precise:

1. **Window batching** (simplest): Approximate concurrency via fixed
   time window. Easy to implement but the window is arbitrary.
2. **Cost-model lookahead** (moderate): Grounded in physical execution
   time. Harder to implement but better justified.
3. **Causal independence** (most precise): Theoretically optimal but
   impractical given opaque scheduler C code.

### 4.3 Impact on Existing Architecture

The current architecture has a clean separation:
1. **Event selection**: `drain_at(t)` selects events.
2. **Partitioning**: `group_events_by_cpu()` splits by CPU.
3. **Processing**: `process_batch_concurrent()` or sequential loop.

Most approaches preserve this separation:
- **Window batching**: Changes only step 1 (wider drain).
- **Island separation**: Changes step 1 (adaptive drain).
- **Cost-model lookahead**: Changes steps 1 and 3 (drain depends on
  processing).
- **Epoch-based**: Changes step 1 (drain by epoch).

The approaches that break this separation (random preemption, hybrid,
causal analysis) are significantly more complex.

### 4.4 Impact on Determinism

All approaches except "random preemption with time checks" preserve
determinism straightforwardly:
- The window/threshold/epoch size is part of the scenario config.
- Event ordering within a batch uses the existing PRNG tiebreaking.
- Token-passing interleaving remains seed-driven.

The key insight: **widening the batch does not affect determinism**. The
existing machinery for deterministic interleaving within a batch (PRNG
tiebreaking, token ring, worker selection) works regardless of how the
batch was formed. What changes is *which* events are in the batch.

### 4.5 Impact on the Cost Model and Clock Advancement

A critical design consideration: events within a concurrent batch
currently share the same `state.clock` value (set at engine.rs:1117).
If events in a widened batch have different timestamps, what clock value
should be used?

Options:
1. **Use the earliest timestamp**: Each CPU's `advance_cpu_clock()` will
   pull its local clock forward to at least the event's timestamp. Events
   at later timestamps naturally get higher local clocks.
2. **Set per-event clock**: Instead of setting `state.clock` for the
   batch, set it per-event within `process_event()`. This is already
   partially done (process_event advances per-CPU clocks, line 1331-1353).
3. **Use epoch-start time**: For epoch-based, use the epoch start as the
   global clock.

Option 2 is most compatible with a widened batch. The per-CPU
`local_clock` already handles divergent timing across CPUs. The global
`state.clock` is mainly used for global events (TaskWake, TimerFired) and
trace timestamps.

---

## 5. Preliminary Recommendations

### 5.1 Recommended Approach: Window-Based Batching (Phase 1)

For an initial implementation, **window-based batching** offers the best
effort-to-value ratio:

1. **Minimal code change**: Replace `drain_at(t)` with `drain_within(t, t + window)`.
   The rest of the pipeline (partitioning, concurrent processing, interleaving)
   stays the same.

2. **Configurable**: Add a `concurrency_window_ns: TimeNs` field to `Scenario`.
   Default to 0 (preserving current behavior). Users can set it to values like
   50-500ns.

3. **Physical justification**: A default of `MIN_CALLBACK_COST_NS` (50ns) has
   a reasonable physical basis: any callback takes at least 50ns, so events
   within 50ns of each other could overlap.

4. **Clock handling**: Set `state.clock` to the earliest timestamp in the batch.
   Per-CPU events at later timestamps will naturally advance their local clocks
   via `advance_cpu_clock()`.

5. **Compatible with existing interleaving**: The TokenRing/PreemptRing
   mechanism works identically -- it does not care about event timestamps,
   only about how many workers are in the concurrent group.

### 5.2 Future Enhancement: Cost-Model Lookahead (Phase 2)

Once window-based batching is proven, enhance it with cost-model-driven
window sizing:

1. Instead of a fixed window, compute the window from the cost model:
   `window = estimate_callback_cost(event.kind)`.
2. This makes the window self-adjusting: expensive operations (dispatch)
   create larger windows than cheap operations (field reads).
3. The kfunc cost tiers already exist (kfuncs.rs:41-46); estimating a
   per-EventKind cost is straightforward.

### 5.3 Not Recommended (For Now)

- **Causal independence analysis**: Too complex for opaque C code.
  Revisit if/when we can introspect scheduler behavior.
- **Random preemption with time checks**: Determinism concerns outweigh
  benefits.
- **Epoch-based**: Too coarse; does not model actual execution overlap.
  Better suited for fully parallel (not interleaved) simulation.

### 5.4 A Note on the Real Goal

It is worth questioning whether widening the concurrency window is the
right goal at all. The simulator's primary purpose is **bug finding**:
exercising different interleavings of scheduler callbacks to find
concurrency bugs. Two factors matter:

1. **Coverage**: How many distinct interleavings are explored?
2. **Realism**: How closely do the explored interleavings match real
   hardware behavior?

The current same-nanosecond batching limits coverage: events 1ns apart
are never interleaved, even though they would be on real hardware. A
wider window directly increases coverage.

However, coverage is already improved by:
- The `dispatch_concurrent` path (engine.rs:2877), which fires whenever
  2+ CPUs are idle, regardless of timestamp.
- PRNG-randomized event ordering, which explores different sequences.
- Preemptive interleaving, which adds mid-callback preemption points.

The window widening specifically addresses the case where events on
different CPUs happen at *nearly* the same time but are not caught by
`dispatch_concurrent` (because they are not dispatch events -- they are
ticks, slice expiries, etc.).

---

## 6. Open Questions

### 6.1 How Often Do Near-Simultaneous Events Occur?

We need empirical data: in typical simulation scenarios, how often do
per-CPU events occur within 1ns, 10ns, 100ns, 1000ns of each other?
If the answer is "almost never" (because events are scheduled at
multiples of milliseconds, like 4ms ticks), then widening the window
may have little practical effect. Ticks are scheduled at TICK_INTERVAL_NS
= 4,000,000ns, so CPU0 and CPU1 ticks are at the exact same time (both
at T = 4,000,000, 8,000,000, etc.). The interesting case is when overhead
modeling shifts events: CPU0's tick at T=4,000,000 may advance its
local_clock to 4,000,200, and its next event is at 4,000,200. CPU1's
next event is still at 4,000,000 (its tick). These are 200ns apart.

**Action**: Instrument the event loop to histogram inter-event gaps per
CPU and cross-CPU. This data will determine whether window widening has
any practical impact.

### 6.2 Should Global Events Participate in Batching?

Currently, global events (TaskWake, TimerFired) are always processed
sequentially before per-CPU events. Should a widened window pull global
events into concurrent batches? On real hardware, a task wakeup on CPU0
is concurrent with a tick on CPU1. But global events modify shared state
(task lists, DSQ queues) that all CPUs access, making them harder to
interleave safely.

**Action**: Analyze which global events could safely participate in
concurrent processing. TaskWake involves select_cpu + enqueue, which
access per-task and per-CPU state. TimerFired could trigger arbitrary
scheduler logic.

### 6.3 What Happens to `state.clock` with Mixed Timestamps?

If a batch contains events at T=4,000,000 and T=4,000,050, what should
`state.clock` be? The global clock is used by:
- `bpf_ktime_get_ns()` (returns `state.clock`)
- `scx_bpf_now()` (returns `state.clock`)
- Trace recording
- Watchdog timeout calculation

For concurrent batches, each CPU already uses its `local_clock`. But
`bpf_ktime_get_ns()` returns the global clock, which could be confusing
if it returns a timestamp earlier than a per-CPU event's timestamp.

**Action**: Analyze all uses of `state.clock` during concurrent batch
processing. Consider whether it should be set to the batch's maximum
timestamp.

### 6.4 How Does This Interact with Replay?

The simulator supports replay mode (preempt/trace.rs) where a recorded
interleaving is replayed exactly. If the concurrency window changes
between recording and replay, the set of concurrent events changes,
potentially invalidating the recorded interleaving.

**Action**: The concurrency window must be part of the replay trace
metadata, and replay must use the same window as recording.

### 6.5 Should the Window Be Per-CPU or Global?

A fixed global window treats all CPUs the same. But CPUs may have
different local clocks due to overhead modeling. Should the window be
relative to each CPU's local clock rather than the global event time?

**Action**: Prototype both and compare. A per-CPU window based on
local_clock may capture the physical intuition more precisely but adds
complexity.

### 6.6 What About the Dispatch Path?

`dispatch_concurrent()` (engine.rs:2877) already handles concurrent
dispatch regardless of timestamps. Should a widened event-loop window
subsume this path, or should they remain separate? Currently they serve
different purposes: `dispatch_concurrent` handles the case where a task
wake finds multiple idle CPUs; event-loop batching handles per-CPU
events at the same time.

**Action**: Analyze whether unifying these paths simplifies the
architecture or creates new complexity.

---

## References

### Literature
1. Fujimoto, R. "Exploiting Temporal Uncertainty in Parallel and Distributed Simulations." 1999. [IEEE Xplore](https://ieeexplore.ieee.org/document/766160)
2. Fujimoto, R. "Parallel and Distributed Simulation Systems." Wiley, 2000. [Amazon](https://www.amazon.com/Parallel-Distributed-Simulation-Systems-Fujimoto/dp/0471183830)
3. Wainer, G. et al. "Synchronization methods in parallel and distributed discrete-event simulation." 2012. [ScienceDirect](https://www.sciencedirect.com/science/article/pii/S1569190X12001244)
4. Varga, A. "A practical efficiency criterion for the null message algorithm." OMNeT++. [PDF](https://doc.omnetpp.org/publications/varga03criterion.pdf)
5. Lemeire, J. et al. "Lookahead Accumulation in Conservative Parallel Discrete Event Simulation." [ResearchGate PDF](https://www.researchgate.net/profile/Jan-Lemeire/publication/228970039_Lookahead_Accumulation_in_Conservative_Parallel_Discrete_Event_Simulation/links/02e7e52259c22d0f37000000/Lookahead-Accumulation-in-Conservative-Parallel-Discrete-Event-Simulation.pdf)
6. Miller, A. et al. "The Effect of Modeling Simultaneous Events on Simulation Results." [AFIT](https://scholar.afit.edu/etd/2249/)
7. Chandy, K.M. and Misra, J. "Distributed Simulation: A Case Study in Design and Verification of Distributed Programs." IEEE TSE, 1979.
8. Jefferson, D.R. "Virtual Time." ACM TOPLAS, 1985.
9. Olszewski, M. et al. "Kendo: Efficient Deterministic Multithreading in Software." ASPLOS, 2009.
10. Sherrill, W. et al. "parti-gem5: gem5's Timing Mode Parallelised." [arXiv](https://arxiv.org/abs/2308.09445)

### Codebase References
All paths relative to the scx_simulator crate root
(`<REPO_ROOT>/rust/scx_simulator/crates/scx_simulator/`):

- `src/engine.rs:188-197` — `EventQueue::drain_at()`: current same-ns batching
- `src/engine.rs:284-297` — `group_events_by_cpu()`: partitioning into global vs per-CPU
- `src/engine.rs:1101-1209` — Main event loop with concurrent batch dispatch
- `src/engine.rs:1318-1417` — `process_event()`: single event handler dispatch
- `src/engine.rs:441-494` — `charge_sched_time()`: cost model for scheduler overhead
- `src/engine.rs:487` — `MIN_CALLBACK_COST_NS = 50`: minimum callback cost
- `src/engine.rs:2877-2995` — `dispatch_concurrent()`: concurrent dispatch for idle CPUs
- `src/engine.rs:3065-3201` — `process_batch_concurrent()`: concurrent per-CPU event batch
- `src/interleave.rs:1-430` — Token-passing cooperative interleaving
- `src/preempt/mod.rs:1-100` — Preemptive interleaving via PMU RBC timer
- `src/kfuncs.rs:41-46` — kfunc cost tiers (TRIVIAL, SIMPLE, etc.)
- `src/kfuncs.rs:408-411` — `advance_cpu_clock()`: per-CPU clock advancement
- `src/kfuncs.rs:799` — `with_sim()`: kfunc wrapper that accumulates cost
- `src/cpu.rs:46` — `SimCpu::local_clock`: per-CPU logical clock
- `src/scenario.rs:474-485` — `Scenario::interleave` and `preemptive` config
