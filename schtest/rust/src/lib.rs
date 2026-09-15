// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! Scheduler testing framework
//!
//! This library provides tools for testing scheduler functionality,
//! running workloads, and benchmarking scheduler performance.

pub mod cases;
pub mod util;
pub mod workloads;

// Re-export structs at crate root for inventory collection
pub use cases::{Benchmark, Test};

// Re-exports for macro $crate:: path resolution.
// In Cargo, $crate = schtest, so macros use $crate::__converge.
// These re-export from workloads where the actual implementations live.
pub use workloads::__converge;
pub use workloads::__measure;
pub use workloads::__Process;
