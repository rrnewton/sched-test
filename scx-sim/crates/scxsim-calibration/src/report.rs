//! The calibration report: metric set, run record, and the negative control.
//!
//! # The negative control
//!
//! [`CalibrationRun::negative_control`] is not optional decoration. A
//! calibration whose tolerances are too loose passes everything, and nothing in
//! a green report distinguishes "the simulator matches reality" from "the bounds
//! are meaningless". The control is a deliberately-wrong comparison that the
//! same tolerances **must reject**.
//!
//! If the control does not produce [`Verdict::Disagree`], the run is
//! [`RunOutcome::Void`] — not a pass. That is the difference between a
//! calibration and a ritual.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::units::{DurationNs, Hz, Quantity, Ratio, SampleCount};
use crate::verdict::{compare, MinSamples, Tolerance, ToleranceKind, Verdict};

/// A quantity compared across the two backends.
///
/// The registry is explicit and small on purpose: only quantities BOTH backends
/// genuinely produce. Anything else is [`Verdict::NotMeasured`], never assumed
/// equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Metric {
    /// Per-task CPU time. VM `cpu_time_ns` vs sim `total_runtime(pid)`.
    CpuTime,
    /// Per-task off-CPU time, `(wall - cpu) / wall` on both sides.
    ///
    /// **NOT COMPARABLE ACROSS BACKENDS AS DEFINED, and recorded via
    /// [`MetricResult::not_comparable`] rather than evaluated.** Both sides
    /// compute the same formula correctly; the formula does not mean the same
    /// thing in the two places.
    ///
    /// Measured on `sched_basic_proportional`, the live guest's off-CPU time is
    /// dominated by something that is not scheduling:
    ///
    /// ```text
    /// cg_0   off_cpu 43.18 ms   run_delay 3.69 ms    91% is not runqueue waiting
    /// cg_1   off_cpu 52.14 ms   run_delay 8.68 ms    83% is not runqueue waiting
    /// ```
    ///
    /// `run_delay` (schedstat, runnable-but-not-running) is the
    /// scheduler-attributable part and is 8-17% of the metric. The remainder is
    /// finely distributed — `max_gap_ms` is 1 and 0, so there was no single
    /// stall — and the guest separately measured the host stealing from it
    /// (`host_dilation` 1.00063, "wall / delivered vCPU CPU time"). That is
    /// virtualization overhead: VM exits, host preemption of the vCPU.
    ///
    /// The simulator has no host, no vCPU and no exits, so it has no
    /// counterpart, and per this project's modelling charter it should not
    /// acquire one — synthesising steal time to close a verdict would be
    /// inventing behaviour.
    ///
    /// On the quantity both sides DO mean, they are close: sim off-CPU is
    /// 6.0 ms against the guest's 3.69 / 8.68 ms of run_delay. The simulator
    /// sits between the two cgroups. It is not 7x low on scheduling delay; it
    /// was 7x low on a number that is mostly not scheduling delay.
    ///
    /// The metric that captures the intent is scheduling delay itself, and it
    /// now exists: [`Metric::SchedulingDelay`], guest `mean_run_delay_us`
    /// against a simulator runnable-but-not-running figure. It was added in two
    /// separate steps on purpose — tolerance first, from the quantity alone and
    /// before either side could compute it, then the extraction — because a
    /// bound chosen by someone who has already seen both numbers is the thing
    /// pre-registration exists to prevent.
    OffCpuTime,
    /// CPU occupancy, busy/elapsed. Dimensionless, so immune to unit-conversion
    /// error on either side — the strictest bound we can fairly demand.
    Occupancy,
    /// Migrations between CPUs.
    Migrations,
    /// Context switches.
    ContextSwitches,
    /// Wake-to-run latency. Distributional, not a mean — see [`crate::sample`].
    WakeLatency,
    /// Mean on-CPU slice length for the workload tasks: total workload on-CPU
    /// time divided by the number of dispatches of those tasks. VM side from
    /// wprof per-task slices; sim side from the simulator trace.
    ///
    /// SAMPLE-FLOOR CAVEAT: PERCENTILE (100) suffices only if the slice
    /// distribution has CV <= ~0.33 (SE = CV/sqrt(n) ~= 3.3% of the mean,
    /// about a third of the tolerance). A bimodal mixture of near-zero early
    /// exits and full slices can reach CV ~ 1, where n=100 gives SE = 10% —
    /// the whole tolerance, making a pass meaningless. Record the observed
    /// sample SD; if CV > 0.33 raise this to TAIL_PERCENTILE. Raising it on
    /// measured dispersion is evidence; lowering it is not.
    MeanSliceLength,
    /// Scheduling delay: runqueue wait, the time a task is runnable but not
    /// running. The scheduler-attributable share of the time a task spends off
    /// CPU, once virtualization overhead is excluded.
    ///
    /// Registered with its tolerance BEFORE either side could compute it, which
    /// is the strongest form of pre-registration available: the bound cannot
    /// have been fitted to a measurement that did not exist yet.
    ///
    /// Both sides are now wired —
    /// [`VmCgroup::run_delay`](crate::vm::VmCgroup::run_delay) from the kernel's
    /// `sched_info.run_delay`, and
    /// [`SimRun::run_delay`](crate::sim::SimRun::run_delay) from the trace's
    /// enqueue-to-dispatch intervals. Each states its definition in full;
    /// they are the same physical quantity, which is what off-CPU time was not.
    ///
    /// **The bound REJECTS on the first run measured, and that is the result.**
    /// On `sched_basic_proportional` the simulator reports ~150us of runqueue
    /// wait against the guest's 3.694ms and 8.683ms — 25x and 58x low. cg_1
    /// fails both arms (98.3% relative; 8.533ms against the 4ms absolute arm,
    /// exceeding it by 2.1x) and is recorded as `Disagree`. cg_0 survives only
    /// because its live value is small enough that the 4ms floor still covers a
    /// 24.55x error, at 89% of that arm.
    ///
    /// The direction is the one the simulator's own construction predicts: no
    /// IRQs, no timer ticks, no kernel threads, no host, and only the
    /// scenario's tasks exist, so its scheduling delay is a FLOOR rather than
    /// an estimate. This is the same missing-interference story as
    /// [`Metric::OffCpuTime`] — but on a quantity that IS comparable across
    /// backends, so it cannot be set aside as a definitional artefact.
    ///
    /// An earlier measurement of this metric agreed on both cgroups. It was
    /// taken against a lowering defect that inserted voluntary yields into a
    /// busy loop, producing 24029 phase-bound dispatches per task whose
    /// fractional queueing summed to a spurious 6.007ms. Fixing the lowering
    /// (`676b42f`) cut dispatches 39.8x and the simulated delay 39.9x with
    /// them. The bound was not moved in either direction: it agreed when the
    /// data was wrong and rejects now that it is right, which is the entire
    /// reason for fixing it before the data existed.
    SchedulingDelay,
}

impl Metric {
    /// The pre-registered tolerance and sample requirement.
    ///
    /// Declared in code, before any run, so a bound cannot be quietly chosen to
    /// fit the data. Each carries its rationale.
    pub fn spec(self) -> MetricSpec {
        match self {
            Metric::Occupancy => MetricSpec {
                tolerance: Tolerance::relative(
                    0.05,
                    "dimensionless ratio: no unit conversion on either side, so a \
                     discrepancy cannot be an artefact of counting differently. The \
                     strictest bound in the set.",
                ),
                min_samples: MinSamples::AGGREGATE,
                distributional: false,
            },
            Metric::CpuTime => MetricSpec {
                tolerance: Tolerance::relative(
                    0.10,
                    "the simulator models neither frequency scaling nor SMT throughput \
                     loss, so identical scheduling decisions still yield somewhat \
                     different CPU time. 10% is the honest band for that gap.",
                ),
                min_samples: MinSamples::AGGREGATE,
                distributional: false,
            },
            Metric::OffCpuTime => MetricSpec {
                tolerance: Tolerance::relative(
                    0.10,
                    "complement of CpuTime over a matched duration; same reasoning.",
                ),
                min_samples: MinSamples::AGGREGATE,
                distributional: false,
            },
            Metric::Migrations => MetricSpec {
                tolerance: Tolerance::relative_or_absolute(
                    0.25,
                    2.0,
                    "counts are small and high-variance; the absolute arm stops 1-vs-2 \
                     reading as a 100% failure while the relative arm still binds at \
                     scale.",
                ),
                min_samples: MinSamples::AGGREGATE,
                distributional: false,
            },
            Metric::ContextSwitches => MetricSpec {
                tolerance: Tolerance::relative_or_absolute(
                    0.15,
                    5.0,
                    "driven by the same dispatch decisions on both sides, so it should \
                     track closely; the absolute arm covers very short runs.",
                ),
                min_samples: MinSamples::AGGREGATE,
                distributional: false,
            },
            Metric::SchedulingDelay => MetricSpec {
                tolerance: Tolerance::relative_or_absolute(
                    0.20,
                    4_000_000.0,
                    "derived from what a policy comparison needs, not from what the \
                     simulator achieves. Two policies whose true delays differ by a \
                     factor R have disjoint error bands at relative error f exactly \
                     when R > (1+f)/(1-f); f=0.20 resolves 1.5x, which is the smallest \
                     difference worth ranking — a third off runqueue wait is a clear \
                     win, below that is workload noise. Equal to WakeLatency because \
                     it shares that metric's weakness (near-constant simulated timings \
                     against a live distribution) and adds runqueue ordering on top, so \
                     it cannot honestly be tighter; looser than Occupancy's 5% and \
                     CpuTime's 10% because it is a difference of two live timestamps \
                     and is exposed to virtualization jitter they are not. The absolute \
                     arm is one scheduler tick (TICK_INTERVAL_NS, CONFIG_HZ 250): below \
                     one tick the scheduler had no opportunity to decide differently, \
                     so no policy conclusion can rest on it. The arms cross at 20ms, \
                     exactly the default slice_ns — not chosen, it falls out.",
                ),
                min_samples: MinSamples::AGGREGATE,
                distributional: false,
            },
            Metric::WakeLatency => MetricSpec {
                tolerance: Tolerance::relative(
                    0.20,
                    "compared POINTWISE per percentile, not as a mean. Wider than the \
                     aggregates because the simulator currently emits near-constant \
                     timings against a live distribution — this metric is EXPECTED to \
                     fail at first, and that failure is the finding.",
                ),
                min_samples: MinSamples::PERCENTILE,
                distributional: true,
            },
            // Derived BLIND under tg `blind_tolerance_derivation_what` by an
            // agent that had not seen the measurement, from decision-relevance
            // rather than from data, and carried across here VERBATIM. See the
            // parent CLAUDE.md section "Blind derivation: pre-registered
            // bounds, and how blinding actually breaks" for the protocol, and
            // that task's notes for the full derivation and for a
            // self-reported blinding breach that occurred AFTER the bound was
            // published (a commit subject leaked the magnitude while the agent
            // was locating the branch). The ordering is established by the tg
            // note timestamps; the bound was not changed afterwards and must
            // not be. If it ever has to move, use `Tolerance::widening()` so
            // the relaxation is conspicuous — never edit the width in place.
            Metric::MeanSliceLength => MetricSpec {
                tolerance: Tolerance::relative_or_absolute(
                    0.10,
                    50_000.0,
                    "slice length drives wake latency, preemption rate, short-window \
                     fairness and time-to-effect of a placement decision LINEARLY, so \
                     it must be bounded tighter than the metrics it causes — \
                     ContextSwitches at 15% and WakeLatency at 20% — or a slice error \
                     alone consumes their whole budget and those bounds stop testing \
                     the scheduler. 10% also resolves a 1.25x slice-policy difference, \
                     the finest we will claim to adjudicate: two policies differing by \
                     factor R have disjoint bands at relative error f exactly when \
                     R > (1+f)/(1-f). Deliberately sharper than its own components \
                     (CpuTime 10% over dispatch count 15% compose adversarially to \
                     ~25%) because those errors share one fidelity gap and cancel in \
                     the ratio; what survives is slice policy modelled wrong, which \
                     nothing else in the registry can see. Absolute arm 50us: below \
                     ~500us mean slice, 10% is comparable to per-event overheads the \
                     two sides account for differently (migration penalty 10us, \
                     cross-LLC 25us). HOLDS ONLY WHEN BOTH SIDES RUN THE SAME \
                     SCHEDULER — different schedulers legitimately choose different \
                     slice lengths; that is a policy difference, not simulator \
                     infidelity. Derived blind under tg blind_tolerance_derivation_what; \
                     do not retune.",
                ),
                min_samples: MinSamples::PERCENTILE,
                distributional: false,
            },
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Metric::CpuTime => "cpu_time",
            Metric::OffCpuTime => "off_cpu_time",
            Metric::Occupancy => "occupancy",
            Metric::Migrations => "migrations",
            Metric::ContextSwitches => "context_switches",
            Metric::WakeLatency => "wake_latency",
            Metric::MeanSliceLength => "mean_slice_length",
            Metric::SchedulingDelay => "scheduling_delay",
        }
    }

    /// Every metric in the registry.
    pub const ALL: &'static [Metric] = &[
        Metric::Occupancy,
        Metric::CpuTime,
        Metric::OffCpuTime,
        Metric::Migrations,
        Metric::ContextSwitches,
        Metric::WakeLatency,
        Metric::MeanSliceLength,
        Metric::SchedulingDelay,
    ];
}

impl fmt::Display for Metric {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The pre-registered comparison rule for one metric.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricSpec {
    pub tolerance: Tolerance,
    pub min_samples: MinSamples,
    /// Compared per-percentile rather than as a single value.
    pub distributional: bool,
}

/// One metric's result in one run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricResult {
    pub metric: Metric,
    /// Sub-label for distributional metrics, e.g. `"p99"`.
    pub at: Option<String>,
    pub sim: Option<Quantity>,
    pub vm: Option<Quantity>,
    pub samples: SampleCount,
    pub verdict: Verdict,
    /// The tolerance actually in force, recorded so a later widening is visible
    /// by diffing two runs rather than by reading the source at two commits.
    pub tolerance: Tolerance,
}

impl MetricResult {
    pub fn evaluate(
        metric: Metric,
        at: Option<String>,
        sim: Option<Quantity>,
        vm: Option<Quantity>,
        samples: SampleCount,
    ) -> Self {
        let spec = metric.spec();
        let verdict = compare(sim, vm, samples, spec.min_samples, &spec.tolerance);
        MetricResult {
            metric,
            at,
            sim,
            vm,
            samples,
            verdict,
            tolerance: spec.tolerance,
        }
    }

    /// A metric BOTH backends produce but whose two numbers are not the same
    /// physical quantity, recorded as [`Verdict::Inconclusive`] with the reason
    /// in `at`.
    ///
    /// This is a distinct failure from [`Verdict::NotMeasured`], and conflating
    /// them would lose the distinction that matters. NotMeasured means nobody
    /// looked. This means both sides looked, both produced a number, and
    /// comparing them would be comparing two different things and calling the
    /// difference fidelity — the error the context-switches line already avoids
    /// by refusing to feed a VM-wide count against a two-task one.
    ///
    /// Inconclusive rather than NotMeasured on purpose: it sits ABOVE Agree in
    /// the severity lattice, so a run carrying one cannot fold to a clean pass.
    /// A metric we cannot interpret must not read as a metric we verified.
    ///
    /// Both sides' values are still recorded. The point is to stop them being
    /// subtracted, not to hide them.
    pub fn not_comparable(
        metric: Metric,
        at: Option<String>,
        sim: Option<Quantity>,
        vm: Option<Quantity>,
        samples: SampleCount,
    ) -> Self {
        MetricResult {
            metric,
            at,
            sim,
            vm,
            samples,
            verdict: Verdict::Inconclusive,
            tolerance: metric.spec().tolerance,
        }
    }

    /// The tolerance in force, with any absolute arm expressed in the metric's
    /// own units.
    ///
    /// [`ToleranceKind`]'s own `Display` cannot do this: it holds a bare `f64`
    /// and has no unit context, so a 4 ms duration bound renders as `4000000`
    /// on a line whose values read `6.007ms`. Those two strings side by side
    /// invite the conclusion that the bound is six orders of magnitude wider
    /// than it is. Counts and ratios were unaffected, which is why this only
    /// surfaced when the first duration metric acquired an absolute arm.
    pub fn tolerance_display(&self) -> String {
        let in_units = |v: f64| match self.sim.or(self.vm) {
            Some(Quantity::Duration(_)) => Quantity::Duration(DurationNs(v as u64)).to_string(),
            Some(Quantity::Ratio(_)) => Quantity::Ratio(Ratio(v)).to_string(),
            Some(Quantity::Rate(_)) => Quantity::Rate(Hz(v)).to_string(),
            // Counts, and the no-value-on-either-side case, print bare.
            _ => format!("{v}"),
        };
        match self.tolerance.kind {
            ToleranceKind::Relative { frac } => format!("+/-{:.1}%", frac * 100.0),
            ToleranceKind::Absolute { abs } => format!("+/-{}", in_units(abs)),
            ToleranceKind::RelativeOrAbsolute { frac, abs } => {
                format!("+/-{:.1}% or +/-{}", frac * 100.0, in_units(abs))
            }
        }
    }

    /// Relative discrepancy, for trend tracking. `None` when either side is
    /// missing or the reference is zero.
    pub fn relative_gap(&self) -> Option<f64> {
        let (s, v) = (self.sim?, self.vm?);
        if !s.same_kind_as(v) || v.value() == 0.0 {
            return None;
        }
        Some((s.value() - v.value()).abs() / v.value().abs())
    }
}

/// Build the `MeanSliceLength` result, applying the same-scheduler guard.
///
/// The guard is the whole point of this function, not an edge case. Mean slice
/// is a statement about SCHEDULER POLICY, so comparing it across two different
/// schedulers is a category error: they legitimately choose different slice
/// lengths, and a red would say "the simulator is wrong" when the honest
/// reading is "these are two schedulers".
///
/// Producing a number there would be worse than having no metric. It would
/// look like a comparison, sit in the report beside real results, and nothing
/// would tell a reader it compared unlike things — the same defect as an
/// `is_exact()` flag that returns true while fabricating a value.
///
/// So the guard runs FIRST, before either measurement is consulted, and the
/// reason for abstaining is recorded in [`MetricResult::at`] rather than left
/// for the reader to infer from an empty cell.
pub fn mean_slice_result(
    sim_scheduler: &str,
    vm_scheduler: &str,
    sim: Option<DurationNs>,
    vm: Option<DurationNs>,
    samples: SampleCount,
) -> MetricResult {
    let metric = Metric::MeanSliceLength;
    let abstain = |why: String| MetricResult {
        metric,
        at: Some(why),
        sim: sim.map(Quantity::Duration),
        vm: vm.map(Quantity::Duration),
        samples,
        verdict: Verdict::NotMeasured,
        tolerance: metric.spec().tolerance,
    };

    if !same_scheduler(sim_scheduler, vm_scheduler) {
        // Deliberately still carries whatever each side measured, so the
        // numbers are visible for inspection — but with NotMeasured, so they
        // cannot be read as a verdict.
        // Kept short so it does not wreck the report table, but explicit
        // enough that a reader does not have to go looking for the reason.
        return abstain(format!("scheduler mismatch {sim_scheduler}/{vm_scheduler}"));
    }
    if sim.is_none() || vm.is_none() {
        return abstain("a side did not report a mean slice".into());
    }
    MetricResult::evaluate(
        metric,
        None,
        sim.map(Quantity::Duration),
        vm.map(Quantity::Duration),
        samples,
    )
}

/// Do the two sides run the same scheduler?
///
/// String identity after a narrow normalisation: the guest names its scheduler
/// as ktstr registers it (`ktstr_sched`) while the simulator names its `.so`
/// (`ktstr`), so a bare `==` would report a mismatch even once they DO match
/// and the metric would abstain forever without anyone noticing.
///
/// Deliberately narrow — it strips a `scx[-_]` prefix and a `_sched`/`-sched`
/// suffix and nothing else. A looser rule risks the failure this whole guard
/// exists to prevent: declaring two different schedulers equivalent.
fn same_scheduler(a: &str, b: &str) -> bool {
    fn norm(s: &str) -> String {
        let s = s.trim().to_ascii_lowercase().replace('-', "_");
        let s = s.strip_prefix("scx_").unwrap_or(&s).to_string();
        s.strip_suffix("_sched").unwrap_or(&s).to_string()
    }
    !a.trim().is_empty() && !b.trim().is_empty() && norm(a) == norm(b)
}

/// Whether a run's result may be believed at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunOutcome {
    /// Negative control rejected as required; every metric agreed.
    Calibrated,
    /// Negative control rejected as required; at least one metric disagreed or
    /// was inconclusive. A real, interpretable result.
    Gap(Verdict),
    /// The negative control did NOT fail. The harness cannot distinguish a
    /// matching simulator from a meaningless tolerance, so nothing in this run
    /// may be cited — including the metrics that "passed".
    Void { reason: String },
}

impl fmt::Display for RunOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunOutcome::Calibrated => f.write_str("CALIBRATED"),
            RunOutcome::Gap(v) => write!(f, "GAP ({v})"),
            RunOutcome::Void { reason } => write!(f, "VOID — {reason}"),
        }
    }
}

/// A deliberately-wrong comparison the tolerances must reject.
///
/// Without one, a green report is unfalsifiable. The control is evaluated with
/// the SAME [`MetricSpec`] as the real comparison — a control that used looser
/// bounds would prove nothing about the bounds actually in use.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NegativeControl {
    pub metric: Metric,
    pub description: String,
    pub result: MetricResult,
}

impl NegativeControl {
    /// Build a control by perturbing the live value far outside its tolerance.
    ///
    /// `factor` must put the perturbed value outside the bound; the constructor
    /// does not check that, but [`CalibrationRun::finish`] does — by requiring
    /// the control to actually come back Disagree.
    pub fn perturbed(metric: Metric, vm: Quantity, factor: f64, samples: SampleCount) -> Self {
        let wrong = match vm {
            Quantity::Duration(d) => Quantity::Duration(crate::units::DurationNs(
                (d.as_nanos() as f64 * factor) as u64,
            )),
            Quantity::Ratio(r) => Quantity::Ratio(crate::units::Ratio(r.get() * factor)),
            Quantity::Rate(h) => Quantity::Rate(crate::units::Hz(h.get() * factor)),
            Quantity::Count(c) => Quantity::Count((c as f64 * factor) as u64),
        };
        NegativeControl {
            metric,
            description: format!(
                "live {metric} scaled by {factor}x — the pre-registered tolerance must \
                 reject this"
            ),
            result: MetricResult::evaluate(
                metric,
                Some("control".into()),
                Some(wrong),
                Some(vm),
                samples,
            ),
        }
    }

    /// Did the control behave as a control must?
    pub fn rejected_as_required(&self) -> bool {
        self.result.verdict == Verdict::Disagree
    }
}

/// One calibration run: which scenario, which commits, what was measured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalibrationRun {
    pub scenario: String,
    /// Commit of the sched-test side.
    pub sched_test_commit: String,
    /// Commit of the ktstr side.
    pub ktstr_commit: String,
    pub results: Vec<MetricResult>,
    pub negative_control: Option<NegativeControl>,
    /// Which model advanced simulated time on the scxsim side
    /// (`scx_simulator::ClockMode::as_str`: "pmu", "e9patch", "fallback",
    /// "off").
    ///
    /// Provenance, exactly like the two commit fields above. The simulated
    /// clock is chosen at runtime from what the host provides, and the PMU
    /// model makes simulated time a function of THIS machine's hardware — so a
    /// calibration verdict recorded under one clock cannot be compared against
    /// one recorded under another. A run that does not record it is not
    /// comparable to anything, which is why this is serialised as `null`
    /// rather than defaulted to a plausible-looking value.
    ///
    /// Held as a String rather than the enum so this crate stays free of a
    /// dependency on the simulator; `ClockMode::as_str()` is pinned by test.
    #[serde(default)]
    pub clock_mode: Option<String>,
}

impl CalibrationRun {
    pub fn new(
        scenario: impl Into<String>,
        sched_test_commit: impl Into<String>,
        ktstr_commit: impl Into<String>,
    ) -> Self {
        CalibrationRun {
            scenario: scenario.into(),
            sched_test_commit: sched_test_commit.into(),
            ktstr_commit: ktstr_commit.into(),
            results: Vec::new(),
            negative_control: None,
            clock_mode: None,
        }
    }

    /// Record which clock advanced simulated time for this run.
    ///
    /// Pass `scx_simulator::Trace::clock_mode().as_str()`.
    pub fn with_clock_mode(mut self, mode: impl Into<String>) -> Self {
        self.clock_mode = Some(mode.into());
        self
    }

    pub fn record(&mut self, r: MetricResult) {
        self.results.push(r);
    }

    pub fn with_control(mut self, c: NegativeControl) -> Self {
        self.negative_control = Some(c);
        self
    }

    /// Decide the run's outcome, enforcing the control.
    ///
    /// A missing control is as void as a failed one: an unfalsifiable green
    /// result and an unverified one are equally uncitable.
    pub fn finish(&self) -> RunOutcome {
        match &self.negative_control {
            None => RunOutcome::Void {
                reason: "no negative control: nothing establishes that these tolerances \
                         are capable of rejecting a wrong simulator"
                    .into(),
            },
            Some(c) if !c.rejected_as_required() => RunOutcome::Void {
                reason: format!(
                    "negative control did NOT fail (verdict {}): the tolerances cannot \
                     distinguish a matching simulator from a wrong one, so no metric in \
                     this run may be cited — including the ones that agreed",
                    c.result.verdict
                ),
            },
            Some(_) => {
                let worst = Verdict::worst(self.results.iter().map(|r| r.verdict));
                if worst == Verdict::Agree {
                    RunOutcome::Calibrated
                } else {
                    RunOutcome::Gap(worst)
                }
            }
        }
    }

    /// Human-readable summary, control and all.
    pub fn render(&self) -> String {
        use std::fmt::Write as _;
        let mut s = String::new();
        let _ = writeln!(
            s,
            "calibration `{}`  sched-test {}  ktstr {}",
            self.scenario, self.sched_test_commit, self.ktstr_commit
        );
        for r in &self.results {
            let at =
                r.at.as_deref()
                    .map(|a| format!("[{a}]"))
                    .unwrap_or_default();
            let gap = r
                .relative_gap()
                .map(|g| format!("{:+.1}%", g * 100.0))
                .unwrap_or_else(|| "-".into());
            let _ = writeln!(
                s,
                "  {:<18}{:<8} sim={:<14} vm={:<14} gap={:<8} {:<6} {:<14} {}",
                r.metric.name(),
                at,
                r.sim.map(|q| q.to_string()).unwrap_or_else(|| "-".into()),
                r.vm.map(|q| q.to_string()).unwrap_or_else(|| "-".into()),
                gap,
                r.samples.to_string(),
                r.tolerance_display(),
                r.verdict
            );
            if let Some(prev) = &r.tolerance.widened_from {
                let _ = writeln!(s, "      WIDENED from {prev} — bound was relaxed");
            }
        }
        match &self.negative_control {
            Some(c) => {
                let _ = writeln!(
                    s,
                    "  negative control: {} -> {}",
                    c.description, c.result.verdict
                );
            }
            None => {
                let _ = writeln!(s, "  negative control: ABSENT");
            }
        }
        let _ = writeln!(s, "  outcome: {}", self.finish());
        s
    }
}

/// A series of runs, so the gap is a trend rather than an anecdote.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GapSeries {
    pub runs: Vec<CalibrationRun>,
}

impl GapSeries {
    pub fn push(&mut self, run: CalibrationRun) {
        self.runs.push(run);
    }

    /// Relative gap for one metric across the series, oldest first.
    ///
    /// Void runs are excluded: their numbers are not evidence, so letting them
    /// into a trend would launder them.
    pub fn trend(&self, metric: Metric) -> Vec<f64> {
        self.runs
            .iter()
            .filter(|r| !matches!(r.finish(), RunOutcome::Void { .. }))
            .filter_map(|run| {
                run.results
                    .iter()
                    .find(|r| r.metric == metric && r.at.is_none())
                    .and_then(|r| r.relative_gap())
            })
            .collect()
    }

    /// Is the gap for this metric closing? `None` with fewer than two usable
    /// points — a single point is not a trend.
    pub fn is_improving(&self, metric: Metric) -> Option<bool> {
        let t = self.trend(metric);
        if t.len() < 2 {
            return None;
        }
        Some(t[t.len() - 1] < t[0])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::units::{DurationNs, Ratio};

    fn q_dur(ns: u64) -> Quantity {
        Quantity::Duration(DurationNs(ns))
    }

    fn run_with_control() -> CalibrationRun {
        CalibrationRun::new("demo", "abc1234", "def5678").with_control(NegativeControl::perturbed(
            Metric::CpuTime,
            q_dur(1_000_000),
            3.0,
            SampleCount(10),
        ))
    }

    /// The control must actually reject. If a 3x error passes the CpuTime
    /// tolerance, the tolerance is wrong.
    /// A calibration verdict is only comparable against another recorded
    /// under the same clock, so the field must survive a JSON round trip and
    /// must be ABSENT rather than invented when nobody recorded it.
    #[test]
    fn clock_mode_is_recorded_in_the_serialised_artifact() {
        let run = CalibrationRun::new("demo", "abc", "def").with_clock_mode("pmu");
        let json = serde_json::to_string(&run).unwrap();
        assert!(
            json.contains("\"clock_mode\":\"pmu\""),
            "clock mode must reach the artifact, not just the console: {json}"
        );
        let back: CalibrationRun = serde_json::from_str(&json).unwrap();
        assert_eq!(back.clock_mode.as_deref(), Some("pmu"));
    }

    #[test]
    fn an_unlabelled_run_records_no_clock_rather_than_guessing_one() {
        let run = CalibrationRun::new("demo", "abc", "def");
        assert_eq!(
            run.clock_mode, None,
            "defaulting to a plausible clock would make an uncomparable result \
             look comparable"
        );
        // And it must still deserialise from an artifact written before this
        // field existed, rather than failing to load.
        let old = r#"{"scenario":"s","sched_test_commit":"c","ktstr_commit":"k","results":[],"negative_control":null}"#;
        let parsed: CalibrationRun = serde_json::from_str(old).unwrap();
        assert_eq!(parsed.clock_mode, None);
    }

    #[test]
    fn negative_control_rejects_a_threefold_error() {
        let c = NegativeControl::perturbed(Metric::CpuTime, q_dur(1_000_000), 3.0, SampleCount(10));
        assert_eq!(c.result.verdict, Verdict::Disagree);
        assert!(c.rejected_as_required());
    }

    /// THE central guard. A run with no control is void even when every metric
    /// agreed — an unfalsifiable green result is not a result.
    #[test]
    fn run_without_a_negative_control_is_void_even_if_everything_agreed() {
        let mut run = CalibrationRun::new("demo", "abc", "def");
        run.record(MetricResult::evaluate(
            Metric::CpuTime,
            None,
            Some(q_dur(100)),
            Some(q_dur(101)),
            SampleCount(10),
        ));
        assert_eq!(
            Verdict::worst(run.results.iter().map(|r| r.verdict)),
            Verdict::Agree
        );
        match run.finish() {
            RunOutcome::Void { reason } => assert!(reason.contains("no negative control")),
            other => panic!("expected Void, got {other:?}"),
        }
    }

    /// And a control that FAILS to fail is equally void — that is the case where
    /// the tolerances have gone slack without anyone noticing.
    #[test]
    fn run_whose_control_passed_is_void_not_green() {
        let mut run = CalibrationRun::new("demo", "abc", "def");
        run.record(MetricResult::evaluate(
            Metric::CpuTime,
            None,
            Some(q_dur(100)),
            Some(q_dur(100)),
            SampleCount(10),
        ));
        // A 1.01x perturbation is inside the 10% CpuTime tolerance, so this
        // control does NOT reject — exactly the broken-harness situation.
        let weak = NegativeControl::perturbed(Metric::CpuTime, q_dur(1_000), 1.01, SampleCount(10));
        assert_eq!(
            weak.result.verdict,
            Verdict::Agree,
            "control failed to fail"
        );
        let run = run.with_control(weak);
        match run.finish() {
            RunOutcome::Void { reason } => {
                assert!(reason.contains("negative control did NOT fail"));
                assert!(
                    reason.contains("including the ones that agreed"),
                    "must say the passing metrics are also uncitable"
                );
            }
            other => panic!("expected Void, got {other:?}"),
        }
    }

    #[test]
    fn a_good_run_with_all_metrics_agreeing_is_calibrated() {
        let mut run = run_with_control();
        run.record(MetricResult::evaluate(
            Metric::Occupancy,
            None,
            Some(Quantity::Ratio(Ratio(0.50))),
            Some(Quantity::Ratio(Ratio(0.51))),
            SampleCount(10),
        ));
        assert_eq!(run.finish(), RunOutcome::Calibrated);
    }

    #[test]
    fn one_disagreement_makes_the_run_a_gap_not_a_pass() {
        let mut run = run_with_control();
        run.record(MetricResult::evaluate(
            Metric::Occupancy,
            None,
            Some(Quantity::Ratio(Ratio(0.50))),
            Some(Quantity::Ratio(Ratio(0.51))),
            SampleCount(10),
        ));
        run.record(MetricResult::evaluate(
            Metric::CpuTime,
            None,
            Some(q_dur(500)),
            Some(q_dur(100)),
            SampleCount(10),
        ));
        assert_eq!(run.finish(), RunOutcome::Gap(Verdict::Disagree));
    }

    /// Occupancy is held to 5% and must actually bind at that width.
    #[test]
    fn occupancy_tolerance_binds_at_five_percent() {
        let ok = MetricResult::evaluate(
            Metric::Occupancy,
            None,
            Some(Quantity::Ratio(Ratio(0.520))),
            Some(Quantity::Ratio(Ratio(0.500))),
            SampleCount(1),
        );
        assert_eq!(ok.verdict, Verdict::Agree, "4% is inside");
        let bad = MetricResult::evaluate(
            Metric::Occupancy,
            None,
            Some(Quantity::Ratio(Ratio(0.560))),
            Some(Quantity::Ratio(Ratio(0.500))),
            SampleCount(1),
        );
        assert_eq!(bad.verdict, Verdict::Disagree, "12% is outside");
    }

    /// Wake latency needs N>=100; a small sample must not read as agreement no
    /// matter how close the numbers are.
    #[test]
    fn wake_latency_under_one_hundred_samples_is_inconclusive() {
        let r = MetricResult::evaluate(
            Metric::WakeLatency,
            Some("p99".into()),
            Some(q_dur(1_000)),
            Some(q_dur(1_000)),
            SampleCount(99),
        );
        assert_eq!(r.verdict, Verdict::Inconclusive);
    }

    /// Void runs must not enter the trend, or their numbers get laundered into
    /// a graph that looks like evidence.
    #[test]
    fn void_runs_are_excluded_from_the_trend() {
        let mut series = GapSeries::default();

        let mut void_run = CalibrationRun::new("s", "c1", "k1"); // no control
        void_run.record(MetricResult::evaluate(
            Metric::CpuTime,
            None,
            Some(q_dur(900)),
            Some(q_dur(100)),
            SampleCount(10),
        ));
        series.push(void_run);

        let mut good = run_with_control();
        good.record(MetricResult::evaluate(
            Metric::CpuTime,
            None,
            Some(q_dur(110)),
            Some(q_dur(100)),
            SampleCount(10),
        ));
        series.push(good);

        let t = series.trend(Metric::CpuTime);
        assert_eq!(t.len(), 1, "only the non-void run contributes");
        assert!((t[0] - 0.10).abs() < 1e-9);
    }

    #[test]
    fn trend_needs_two_points_to_have_a_direction() {
        let mut series = GapSeries::default();
        let mut a = run_with_control();
        a.record(MetricResult::evaluate(
            Metric::CpuTime,
            None,
            Some(q_dur(150)),
            Some(q_dur(100)),
            SampleCount(10),
        ));
        series.push(a);
        assert_eq!(series.is_improving(Metric::CpuTime), None);

        let mut b = run_with_control();
        b.record(MetricResult::evaluate(
            Metric::CpuTime,
            None,
            Some(q_dur(105)),
            Some(q_dur(100)),
            SampleCount(10),
        ));
        series.push(b);
        assert_eq!(series.is_improving(Metric::CpuTime), Some(true));
    }

    /// A widened bound must be visible in the rendered report.
    /// An absolute tolerance is rendered in the metric's own units.
    ///
    /// The regression this pins: `ToleranceKind`'s `Display` prints a bare f64,
    /// so the scheduling-delay bound appeared as `+/-4000000` on a line reading
    /// `sim=6.007ms vm=3.694ms`. Nothing there tells a reader the bound is 4 ms
    /// and not 4 million of whatever the values are in — and the whole value of
    /// this report is that someone can read a verdict and check it.
    #[test]
    fn an_absolute_tolerance_renders_in_the_metrics_own_units() {
        let r = MetricResult::evaluate(
            Metric::SchedulingDelay,
            Some("cg_0".into()),
            Some(Quantity::Duration(DurationNs(6_007_000))),
            Some(Quantity::Duration(DurationNs(3_694_068))),
            SampleCount(1),
        );
        assert_eq!(r.tolerance_display(), "+/-20.0% or +/-4.000ms");

        // Counts keep the bare rendering: there is no unit to apply.
        let c = MetricResult::evaluate(
            Metric::Migrations,
            None,
            Some(Quantity::Count(0)),
            Some(Quantity::Count(16)),
            SampleCount(1),
        );
        assert_eq!(c.tolerance_display(), "+/-25.0% or +/-2");
    }

    #[test]
    fn render_flags_a_widened_tolerance() {
        let mut run = run_with_control();
        let mut r = MetricResult::evaluate(
            Metric::CpuTime,
            None,
            Some(q_dur(100)),
            Some(q_dur(100)),
            SampleCount(10),
        );
        r.tolerance = Tolerance::relative(0.90, "relaxed")
            .widening(crate::verdict::ToleranceKind::Relative { frac: 0.10 });
        run.record(r);
        let out = run.render();
        assert!(out.contains("WIDENED from"), "{out}");
    }

    /// The scheduling-delay bound was derived from what a policy comparison
    /// needs, before either side could measure it. Pin the exact values so a
    /// later edit has to be deliberate and visible in a diff, rather than a
    /// quiet retune once a real measurement lands and disagrees.
    #[test]
    fn scheduling_delay_tolerance_is_the_pre_registered_one() {
        let spec = Metric::SchedulingDelay.spec();
        assert_eq!(
            spec.tolerance.kind,
            crate::verdict::ToleranceKind::RelativeOrAbsolute {
                frac: 0.20,
                abs: 4_000_000.0,
            },
            "scheduling-delay tolerance changed. 0.20 resolves a 1.5x policy \
             difference ((1+f)/(1-f)); 4ms is one scheduler tick, below which no \
             policy conclusion can rest. If this must change, use \
             Tolerance::widened_from so the report shows it."
        );
        assert!(
            !spec.distributional,
            "registered as an aggregate, not a distribution"
        );
        assert!(
            spec.tolerance.widened_from.is_none(),
            "the pre-registered bound has been widened; that must be argued, not assumed"
        );
    }

    /// A run in which either side fails to supply the quantity must report
    /// NotMeasured — never a pass. Extraction is wired on both sides now, so
    /// this covers the case where a run does not produce it: an empty cgroup on
    /// the live side, or a scenario with no task in it on the simulated side.
    /// A bound with no measurement behind it silently counting as agreement is
    /// the failure this crate exists to stop.
    #[test]
    fn scheduling_delay_is_not_measured_until_both_sides_supply_it() {
        let r = MetricResult::evaluate(Metric::SchedulingDelay, None, None, None, SampleCount(0));
        assert_eq!(r.verdict, Verdict::NotMeasured);
    }

    #[test]
    fn every_registry_metric_has_a_rationale() {
        for m in Metric::ALL {
            let spec = m.spec();
            assert!(
                spec.tolerance.rationale.len() > 30,
                "{m} needs a real rationale, got {:?}",
                spec.tolerance.rationale
            );
        }
    }
    /// The mean-slice bound is PINNED, because its only property is that it was
    /// fixed before anyone compared the two sides.
    ///
    /// Derived blind under tg `blind_tolerance_derivation_what` by an agent
    /// that had not seen the measurement. If a future run comes back far
    /// outside 10% and 10% starts to feel harsh, THAT IS THE BOUND WORKING.
    /// Changing these numbers in place silently converts a pre-registered
    /// tolerance into one fitted to the result — use `Tolerance::widening()`
    /// instead, which records the previous bound and prints it in the report.
    #[test]
    fn the_blind_mean_slice_bound_is_exactly_as_derived() {
        let spec = Metric::MeanSliceLength.spec();
        assert_eq!(
            spec.tolerance.kind,
            crate::verdict::ToleranceKind::RelativeOrAbsolute {
                frac: 0.10,
                abs: 50_000.0,
            },
            "relative 0.10 with a 50us absolute arm, as derived blind",
        );
        assert_eq!(spec.min_samples, MinSamples::PERCENTILE);
        assert!(!spec.distributional);
        assert!(
            spec.tolerance.widened_from.is_none(),
            "the bound has never been relaxed; if it is, that must be recorded \
             as a widening rather than edited in place",
        );
        // The scope restriction is load-bearing, not commentary: comparing
        // slice lengths across two different schedulers is a category error.
        assert!(
            spec.tolerance.rationale.contains("SAME"),
            "the rationale must keep the same-scheduler scope explicit",
        );
    }
    /// The guard, in both directions.
    ///
    /// The abstention is the case that matters, but a guard that abstains
    /// unconditionally is indistinguishable from a broken metric — so the
    /// positive direction is tested too.
    #[test]
    fn mean_slice_abstains_across_schedulers_and_evaluates_within_one() {
        let sim = Some(DurationNs(20_000_000));
        let vm = Some(DurationNs(21_000_000)); // 5% apart: inside the 10% bound.
        let n = SampleCount(200);

        let across = mean_slice_result("simple", "ktstr_sched", sim, vm, n);
        assert_eq!(across.verdict, Verdict::NotMeasured);
        assert!(
            across
                .at
                .as_deref()
                .unwrap_or_default()
                .contains("scheduler mismatch"),
            "the abstention must name its reason: {:?}",
            across.at,
        );
        assert!(
            across.sim.is_some() && across.vm.is_some(),
            "both measurements stay visible for inspection — they just carry no verdict",
        );

        let within = mean_slice_result("simple", "simple", sim, vm, n);
        assert_eq!(
            within.verdict,
            Verdict::Agree,
            "same scheduler, 5% apart, 200 samples — must actually evaluate",
        );
    }

    /// Naming differs across the two sides for the SAME scheduler, and a bare
    /// `==` would abstain forever without anyone noticing.
    #[test]
    fn scheduler_identity_tolerates_naming_but_not_difference() {
        assert!(
            same_scheduler("ktstr", "ktstr_sched"),
            "sim .so vs guest name"
        );
        assert!(same_scheduler("scx_lavd", "lavd"));
        assert!(same_scheduler("Simple", "simple"));
        assert!(!same_scheduler("simple", "lavd"), "different schedulers");
        assert!(!same_scheduler("simple", "ktstr_sched"), "today's pairing");
        assert!(!same_scheduler("", "simple"), "unknown is not a match");
    }

    /// A same-scheduler pairing still abstains when a side has no measurement.
    /// Absence must not read as agreement.
    #[test]
    fn mean_slice_abstains_when_a_side_reported_nothing() {
        let r = mean_slice_result(
            "simple",
            "simple",
            Some(DurationNs(20_000_000)),
            None,
            SampleCount(200),
        );
        assert_eq!(r.verdict, Verdict::NotMeasured);
        assert!(r
            .at
            .as_deref()
            .unwrap_or_default()
            .contains("did not report"));
    }

    /// The bound binds once the guard lets a comparison through — 70% apart
    /// must fail. Guards the case where a future change makes the guard pass
    /// everything.
    #[test]
    fn a_large_same_scheduler_divergence_is_a_disagreement() {
        let r = mean_slice_result(
            "simple",
            "simple",
            Some(DurationNs(20_000_000)),
            Some(DurationNs(34_000_000)),
            SampleCount(200),
        );
        assert_eq!(r.verdict, Verdict::Disagree);
    }
}
