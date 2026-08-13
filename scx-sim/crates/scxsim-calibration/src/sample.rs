//! Sample sets and distribution comparison.
//!
//! # Why distributions and not means
//!
//! The known modelling weakness is that machine timings are modelled as
//! constants rather than distributions. A mean-only comparison cannot see that:
//! a constant 500us and a live distribution with median 500us and a 5ms tail
//! have the same mean and are not the same workload. Comparing means would
//! report agreement and the weakness would stay invisible.
//!
//! So a distributional metric is compared **pointwise across percentiles**, and
//! separately for **spread**. The live side already supplies the raw vector
//! (ktstr's `WorkerReport::wake_latencies_ns`), so this is measurable today
//! without new instrumentation on the VM side.

use serde::{Deserialize, Serialize};

use crate::units::{DurationNs, SampleCount};

/// An ordered set of observations of one quantity.
///
/// Stored sorted so percentiles are cheap and, more importantly, so a
/// [`Distribution`] cannot be built without the caller having handed over every
/// sample — there is no constructor that takes a pre-computed mean.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Samples {
    sorted_ns: Vec<u64>,
}

impl Samples {
    pub fn from_nanos(values: impl IntoIterator<Item = u64>) -> Self {
        let mut v: Vec<u64> = values.into_iter().collect();
        v.sort_unstable();
        Samples { sorted_ns: v }
    }

    pub fn from_durations(values: impl IntoIterator<Item = DurationNs>) -> Self {
        Self::from_nanos(values.into_iter().map(|d| d.as_nanos()))
    }

    pub fn count(&self) -> SampleCount {
        SampleCount(self.sorted_ns.len())
    }

    pub fn is_empty(&self) -> bool {
        self.sorted_ns.is_empty()
    }

    /// Nearest-rank percentile. `None` for an empty set — a percentile of
    /// nothing is undefined, not zero.
    pub fn percentile(&self, p: f64) -> Option<DurationNs> {
        if self.sorted_ns.is_empty() {
            return None;
        }
        let p = p.clamp(0.0, 100.0);
        let rank = ((p / 100.0) * self.sorted_ns.len() as f64).ceil() as usize;
        let idx = rank.saturating_sub(1).min(self.sorted_ns.len() - 1);
        Some(DurationNs(self.sorted_ns[idx]))
    }

    pub fn min(&self) -> Option<DurationNs> {
        self.sorted_ns.first().copied().map(DurationNs)
    }

    pub fn max(&self) -> Option<DurationNs> {
        self.sorted_ns.last().copied().map(DurationNs)
    }

    pub fn mean(&self) -> Option<DurationNs> {
        if self.sorted_ns.is_empty() {
            return None;
        }
        let sum: u128 = self.sorted_ns.iter().map(|&v| v as u128).sum();
        Some(DurationNs((sum / self.sorted_ns.len() as u128) as u64))
    }

    /// Coefficient of variation (stddev / mean).
    ///
    /// The single number that most directly exposes constant-vs-distribution:
    /// a constant has CV 0, and any real machine timing does not. `None` when
    /// the mean is zero, since CV is then undefined.
    pub fn coefficient_of_variation(&self) -> Option<f64> {
        let n = self.sorted_ns.len();
        if n == 0 {
            return None;
        }
        let mean = self.sorted_ns.iter().map(|&v| v as f64).sum::<f64>() / n as f64;
        if mean == 0.0 {
            return None;
        }
        let var = self
            .sorted_ns
            .iter()
            .map(|&v| {
                let d = v as f64 - mean;
                d * d
            })
            .sum::<f64>()
            / n as f64;
        Some(var.sqrt() / mean)
    }

    pub fn as_nanos(&self) -> &[u64] {
        &self.sorted_ns
    }
}

/// The percentiles a distributional metric is compared at.
///
/// P99.9 is deliberately absent from the default set: it needs N>=1000
/// (CACHE_REPRODUCER.md §5.2), and including it by default would produce a
/// stream of Inconclusive verdicts that trains readers to ignore them.
pub const DEFAULT_PERCENTILES: &[f64] = &[50.0, 90.0, 99.0];

/// One percentile compared across the two backends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PercentilePoint {
    pub percentile: f64,
    pub sim: Option<DurationNs>,
    pub vm: Option<DurationNs>,
}

/// A side-by-side distributional comparison.
///
/// Carries the CV of both sides explicitly: that is the number which reveals
/// "the sim emits a constant where the live run has spread", which is the named
/// modelling weakness and would be invisible in a percentile table alone if the
/// percentiles happened to line up.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DistributionComparison {
    pub points: Vec<PercentilePoint>,
    pub sim_count: SampleCount,
    pub vm_count: SampleCount,
    pub sim_cv: Option<f64>,
    pub vm_cv: Option<f64>,
}

impl DistributionComparison {
    pub fn new(sim: &Samples, vm: &Samples, percentiles: &[f64]) -> Self {
        DistributionComparison {
            points: percentiles
                .iter()
                .map(|&p| PercentilePoint {
                    percentile: p,
                    sim: sim.percentile(p),
                    vm: vm.percentile(p),
                })
                .collect(),
            sim_count: sim.count(),
            vm_count: vm.count(),
            sim_cv: sim.coefficient_of_variation(),
            vm_cv: vm.coefficient_of_variation(),
        }
    }

    /// The sample count that governs the verdict: the smaller of the two, since
    /// an estimate is only as strong as its weaker side.
    pub fn governing_count(&self) -> SampleCount {
        SampleCount(self.sim_count.get().min(self.vm_count.get()))
    }

    /// True when the simulator produced (near-)constant values where the live
    /// run varied.
    ///
    /// Reported separately from the percentile verdicts because it is the
    /// *named* modelling gap, and because a sim can match every percentile
    /// under a loose tolerance while still being a constant.
    pub fn sim_is_constant_but_vm_varies(&self, vm_cv_floor: f64) -> bool {
        match (self.sim_cv, self.vm_cv) {
            (Some(s), Some(v)) => s < 1e-9 && v > vm_cv_floor,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_use_nearest_rank_and_are_undefined_when_empty() {
        let s = Samples::from_nanos(1..=100);
        assert_eq!(s.percentile(50.0), Some(DurationNs(50)));
        assert_eq!(s.percentile(99.0), Some(DurationNs(99)));
        assert_eq!(s.percentile(100.0), Some(DurationNs(100)));
        assert_eq!(Samples::from_nanos([]).percentile(50.0), None);
    }

    #[test]
    fn samples_are_sorted_regardless_of_input_order() {
        let s = Samples::from_nanos([5, 1, 3]);
        assert_eq!(s.as_nanos(), &[1, 3, 5]);
        assert_eq!(s.min(), Some(DurationNs(1)));
        assert_eq!(s.max(), Some(DurationNs(5)));
    }

    /// CV is the constant-detector: exactly zero for a constant, non-zero for
    /// anything that varies.
    #[test]
    fn coefficient_of_variation_separates_constant_from_varying() {
        let constant = Samples::from_nanos(std::iter::repeat_n(500u64, 200));
        assert_eq!(constant.coefficient_of_variation(), Some(0.0));

        let varying = Samples::from_nanos((1..=200u64).map(|i| i * 10));
        assert!(varying.coefficient_of_variation().unwrap() > 0.1);
    }

    #[test]
    fn cv_is_undefined_for_empty_or_all_zero() {
        assert_eq!(Samples::from_nanos([]).coefficient_of_variation(), None);
        assert_eq!(
            Samples::from_nanos([0, 0, 0]).coefficient_of_variation(),
            None
        );
    }

    /// THE named modelling weakness, made detectable: two sets whose medians
    /// agree, where one is a constant and the other is not. A mean- or
    /// median-only comparison calls this agreement.
    #[test]
    fn constant_sim_versus_varying_vm_is_detected_despite_matching_median() {
        let sim = Samples::from_nanos(std::iter::repeat_n(500u64, 200));
        // Same median (500), wide spread.
        let mut vm_vals: Vec<u64> = (1..=100).map(|i| i * 5).collect(); // 5..500
        vm_vals.extend((1..=100).map(|i| 500 + i * 45)); // 545..5000
        let vm = Samples::from_nanos(vm_vals);

        let cmp = DistributionComparison::new(&sim, &vm, DEFAULT_PERCENTILES);
        assert_eq!(cmp.sim_cv, Some(0.0));
        assert!(cmp.vm_cv.unwrap() > 0.5);
        assert!(
            cmp.sim_is_constant_but_vm_varies(0.05),
            "a constant sim against a varying VM must be flagged"
        );
        // And the tail is where it shows up in the percentiles.
        let p99 = cmp.points.iter().find(|p| p.percentile == 99.0).unwrap();
        assert_eq!(p99.sim, Some(DurationNs(500)));
        assert!(p99.vm.unwrap().as_nanos() > 4_000);
    }

    /// The weaker side governs: 1000 sim samples against 3 VM samples is a
    /// 3-sample estimate.
    #[test]
    fn governing_count_is_the_smaller_side() {
        let cmp = DistributionComparison::new(
            &Samples::from_nanos(0..1000),
            &Samples::from_nanos(0..3),
            DEFAULT_PERCENTILES,
        );
        assert_eq!(cmp.governing_count(), SampleCount(3));
    }
}
