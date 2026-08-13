//! Tolerances and verdicts — where "calibrated" is given a meaning that can fail.
//!
//! # The failure mode this module exists to prevent
//!
//! A comparison with a loose enough tolerance always passes. A calibration that
//! cannot produce a failing outcome measures nothing while looking like
//! evidence, and that is worse than no calibration, because it gets cited.
//!
//! Three mechanisms, all required:
//!
//! 1. **Tolerances are pre-registered.** A [`Tolerance`] carries the rationale
//!    for its width, and widening one after the fact is recorded as a widening
//!    (see [`Tolerance::widened_from`]) rather than silently replacing the old
//!    value.
//! 2. **Under-powered estimates are [`Verdict::Inconclusive`], never
//!    [`Verdict::Agree`].** A P99 from twelve samples is not agreement; it is
//!    absence of evidence, and it has its own outcome so it cannot be counted as
//!    success.
//! 3. **A negative control must fail.** See [`crate::report::CalibrationRun`]
//!    — a run whose negative control *passed* is void, not green.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::units::{Quantity, SampleCount};

/// How close the two backends must be for a metric to count as agreeing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tolerance {
    pub kind: ToleranceKind,
    /// Why this width and not another. Required, because an unjustified
    /// tolerance is the thing that later gets widened without argument.
    pub rationale: String,
    /// Set when this tolerance replaced a tighter one. The report surfaces it,
    /// so relaxing a bound is conspicuous rather than invisible.
    pub widened_from: Option<Box<ToleranceKind>>,
}

/// The comparison rule itself.
///
/// A kind rather than a bare number so a `5` can never mean "5%" in one place
/// and "5ns" in another.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum ToleranceKind {
    /// |sim - vm| / |vm| <= frac. `vm` is the reference; the live run is the
    /// thing being matched, not the average of the two.
    Relative { frac: f64 },
    /// |sim - vm| <= abs, in the metric's own units. For quantities whose
    /// relative error explodes near zero (small counts).
    Absolute { abs: f64 },
    /// Either bound suffices — relative for large values, absolute for small.
    /// Prevents a migration count of 1-vs-2 reading as a 100% failure.
    RelativeOrAbsolute { frac: f64, abs: f64 },
}

impl ToleranceKind {
    /// Does `sim` agree with the reference `vm`?
    pub fn accepts(&self, sim: f64, vm: f64) -> bool {
        let delta = (sim - vm).abs();
        match *self {
            ToleranceKind::Relative { frac } => {
                if vm == 0.0 {
                    // A relative bound against a zero reference is undefined —
                    // every non-zero sim value is infinitely wrong. Only exact
                    // agreement passes; the caller should have used an absolute
                    // or combined bound.
                    delta == 0.0
                } else {
                    delta / vm.abs() <= frac
                }
            }
            ToleranceKind::Absolute { abs } => delta <= abs,
            ToleranceKind::RelativeOrAbsolute { frac, abs } => {
                delta <= abs || (vm != 0.0 && delta / vm.abs() <= frac)
            }
        }
    }
}

impl fmt::Display for ToleranceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ToleranceKind::Relative { frac } => write!(f, "+/-{:.1}%", frac * 100.0),
            ToleranceKind::Absolute { abs } => write!(f, "+/-{abs}"),
            ToleranceKind::RelativeOrAbsolute { frac, abs } => {
                write!(f, "+/-{:.1}% or +/-{abs}", frac * 100.0)
            }
        }
    }
}

impl Tolerance {
    pub fn relative(frac: f64, rationale: impl Into<String>) -> Self {
        Tolerance {
            kind: ToleranceKind::Relative { frac },
            rationale: rationale.into(),
            widened_from: None,
        }
    }

    pub fn relative_or_absolute(frac: f64, abs: f64, rationale: impl Into<String>) -> Self {
        Tolerance {
            kind: ToleranceKind::RelativeOrAbsolute { frac, abs },
            rationale: rationale.into(),
            widened_from: None,
        }
    }

    /// Record that this tolerance replaced a tighter one.
    ///
    /// Use this instead of editing the width in place. The report prints
    /// widenings, which is the only thing standing between "we tightened our
    /// model" and "we loosened our test".
    pub fn widening(mut self, previous: ToleranceKind) -> Self {
        self.widened_from = Some(Box::new(previous));
        self
    }

    pub fn accepts(&self, sim: f64, vm: f64) -> bool {
        self.kind.accepts(sim, vm)
    }
}

/// The outcome of comparing one metric across the two backends.
///
/// Ordered by severity so a run's overall verdict is the worst of its parts:
/// `Disagree > Inconclusive > Agree > NotMeasured`. This is deliberately the
/// same lattice ktstr already uses for its assertions, rather than a second
/// vocabulary that would have to be mentally translated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Verdict {
    /// The metric was not produced by one or both backends. Not a judgement —
    /// there was nothing to judge.
    NotMeasured,
    /// Within tolerance.
    Agree,
    /// Measured, but the estimate is too weak to support a conclusion (too few
    /// samples for the percentile being claimed). NOT agreement.
    Inconclusive,
    /// Outside tolerance. The simulator and the live run disagree.
    Disagree,
}

impl Verdict {
    /// Fold over many verdicts, worst wins.
    pub fn worst(verdicts: impl IntoIterator<Item = Verdict>) -> Verdict {
        verdicts.into_iter().max().unwrap_or(Verdict::NotMeasured)
    }

    /// True only for [`Verdict::Agree`]. Deliberately strict: Inconclusive and
    /// NotMeasured both read as false, so "did it pass?" cannot accidentally
    /// include "we could not tell".
    pub fn is_agreement(self) -> bool {
        matches!(self, Verdict::Agree)
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Verdict::NotMeasured => "not-measured",
            Verdict::Agree => "agree",
            Verdict::Inconclusive => "inconclusive",
            Verdict::Disagree => "DISAGREE",
        };
        f.write_str(s)
    }
}

/// Minimum samples an estimate needs before it may return Agree or Disagree.
///
/// Thresholds follow the project's existing methodology (CACHE_REPRODUCER.md
/// §5.2: N>=100 for P50/P90/P99, N>=1000 for P99.9) rather than being invented
/// here. Below the threshold the verdict is [`Verdict::Inconclusive`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MinSamples(pub usize);

impl MinSamples {
    /// A mean or total over a run. One sample is a real observation.
    pub const AGGREGATE: MinSamples = MinSamples(1);
    /// P50/P90/P99 — CACHE_REPRODUCER.md §5.2.
    pub const PERCENTILE: MinSamples = MinSamples(100);
    /// P99.9 — CACHE_REPRODUCER.md §5.2.
    pub const TAIL_PERCENTILE: MinSamples = MinSamples(1000);

    pub fn satisfied_by(self, n: SampleCount) -> bool {
        n.get() >= self.0
    }
}

/// Compare one metric, applying the sample-count rule before the tolerance.
///
/// Order matters: an under-powered estimate is Inconclusive **regardless of
/// whether it happens to fall inside the tolerance**. A P99 from three samples
/// that lands within 20% is luck, not agreement, and calling it agreement is how
/// a calibration comes to prove nothing.
pub fn compare(
    sim: Option<Quantity>,
    vm: Option<Quantity>,
    n: SampleCount,
    min: MinSamples,
    tol: &Tolerance,
) -> Verdict {
    let (sim, vm) = match (sim, vm) {
        (Some(s), Some(v)) => (s, v),
        _ => return Verdict::NotMeasured,
    };
    if !sim.same_kind_as(vm) {
        // Comparing a duration to a ratio would produce a number. Refuse.
        return Verdict::NotMeasured;
    }
    if !min.satisfied_by(n) {
        return Verdict::Inconclusive;
    }
    if tol.accepts(sim.value(), vm.value()) {
        Verdict::Agree
    } else {
        Verdict::Disagree
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::units::{DurationNs, Ratio};

    fn tol10() -> Tolerance {
        Tolerance::relative(0.10, "test")
    }

    #[test]
    fn relative_tolerance_accepts_inside_and_rejects_outside() {
        let t = ToleranceKind::Relative { frac: 0.10 };
        assert!(t.accepts(105.0, 100.0));
        assert!(t.accepts(95.0, 100.0));
        assert!(!t.accepts(111.0, 100.0));
        assert!(!t.accepts(89.0, 100.0));
    }

    /// A relative bound against a zero reference is undefined; only exact
    /// agreement may pass, otherwise every metric that happens to be zero on the
    /// live side would silently accept anything.
    #[test]
    fn relative_tolerance_against_zero_reference_requires_exactness() {
        let t = ToleranceKind::Relative { frac: 0.99 };
        assert!(t.accepts(0.0, 0.0));
        assert!(!t.accepts(1.0, 0.0));
    }

    /// Small counts: 1 vs 2 is a 100% relative error but not a meaningful
    /// disagreement, which is what the absolute arm is for.
    #[test]
    fn relative_or_absolute_covers_small_counts() {
        let t = ToleranceKind::RelativeOrAbsolute {
            frac: 0.25,
            abs: 2.0,
        };
        assert!(t.accepts(2.0, 1.0), "within the absolute arm");
        assert!(t.accepts(1000.0, 900.0), "within the relative arm");
        assert!(!t.accepts(100.0, 1.0), "outside both");
    }

    /// THE central rule. An under-powered estimate must be Inconclusive even
    /// when it lands inside the tolerance — otherwise a lucky three-sample P99
    /// counts as calibration evidence.
    #[test]
    fn under_powered_estimate_is_inconclusive_even_when_within_tolerance() {
        let v = compare(
            Some(Quantity::Duration(DurationNs(100))),
            Some(Quantity::Duration(DurationNs(101))),
            SampleCount(3),
            MinSamples::PERCENTILE,
            &tol10(),
        );
        assert_eq!(v, Verdict::Inconclusive);
        assert!(!v.is_agreement(), "Inconclusive must not read as agreement");
    }

    #[test]
    fn well_powered_estimate_agrees_or_disagrees() {
        let agree = compare(
            Some(Quantity::Duration(DurationNs(100))),
            Some(Quantity::Duration(DurationNs(101))),
            SampleCount(500),
            MinSamples::PERCENTILE,
            &tol10(),
        );
        assert_eq!(agree, Verdict::Agree);

        let disagree = compare(
            Some(Quantity::Duration(DurationNs(300))),
            Some(Quantity::Duration(DurationNs(100))),
            SampleCount(500),
            MinSamples::PERCENTILE,
            &tol10(),
        );
        assert_eq!(disagree, Verdict::Disagree);
    }

    /// A metric one side never produced is NotMeasured, not agreement — the
    /// difference between "the same" and "we did not look".
    #[test]
    fn missing_side_is_not_measured() {
        assert_eq!(
            compare(
                None,
                Some(Quantity::Duration(DurationNs(1))),
                SampleCount(500),
                MinSamples::AGGREGATE,
                &tol10()
            ),
            Verdict::NotMeasured
        );
    }

    /// Comparing across kinds would yield a number with no meaning. Refuse.
    #[test]
    fn cross_kind_comparison_is_refused() {
        assert_eq!(
            compare(
                Some(Quantity::Duration(DurationNs(10))),
                Some(Quantity::Ratio(Ratio(10.0))),
                SampleCount(500),
                MinSamples::AGGREGATE,
                &tol10()
            ),
            Verdict::NotMeasured
        );
    }

    /// Severity ordering: one disagreement dominates any number of agreements.
    #[test]
    fn worst_verdict_wins_the_fold() {
        assert_eq!(
            Verdict::worst([Verdict::Agree, Verdict::Disagree, Verdict::Agree]),
            Verdict::Disagree
        );
        assert_eq!(
            Verdict::worst([Verdict::Agree, Verdict::Inconclusive]),
            Verdict::Inconclusive
        );
        assert_eq!(Verdict::worst([]), Verdict::NotMeasured);
        assert_eq!(Verdict::worst([Verdict::Agree]), Verdict::Agree);
    }

    /// Widening must leave a trace. This is the audit hook against relaxing a
    /// bound until the run goes green.
    #[test]
    fn widening_a_tolerance_records_the_previous_bound() {
        let t = Tolerance::relative(0.50, "relaxed after run 4")
            .widening(ToleranceKind::Relative { frac: 0.10 });
        assert_eq!(
            t.widened_from.as_deref(),
            Some(&ToleranceKind::Relative { frac: 0.10 })
        );
    }
}
