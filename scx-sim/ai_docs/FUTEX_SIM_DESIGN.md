# Futex Simulation Event Model & LAVD Integration — Design

Status: design (tg `design-futex-sim`, 2026-07-23). Author: lavd-gaps.
Depends on the three completed research tasks: `research-lavd-futex-hooks`
(lock.bpf.c map), `research-scxsim-event-model` (engine + injection points),
`research-rtapp-locks` (rt-app lock→futex behavior). Tracked substrate issue:
mb **sim-0957ea**.

---

## 1. Motivation & scope

LAVD's futex **lock-holder boosting** subsystem (`lock.bpf.c`, 12 SEC hooks +
`inc/dec/reset_futex_boost`) runs **0% under scxsim** even though `lock.bpf.c`
is compiled into `libscx_lavd.so` (`schedulers/lavd/wrapper.c:584 #include
"lock.bpf.c"`). Nothing in the engine ever invokes the hooks
(`COVERAGE_AUDIT_20260722.md §3a`: 16/17 futex functions dark). The *consumer*
side (`is_lock_holder(_running)`, `reset_lock_futex_boost`, the `lat_cri`
`NEED_LOCK_BOOST` branch) executes every scheduling cycle but always with
`LAVD_FLAG_FUTEX_BOOST == false`, so every boosted branch is dead.

This document designs the **minimum substrate** to drive the futex boost path so
the real `lock.bpf.c` logic executes under simulation — for coverage *and* for
future faithfulness checks of LAVD's lock-holder boosting against live traces.

**Non-goals.** We do not model futex *correctness* (wait queues, ownership,
PI chains), userspace lock internals, or uncontended fast-path CAS (which emits
no syscall and is invisible to the scheduler anyway). We model exactly what the
kernel makes the *scheduler* observe: a futex wait that returned success and a
futex wake that woke ≥1 waiter.

---

## 2. Background — what the scheduler actually observes

(Condensed from `research-lavd-futex-hooks`; full map in that task's notes.)

LAVD approximates userspace lock state from kernel futex calls:

- **futex_wait returns 0** ⇒ a *contended* waiter acquired the lock ⇒
  `inc_futex_boost` sets `LAVD_FLAG_FUTEX_BOOST` on the running task's
  `task_ctx.flags` and mirrors it into `cpu_ctx.flags`.
- **futex_wake returns >0** ⇒ the lock was released and a waiter woken ⇒
  `dec_futex_boost` clears the flag.

Two mutually-exclusive attach routes (userspace `main.rs:479` tries fexit
ftraces first, else the syscall tracepoints; `--no-futex-boost` disables):

| Route | Programs | ctx shape |
|---|---|---|
| fexit (preferred) | `fexit___futex_wait`, `…wait_multiple`, `…wait_requeue_pi`, `…wake`, `…wake_op`, `…lock_pi`, `…unlock_pi` | `BPF_PROG` → `(u64 *ctx)`, trailing slot = `ret` |
| tracepoint (fallback) | `rtp_sys_enter_futex`, `rtp_sys_exit_futex`, `rtp_sys_exit_futex_wait`, `…_waitv`, `…_wake` | plain `struct tp_syscall_{enter_futex,exit} *` |

All hooks funnel to `inc_futex_boost` / `dec_futex_boost` →
`__inc/__dec_futex_boost`. The **tracepoint route is far easier to invoke from
C** because its programs take plain struct pointers (defined in `lock.bpf.c`
lines 254–269), not the `BPF_PROG` `u64 ctx[]` packing.

Downstream effect of the flag (all outside `lock.bpf.c`):

1. `is_lock_holder_running(cpuc)` → `preempt.bpf.c:56` **never preempts** a CPU
   running a lock holder; `main.bpf.c:1255` `lavd_dispatch` keeps the holder
   running (forward progress).
2. `reset_lock_futex_boost` (running:484 / stopping:641 / consume_prev:1211)
   converts `FUTEX_BOOST` → one-shot `LAVD_FLAG_NEED_LOCK_BOOST`.
3. `lat_cri.bpf.c:105`: `NEED_LOCK_BOOST` adds `LAVD_LC_WEIGHT_BOOST_REGULAR`
   (+128) to the task's weight ⇒ higher latency-criticality ⇒ earlier virtual
   deadline ⇒ scheduled sooner (applies exactly once, then cleared).

**Timing that matters:** the boost fires *when the wait returns* (task is
running again) and *when the wake syscall runs* (waker is running). Any faithful
model must fire the hook while the target task is the **current running task on
its CPU**, because `bpf_get_current_task_btf()` / `get_cpu_ctx()` resolve to
`cpus[current_cpu].current_task` (`kfuncs.rs:2675`) and the per-CPU ctx.

---

## 3. Design principles

- **NO-STUB (hard constraint).** The engine only *delivers* the event — exactly
  as the kernel delivers a tracepoint/fexit. The boost decision (flag set/clear,
  the op switch, the mirror into `cpu_ctx.flags`) executes **entirely in the real
  `lock.bpf.c`**. The engine must never set `LAVD_FLAG_FUTEX_BOOST` itself, and
  the C wrapper shim must only marshal args and call the real hook.
- **Twin-Design Principle 1 (production-fidelity, default).** A futex hook firing
  on a contended wait/wake is *real kernel behavior*, not an exaggeration. This
  is baseline substrate, **not** an opt-in Principle-2 stress knob — it is only
  "off" in the trivial sense that a workload must contain futex phases to
  exercise it. (LAVD's own wait-success⇒acquired approximation is the
  *scheduler's* model, which we faithfully drive; we add no approximation atop.)
- **Lightweight / additive.** No scheduler edits. Follow the existing
  `lavd_fire_timer` (non-struct_ops C entry) and `IrqEvent` (scheduled scenario
  event) precedents so the change is a narrow, well-named substrate addition.

---

## 4. Event model

### 4.1 What the four requested event types collapse to

The task names four types (`lock_acquire`, `lock_release`, `lock_wait`,
`lock_wake`). In LAVD's kernel-observable model these collapse to **two**
scheduler-visible transitions:

| Requested | LAVD-observable futex event | Hook effect |
|---|---|---|
| `lock_wait` (blocked) | *(no scheduler event — modeled as a block)* | — |
| `lock_acquire` (wait returned 0) | **`FutexWaitAcquired`** | `inc_futex_boost` (set FUTEX_BOOST) |
| `lock_wake` / `lock_release` (wake returned >0) | **`FutexWakeReleased`** | `dec_futex_boost` (clear FUTEX_BOOST) |

`lock_wait` (the blocking) is not itself a scheduler event — it is just the task
going off-CPU (a `Sleep`/quiescent); the observable event is the *return* from
wait (`FutexWaitAcquired`) when the task resumes. So the core model is a single
enum with two primary variants:

```rust
/// A userspace-lock futex transition the scheduler can observe, as delivered
/// by the kernel's futex tracepoint/fexit. Op values mirror the FUTEX_* cmd
/// space so op-parameterized variants can exercise every lock.bpf.c hook arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FutexOp {
    /// futex_wait / futex_wait_bitset / lock_pi returned success → boost holder.
    WaitAcquired,
    /// futex_wake / wake_bitset / wake_op / unlock_pi woke ≥1 waiter → unboost.
    WakeReleased,
    // Optional op-parameterized variants (for full 12-hook symbol coverage):
    // WaitRequeuePi, WaitMultiple, LockPi, UnlockPi, WakeOp, ...
}
```

For **boost-logic coverage**, `WaitAcquired` + `WakeReleased` is sufficient (all
hooks funnel to the same `inc/dec`). The extra op-parameterized variants exist
only if we want each of the 12 hook *symbols* individually covered; they are
functionally redundant and belong in a later phase (see §9, §11).

### 4.2 Mapping op → real hook

Delivered through the tracepoint route (chosen for its plain-struct ABI):

- `WaitAcquired`  → `rtp_sys_enter_futex(op=FUTEX_WAIT)` then
  `rtp_sys_exit_futex(ret=0)` — the enter stores `op` into `cpuc->futex_op`, the
  exit runs the switch → `__inc_futex_boost`.
- `WakeReleased`  → `rtp_sys_enter_futex(op=FUTEX_WAKE)` then
  `rtp_sys_exit_futex(ret=1)` (ret = #woken > 0) → `__dec_futex_boost`.

This single enter+exit pair drives the whole `rtp_sys_exit_futex` op switch
(`lock.bpf.c:295–321`), covering `rtp_sys_enter_futex`, `rtp_sys_exit_futex`,
and both `__inc`/`__dec` helpers with one C entry point.

---

## 5. Workload spec additions

Two ways to express futex activity, in priority order.

### 5.1 Primary: task Phases (ergonomic, timing-correct)

Extend the task behavior vocabulary (`task.rs:79`, currently
`Run|Sleep|Wake(Pid)`) with two variants:

```rust
pub enum Phase {
    Run(TimeNs),
    Sleep(TimeNs),
    Wake(Pid),
    /// Block on a contended lock, then acquire it. Behaves like a
    /// Sleep-until-woken; when the task next starts running the engine
    /// delivers FutexOp::WaitAcquired to it (boost fires at wait-return,
    /// exactly as the kernel fexit/tracepoint would).
    FutexWait,
    /// Release a lock and wake a waiter. Behaves like Wake(pid) (pushes a
    /// TaskWake for `pid`) AND delivers FutexOp::WakeReleased to the CURRENT
    /// task (the releaser) at this instant.
    FutexWake(Pid),
}
```

Example JSON/builder — a classic waiter/holder pair contended by a hog:

```
holder (pid 1): [Run(200us), FutexWake(2), Run(50us)]  repeat Forever
waiter (pid 2): [FutexWait, Run(300us)]                repeat Forever   # critical section
hog    (pid 3): [Run(forever)]                          # competes for the CPU
```

- pid 2 blocks in `FutexWait`; pid 1 (holding the lock) runs its section, then
  `FutexWake(2)` wakes pid 2 **and** fires `WakeReleased` on pid 1.
- pid 2 resumes → engine fires `WaitAcquired` on pid 2 → it becomes a boosted
  lock holder for its 300µs critical section; the hog (pid 3) cannot preempt it
  and the `NEED_LOCK_BOOST` deadline bump applies on its next reschedule.

This maps entirely onto the **existing** Sleep/Wake machinery
(`handle_task_phase_complete:3941` for the Sleep-like block + future
`TaskWake{waker:None}`; `Wake(pid)` → `TaskWake{waker:Some}` at :3996); the only
additions are (a) firing the hook on the resume of a `FutexWait` and (b) firing
the hook on execution of a `FutexWake`.

### 5.2 Secondary: scheduled scenario events (ad-hoc / precise tests)

Mirror `IrqEvent` (`scenario.rs:106`, builder `.hardirq()` at :1316) for tests
that want to place a futex transition at a precise time without restructuring a
task's phase list:

```rust
pub struct FutexEvent {
    pub pid: Pid,        // task that performs the futex op
    pub at_ns: TimeNs,   // when
    pub op: FutexOp,     // WaitAcquired | WakeReleased
}
// Scenario { .. futex_events: Vec<FutexEvent> }  + builder .futex_event(pid, at_ns, op)
```

Caveat (documented for users): the target `pid` must be the **running** task on
its CPU at `at_ns` for correct attribution; otherwise the handler logs a warning
and no-ops (never silently attributes to the wrong task — No-Silent-Failures).
The Phase form (§5.1) avoids this by construction, so it is preferred; the
scheduled form is a power-user escape hatch.

### 5.3 Future: rt-app lock mapping

`research-rtapp-locks` established that rt-app (and rt-app-rs) express
mutex/condvar/barrier ops that produce `FUTEX_WAIT_PRIVATE`/`FUTEX_WAKE_PRIVATE`
on a live kernel, but scxsim's rt-app loader (`rtapp.rs`) currently **skips**
`lock/unlock/wait/signal/broad/sync/barrier`. A later phase can map rt-app
`lock`(contended)→`FutexWait` and `unlock`(with waiter)→`FutexWake`, reusing the
Phase substrate here. Out of scope for the minimum slice because it also
requires modeling *contention* (only contended locks emit futex).

---

## 6. Simulation engine changes

All additive; follows the IRQ/timer precedents. Files: `engine.rs`, `task.rs`,
`sim_task.rs`, `scenario.rs`, `ffi.rs`, `trace.rs`.

### 6.1 Event kind + dispatch

Add one `EventKind` (`engine.rs:614`) used by both the Phase and scheduled-event
forms:

```rust
EventKind::FutexOp { cpu: CpuId, pid: Pid, op: FutexOp },
```

- Include `cpu` in the CPU-extract arm (`engine.rs:2443`) so the local clock is
  advanced like other per-CPU events.
- Add a dispatch arm (`engine.rs:2462` match) → `handle_futex_op(...)`, mirroring
  the `IrqStart`/`FireTimer` arms (drop guard, call handler, re-lock).

### 6.2 Seeding

- **Phase form:** in `handle_task_phase_complete` (`engine.rs:3768`, advance via
  `sim_task.rs:135`):
  - `Phase::FutexWait` → treat like `Sleep(∞)`: set the task Sleeping and mark a
    per-task flag `pending_futex_acquire = true` (new bool on the sim task).
    No self-scheduled wake (the peer's `FutexWake` provides it).
  - `Phase::FutexWake(pid)` → do the normal `Wake(pid)` push **and** enqueue an
    immediate `FutexOp{cpu=current, pid=self, op=WakeReleased}` (or call the
    handler inline before advancing).
  - In `handle_task_wake` / the start-running path (`engine.rs:3454`/`4806`): if
    the woken task has `pending_futex_acquire`, clear it and enqueue an immediate
    `FutexOp{cpu, pid, op=WaitAcquired}` to fire once it is the running task.
- **Scheduled form:** seed one `FutexOp` per `FutexEvent` at `at_ns` during
  scenario setup (next to IRQ seeding, `engine.rs:2014`).

### 6.3 Handler + timing model

```rust
fn handle_futex_op(&self, cpu, pid, op, sim_arc, monitor) {
    let mut guard = sim_arc.lock().unwrap();
    let s = &mut *guard;
    // Attribution guard: the target must be the running task on `cpu`.
    if s.sim.cpus[cpu].current_task != Some(pid) {
        tracing::warn!(pid, cpu, "futex op for non-running task; skipped");
        return; // No-Silent-Failures: never mis-attribute the boost.
    }
    s.sim.advance_cpu_clock(cpu);
    set_ops_context(&mut s.sim, OpsContext::FutexOp); // new context tag
    start_rbc(&mut s.sim);
    sim_callback!(s, guard, sim_arc, cpu, {
        self.scheduler.futex_op(op);   // FFI → real lock.bpf.c (see §7)
    });
    let s = &mut *guard;
    charge_sched_time(&mut s.sim, cpu, "futex_op");
    // Observe the resulting flag for the trace (read task_ctx flags; §8).
    emit_futex_boost_trace(s, pid, op, monitor);
}
```

Timing: `WaitAcquired` is delivered at the instant the waiter resumes (so the
boost covers its critical section); `WakeReleased` at the instant the releaser
runs its wake. Both fire while the target is `current_task` on `cpu`, so
`bpf_get_current_task_btf()`/`get_cpu_ctx()` resolve correctly with **no extra
setup** (`research-scxsim-event-model` finding). `charge_sched_time` accounts the
hook's ~130ns tracepoint cost (documented overhead in `lock.bpf.c`) — optional;
can be zero for a first cut.

### 6.4 FFI wiring

Add an optional scheduler op (like `fire_timer`) in `ffi.rs`:

```rust
type FutexOpFn = unsafe extern "C" fn(i32 /*op*/, i64 /*ret*/);
// SchedOps { .. futex_op: Option<FutexOpFn> }, resolved via try_get!("lavd_futex_hook")
```

Only LAVD provides `lavd_futex_hook`; for other schedulers `futex_op` is `None`
and `handle_futex_op` becomes a no-op (a `FutexOp` event on a non-futex
scheduler warns once). This matches the `fire_timer`/`try_get!` precedent
(`ffi.rs:1364`) and needs no `SchedOps` change for other schedulers.

---

## 7. LAVD C wrapper changes

One new entry in `schedulers/lavd/wrapper.c`, defined **after** the
`#include "lock.bpf.c"` at line 584 (so the hooks and the `tp_syscall_*` structs
are in scope). Mirrors `lavd_fire_timer` (`wrapper.c:944`) — a substrate entry
that invokes real BPF code:

```c
/*
 * Deliver a simulated futex transition to the REAL lock.bpf.c hooks.
 * op is a FUTEX_* command (FUTEX_WAIT / FUTEX_WAKE / FUTEX_LOCK_PI / ...);
 * ret is the syscall return the scheduler observes (0 = wait success,
 * >0 = #waiters woken). This is the kernel's tracepoint-delivery job; the
 * boost decision runs entirely inside lock.bpf.c (No-Stub).
 *
 * Called by the Rust engine (get_symbol -> futex_op) inside sim_callback!,
 * with current_cpu already set to the running task's CPU, so
 * bpf_get_current_task_btf()/get_cpu_ctx() attribute the boost correctly.
 */
void lavd_futex_hook(int op, long ret)
{
    struct tp_syscall_enter_futex enter = { .op = op };
    struct tp_syscall_exit        exit  = { .ret = ret };
    rtp_sys_enter_futex(&enter);   /* stores op into cpuc->futex_op */
    rtp_sys_exit_futex(&exit);     /* op switch → __inc/__dec_futex_boost */
}
```

Rust maps `FutexOp::WaitAcquired → (FUTEX_WAIT, 0)` and
`FutexOp::WakeReleased → (FUTEX_WAKE, 1)`.

Why the tracepoint route (not fexit): `rtp_sys_enter/exit_futex` take plain
`struct` pointers whose layout is defined in `lock.bpf.c`; the fexit programs are
`BPF_PROG`-wrapped `(u64 *ctx)` requiring hand-packed arg arrays. The tracepoint
pair is simpler, and one call covers the whole op switch. (A later phase can add
`lavd_futex_hook_fexit(...)` shims to cover the 7 fexit symbols individually if
per-symbol coverage is wanted — §9/§11.)

`cpuc->futex_op` persistence: the enter and exit are issued back-to-back on the
same `current_cpu` within one `lavd_futex_hook` call, so the op set by enter is
read by exit before anything else can clobber it — matching the kernel's
enter→exit ordering.

---

## 8. Observability

Add `TraceKind::FutexBoost { pid: Pid, op: FutexOp, boosted: bool }` (in
`trace.rs`, next to the other kinds), emitted by `handle_futex_op` after the hook
runs, reading the resulting `task_ctx.flags & LAVD_FLAG_FUTEX_BOOST` (via the
existing per-task ctx accessor pattern used elsewhere). This lets tests assert:

- the boost was **set** after `WaitAcquired` and **cleared** after
  `WakeReleased`;
- the holder was **not preempted** by a competing task while boosted (compare
  `preempt_count`/`schedule_count` of the holder vs. the hog);
- the one-shot deadline bump changed ordering (holder scheduled sooner on its
  next wake).

Reading the flag for the trace is *observation*, not modeling — the flag itself
is owned and set by `lock.bpf.c`.

---

## 9. Coverage mapping — what each event lights up

| Event | Functions executed (real `lock.bpf.c` + consumers) |
|---|---|
| `WaitAcquired` (once, holder + a competing task) | `rtp_sys_enter_futex`, `rtp_sys_exit_futex` (WAIT arm), `__inc_futex_boost`; then via normal callbacks: `is_lock_holder`, `is_lock_holder_running` (preempt.bpf.c:56 + dispatch:1255), `reset_lock_futex_boost` boosted branch (running/stopping), `lat_cri.bpf.c:105` `NEED_LOCK_BOOST` branch |
| `WakeReleased` | `rtp_sys_enter_futex`, `rtp_sys_exit_futex` (WAKE arm), `__dec_futex_boost` |

That is the entire boost **logic** (the audit's concern). The remaining hook
**symbols** — the 7 fexit programs and the 3 dedicated `rtp_sys_exit_futex_wait/
_waitv/_wake` programs — are functionally identical (all call `inc`/`dec`). They
are covered only if we add op-parameterized variants + matching shims (§11
Phase 3); recommended to defer, since they add symbols without new logic.

---

## 10. Testing plan

New integration test `tests/lavd_futex_boost.rs` (whitebox pattern from
`lavd_coverage_gaps.rs` — `setup_pco`, `detect_bpf_errors`):

1. **Boost set/clear:** waiter/holder pair (§5.1) + one hog. Assert
   `FutexBoost{set=true}` after acquire and `{set=false}` after release.
2. **Preempt protection:** while the boosted holder runs its critical section, a
   higher-`lat_cri` competitor does **not** preempt it (holder's `preempt_count`
   stays flat across the window; `is_lock_holder_running` path taken).
3. **Deadline boost ordering:** the holder is scheduled sooner on its next wake
   than an identical non-lock-holding control task (NEED_LOCK_BOOST effect).
4. **No-boost control:** identical workload with `--no-futex-boost` (or no futex
   phases) → no `FutexBoost` events, boosted branches dark (guards against the
   engine accidentally setting the flag → No-Stub regression check).
5. **Coverage assertion (optional, via coverage.sh):** `lock.bpf.c`
   `__inc/__dec_futex_boost` + `rtp_sys_*_futex` move from 0% to covered.

---

## 11. Phasing (minimum viable slice first)

- **Phase 1 (MVP, ~1 change each):** `lavd_futex_hook` shim; `futex_op` FFI op;
  `EventKind::FutexOp` + `handle_futex_op`; the two `Phase` variants;
  `TraceKind::FutexBoost`; test #1–#4. Delivers all boost **logic** coverage.
- **Phase 2:** scheduled `FutexEvent` scenario builder (§5.2) for ad-hoc tests;
  coverage assertion (test #5).
- **Phase 3 (optional):** op-parameterized `FutexOp` variants + fexit-route
  shims to cover the remaining 9 redundant hook symbols; rt-app lock mapping
  (§5.3).

MVP touch list: `wrapper.c` (+~12 lines), `ffi.rs` (+~6), `engine.rs` (+~40),
`task.rs`/`sim_task.rs` (+~10), `scenario.rs` (Phase only, +0 for MVP),
`trace.rs` (+~6), one new test file. No scheduler `.bpf.c` edits.

---

## 12. Risks & open questions

- **Attribution timing.** The hook must fire while the target is `current_task`
  on its CPU. The Phase form guarantees this (fire on resume / on the running
  waker). The scheduled-event form can miss it → handled by the warn+skip guard
  (§6.3); documented as the reason to prefer Phases.
- **`cpu_ctx.flags` mirror vs. re-run.** `reset_lock_futex_boost` clears
  `FUTEX_BOOST` on the *first* running/stopping after acquire (boost-once). Tests
  must observe preempt protection in the window *between* acquire and that first
  reset. Confirm the ordering in the sim matches kernel (running callback runs
  once at dispatch, before the critical-section run) — validate with the trace.
- **Charge model.** Whether to charge the ~130ns tracepoint cost
  (`charge_sched_time`) — start with 0 for determinism simplicity; revisit if we
  compare against live traces.
- **Generality.** `lavd_futex_hook` is LAVD-specific (only LAVD boosts lock
  holders). If another scheduler later needs it, promote `futex_op` to a shared
  wrapper contract (like `fire_timer`); no change needed now.
- **Fidelity check (future).** Once live futex bpftrace captures exist, the
  side-by-side diff harness (`--trace-format perfetto` + bpftrace recipe) can
  validate that sim futex-boost events match live-kernel ordering — the canonical
  Principle-1 debt-discovery channel.

---

## Appendix — key anchor points (verified 2026-07-23)

- `lock.bpf.c` hooks: `scx/scheds/rust/scx_lavd/src/bpf/lock.bpf.c`
  (`rtp_sys_enter_futex:271`, `rtp_sys_exit_futex:281`, switch :295–321;
  `tp_syscall_enter_futex:254`, `tp_syscall_exit:265`).
- Compiled in: `schedulers/lavd/wrapper.c:584 #include "lock.bpf.c"`;
  shim precedent `lavd_fire_timer` at `:944`.
- Engine: min-heap event loop `engine.rs` (`EventKind:614`, dispatch `:2462`,
  `IrqStart` handler `:2541`, `handle_timer_fired`/`sim_callback!` `:2625`,
  `handle_task_phase_complete:3768`, `handle_task_wake:3454`).
- Phases: `task.rs:79`; advance `sim_task.rs:135`.
- Scheduled-event precedent: `IrqEvent` `scenario.rs:106`, builder `.hardirq()`
  `:1316`, seed `engine.rs:2014`.
- FFI: `DynamicScheduler::get_symbol` `ffi.rs:924`; `try_get!`/`fire_timer`
  `ffi.rs:1364`; `bpf_get_current_task_btf` `kfuncs.rs:2675`.
- Substrate issue: mb **sim-0957ea**.
