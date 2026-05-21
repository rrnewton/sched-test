# Engine

> **Status — stub.** Will describe `safe/engine.rs` — the discrete-event
> simulation loop, virtual-time advancement, per-CPU run loops,
> watchdog wiring, and the struct_ops dispatch entry point.

Key sources:

- [`safe/engine.rs`](https://github.com/facebookexperimental/sched-test/blob/simulator.v6/scx-sim/crates/scx_simulator/src/safe/engine.rs)
- [`safe/scenario.rs`](https://github.com/facebookexperimental/sched-test/blob/simulator.v6/scx-sim/crates/scx_simulator/src/safe/scenario.rs)
- `ai_docs/concurrency_model_exploration.md`
- `ai_docs/widened_concurrency_plan.md`
