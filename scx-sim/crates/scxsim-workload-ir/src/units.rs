//! Strongly-typed scalars for the IR.
//!
//! Every quantity in the IR is a newtype, not a bare `u64` / `usize` / `String`.
//! The IR is produced by a compiler and consumed by a simulator backend; a
//! swapped pair of `u64`s (a duration where an iteration count belongs, a CPU
//! index where a task id belongs) would type-check and then silently describe a
//! different workload. These types make that a compile error.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// A duration in simulated nanoseconds.
///
/// The IR is denominated in simulated ns, not wall-clock: it is the unit the
/// simulator advances in, and it degrades cleanly to wall-clock on a VM backend
/// (the reverse does not hold — wall-clock cannot be replayed deterministically).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DurationNs(pub u64);

impl DurationNs {
    pub const ZERO: DurationNs = DurationNs(0);

    pub const fn from_nanos(ns: u64) -> Self {
        DurationNs(ns)
    }

    pub const fn from_micros(us: u64) -> Self {
        DurationNs(us.saturating_mul(1_000))
    }

    pub const fn from_millis(ms: u64) -> Self {
        DurationNs(ms.saturating_mul(1_000_000))
    }

    pub const fn from_secs(s: u64) -> Self {
        DurationNs(s.saturating_mul(1_000_000_000))
    }

    /// Lossless for any `Duration` the simulator can represent; saturates rather
    /// than wrapping on the absurd end of the range.
    pub fn from_std(d: Duration) -> Self {
        DurationNs(u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
    }

    pub const fn as_nanos(self) -> u64 {
        self.0
    }

    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    pub const fn saturating_add(self, other: DurationNs) -> DurationNs {
        DurationNs(self.0.saturating_add(other.0))
    }

    pub const fn saturating_mul(self, k: u64) -> DurationNs {
        DurationNs(self.0.saturating_mul(k))
    }
}

impl fmt::Display for DurationNs {
    /// Human-scaled, so a pretty-printed IR is readable at a glance. Exact
    /// nanosecond values below 1us are printed as-is rather than rounded to 0.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ns = self.0;
        if ns == 0 {
            write!(f, "0")
        } else if ns < 1_000 {
            write!(f, "{ns}ns")
        } else if ns < 1_000_000 {
            write!(f, "{:.3}us", ns as f64 / 1e3)
        } else if ns < 1_000_000_000 {
            write!(f, "{:.3}ms", ns as f64 / 1e6)
        } else {
            write!(f, "{:.3}s", ns as f64 / 1e9)
        }
    }
}

/// Index of a logical CPU in the declared topology.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CpuIndex(pub u32);

impl fmt::Display for CpuIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cpu{}", self.0)
    }
}

/// Identity of a task within one lowered workload.
///
/// Assigned by the lowering, dense from 0. Distinct from any pid the backend
/// later chooses: the IR does not claim to know pids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TaskId(pub u32);

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "t{}", self.0)
    }
}

/// Name of a cgroup in the declared hierarchy.
///
/// A newtype rather than `String` so it cannot be crossed with a task name or a
/// work-type label, both of which are also strings at the source level.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CgroupName(pub String);

impl CgroupName {
    pub fn new(s: impl Into<String>) -> Self {
        CgroupName(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CgroupName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Scheduling priority, in Linux `nice` units (-20 highest .. 19 lowest).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct Nice(pub i8);

impl fmt::Display for Nice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "nice={}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Display is the pretty-printer's workhorse; pin the unit boundaries so a
    /// printed IR stays comparable across runs and a sub-microsecond duration is
    /// never rounded away to "0".
    #[test]
    fn duration_display_scales_and_never_rounds_to_zero() {
        assert_eq!(DurationNs::ZERO.to_string(), "0");
        assert_eq!(DurationNs(1).to_string(), "1ns");
        assert_eq!(DurationNs(999).to_string(), "999ns");
        assert_eq!(DurationNs::from_micros(1).to_string(), "1.000us");
        assert_eq!(DurationNs::from_millis(1).to_string(), "1.000ms");
        assert_eq!(DurationNs::from_secs(2).to_string(), "2.000s");
    }

    /// Saturation, not wraparound: an absurd source duration must not silently
    /// become a tiny one.
    #[test]
    fn duration_saturates_rather_than_wrapping() {
        assert_eq!(
            DurationNs(u64::MAX).saturating_add(DurationNs(1)).0,
            u64::MAX
        );
        assert_eq!(DurationNs(u64::MAX).saturating_mul(2).0, u64::MAX);
        assert_eq!(DurationNs::from_secs(u64::MAX).0, u64::MAX);
        assert_eq!(DurationNs::from_std(Duration::MAX).0, u64::MAX);
    }

    #[test]
    fn duration_from_std_is_lossless_in_range() {
        assert_eq!(
            DurationNs::from_std(Duration::from_millis(250)).as_nanos(),
            250_000_000
        );
    }
}
