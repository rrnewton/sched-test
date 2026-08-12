//! What it means for the simulator to agree with a live VM run — and how that
//! is measured.
//!
//! # What "calibrated" means here
//!
//! Calibration is a comparison with a **pre-registered rejection rule**. For
//! each quantity both backends genuinely produce, the crate fixes in advance:
//!
//! * the estimator (what is computed from each side),
//! * the tolerance, with its rationale ([`Metric::spec`]),
//! * the sample count below which no conclusion may be drawn,
//! * and a [`Verdict`] with four outcomes, not two.
//!
//! # Why it can fail
//!
//! A comparison with a loose enough tolerance always passes, so a calibration
//! that cannot fail proves nothing while looking like evidence. Three mechanisms
//! prevent that, and all are enforced by the types:
//!
//! 1. **Tolerances are declared before the run**, in [`Metric::spec`], each with
//!    a written rationale. Relaxing one is recorded as a widening
//!    ([`Tolerance::widening`]) and printed in the report, so a loosened bound
//!    is conspicuous rather than invisible.
//! 2. **A negative control must fail.** [`NegativeControl`] is a
//!    deliberately-wrong comparison run through the *same* tolerances. If it
//!    does not come back [`Verdict::Disagree`], the run is
//!    [`RunOutcome::Void`] — and the metrics that agreed are uncitable too,
//!    because nothing establishes the bounds could have rejected them.
//! 3. **Under-powered estimates are [`Verdict::Inconclusive`]**, never
//!    agreement. A P99 from twelve samples is absence of evidence; it gets its
//!    own outcome so it cannot be counted as success.
//!
//! # Distributions, not means
//!
//! The known modelling weakness is that machine timings are constants rather
//! than distributions. A mean-only comparison cannot see it: a constant 500us
//! and a live distribution with median 500us and a 5ms tail share a mean.
//! [`Metric::WakeLatency`] is therefore compared pointwise per percentile, and
//! [`sample::DistributionComparison`] reports the coefficient of variation of
//! both sides so "the sim emits a constant where the live run varies" is
//! detected even when the percentiles happen to line up.
//!
//! # Scope
//!
//! This is the **quantitative** axis. scx-sim already has a structural one —
//! the bpftrace structops/helpers tracer plus `structops_jsonl` and its
//! sequence diff — which answers "the same calls in the same order". This crate
//! answers "the same numbers", and deliberately does not duplicate it.

#![forbid(unsafe_code)]

pub mod report;
pub mod sample;
pub mod units;
pub mod verdict;

pub use report::{
    CalibrationRun, GapSeries, Metric, MetricResult, MetricSpec, NegativeControl, RunOutcome,
};
pub use sample::{DistributionComparison, PercentilePoint, Samples, DEFAULT_PERCENTILES};
pub use units::{DurationNs, Hz, Quantity, Ratio, SampleCount, TimestampNs};
pub use verdict::{compare, MinSamples, Tolerance, ToleranceKind, Verdict};
