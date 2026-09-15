# Reconciling Parallel Systems with Sequential Discrete Event Simulators

## Research Survey (February 2026)

This document surveys techniques for reconciling inherently parallel/concurrent
systems (like a multi-core OS scheduler) with sequential or deterministic
discrete event simulators. The focus is on practical applicability to our
scx-sim project: a DES modeling a multi-core Linux sched_ext scheduler.

---

## 1. Parallel Discrete Event Simulation (PDES) -- Core Approaches

### 1.1 Conservative Synchronization (Chandy-Misra-Bryant, 1979)

**Core idea:** Each logical process (LP) only processes an event when it can
*guarantee* no earlier event will arrive from any other LP. Safety first: never
process speculatively.

**Mechanism:**
- The simulation is decomposed into LPs connected by channels.
- Each LP maintains an input queue per channel. It only processes an event at
  time T if it knows no event with timestamp < T can arrive on *any* input.
- **Null messages** are sent to announce "I will not send you anything before
  time T+L," where L is the *lookahead* -- a lower bound on how far in the
  future a process's output can affect another process.
- Without sufficient lookahead, conservative simulation can deadlock (all
  processes waiting on each other). Null messages break potential deadlocks.

**Strengths:** No rollback, deterministic, low memory overhead.
**Weaknesses:** Performance depends critically on *lookahead*. If events on
CPU0 can immediately trigger events on CPU1 (lookahead ~= 0), processes
serialize. For OS scheduling, inter-CPU interactions (migrations, load
balancing, IPI-triggered reschedules) have very small lookahead.

**Key references:**
- Chandy & Misra, "Distributed Simulation: A Case Study in Design and
  Verification of Distributed Programs" (1979)
- Fujimoto, "Parallel Discrete Event Simulation" (CACM 1990)
  https://www.researchgate.net/publication/228018186_Parallel_Discrete-Event_Simulation
- Fujimoto presentation: https://www.scribd.com/presentation/375020499/Fujimoto

### 1.2 Optimistic Synchronization (Time Warp / Jefferson, 1985)

**Core idea:** Process events speculatively. If a causality violation is
detected (an event arrives with a timestamp in the LP's past), *roll back* to
before that event and re-execute.

**Mechanism:**
- Each LP maintains a *state history* (periodic state snapshots or undo logs).
- When a *straggler* message arrives with timestamp T < LP's current virtual
  time, the LP rolls back to time T, restoring saved state.
- **Anti-messages** are sent to cancel any messages the LP sent during the
  rolled-back period, potentially causing cascading rollbacks.
- **GVT (Global Virtual Time):** The minimum timestamp across all unprocessed
  events and in-transit messages. State before GVT is committed and can be
  garbage collected ("fossil collection").

**Strengths:** Exploits natural parallelism; no lookahead needed; often faster
when interactions are sparse.
**Weaknesses:** Rollback overhead (state saving, anti-messages); unbounded
memory for state history; cascading rollbacks can thrash. Complex to implement
correctly.

**Key frameworks:**
- **ROSS** (Rensselaer's Optimistic Simulation System): A high-performance Time
  Warp engine in C. Uses *reverse computation* instead of state saving -- each
  event handler has a reverse handler that undoes its effects, trading CPU for
  memory. Scaled to millions of cores.
  https://ross-org.github.io/about.html
  https://ross-org.github.io/ROSS-docs/docs/html/
- **Warped2**: A more recent C++ Time Warp framework.
  https://arxiv.org/abs/2507.18050v1

**Key references:**
- Jefferson, "Virtual Time" (1985) -- the original Time Warp paper
- "A Brief History of Time Warp": https://link.springer.com/chapter/10.1007/978-3-319-64182-9_7
- Rollback reduction: "STW: Switch Time Warp" -- https://www.worldscientific.com/doi/10.1142/9781848160170_0047

### 1.3 Virtual Time III: Unification (Jefferson & Misra, 2017-2022)

A recent line of work that unifies conservative and optimistic approaches into
a single framework. LPs can independently choose conservative or optimistic
modes, adapting dynamically based on local conditions.

**References:**
- https://ieeexplore.ieee.org/abstract/document/8247832/
- https://dl.acm.org/doi/abs/10.1145/3505248
- https://www.osti.gov/biblio/1986609

### 1.4 Window-Based / Bounded Optimism

**Core idea:** Process all events within a bounded time window concurrently,
then synchronize at window boundaries (barrier). A hybrid between conservative
(safe within window) and optimistic (speculative across windows).

**Mechanism:**
- Define a time window W. All events with timestamps in [T, T+W) are processed
  concurrently. At the end of the window, a global barrier synchronizes all LPs.
- **Window Racer** (recent algorithm, 2024-2025): A bounded-optimism PDES
  algorithm that dynamically adjusts the window size. Compared favorably to pure
  Time Warp in recent benchmarks.
  https://www.spiedigitallibrary.org/conference-proceedings-of-spie/13651/136510D/
- Events that generate new events within the same window may need special
  handling (either buffered until next window, or processed optimistically with
  rollback).

**Relevance to our design:** This is closest to our "widen the batch window"
idea. The key question is what invariants hold within a window -- if events in
the window can interact (e.g., CPU0 migrates a task to CPU1), intra-window
ordering matters.

---

## 2. Temporal Decoupling / Quantum-Based Synchronization

This approach, heavily used in hardware simulation, is arguably the most
directly relevant to our problem.

### 2.1 The Concept

Used by SystemC TLM, gem5, QEMU, and other multi-core simulators:

- Each simulated core runs *independently* and advances its local virtual clock.
- Cores synchronize at **quantum boundaries** -- periodic synchronization
  points separated by a configurable time quantum Q.
- Within a quantum, cores are "temporally decoupled" -- they do not observe each
  other's state changes. They can run ahead up to Q time units beyond the global
  synchronized time.
- At each quantum boundary, all cores synchronize: cross-core effects (cache
  coherence, interrupts, shared memory) are resolved.

### 2.2 Accuracy vs. Performance Tradeoff

- **Small Q (e.g., 1ns):** Near-cycle-accurate, but cores synchronize almost
  every cycle -- minimal parallelism, essentially sequential.
- **Large Q (e.g., 1ms):** High parallelism, but events that *should* interact
  within the quantum are invisible to each other until the boundary. Can miss
  race conditions, produce unrealistic interleavings.
- **The "optimal quantum"** depends on the frequency of cross-core interactions.
  Blog post analysis: https://www.chciken.com/simulation/2023/11/14/the-optimal-quantum.html
  (title: "The Optimal Quantum of Temporal Decoupling")

### 2.3 Key Implementations

**gem5 (parti-gem5):**
- The standard gem5 simulator is sequential. parti-gem5 (2023) parallelizes
  gem5's timing mode by running each simulated core in a separate host thread.
- Uses a quantum-based barrier: cores advance up to Q cycles, then synchronize.
- Cross-core events (cache coherence messages) are buffered and applied at
  quantum boundaries.
- https://arxiv.org/abs/2308.09445v2

**SystemC TLM 2.0:**
- The `tlm_global_quantum` and `tlm_quantumkeeper` classes implement temporal
  decoupling. Each initiator socket tracks its local time offset and yields
  control when it exceeds the global quantum.
- Accellera forum discussion: https://forums.accellera.org/topic/7111-temporal-decoupling-global_quantum/

**QEMU (Parallelized-QEMU):**
- QEMU's TCG historically ran vCPUs in round-robin on one host thread.
  Parallelized-QEMU (2021) enables true parallel vCPU execution with a
  synchronization quantum (the icount mode's "virtual time step").
- Each vCPU runs independently within a quantum; cross-vCPU interactions
  (MMIO, IPI) force synchronization.
- https://www.mdpi.com/2079-9292/10/6/759

**SST (Structural Simulation Toolkit):**
- Sandia's PDES framework for computer architecture simulation.
- Each component (core, cache, memory controller) is an LP.
- Uses a conservative synchronization algorithm with configurable link
  latencies providing the lookahead.
- https://github.com/sstsimulator/sst-core

### 2.4 Relevance to Our Design

Our `NativeOrchestrator` approach -- continuous per-CPU threads with
clock-window throttling -- is essentially temporal decoupling with a quantum.
The quantum Q corresponds to our clock window W. The literature confirms this
is a well-established and practical approach. Key design decisions:

1. **Quantum size:** Must be tuned. For scheduler simulation, cross-CPU events
   (task migrations, load balance ticks, IPI reschedules) set the lower bound
   on useful Q. If we set Q too large, we miss realistic concurrency bugs. If
   too small, we lose parallelism.

2. **Cross-core event handling:** When CPU0 dispatches a task to CPU1 during a
   quantum, does CPU1 see it immediately or at the next boundary? In our case,
   scheduler dispatch calls are cross-core events that need careful handling.

3. **Determinism:** Temporal decoupling inherently introduces non-determinism
   (the exact interleaving depends on host thread scheduling). For
   deterministic replay, we would need to record and replay the ordering of
   cross-core events within each quantum.

---

## 3. OS / Kernel Scheduling Simulators

### 3.1 Existing Work

Surprisingly little work exists on *faithful* multi-core kernel scheduler
simulation at the level of detail we're targeting:

**SimGrid:**
- A well-established simulator for distributed/parallel computing systems.
- Models multi-core machines, task scheduling, network, and I/O.
- Uses a *centralized sequential DES* engine -- does NOT parallelize the
  simulation itself, even when modeling parallel systems.
- Design philosophy: "SimGrid is a sequential simulator that models concurrent
  systems." The concurrency of the modeled system is captured in the event
  ordering, not in the simulator's execution.
- https://simgrid.org/doc/latest/Design_goals.html

**ns-3 (network simulator):**
- Has a distributed simulation mode using MPI with conservative (CMB-style)
  synchronization.
- Network links provide natural lookahead (propagation delay).
- https://www.semanticscholar.org/paper/Distributed-simulation-with-MPI-in-ns-3-Pelkey-Riley/e647b567562af225048daeed1af7ccbd22ba5

**Academic OS scheduling simulators:**
- Most are simple pedagogical tools (single-queue, single-core, FIFO/RR/SJF).
  e.g., https://github.com/hs-harsh/OS-Ass2-Discrete-event-process-scheduling-simulator
- None found that model sched_ext BPF callbacks, multi-core dispatch, or
  realistic kernel concurrency at the level we're targeting.

### 3.2 Implication

We appear to be in relatively novel territory. The closest analog is hardware
simulation (gem5, SST), where the "system being simulated" has inherent
concurrency across cores and the simulator must decide how to model it. SimGrid
validates that a sequential DES modeling a concurrent system is a mainstream
and defensible approach.

---

## 4. Approaches to Our Specific Challenge

Given our context -- a DES modeling a multi-core scheduler where events on
different CPUs at nearby timestamps would execute concurrently in reality --
here is how the surveyed techniques map to our design options:

### 4.1 Exact-Timestamp Batching (Current Approach)

- Events at the *exact same nanosecond* are treated as concurrent, processed
  in a batch (all orderings are valid).
- Events at different timestamps are strictly ordered.
- **Assessment:** Overly conservative. In reality, events 1-10ns apart on
  different CPUs would execute truly concurrently -- they have no causal
  ordering. By serializing them, we may miss concurrency bugs and over-constrain
  the exploration space.

### 4.2 Widened Batch Window (Epsilon-Concurrent)

- All events within W nanoseconds of each other are treated as concurrent.
- Process them in a batch; explore different orderings.
- This is analogous to **window-based PDES** (Section 1.4) where W is the
  window size.

**Tradeoffs:**
- W too small: miss realistic concurrency.
- W too large: batch unrelated events, exploding the permutation space.
- Need a principled basis for W. Candidate: the minimum time for a cross-core
  effect to propagate (IPI latency, cache line transfer time ~ 50-200ns on
  modern hardware).
- **Determinism:** Within a batch, we need to either explore all orderings
  (combinatorial explosion) or pick one randomly (requires seed for replay).

### 4.3 Island-Based Grouping

- Group events into "islands" separated by idle gaps (no events for >= G ns).
- Within an island, all events are concurrent; between islands, strict ordering.
- A natural extension of epsilon-concurrent batching with adaptive window sizing.

**Tradeoffs:**
- Simple conceptually, but islands can grow very large under heavy load
  (no gaps), making the batch enormous.
- Works well for bursty workloads with natural idle periods.

### 4.4 Preemption-Point Based

- Events that start within N ns of each other on different CPUs are concurrent
  until a preemption point (e.g., after some scheduler overhead duration).
- Models the idea that "CPU0 is in the middle of ops.enqueue when CPU1 starts
  ops.select_cpu" -- they're truly concurrent until one finishes and does
  something globally visible.

**Assessment:** This is the most semantically accurate model. It maps naturally
to the real kernel, where each CPU holds `rq->lock` for its local queue but
operations on different CPUs' queues truly overlap. The "preemption point" is
the moment a CPU touches shared state (global DSQ, task migration, etc.).

### 4.5 Continuous Per-CPU Threads with Quantum Throttling (NativeOrchestrator)

- Each CPU runs as a separate thread, advancing independently.
- A clock window (quantum Q) limits how far ahead any CPU can get relative to
  the slowest.
- Cross-CPU operations (dispatch to another CPU's DSQ, kick_cpu, etc.) either
  synchronize immediately or are buffered until the next quantum boundary.

**Assessment:** This is precisely the **temporal decoupling** approach used by
gem5/SystemC/QEMU (Section 2). It is the most mature and well-understood
approach for this class of problem. Key insight from the literature:

> "The quantum size should be chosen based on the expected frequency of
> cross-core interactions. For systems with frequent inter-core communication,
> smaller quanta are needed for accuracy, at the cost of parallelism."

For kernel scheduling, cross-core interactions include:
- `scx_bpf_dispatch()` to a remote CPU's local DSQ
- `scx_bpf_kick_cpu()` / IPI for rescheduling
- Load balancing timer ticks
- `select_cpu()` examining remote CPU state

The frequency of these interactions is workload-dependent, making a fixed
quantum suboptimal. An *adaptive quantum* (like Window Racer's approach) would
dynamically adjust Q based on observed cross-core event frequency.

---

## 5. Recommendations for Our Design

### 5.1 The Temporal Decoupling / Quantum Approach Is Best-Validated

The literature strongly supports the NativeOrchestrator's approach (per-CPU
threads with clock-window throttling). This is exactly temporal decoupling, and
it is the standard approach in gem5, SystemC TLM, QEMU, and SST for simulating
inherently parallel hardware. The key parameters to get right:

1. **Quantum size Q:** Start with a configurable value. Reasonable defaults for
   scheduler simulation might be 100ns-1us (roughly the duration of a scheduler
   operation like enqueue/dequeue/dispatch).

2. **Cross-core event handling:** When a cross-core event occurs (dispatch to
   remote DSQ, kick_cpu), force a synchronization point -- do not defer to the
   quantum boundary. This is what gem5 calls a "timing annotation" and what
   SystemC TLM calls a "quantum keeper sync."

3. **Determinism strategy:** For reproducibility, record the sequence of
   cross-core synchronization events and their virtual timestamps. On replay,
   enforce the same ordering.

### 5.2 Optimistic (Time Warp) Is Likely Overkill

Time Warp requires reversible state, anti-messages, and GVT computation. For
scheduler simulation where cross-core interactions are frequent and state is
complex (task state, DSQ contents, vruntime trees), rollback would be expensive
and complex to implement. The ROI is poor for our use case.

### 5.3 Conservative (CMB) Has a Lookahead Problem

The kernel scheduler has near-zero lookahead for cross-core events (a
select_cpu call can immediately trigger a dispatch to any other CPU). This
would force conservative simulation to essentially serialize, losing any
benefit of parallelism.

### 5.4 Widened Batching Is a Simpler Alternative

If the full NativeOrchestrator (per-CPU threads) is too complex, a simpler
approach is to widen the batch window in the existing sequential DES:
- Define W (e.g., 100ns).
- All events with timestamps in [T, T+W) are collected into a batch.
- Within the batch, process events in random order (seeded for replay).
- This gives some concurrency exploration without requiring threads.

This is essentially what Window Racer does, and it is a reasonable stepping
stone toward full temporal decoupling.

---

## 6. Key References Summary

| Topic | Reference | URL |
|-------|-----------|-----|
| PDES overview (Fujimoto) | "Parallel Discrete Event Simulation" (1990) | https://www.researchgate.net/publication/228018186 |
| Conservative sync | Chandy & Misra (1979) | (foundational paper, widely cited) |
| Optimistic sync | Jefferson, "Virtual Time" (1985) | (foundational paper) |
| ROSS framework | Carothers et al. | https://ross-org.github.io/about.html |
| Virtual Time III (unified) | Jefferson & Misra (2022) | https://dl.acm.org/doi/abs/10.1145/3505248 |
| Window Racer | SPIE 2024 | https://www.spiedigitallibrary.org/conference-proceedings-of-spie/13651/136510D/ |
| Temporal decoupling | "The Optimal Quantum" blog | https://www.chciken.com/simulation/2023/11/14/the-optimal-quantum.html |
| parti-gem5 | Parallelized gem5 timing (2023) | https://arxiv.org/abs/2308.09445v2 |
| Parallelized QEMU | Multi-vCPU synchronization (2021) | https://www.mdpi.com/2079-9292/10/6/759 |
| SST Core | Sandia PDES framework | https://github.com/sstsimulator/sst-core |
| SimGrid | Sequential DES for concurrent systems | https://simgrid.org/doc/latest/Design_goals.html |
| ns-3 distributed | Conservative PDES with MPI | Pelkey & Riley, ns-3 distributed simulation |
| Sync methods survey | Perumalla (2005) | https://www.researchgate.net/publication/230636266 |
| PARSIR | Multi-processor PDES package | https://arxiv.org/abs/2410.00644 |
| Lamport clocks | Happened-before, causality | Lamport, "Time, Clocks, and the Ordering of Events" (1978) |
