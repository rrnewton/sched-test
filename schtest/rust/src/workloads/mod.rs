// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! Workload implementations for the scheduler testing framework
//!
//! This module provides various workload implementations for testing scheduler
//! functionality, including spinners, semaphores, and benchmarking utilities.

pub mod benchmark;
pub mod context;
pub mod process;
pub mod semaphore;
pub mod spinner;

// Re-exports at module/crate root for macro $crate:: path resolution.
// In Buck, this module IS the crate root, so $crate::__converge works.
// In Cargo, these are re-exported again from lib.rs.
pub use benchmark::converge as __converge;
pub use benchmark::measure as __measure;
pub use process::Process as __Process;
