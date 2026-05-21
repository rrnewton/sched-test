# Safe vs Unsafe Layers

> **Status — stub.** Will document the split between `safe/` (pure
> Rust simulator state) and `unsafe_impl/` (FFI bridge into the
> scheduler `.so` and kernel-emulation helpers).

The pattern: `safe/` owns deterministic state and is normally what
tests and recipe writers reason about; `unsafe_impl/` owns
`#[no_mangle]` exports, raw pointer trampolines into BPF code, dispatch
worker pool, e9patch / PMU preemption injection, and the kfunc
emulation shim.

Key sources:

- [`safe/`](https://github.com/facebookexperimental/sched-test/tree/simulator.v6/scx-sim/crates/scx_simulator/src/safe)
- [`unsafe_impl/`](https://github.com/facebookexperimental/sched-test/tree/simulator.v6/scx-sim/crates/scx_simulator/src/unsafe_impl)
- `ai_docs/safety_audit.md`
- `ai_docs/concurrency_protocol.md`
