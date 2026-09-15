# EngineRing Concurrency Protocol

This document describes the complete concurrency protocol of the
EngineRing-based dispatch system. It covers the thread architecture,
shared state inventory, synchronization primitives, protocol state
machines, and an informal correctness argument.

Target audience: an engineer who needs to modify this code and must
understand what invariants to maintain.

---

## 1. Thread Architecture

### 1.1 Thread Roles

| Thread           | Count   | Lifecycle                                    |
|------------------|---------|----------------------------------------------|
| **Engine**       | 1       | The calling thread that invokes `engine_loop` |
| **Workers**      | 1-64    | One per simulated CPU; `thread::scope` or `WorkerPool` |

There are two worker spawning strategies:

- **Scoped threads** (`std::thread::scope`): Workers are spawned per
  dispatch round via `run_cooperative_dispatch`, `run_preemptive_dispatch`,
  etc. The scope guarantees all workers join before the function returns,
  which ensures the `EngineRing`, `PreemptRing`, and `SimulatorState` all
  outlive the workers.

- **Persistent pool** (`WorkerPool`): Workers are spawned once and park
  between rounds via futex. TLS is installed at creation. Each round, the
  engine writes `WorkDesc` to each worker's slot, issues a command, and
  waits for completion. This is a separate layer that sits *above* the
  EngineRing protocol (a pool round dispatches work that internally uses
  EngineRing for token passing).

### 1.2 Execution Model

Only one worker executes at a time. The system is a cooperative
single-threaded simulation running on multiple OS threads, where the
EngineRing serializes access to shared `SimulatorState`.

The engine thread does not execute simulation work. It receives yield
notifications, reads CPU clocks from `SimulatorState` (safe because all
workers are parked when the engine runs), picks the next worker by
minimum local clock, and wakes it.

---

## 2. Shared State Inventory

### 2.1 EngineRing Fields

```
EngineRing {
    workers: Box<[AtomicWorkerState]>,     // Per-worker: Parked(0) | Running(1)
    engine_wake: AtomicEngineWake,         // Sleeping(0) | Woken(1)
    yielded_worker: AtomicYieldedWorker,   // WorkerId of last yielder
    yield_reason: AtomicYieldReason,       // Cooperative | Preemption | Finished
    finished_mask: AtomicFinishedMask,     // Bitmask of completed workers (u64)
    total: usize,                          // Immutable after construction
    worker_cpu_map: Box<[CpuId]>,          // Immutable after construction
}
```

**Access pattern:**

| Field            | Written by        | Read by          | Ordering |
|------------------|-------------------|------------------|----------|
| `workers[i]`     | Worker i (park), Engine (set_running) | Worker i (wait loop) | SeqCst |
| `engine_wake`    | Worker (store Woken), Engine (swap Sleeping) | Engine (swap + futex_wait) | SeqCst |
| `yielded_worker` | Worker (before waking engine) | Engine (after swap returns Woken) | SeqCst |
| `yield_reason`   | Worker (before waking engine) | Engine (after swap returns Woken) | SeqCst |
| `finished_mask`  | Worker (fetch_or) | Engine (load)    | SeqCst |

### 2.2 SimulatorState

`SimulatorState` is accessed through raw pointers (`SendPtr<SimulatorState>`)
that are sent across thread boundaries. Only the token holder may access
it. The engine thread accesses it when all workers are parked (e.g., to
read `cpus[cpu].local_clock` for scheduling decisions).

The `SendPtr` wrapper's `Send`/`Sync` impls are the key unsafe
assertion: the token-passing protocol serializes access, making it safe
despite using raw pointers.

### 2.3 Thread-Local State (TLS)

Three categories of TLS exist:

| TLS Variable       | Purpose                              | Installed by           |
|---------------------|--------------------------------------|------------------------|
| `INTERLEAVE_CTX`    | Cooperative yield fn pointer + WorkerId | `install_engine_ring()` |
| `PREEMPT_CTX`       | PMU timer fd, ring pointers, worker id | `preempt::install()`   |
| `PREEMPT_INHIBIT`   | Nesting counter to suppress PMU yields | `inhibit_preemption()` |
| `REPLAY_CTX`        | Replay cursor + breakpoint state       | `install_replay_ctx()` |
| `E9_REPLAY_CTX`     | e9patch replay variant                 | `install_e9_replay_ctx()` |
| `STRUCTOP_*` cells  | Per-worker structop counters           | `seed_structop()`      |
| `CALLBACK_CTX`      | Per-callback CPU/ops context           | `install_callback_ctx()` |
| `SIM_ARC`           | Arc to the sim mutex (for kfuncs)      | `install_sim_arc()`    |

All TLS variables use `Cell<Option<T>>` or `Cell<T>` for `Copy` types.
`Cell` is async-signal-safe because it requires no locking and operates
on a single thread (the signal handler runs on the interrupted thread's
stack).

### 2.4 PreemptRing Counters and Records

```
PreemptRing {
    total: usize,                           // Immutable
    timeslice: TimeslicePrng,               // AtomicU32, CAS-based PRNG
    signal_preempt_count: AtomicU64,        // fetch_add from signal handler
    cooperative_yield_count: AtomicU64,     // fetch_add from coop yield
    preemption_records: PreemptionRecordStore, // Fixed-size AtomicU64 array
}
```

The `PreemptionRecordStore` is a pre-allocated fixed-size array of
`AtomicU64` slots (no heap allocation in the signal handler). The count
index is atomically incremented; records beyond `MAX_PREEMPTION_RECORDS`
are dropped.

---

## 3. Synchronization Primitives

### 3.1 Futex Words

The system uses three categories of futex words:

1. **`workers[i]`** (per-worker `AtomicU32`): Worker i futex-waits on
   its own word with expected value `Parked(0)`. Engine futex-wakes it
   after storing `Running(1)`.

2. **`engine_wake`** (`AtomicU32`): Engine futex-waits on this word with
   expected value `Sleeping(0)`. Workers store `Woken(1)` and
   futex-wake(1) to notify the engine.

3. **WorkerPool `commands[i]` / `completions[i]`** (`AtomicU32`):
   Per-worker command and completion futex words for the persistent pool
   protocol (separate from the EngineRing yield protocol).

All futex operations use `FUTEX_PRIVATE_FLAG` since all threads share
the same address space.

### 3.2 Atomic Ordering

All atomic operations use `SeqCst` (sequentially consistent) ordering.
This is intentionally conservative -- it eliminates ordering subtleties
at the cost of potential memory-fence overhead. Given that workers yield
relatively infrequently (kfunc boundaries or PMU signals), the overhead
is negligible.

### 3.3 The Token-Passing Protocol

The "execution token" is a logical concept: at any point in time, at
most one worker has `WorkerState::Running`. This worker is the token
holder and has exclusive access to `SimulatorState`.

**Invariant**: Between `start_first_worker()` and all workers finishing,
exactly one worker is in the `Running` state (or zero, briefly during
the atomic swap handoff). No two workers are simultaneously `Running`.

Token transfer sequence:
1. Current holder stores `Parked` for itself
2. Current holder publishes `yielded_worker` and `yield_reason`
3. Current holder stores `Woken` to `engine_wake` and futex-wakes engine
4. Current holder futex-waits on `workers[self]` with expected `Parked`
5. Engine wakes from `engine_wake`, reads yield info, decides next
6. Engine stores `Running` for next worker, futex-wakes it
7. New holder resumes from its futex-wait loop

### 3.4 Signal Masking

SIGSTKFLT is the preemption signal. The signal handler itself does not
mask additional signals (no `SA_NODEFER` is set, but the default
behavior masks the same signal during handler execution). This prevents
recursive signal delivery.

The `PREEMPT_INHIBIT` TLS counter prevents yields from the signal
handler when the worker holds a mutex (notably `SIM_ARC`). When
inhibited, the handler simply disables the PMU timer and returns; the
next cooperative yield at a kfunc boundary will yield instead.

---

## 4. Protocol State Machines

### 4.1 EngineRing Yield Protocol

This is the core protocol. Two actors: one Worker (W) and the Engine (E).

```
  Worker W                                     Engine E
  --------                                     --------

  [RUNNING]                                    [SLEEPING on engine_wake]
      |                                             |
      | store workers[W] = Parked                   |
      | store yielded_worker = W                    |
      | store yield_reason = reason                 |
      | store engine_wake = Woken                   |
      | futex_wake(engine_wake, 1)                  |
      |                                             |
      | futex_wait(workers[W], Parked)         (wakes from futex or
      |   [PARKED]                              spurious wakeup)
      |                                             |
      |                                        old = swap(engine_wake, Sleeping)
      |                                        if old == Sleeping:
      |                                           futex_wait(engine_wake, Sleeping)
      |                                           continue (re-check)
      |                                        // old == Woken
      |                                        read yielded_worker -> W
      |                                        read yield_reason -> reason
      |                                        check finished_mask
      |                                        call on_yield(W, reason) -> next
      |                                        store workers[next] = Running
      |                                        futex_wake(workers[next], 1)
      |                                             |
      |                                        [SLEEPING on engine_wake]
      |                                             :
  (if next == W):                                   :
      | (futex_wait returns, sees Running)          :
      | [RUNNING]                                   :
```

#### Engine Wake State Machine

```
         store Woken
    +------(W)------+
    |                |
    v                |
 [Sleeping] ---- [Woken]
    ^      swap()    |
    |  returns Woken |
    +----------------+
         (E)
```

The engine atomically swaps `Sleeping` into `engine_wake`. If the old
value was `Woken`, a worker signal is consumed. If `Sleeping`, no signal
was pending and the engine blocks via `futex_wait`.

#### Worker State Machine (per worker)

```
  [Parked] -----set_running (Engine)-----> [Running]
     ^                                         |
     |                                         |
     +----------park (Worker self)-------------+
```

### 4.2 Cooperative Yield (Kfunc Boundary)

Triggered by `interleave::maybe_yield()` or `preempt::maybe_yield_preemptive()`.

```
  Worker W at kfunc entry:
      |
      | (BEFORE with_sim -- no &mut SimulatorState exists)
      |
      | [if PREEMPT_CTX installed]:
      |     inhibit_preemption()
      |     disable_timer(timer_fd)
      |     save CALLBACK_CTX
      |     disable_measurement(measure_fd)
      |     yield_to_engine(W, Cooperative)  -- BLOCKS --
      |     (resumed)
      |     restore CALLBACK_CTX
      |     enable_measurement(measure_fd)
      |     allow_preemption()
      |     (if Post phase: rearm_timer)
      |
      | [elif INTERLEAVE_CTX installed]:
      |     save CALLBACK_CTX
      |     yield_via_engine_ring(ring, W)   -- BLOCKS --
      |     (resumed)
      |     restore CALLBACK_CTX
      |
      | [else]: no-op
```

### 4.3 Preemptive Yield (PMU Signal Handler)

Triggered by SIGSTKFLT (PMU RBC counter overflow).

```
  Worker W running C scheduler code:
      |
      | --- SIGSTKFLT delivered ---
      |
      | preempt_handler():
      |     read PREEMPT_CTX -> pctx
      |     if None: return
      |     if is_preemption_inhibited():
      |         disable_timer(timer_fd)
      |         return              // defer to next coop yield
      |     disable_timer(timer_fd)
      |     disable_measurement(measure_fd)
      |     capture RIP from ucontext
      |     read RBC count from timer_fd
      |     save CALLBACK_CTX
      |     record_rbc_preemption(rbc_count)
      |     record preemption to PreemptionRecordStore
      |     yield_to_engine(W, Preemption)  -- BLOCKS --
      |     (resumed)
      |     restore CALLBACK_CTX
      |     enable_measurement(measure_fd)
      |     (timer NOT re-armed here -- deferred to resume_timer)
      |
      | --- returns from signal handler ---
      | (continues C code until next kfunc, where timer is re-armed)
```

### 4.4 Worker Finish (Non-Blocking)

```
  Worker W:
      |
      | finished_mask |= (1 << W)     // atomic fetch_or
      | store yielded_worker = W
      | store yield_reason = Finished
      | store engine_wake = Woken
      | futex_wake(engine_wake, 1)
      | return (NO futex_wait)
      |
  Engine:
      | (wakes, reads yield info)
      | checks all_done()
      | if all done: break loop
      | else: pick next non-finished worker, wake it
```

---

## 5. The Engine Loop Protocol

### 5.1 The Atomic-Swap Wakeup Pattern

The engine uses an atomic swap to detect pending wakeups without lost
signals. This is the standard "edge-triggered futex" pattern:

```rust
loop {
    let old = engine_wake.swap(Sleeping);  // SeqCst
    if old == Sleeping {
        // No pending signal. Block.
        futex_wait(engine_wake, Sleeping);
        continue; // Spurious wakeup possible.
    }
    // old == Woken: a worker yielded. Process it.
    ...
}
```

**Why this is correct**: The swap atomically consumes the `Woken` signal
and resets to `Sleeping` in one operation. Three races are handled:

1. **Worker signals before engine swaps**: `swap` returns `Woken`, engine
   processes immediately without blocking.

2. **Worker signals after swap but before futex_wait**: The worker's
   `store(Woken)` changes the futex word to `Woken`. `futex_wait(Sleeping)`
   sees the value is no longer `Sleeping` and returns immediately
   (spurious from the kernel's perspective, but correct). The engine
   loops, swaps, and finds `Woken`.

3. **Worker signals after futex_wait begins**: `futex_wake` wakes the
   engine. `futex_wait` returns. Engine loops, swaps, finds `Woken`.

No lost wakeup is possible because `swap` and `futex_wait` operate on
the same atomic word: if the value changed between the swap and the
wait, the wait returns immediately.

### 5.2 The Yielded Worker / Yield Reason Publication Protocol

The yielding worker publishes yield info in a specific order:

```
1. workers[W].park()              // SeqCst store
2. yielded_worker.store(W)        // SeqCst store
3. yield_reason.store(reason)     // SeqCst store
4. engine_wake.store(Woken)       // SeqCst store
5. futex_wake(engine_wake)
```

The engine reads yield info only after observing `Woken` via the atomic
swap (step 4's store happens-before the engine's swap load under SeqCst):

```
old = engine_wake.swap(Sleeping)  // observes Woken
// => happens-after step 4
// => happens-after steps 2 and 3 (SeqCst total order)
yielded = yielded_worker.load()   // sees the value from step 2
reason = yield_reason.load()      // sees the value from step 3
```

### 5.3 Why Concurrent Yields Cannot Happen

The token-passing protocol guarantees that at most one worker is
`Running` at any time. A worker can only yield when it holds the token
(is `Running`). Therefore:

- Only one worker can be in `yield_to_engine` at a time.
- `yielded_worker` and `yield_reason` are a "single-slot mailbox" --
  written by the yielding worker and read by the engine before any other
  worker can yield.
- The engine reads the mailbox, makes a decision, wakes the *next*
  worker, and goes back to sleep. Only then can the next worker
  potentially yield, overwriting the mailbox.

This serialization means the single-slot `yielded_worker`/`yield_reason`
pair is never subject to concurrent writes.

### 5.4 The finish_worker Non-Blocking Path

When a worker finishes, it does NOT block:

```rust
pub fn finish_worker(&self, worker_id: WorkerId) {
    self.finished_mask.mark_finished(worker_id);  // atomic fetch_or
    self.yielded_worker.store(worker_id);
    self.yield_reason.store(YieldReason::Finished);
    self.engine_wake.wake();
    // returns immediately -- no futex_wait
}
```

This is safe because:
- The finished worker will not access `SimulatorState` again after
  calling `finish_worker` (the post-finish code only does TLS cleanup).
- The engine wakes, reads the Finished yield, and picks the next
  non-finished worker. The finished worker's thread continues to run
  concurrently with the next worker, but it only performs
  signal-handler-free cleanup (uninstall TLS, close fds).
- `clear_ops_and_finish` calls `exit_sim_no_clear_ops()` *after*
  `finish_worker`, but it only clears TLS -- it does not access shared
  `SimulatorState`.
- The `thread::scope` join ensures the finished worker's thread
  completes before the scope exits and `EngineRing` is dropped.

---

## 6. Signal Handler Safety

The signal handler (`preempt_handler`) is fully async-signal-safe. This
is guaranteed by restricting it to the following primitives:

### 6.1 Allowed Operations

| Operation                    | Why it is async-signal-safe            |
|------------------------------|----------------------------------------|
| `Cell<T>.get()`/`.set()`     | Thread-local, no lock, single-threaded |
| `AtomicU32.store/load/swap`  | Lock-free CPU instructions             |
| `AtomicU64.fetch_add/store`  | Lock-free CPU instructions             |
| `futex_wait/futex_wake`      | Raw syscall, POSIX-safe                |
| `libc::ioctl` on perf fd     | POSIX async-signal-safe                |
| `libc::read` on perf fd      | POSIX async-signal-safe                |
| `libc::write(STDERR_FILENO)` | POSIX async-signal-safe                |
| Raw pointer dereference      | No library calls                       |

### 6.2 Forbidden Operations

The handler must NOT use:
- `Mutex`, `RwLock`, `Condvar` (may deadlock)
- `tracing::trace!` or similar (uses internal mutexes)
- Heap allocation (`Box::new`, `Vec::push`, etc.)
- `println!` / `eprintln!` (uses `stdout`/`stderr` `Mutex` internally)

For trace output, the handler checks `tracing::level_filters::LevelFilter::current()`
(an atomic read) and writes directly to stderr via `libc::write` using a
stack-allocated `StackWriter` buffer.

### 6.3 Constraints on Calling Code

1. **No `&mut SimulatorState` across yield points**: `maybe_yield()` must
   be called BEFORE `with_sim()`, so no mutable reference exists when the
   worker parks.

2. **Inhibit preemption around mutex acquisition**: Before locking
   `SIM_ARC` (or any other mutex), call `inhibit_preemption()`. This
   prevents the scenario: signal fires -> handler tries `yield_to_engine`
   -> engine wakes another worker -> that worker blocks on the same
   mutex -> deadlock.

3. **Timer disabled during kfuncs**: The PMU timer is disabled via
   `pause_timer()` at kfunc entry and re-armed via `resume_timer()` at
   kfunc exit. This ensures signals only fire during scheduler C code.

---

## 7. Informal Correctness Argument

### 7.1 No Deadlock

**Claim**: Every `futex_wait` has a guaranteed matching waker.

**Worker waiting on `workers[W]`**:

The worker calls `futex_wait(workers[W], Parked)` only after parking
itself. Before parking, it woke the engine via `engine_wake.wake()`.
The engine will eventually process this wakeup (see below for why
it cannot miss it). The engine's `on_yield` callback returns
`Some(next)` as long as non-finished workers exist. The engine stores
`Running` for `next` and futex-wakes it. If `next == W` (same worker
re-selected), W is woken immediately. If `next != W`, W remains parked
until a future engine decision selects it.

For `finish_worker`: the worker does NOT wait, so no deadlock.

**Engine waiting on `engine_wake`**:

The engine calls `futex_wait(engine_wake, Sleeping)` only after
`swap(Sleeping)` returned `Sleeping`. If a worker subsequently calls
`engine_wake.wake()` (store `Woken` + `futex_wake`), the engine is
woken. If the worker's store happens between the swap and the wait,
`futex_wait` sees the value is no longer `Sleeping` and returns
immediately.

The engine cannot deadlock because:
- It woke a worker (or the first worker was started by
  `start_first_worker`).
- The woken worker must eventually either `yield_to_engine` (which wakes
  the engine) or `finish_worker` (which also wakes the engine).
- Workers cannot silently hang: they execute deterministic simulator
  code that always reaches a kfunc boundary (cooperative yield) or a PMU
  timer overflow (preemptive yield), or finishes.

**Preempt-inhibit cannot cause deadlock**:

When preemption is inhibited and a PMU signal fires, the handler disables
the timer and returns without yielding. The worker continues to the next
kfunc boundary, where `cooperative_yield_impl` performs the yield. Since
kfunc boundaries are guaranteed to be reached (scheduler code always
calls kfuncs), the yield is merely deferred, not lost.

### 7.2 No Lost Wakeup

**Claim**: The atomic-swap pattern prevents lost wakeups.

The only race window for a lost wakeup is:

```
Engine: swap(Sleeping) -> returns Sleeping
... gap ...
Engine: futex_wait(engine_wake, Sleeping)
```

If a worker stores `Woken` and calls `futex_wake` during the gap:
- `futex_wait` checks the value atomically. It sees `Woken != Sleeping`
  and returns immediately (EAGAIN).
- The engine loops and swaps again, consuming the `Woken` signal.

If a worker stores `Woken` after `futex_wait` begins sleeping:
- `futex_wake` wakes the engine from sleep.
- The engine loops and swaps, consuming the signal.

Both cases are handled correctly. The key insight is that `swap` and
`futex_wait` operate on the **same atomic word**, so there is no TOCTOU
gap between checking and sleeping.

### 7.3 No Data Race on Shared State

**Claim**: `SimulatorState` is never accessed concurrently.

The token-passing protocol ensures at most one worker is `Running` at a
time. The `Running` worker has exclusive access to `SimulatorState`.
When the worker yields:

1. It first parks itself (`workers[W] = Parked`), relinquishing the token.
2. It then wakes the engine.
3. The engine reads `SimulatorState` (e.g., `local_clock`) when all
   workers are parked. This is safe because no worker can be accessing
   the state.
4. The engine wakes the next worker, which resumes and accesses
   `SimulatorState`.

Steps 1-4 form a happens-before chain under SeqCst ordering:
- Worker's `park()` store happens-before `engine_wake.store(Woken)`
- Engine's `swap` load happens-after `engine_wake.store(Woken)`
- Engine's state reads happen-after the swap
- Engine's `set_running(next)` store happens-after its state reads
- Next worker's futex-wait return happens-after `set_running`

The `SendPtr<SimulatorState>` wrapper's `unsafe impl Send + Sync` is
justified by this serialization.

### 7.4 No Use-After-Free

**Claim**: All shared data outlives all worker accesses.

**`thread::scope` path**: The `EngineRing`, `PreemptRing`, and
`SimulatorState` are allocated on the scope-parent's stack (or owned by
the engine). `std::thread::scope` guarantees all spawned threads are
joined before the scope block exits. Therefore, the shared data outlives
all workers.

**`WorkerPool` path**: The pool owns the `commands`, `completions`, and
`work_descs` arrays. `Drop::drop` calls `shutdown()`, which sends
`Shutdown` to all workers and joins all thread handles before the arrays
are freed.

**TLS pointers**: `PREEMPT_CTX` and `INTERLEAVE_CTX` hold raw pointers
to `PreemptRing` and `EngineRing`. These pointers are set from references
that live in a `thread::scope` block. Workers call `uninstall()` to clear
the TLS before the scope exits.

---

## 8. Known Limitations and Open Issues

### 8.1 Double Futex Round-Trip Performance Overhead

Every yield requires two futex round-trips:

```
Worker -> Engine:  worker parks + futex_wake(engine_wake) + engine futex_wait return
Engine -> Worker:  engine stores Running + futex_wake(workers[next]) + worker futex_wait return
```

A direct-handoff design (worker directly wakes the next worker, like
PreemptRing's old PRNG-based approach) would need only one round-trip.
The extra round-trip is the cost of engine-mediated scheduling decisions.

In practice, the overhead is small relative to the scheduler C code
execution time between yields. The engine's decision logic (min-clock
pick) is O(N) where N is the number of workers (typically 2-8).

### 8.2 Single-Slot Yielded Worker Assumption

`yielded_worker` and `yield_reason` are a single-slot mailbox without
any buffering. This is correct *only because* the token-passing protocol
serializes yields: at most one worker can yield at a time, and the engine
processes the yield before any other worker can run.

If this invariant were violated (e.g., by a bug that marks two workers
as `Running` simultaneously), the single-slot mailbox would lose yield
information and the engine would process stale data.

The `finished_mask` is safe from this concern because it uses `fetch_or`
(each worker sets only its own bit), but `yielded_worker`/`yield_reason`
are plain stores that would clobber each other under concurrent access.

### 8.3 SeqCst Everywhere

All atomics use `SeqCst`, which is the strongest (and most expensive)
ordering. In principle, many of these could use `Release`/`Acquire` pairs
(e.g., the worker's publication stores need `Release`, the engine's reads
need `Acquire`). However, the performance difference is negligible in
this context (yields happen at kfunc granularity, not per-instruction),
and `SeqCst` eliminates subtle ordering bugs.

### 8.4 Futex Duplication

`futex_wait` and `futex_wake` are defined as local functions in three
places: `engine_ring.rs`, `worker_pool.rs`, and (historically)
`preempt/mod.rs`. These are marked for deduplication into a shared
crate-level module in Phase E.

### 8.5 PRNG Consumption Coupling

In preemptive mode, the `TimeslicePrng` is consumed at every
`rearm_timer` call to maintain deterministic sequencing. In replay mode,
the rolled timeslice is discarded (replay manages its own timer periods),
but consuming it keeps the PRNG synchronized with the recording run. If
a code path rearms the timer a different number of times between
recording and replay, the PRNG sequence will diverge and replay
scheduling decisions may differ. This is detected by the replay overshoot
retry logic.

---

## Appendix A: Complete Yield Sequence (Preemptive, PMU Signal)

For reference, here is the complete sequence of events for a single
PMU-triggered preemption:

```
 C scheduler code executing on Worker W
      |
 1.   PMU counter overflows -> kernel delivers SIGSTKFLT
 2.   preempt_handler() entry
 3.     read PREEMPT_CTX -> pctx (TLS Cell)
 4.     check PREEMPT_INHIBIT -> not inhibited
 5.     ioctl(timer_fd, DISABLE)          // prevent recursive signals
 6.     ioctl(measure_fd, DISABLE)        // stop counting handler branches
 7.     extract RIP from ucontext
 8.     read(timer_fd) -> rbc_count
 9.     read CALLBACK_CTX -> saved        // save per-callback context
10.     record_rbc_preemption(rbc_count)   // update TLS cumulative RBC
11.     structop_info() -> sinfo           // snapshot all TLS counters
12.     ring.record_preemption(...)        // write to PreemptionRecordStore
13.     ring.inc_signal_preempt()          // atomic counter
14.  -- yield_to_engine(W, Preemption) --
15.     workers[W].park()                  // store Parked
16.     yielded_worker.store(W)
17.     yield_reason.store(Preemption)
18.     engine_wake.store(Woken)
19.     futex_wake(engine_wake, 1)
20.     futex_wait(workers[W], Parked)     // BLOCKS
           ...
           (Engine runs: swap, read yield info, pick next by min-clock,
            wake next worker. If next == W, futex_wait returns.)
           ...
21.     (futex_wait returns, workers[W] == Running)
22.  -- yield_to_engine returns true --
23.     inc_interleave()                   // TLS counter
24.     install_callback_ctx(saved)        // restore per-callback context
25.     ioctl(measure_fd, ENABLE)          // resume counting
26.  preempt_handler() returns
27.  (timer NOT re-armed -- waits for next kfunc's resume_timer)
28.  C scheduler code continues
```

## Appendix B: Engine Decision Loop

```
engine_loop(on_yield):
    loop:
        old = engine_wake.swap(Sleeping)       // SeqCst
        if old == Sleeping:
            futex_wait(engine_wake, Sleeping)   // block
            continue                            // spurious or real wakeup

        // old == Woken: a worker yielded
        yielded = yielded_worker.load()
        reason = yield_reason.load()

        if all_done():
            break

        match on_yield(yielded, reason):
            Some(next):
                workers[next].set_running()     // store Running
                workers[next].futex_wake_one()  // wake worker
            None:
                break                           // callback says stop
```

The `on_yield` callback for dispatch/batch mode reads `local_clock` from
each CPU's state (safe because all workers are parked) and picks the
non-finished worker with the smallest clock, breaking ties by CpuId.
