//! How faithful a lowering was, recorded rather than discarded.
//!
//! The lowering is deliberately permissive: the simulator models *how much real
//! time passes* and *what the scheduler sees*, so a work type whose point is a
//! cache footprint or an IPC ratio can legitimately collapse to "spin for this
//! long". That is a modelling decision, not a bug.
//!
//! What would be a bug is doing it silently. Every construct that does not
//! survive lowering intact produces an [`Approximation`] naming the source
//! construct, what it became, and **which dimension was dropped**. The report
//! travels with the IR, so a consumer can print it, gate on it, or refuse a
//! workload whose fidelity is too low for the question being asked.
//!
//! The third outcome is refusal. See [`crate::LoweringError`]: a construct that
//! cannot be expressed without inventing behaviour is rejected, never
//! approximated into something plausible.

use std::fmt;

use serde::{Deserialize, Serialize};

/// How completely one source construct survived lowering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Fidelity {
    /// Scheduler-visible semantics preserved. The simulator will see what the
    /// real workload would have made it see.
    Exact,
    /// Expressible as time plus scheduler state, with some dimension of the
    /// original dropped. The dropped dimension is named in the
    /// [`Approximation`].
    Approximated,
}

impl fmt::Display for Fidelity {
    /// Deliberately says what was *carried*, not that the run is faithful.
    ///
    /// This used to render as the bare word `exact`, which was true of the
    /// lowering and read as a verdict on the simulation. The two are different
    /// claims and they were spelled identically, so a reader could see
    /// `fidelity: exact` on a scenario whose whole point was cpuset
    /// confinement, on a run where the simulator ignored the cpuset entirely
    /// (sim-4qlh5), and reasonably conclude the property had been reproduced.
    ///
    /// A self-report whose scope is narrower than its apparent claim is worse
    /// than a missing one: it is trusted. The wording is now scoped to the only
    /// thing this type can actually know — the lowering runs before the
    /// simulator exists, so it cannot verify downstream honouring, and it must
    /// not sound as though it has.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Fidelity::Exact => f.write_str("all declared fields carried"),
            Fidelity::Approximated => f.write_str("approximated"),
        }
    }
}

/// Why an approximation is acceptable — the class of thing that was dropped.
///
/// Grouped rather than free-text so a consumer can gate mechanically ("refuse
/// any workload with a dropped [`Cause::BlockingMechanism`]") instead of
/// grepping prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Cause {
    /// Cache footprint, IPC, ALU width, SMT sibling pressure. The simulator has
    /// no microarchitectural model and, per the design, does not want one: only
    /// elapsed time and scheduler state matter.
    Microarchitectural,
    /// Page placement, working-set sweeps, fault churn. The simulator models
    /// NUMA topology but not page residency.
    MemoryPlacement,
    /// Real I/O. Off-CPU *time* is preserved (it is scheduler state); the
    /// device, queue, and byte counts are not.
    IoMechanism,
    /// The specific mechanism a task blocks or is woken by (futex vs pipe vs
    /// epoll vs signal). The wake edge is preserved; which syscall produced it
    /// is not.
    BlockingMechanism,
    /// Runtime changes to affinity, nice, or scheduling policy that the IR
    /// records as an initial attribute instead of a mid-run mutation.
    DynamicSchedAttr,
    /// Task creation/exit churn folded into a fixed task set.
    TaskLifecycle,
    /// An iteration count that had to become a duration. Iterations are not a
    /// unit the simulator advances in; see [`crate::lower::ITER_NS`].
    IterationsToTime,
}

impl Cause {
    /// One-line explanation, used by the pretty printer and the warning text.
    pub fn explain(self) -> &'static str {
        match self {
            Cause::Microarchitectural => {
                "microarchitectural effect; the simulator models elapsed time and scheduler state only"
            }
            Cause::MemoryPlacement => "page placement; the simulator models NUMA topology but not residency",
            Cause::IoMechanism => "I/O mechanism; off-CPU time is preserved, the device is not modelled",
            Cause::BlockingMechanism => "block/wake mechanism; the wake edge is preserved, the syscall is not",
            Cause::DynamicSchedAttr => "runtime sched-attribute change lowered to an initial attribute",
            Cause::TaskLifecycle => "task create/exit churn folded into a fixed task set",
            Cause::IterationsToTime => "iteration count converted to a duration estimate",
        }
    }
}

impl fmt::Display for Cause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Cause::Microarchitectural => "microarchitectural",
            Cause::MemoryPlacement => "memory-placement",
            Cause::IoMechanism => "io-mechanism",
            Cause::BlockingMechanism => "blocking-mechanism",
            Cause::DynamicSchedAttr => "dynamic-sched-attr",
            Cause::TaskLifecycle => "task-lifecycle",
            Cause::IterationsToTime => "iterations-to-time",
        };
        f.write_str(s)
    }
}

/// One recorded loss of fidelity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approximation {
    /// The source construct, named as the author wrote it (e.g.
    /// `"WorkType::CachePressure"`).
    pub source: String,
    /// What it became in the IR (e.g. `"Run(1.000ms) x repeat"`).
    pub lowered_to: String,
    /// The class of thing that was dropped.
    pub cause: Cause,
    /// The specific dropped detail, with values (e.g.
    /// `"size_kib=256, stride=64"`). Concrete, so a reader can judge whether it
    /// mattered for their question.
    pub dropped: String,
}

impl fmt::Display for Approximation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} -> {} [{}] dropped: {} ({})",
            self.source,
            self.lowered_to,
            self.cause,
            self.dropped,
            self.cause.explain()
        )
    }
}

/// Every approximation made while lowering one workload.
///
/// Attached to the [`crate::WorkloadIr`] rather than logged, so it cannot be
/// lost between producing the IR and acting on it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FidelityReport {
    approximations: Vec<Approximation>,
}

impl FidelityReport {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, a: Approximation) {
        self.approximations.push(a);
    }

    pub fn approximations(&self) -> &[Approximation] {
        &self.approximations
    }

    /// True when every construct lowered exactly — i.e. the IR carried every
    /// declared field.
    ///
    /// This is about the LOWERING and nothing else. A field can be carried
    /// faithfully and then ignored by the simulator, and this still returns
    /// true: sim-4qlh5 was exactly that, a cpuset that arrived intact and was
    /// not enforced at placement. Do not read it as "the run was faithful".
    pub fn is_exact(&self) -> bool {
        self.approximations.is_empty()
    }

    /// Overall fidelity: [`Fidelity::Exact`] only if nothing was approximated.
    pub fn overall(&self) -> Fidelity {
        if self.is_exact() {
            Fidelity::Exact
        } else {
            Fidelity::Approximated
        }
    }

    /// Approximations of one cause — the gating hook. A caller asking a question
    /// that depends on real I/O can refuse a workload with any
    /// [`Cause::IoMechanism`] entry, while a caller asking about fairness can
    /// ignore it.
    pub fn by_cause(&self, cause: Cause) -> impl Iterator<Item = &Approximation> {
        self.approximations.iter().filter(move |a| a.cause == cause)
    }

    /// Distinct causes present, in a stable order (sorted), so callers and tests
    /// can compare reports without depending on lowering order.
    pub fn causes(&self) -> Vec<Cause> {
        let mut c: Vec<Cause> = self.approximations.iter().map(|a| a.cause).collect();
        c.sort_unstable();
        c.dedup();
        c
    }

    /// Human-readable warning block, one line per approximation. Empty string
    /// when exact, so a caller can `if !s.is_empty() { eprint!("{s}") }`.
    pub fn warnings(&self) -> String {
        if self.approximations.is_empty() {
            return String::new();
        }
        let mut s = format!(
            "workload IR: {} approximation(s) — the simulator will not reproduce these dimensions:\n",
            self.approximations.len()
        );
        for a in &self.approximations {
            s.push_str(&format!("  warning: {a}\n"));
        }
        s
    }

    pub fn merge(&mut self, other: FidelityReport) {
        self.approximations.extend(other.approximations);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(cause: Cause) -> Approximation {
        Approximation {
            source: "WorkType::X".into(),
            lowered_to: "Run(1.000ms)".into(),
            cause,
            dropped: "detail".into(),
        }
    }

    #[test]
    fn empty_report_is_exact_and_silent() {
        let r = FidelityReport::new();
        assert!(r.is_exact());
        assert_eq!(r.overall(), Fidelity::Exact);
        assert_eq!(
            r.warnings(),
            "",
            "an exact lowering must emit no warning text"
        );
        assert!(r.causes().is_empty());
    }

    /// The gating hook: a caller must be able to select the approximations that
    /// matter for its question without string-matching.
    #[test]
    fn by_cause_selects_and_causes_are_sorted_and_deduped() {
        let mut r = FidelityReport::new();
        r.record(approx(Cause::IoMechanism));
        r.record(approx(Cause::Microarchitectural));
        r.record(approx(Cause::IoMechanism));

        assert_eq!(r.by_cause(Cause::IoMechanism).count(), 2);
        assert_eq!(r.by_cause(Cause::MemoryPlacement).count(), 0);
        // Sorted + deduped => stable regardless of the order they were recorded.
        assert_eq!(
            r.causes(),
            vec![Cause::Microarchitectural, Cause::IoMechanism]
        );
        assert_eq!(r.overall(), Fidelity::Approximated);
    }

    /// The whole point of the type: the dropped dimension and its values must
    /// reach the reader, not just the fact that something was dropped.
    #[test]
    fn warning_text_names_the_dropped_dimension_with_values() {
        let mut r = FidelityReport::new();
        r.record(Approximation {
            source: "WorkType::CachePressure".into(),
            lowered_to: "Run(1.000ms) forever".into(),
            cause: Cause::Microarchitectural,
            dropped: "size_kib=256, stride=64".into(),
        });
        let w = r.warnings();
        assert!(
            w.contains("WorkType::CachePressure"),
            "names the source construct"
        );
        assert!(
            w.contains("size_kib=256, stride=64"),
            "names the dropped values"
        );
        assert!(w.contains("microarchitectural"), "names the cause class");
        assert!(w.contains("warning:"), "reads as a warning");
    }

    #[test]
    fn merge_concatenates() {
        let mut a = FidelityReport::new();
        a.record(approx(Cause::IoMechanism));
        let mut b = FidelityReport::new();
        b.record(approx(Cause::TaskLifecycle));
        a.merge(b);
        assert_eq!(a.approximations().len(), 2);
    }
}
