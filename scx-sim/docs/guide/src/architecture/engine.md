# Engine

The engine is the discrete-event simulator at the heart of scxsim:
it advances virtual time, fires events into the scheduler's
struct_ops callbacks, and drives per-CPU run loops. Implemented in
[`safe/engine.rs`][engine-rs] and `safe/scenario.rs`.

[engine-rs]: https://github.com/facebookexperimental/sched-test/blob/simulator.v6/scx-sim/crates/scx_simulator/src/safe/engine.rs

## Event loop

The core loop pops the next event from a min-heap keyed by simulated
time. For each event:

1. Advance virtual time to the event's timestamp.
2. Dispatch by event kind:
   - `Tick` → invoke `ops.tick` on the target CPU.
   - `Wake` → enqueue the task; invoke `ops.select_cpu` then
     `ops.enqueue`.
   - `Sleep` → invoke `ops.stopping`.
   - `CgroupBwReplenish` → top-up cgroup runtime; possibly clear
     `is_throttled`.
   - `WatchdogCheck` → see watchdog wiring below.
3. Re-arm any timers (next tick, next replenish).
4. Emit one or more `TraceKind` events to the trace pipeline.

The loop terminates when:

- the event heap is empty, or
- simulated time reaches `--duration`, or
- an `ExitKind` non-`Normal` is signaled (stall, BPF error, ...).

## Virtual time

There is no `sleep`-style wallclock interaction. Time advances by
jumping the global clock to the next event's timestamp. A 1-second
simulation finishes in well under a second of wallclock on a typical
machine — this is what gives scxsim its speed.

Tick events default to ~4 ms periodicity per CPU, with PRNG-driven
jitter suppressible by `--no-noise`. Context-switch overhead is
charged as PRNG noise on top of structop RBC (suppressible by
`--no-overhead`). PMU-derived overhead charges
`rbc_count * --rbc-ns` to each callback's notional cost.

## Per-CPU run loop

Each simulated CPU has its own:

- current task (or idle),
- local DSQ (FIFO + vtime),
- pending tick timestamp.

The engine maintains a flat array of these. The scheduler queries
and mutates them via the struct_ops callbacks and the kfunc
emulation shim (see [Architecture → Safe vs Unsafe](./safe-unsafe.md)).

## Watchdog

`--watchdog-timeout DUR` (alias `--watchdog`, default `30s`) wires
in a periodic `WatchdogCheck` event. The check tracks the maximum
time any runnable task has been waiting; if it exceeds the
threshold, the engine emits `ExitKind::ErrorStall` (exit code 42)
and terminates.

This is the basis of stall-detection in reproducers like Bug-1; see
[Recipes → Reproducing a Stall Bug](../recipes/repro-stall.md).

## Struct_ops dispatch

When the engine needs to invoke a scheduler callback, it goes
through `unsafe_impl/struct_ops.rs` — the FFI trampoline. The
trampoline:

1. Loads arguments into the calling convention the BPF-emitted code
   expects.
2. Calls the function pointer fetched from the dlopen'd `.so`.
3. Pre/post wraps with RBC counter reads (for the overhead model).
4. Emits a `TraceKind::StructOp{Entry, Exit}` event.

Mid-callback preemption is opt-in (`--preemptive`); without it,
each callback runs to completion before the engine resumes.

## Sources

- [`safe/engine.rs`][engine-rs] — the event loop and per-CPU state.
- [`safe/scenario.rs`](https://github.com/facebookexperimental/sched-test/blob/simulator.v6/scx-sim/crates/scx_simulator/src/safe/scenario.rs) —
  workload → initial event-heap translation.
- `ai_docs/concurrency_model_exploration.md` — design notes on
  serial vs interleaved dispatch.
- `ai_docs/widened_concurrency_plan.md` — the `--native-concurrent`
  design.
