//! Task model for the simulator.
//!
//! This module re-exports the safe type definitions from `safe::task`
//! and the FFI-backed `SimTask` from `unsafe_impl::sim_task`, providing
//! a single `crate::task` path for all task-related types.

// Safe type definitions (zero unsafe).
pub use crate::safe::task::{
    nice_to_weight, sched_weight_to_cgroup, OpsTaskState, Phase, RepeatMode, TaskBehavior, TaskDef,
    TaskState,
};

// FFI-backed runtime task (contains unsafe).
pub use crate::unsafe_impl::sim_task::SimTask;
