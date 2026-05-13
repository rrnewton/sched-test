//! Safe modules — all modules in this directory have zero `unsafe` usage.
//!
//! These modules are grouped here to make the safety boundary explicit:
//! everything under `safe/` is guaranteed free of `unsafe` code.
//!
//! The `forbid(unsafe_code)` attribute below is enforced by the compiler and
//! cannot be overridden by inner `#[allow(unsafe_code)]` in submodules.
#![forbid(unsafe_code)]

pub mod atomic_types;
pub mod bpf_trace;
pub mod cgroup;
pub mod cpu;
pub mod det_hashmap;
pub mod dsq;
pub mod engine;
pub mod fmt;
pub mod monitor;
pub mod perf;
pub(crate) mod perfetto;
pub mod rtapp;
pub mod scenario;
pub mod stats;
pub mod task;
pub mod trace;
pub mod types;
pub mod workloads;
