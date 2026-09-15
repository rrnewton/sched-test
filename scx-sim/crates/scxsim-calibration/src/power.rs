//! Metric power: whether a metric *could* have disagreed.
//!
//! # The hole this closes
//!
//! Tolerances in this crate are pre-registered ([`crate::Metric::spec`]), which
//! stops a bound being chosen to fit the data. It does **not** stop a *metric*
//! being chosen that cannot move. Those are different failures and only the
//! first was guarded.
//!
//! The worked example is `sched_basic_proportional`. Two always-runnable
//! spinners on two CPUs saturate the machine, so per-cgroup CPU time is pinned
//! by `wall x nr_cpus` rather than by anything the scheduler decides.
//! `occupancy` ranged over `0.9995 ..= 1.0000` across every perturbation
//! available — **0.05%, one hundredth of its ±5% tolerance.** It reported
//! `agree`, and could not have reported anything else. Three metrics passed
//! that way and the run was described as a success.
//!
//! # What is computed
//!
//! For a metric, the **achievable range** is how far the simulator-side
//! quantity moves when the simulator is perturbed to a physically meaningful
//! extreme. Divided by the tolerance width, that gives [`Power::ratio`]:
//! roughly "how many tolerances wide is the space this metric can explore".
//!
//! * ratio >= 1 — the metric can traverse its whole tolerance. A pass means
//!   something.
//! * ratio well below 1 — the metric cannot reach its own bound. A pass means
//!   the quantity is pinned, not that the model is right.
//!
//! # Why this is a lower bound, and why that is the safe direction
//!
//! One perturbation cannot prove a metric is free to move in every way that
//! matters; it can only witness movement. So [`Power`] establishes a
//! **lower bound** on achievable range. A metric that moves is demonstrably not
//! pinned. A metric that does not move under an extreme, physically meaningful
//! perturbation is reported as [`Verdict::Powerless`] — which is a claim about
//! *this* scenario, not about the metric in general, and the remedy is a
//! scenario that exercises it rather than a wider tolerance.
//!
//! # Liveness comes first
//!
//! A metric shown flat under a perturbation that does nothing proves nothing.
//! On the motivating workload, raising involuntary context-switch cost 200x and
//! scheduler execution cost 100x changed *not one digit of output* — not
//! because those knobs are unwired (they are parsed and reach the scenario
//! builder) but because two spinners that never switch never pay either cost.
//! Had power been computed by sweeping one of those, every metric would have
//! looked pinned for the wrong reason.
//!
//! So [`Perturbation::is_live`] must witness movement in *something* before
//! any metric may be called powerless. If the perturbation moved nothing at
//! all, power is [`Power::Unknown`] and no powerlessness is claimed.
//!
//! # Cost
//!
//! One extra simulator run per calibration, shared by every metric — not one
//! sweep per metric. The calibration already runs the simulator once; this
//! makes it twice. The cheaper alternative, an analytic bound for
//! capacity-saturated sums, was rejected: it would have caught `cpu_time` and
//! `occupancy` on this workload but not `context_switches`, and a check that
//! only catches the cases you already thought of is the thing this crate exists
//! to avoid.

use serde::{Deserialize, Serialize};
use std::fmt;

use crate::verdict::Verdict;

/// Below this fraction of its tolerance, a metric is treated as unable to fail.
///
/// 0.10 is deliberately generous: a metric must be unable to explore even a
/// *tenth* of its own bound before it is called powerless. On the motivating
/// workload the three pinned metrics scored 0.010, 0.005 and 0.002 — an order
/// of magnitude clear of the line — so the exact threshold is not load-bearing
/// for the case that prompted it.
pub const POWERLESS_BELOW: f64 = 0.10;

/// A perturbation applied to the simulator to see what moves.
///
/// Not a noise setting: an *extreme*. The point is to bound what the metric
/// could do, so the perturbation should sit at the edge of what is physically
/// meaningful — e.g. "all modelled overhead removed" — rather than at a
/// plausible operating point.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Perturbation {
    /// Human-readable, printed in the report so a reader can judge whether the
    /// bound is a fair one.
    pub description: String,
    /// Observables that moved. Empty means the perturbation did nothing, and
    /// no powerlessness may be inferred from it.
    pub witnessed_changes: Vec<String>,
}

impl Perturbation {
    pub fn new(description: impl Into<String>) -> Self {
        Perturbation {
            description: description.into(),
            witnessed_changes: Vec::new(),
        }
    }

    /// Record an observable that the perturbation moved.
    pub fn witness(mut self, what: impl Into<String>) -> Self {
        self.witnessed_changes.push(what.into());
        self
    }

    /// Did the perturbation demonstrably change simulator behaviour?
    ///
    /// This gate is the difference between "the metric cannot move" and "the
    /// knob did nothing".
    pub fn is_live(&self) -> bool {
        !self.witnessed_changes.is_empty()
    }
}

/// How far a metric can move, relative to the tolerance it is judged against.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Power {
    /// The perturbation was not live, so nothing can be concluded.
    Unknown,
    /// Measured: `achievable_range / tolerance_width`.
    Measured {
        achievable_range: f64,
        tolerance_width: f64,
    },
}

impl Power {
    /// Achievable range as a multiple of the tolerance width.
    ///
    /// `None` when unknown, or when the tolerance is zero (a degenerate bound
    /// that would make every ratio infinite).
    pub fn ratio(&self) -> Option<f64> {
        match self {
            Power::Unknown => None,
            Power::Measured {
                achievable_range,
                tolerance_width,
            } => (*tolerance_width > 0.0).then(|| achievable_range / tolerance_width),
        }
    }

    /// True when the metric demonstrably cannot reach its own tolerance.
    ///
    /// False for [`Power::Unknown`]: absence of evidence is not powerlessness,
    /// for the same reason an under-powered estimate is Inconclusive rather
    /// than agreement.
    pub fn is_powerless(&self) -> bool {
        self.ratio().is_some_and(|r| r < POWERLESS_BELOW)
    }

    /// Compute from a baseline and a perturbed observation of the same metric.
    pub fn from_observations(
        baseline: f64,
        perturbed: f64,
        tolerance_width: f64,
        perturbation: &Perturbation,
    ) -> Power {
        if !perturbation.is_live() {
            return Power::Unknown;
        }
        Power::Measured {
            achievable_range: (perturbed - baseline).abs(),
            tolerance_width,
        }
    }
}

impl fmt::Display for Power {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.ratio() {
            None => f.write_str("power=?"),
            Some(r) if r < POWERLESS_BELOW => write!(f, "power={r:.3}x TOL"),
            Some(r) => write!(f, "power={r:.2}x tol"),
        }
    }
}

/// Fold power into a verdict.
///
/// A powerless metric cannot report [`Verdict::Agree`] — that is the entire
/// point. It *can* still report [`Verdict::Disagree`]: if a metric that
/// should not be able to move nevertheless landed outside tolerance, that is a
/// real and rather alarming finding, and suppressing it would be the same
/// mistake in the other direction.
pub fn apply_power(verdict: Verdict, power: &Power) -> Verdict {
    match verdict {
        Verdict::Agree if power.is_powerless() => Verdict::Powerless,
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The case that motivated this module, with its real numbers.
    ///
    /// occupancy on `sched_basic_proportional`: 0.9995 baseline, 1.0000 with
    /// all overhead removed, judged against ±5%. Measured 2026-08-12 at
    /// `ba8d028` via `SCX_SIM_OVERHEAD=0`.
    #[test]
    fn occupancy_on_saturating_spinners_is_powerless() {
        let p = Perturbation::new("SCX_SIM_OVERHEAD=0 (all modelled overhead removed)")
            .witness("total time slices 48067 -> 48080")
            .witness("wake p99 5.701us -> 0");
        let power = Power::from_observations(0.9995, 1.0000, 0.05, &p);

        let ratio = power.ratio().expect("live perturbation yields a ratio");
        assert!(ratio < 0.02, "occupancy explores {ratio} of its tolerance");
        assert!(power.is_powerless());

        // The whole point: it must not read as a pass.
        assert_eq!(apply_power(Verdict::Agree, &power), Verdict::Powerless);
        assert!(!apply_power(Verdict::Agree, &power).is_agreement());
    }

    /// wake_latency on the same workload moves 5.701us -> 0 against a ±20%
    /// tolerance: enormously powered, even though it is unscored for want of a
    /// guest-side counterpart.
    #[test]
    fn wake_latency_is_powered_even_when_unscored() {
        let p = Perturbation::new("SCX_SIM_OVERHEAD=0").witness("wake p99 moved");
        let power = Power::from_observations(5.701, 0.0, 0.2 * 5.701, &p);
        assert!(power.ratio().unwrap() > 1.0);
        assert!(!power.is_powerless());
        // Powered but not measured stays not-measured — power does not upgrade.
        assert_eq!(
            apply_power(Verdict::NotMeasured, &power),
            Verdict::NotMeasured
        );
    }

    /// A dead knob must not manufacture powerlessness. This is the 200x
    /// context-switch-cost case: nothing moved, so nothing may be concluded.
    #[test]
    fn a_perturbation_that_moved_nothing_yields_unknown_not_powerless() {
        let dead = Perturbation::new("SCX_SIM_INVOL_CSW_NS=200000 (200x)");
        assert!(!dead.is_live());

        let power = Power::from_observations(0.9995, 0.9995, 0.05, &dead);
        assert_eq!(power, Power::Unknown);
        assert!(
            !power.is_powerless(),
            "absence of evidence is not powerlessness"
        );
        assert_eq!(apply_power(Verdict::Agree, &power), Verdict::Agree);
    }

    /// Powerlessness must never mask a real disagreement.
    #[test]
    fn powerless_does_not_suppress_disagree() {
        let p = Perturbation::new("x").witness("something moved");
        let power = Power::from_observations(1.0, 1.0, 0.05, &p);
        assert!(power.is_powerless());
        assert_eq!(apply_power(Verdict::Disagree, &power), Verdict::Disagree);
    }

    /// A zero-width tolerance is degenerate; ratio is undefined rather than
    /// infinite, and no powerlessness is claimed.
    #[test]
    fn zero_tolerance_is_not_infinite_power() {
        let p = Perturbation::new("x").witness("moved");
        let power = Power::from_observations(1.0, 2.0, 0.0, &p);
        assert_eq!(power.ratio(), None);
        assert!(!power.is_powerless());
    }
}

/// Is a whole run uncitable because nothing in it could have failed?
///
/// The crate already voids a run whose negative control did not reject, on the
/// grounds that nothing establishes the tolerances could reject anything. All
/// metrics being powerless is the same objection reached from the other side:
/// the bounds may be fine, but no *quantity* in the run was free to breach
/// them. Both deserve [`crate::report::RunOutcome::Void`] rather than a pass.
///
/// Verdicts that were never in contention — `NotMeasured` — are excluded: a
/// run is not voided for metrics it never claimed to judge. If nothing at all
/// was compared there is nothing to void, and the negative-control rule and
/// sample-count rules already cover that case.
pub fn all_compared_metrics_are_powerless(verdicts: &[Verdict]) -> bool {
    let compared: Vec<Verdict> = verdicts
        .iter()
        .copied()
        .filter(|v| *v != Verdict::NotMeasured)
        .collect();
    !compared.is_empty() && compared.iter().all(|v| *v == Verdict::Powerless)
}

#[cfg(test)]
mod void_tests {
    use super::*;

    #[test]
    fn a_run_of_only_powerless_metrics_is_uncitable() {
        assert!(all_compared_metrics_are_powerless(&[
            Verdict::Powerless,
            Verdict::Powerless,
            Verdict::NotMeasured,
        ]));
    }

    #[test]
    fn one_real_verdict_is_enough_to_keep_the_run() {
        assert!(!all_compared_metrics_are_powerless(&[
            Verdict::Powerless,
            Verdict::Disagree,
        ]));
        assert!(!all_compared_metrics_are_powerless(&[
            Verdict::Powerless,
            Verdict::Agree,
        ]));
    }

    #[test]
    fn nothing_compared_is_not_a_void_for_this_reason() {
        assert!(!all_compared_metrics_are_powerless(&[Verdict::NotMeasured]));
        assert!(!all_compared_metrics_are_powerless(&[]));
    }
}
