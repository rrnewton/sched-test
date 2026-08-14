//! Safe modules — all modules in this directory have zero `unsafe` usage.
//!
//! These modules are grouped here to make the safety boundary explicit:
//! everything under `safe/` is guaranteed free of `unsafe` code.
//!
//! The `forbid(unsafe_code)` attribute below is enforced by the compiler and
//! cannot be overridden by inner `#[allow(unsafe_code)]` in submodules.
#![forbid(unsafe_code)]

// Dormant parallel/race + preemptive-path support types (PRNG, WorkerState,
// YieldReason, WorkerCounter, ...): consumed only by engine_ring/interleave and
// the preempt PreemptRing, all dormant in the sequential engine.
#[allow(dead_code)]
pub mod atomic_types;
pub mod bpf_trace;
pub mod cgroup;
pub mod clock_mode;
pub mod cpu;
// Deterministic-iteration map utility, currently exercised only by its own unit
// tests; retained as a determinism primitive (the crate's reason for being).
#[allow(dead_code)]
pub mod det_hashmap;
pub mod dsq;
pub mod engine;
pub mod fmt;
pub mod layered;
pub mod layered_alloc;
pub mod layered_control;

/// scx_layered's real CPU allocator, compiled from the upstream source.
/// See `layered_alloc` for why this is included verbatim rather than
/// re-implemented, and for the one vendored helper it needs.
#[path = "../../../../../scx/scheds/rust/scx_layered/src/alloc.rs"]
pub mod layered_alloc_upstream;
pub mod monitor;
pub mod perf;
pub(crate) mod perfetto;
pub(crate) mod perfetto_pb;
pub mod rtapp;
pub mod rtapp_iorun;
pub mod scenario;
pub mod starvation;
pub mod stats;
pub mod structops_jsonl;
pub mod task;
pub mod trace;
pub mod types;
pub mod workloads;
