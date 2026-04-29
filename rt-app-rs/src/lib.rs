//! rt-app-rs: Cgroup-aware workload specification and runtime.
//!
//! Defines a JSON spec for workloads with cgroup v2 hierarchy support.
//! At runtime, creates the hierarchy under `/sys/fs/cgroup/`, applies
//! `cpu.max` bandwidth limits, and places threads.
//!
//! # JSON Schema
//!
//! ```json
//! {
//!   "global": { ... },
//!   "cgroups": {
//!     "/workload": { "cpu.max": { "quota": "max", "period": 100000 } },
//!     "/workload/batch": { "cpu.max": { "quota": 200000, "period": 100000 } }
//!   },
//!   "tasks": {
//!     "worker": { "cgroup": "/workload/batch", ... }
//!   }
//! }
//! ```

pub mod cgroup;
pub mod spec;

pub use cgroup::{CgroupHierarchy, CgroupNode};
pub use spec::{CgroupDef, CpuMax, RtAppSpec, TaskDef};
