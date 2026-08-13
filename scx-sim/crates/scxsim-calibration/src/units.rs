//! Quantities, typed so the wrong comparison does not compile.
//!
//! Trace comparison is the worst case for bare integers: a timestamp, a
//! duration, a sample count and a nanosecond tolerance are all `u64`, and every
//! one of them can be passed where another is expected. The compiler will accept
//! it and the calibration will report a number that means nothing.
//!
//! So the affine structure is enforced in the type system:
//!
//! * `Timestamp - Timestamp = Duration` — the difference of two points is an
//!   interval.
//! * `Timestamp + Duration = Timestamp` — offsetting a point gives a point.
//! * `Duration + Duration = Duration`.
//! * **`Timestamp + Timestamp` does not exist.** Adding two points in time is
//!   meaningless, and the absence of the impl is what says so.
//!
//! [`DurationNs`] is re-used from `scxsim-workload-ir` rather than redefined, so
//! a duration flowing out of the lowering and a duration measured from a trace
//! are the same type and cannot drift apart.

use std::fmt;
use std::ops::{Add, Sub};

use serde::{Deserialize, Serialize};

pub use scxsim_workload_ir::DurationNs;

/// A point on a run's clock, in nanoseconds from the start of that run.
///
/// Deliberately NOT `DurationNs`. "12ms into the run" and "lasted 12ms" are
/// different facts, and a comparison that mixes them silently reports a
/// discrepancy that does not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TimestampNs(pub u64);

impl TimestampNs {
    pub const START: TimestampNs = TimestampNs(0);

    pub const fn from_nanos(ns: u64) -> Self {
        TimestampNs(ns)
    }

    pub const fn as_nanos(self) -> u64 {
        self.0
    }

    /// Interval since an earlier point. Saturates at zero rather than wrapping,
    /// so a mis-ordered pair reports "no time passed" instead of ~584 years.
    pub fn since(self, earlier: TimestampNs) -> DurationNs {
        DurationNs(self.0.saturating_sub(earlier.0))
    }
}

impl Sub for TimestampNs {
    type Output = DurationNs;
    fn sub(self, rhs: TimestampNs) -> DurationNs {
        self.since(rhs)
    }
}

impl Add<DurationNs> for TimestampNs {
    type Output = TimestampNs;
    fn add(self, rhs: DurationNs) -> TimestampNs {
        TimestampNs(self.0.saturating_add(rhs.as_nanos()))
    }
}

// NOTE: there is deliberately no `impl Add<TimestampNs> for TimestampNs`.
// Summing two points on a clock is not a meaningful operation, and leaving the
// impl out is how that is enforced rather than merely documented.

impl fmt::Display for TimestampNs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "t={}", DurationNs(self.0))
    }
}

/// How many observations an estimate rests on.
///
/// Its own type because sample count is the input to the Inconclusive rule, and
/// confusing it with a measured value is exactly how an under-powered estimate
/// gets reported as agreement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SampleCount(pub usize);

impl SampleCount {
    pub const fn get(self) -> usize {
        self.0
    }

    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }
}

impl fmt::Display for SampleCount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "n={}", self.0)
    }
}

/// A dimensionless ratio, e.g. CPU occupancy (busy / elapsed).
///
/// The cleanest thing to compare across backends: no unit conversion, so a
/// discrepancy cannot be an artefact of one side counting in different units.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct Ratio(pub f64);

impl Ratio {
    pub fn new(v: f64) -> Self {
        Ratio(v)
    }

    /// `busy / elapsed`, or `None` when no time elapsed — an occupancy over a
    /// zero-length window is undefined, not zero.
    pub fn occupancy(busy: DurationNs, elapsed: DurationNs) -> Option<Ratio> {
        if elapsed.is_zero() {
            None
        } else {
            Some(Ratio(busy.as_nanos() as f64 / elapsed.as_nanos() as f64))
        }
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

impl fmt::Display for Ratio {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.4}", self.0)
    }
}

/// A rate in events per second.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct Hz(pub f64);

impl Hz {
    /// Events over a window. `None` for a zero-length window rather than an
    /// infinite rate.
    pub fn from_count(count: u64, over: DurationNs) -> Option<Hz> {
        if over.is_zero() {
            None
        } else {
            Some(Hz(count as f64 * 1e9 / over.as_nanos() as f64))
        }
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

impl fmt::Display for Hz {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.2}Hz", self.0)
    }
}

/// A measured quantity, tagged with what kind of thing it is.
///
/// The comparison layer works on these rather than on `f64`, so it can refuse to
/// compare a duration against a ratio instead of silently producing a number.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Quantity {
    Duration(DurationNs),
    Ratio(Ratio),
    Rate(Hz),
    /// A dimensionless count (migrations, context switches).
    Count(u64),
}

impl Quantity {
    /// Numeric value for arithmetic. Only ever used after a same-kind check.
    pub fn value(self) -> f64 {
        match self {
            Quantity::Duration(d) => d.as_nanos() as f64,
            Quantity::Ratio(r) => r.get(),
            Quantity::Rate(h) => h.get(),
            Quantity::Count(c) => c as f64,
        }
    }

    /// Kind tag, used to reject cross-kind comparisons.
    pub fn kind(self) -> &'static str {
        match self {
            Quantity::Duration(_) => "duration",
            Quantity::Ratio(_) => "ratio",
            Quantity::Rate(_) => "rate",
            Quantity::Count(_) => "count",
        }
    }

    pub fn same_kind_as(self, other: Quantity) -> bool {
        self.kind() == other.kind()
    }
}

impl fmt::Display for Quantity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Quantity::Duration(d) => write!(f, "{d}"),
            Quantity::Ratio(r) => write!(f, "{r}"),
            Quantity::Rate(h) => write!(f, "{h}"),
            Quantity::Count(c) => write!(f, "{c}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The affine structure is the whole point of these types.
    #[test]
    fn timestamp_arithmetic_is_affine() {
        let a = TimestampNs::from_nanos(1_000);
        let b = TimestampNs::from_nanos(3_500);
        assert_eq!(b - a, DurationNs(2_500));
        assert_eq!(a + DurationNs(500), TimestampNs::from_nanos(1_500));
    }

    /// A mis-ordered pair must report "no time passed", not a near-u64::MAX
    /// interval that would look like a catastrophic discrepancy.
    #[test]
    fn reversed_subtraction_saturates_at_zero() {
        let a = TimestampNs::from_nanos(10);
        let b = TimestampNs::from_nanos(5);
        assert_eq!(b - a, DurationNs(0));
    }

    /// Undefined rather than zero: an occupancy over no elapsed time is not 0%,
    /// it is unanswerable, and reporting 0% would read as a real measurement.
    #[test]
    fn occupancy_over_zero_window_is_undefined() {
        assert_eq!(Ratio::occupancy(DurationNs(0), DurationNs(0)), None);
        assert_eq!(Hz::from_count(5, DurationNs(0)), None);
    }

    #[test]
    fn occupancy_and_rate_compute() {
        let r = Ratio::occupancy(DurationNs(250), DurationNs(1_000)).unwrap();
        assert!((r.get() - 0.25).abs() < 1e-12);
        let h = Hz::from_count(500, DurationNs::from_secs(1)).unwrap();
        assert!((h.get() - 500.0).abs() < 1e-9);
    }

    /// Cross-kind comparison must be refusable — comparing a duration against a
    /// ratio yields a number, and that number is meaningless.
    #[test]
    fn quantity_kinds_are_distinguishable() {
        let d = Quantity::Duration(DurationNs(10));
        let r = Quantity::Ratio(Ratio(10.0));
        assert!(!d.same_kind_as(r));
        assert!(d.same_kind_as(Quantity::Duration(DurationNs(999))));
    }
}
