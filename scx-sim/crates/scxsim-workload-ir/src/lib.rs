//! A restricted workload IR shared between ktstr and scx-sim, and the lowering
//! that compiles ktstr's declarative op vocabulary onto it.
//!
//! # Why this crate lives here
//!
//! It sits next to scx-sim in sched-test, not in ktstr, because ktstr depends on
//! scx-sim as a library and not the reverse. Putting the shared types in ktstr
//! would invert that. So the crate carries *both* halves as plain data — a
//! source vocabulary mirroring ktstr's declarative surface, and the IR — and
//! ktstr depends on it.
//!
//! # The modelling premise
//!
//! The simulator models **how much real time passes** and **what the scheduler
//! sees**. It has no cache, no page tables, no devices. So a work type whose
//! point is a cache footprint or an IPC ratio can legitimately collapse to "spin
//! for this long": the dimension it exercised is not one the simulator has.
//!
//! That makes the lowering permissive by design. It is not, however, silent:
//!
//! * [`Fidelity::Exact`] — scheduler-visible semantics preserved.
//! * [`Fidelity::Approximated`] — expressible as time plus scheduler state, with
//!   the dropped dimension named and its values recorded in the
//!   [`FidelityReport`] carried on the IR.
//! * Refusal — a construct that cannot be expressed without inventing behaviour
//!   is a hard error. Emitting a plausible-looking phase list for a benchmark
//!   binary nobody modelled would be a fabrication, and fabrications get
//!   believed.
//!
//! Approximation is a sanctioned modelling decision and is always on the record.
//! Invention is refused.

#![forbid(unsafe_code)]

pub mod fidelity;
pub mod ir;
pub mod units;

pub use fidelity::{Approximation, Cause, Fidelity, FidelityReport};
pub use ir::{
    Bandwidth, Cgroup, CpuSet, Mutation, Phase, Probe, Repeat, SchedPolicy, Task, TimedMutation,
    Topology, ValidationError, WorkloadIr,
};
pub use units::{CgroupName, CpuIndex, DurationNs, Nice, TaskId};
