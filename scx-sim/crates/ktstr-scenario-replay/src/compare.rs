//! Hold the two backends to the same number.
//!
//! One ktstr scenario definition drives two backends: the VM (ktstr's own
//! runner, on a real kernel) and the simulator. The milestone's central claim is
//! that they produce the same answer. Until this module existed that claim was
//! checked by a human reading two sets of numbers side by side when told to go
//! and look — and a 2x disagreement on `sched_dynamic_add` survived both test
//! suites for exactly that reason. One suite asserts that each scenario
//! executes; the other asserts cpuset containment. Neither asserted that the two
//! backends agree about anything numeric.
//!
//! # What is compared
//!
//! Per-cgroup CPU time, reduced to each cgroup's **share** of the run's total:
//! `s_c = cpu_c / sum(cpu)`.
//!
//! Share rather than absolute CPU-seconds, because absolute time is not what the
//! simulator is trusted for and differs between the backends for reasons that
//! are not fidelity — boot, ramp, and simulated-versus-real duration. The
//! conclusions anyone draws from a scenario ("these cgroups split the machine
//! evenly", "this one is starved") are share conclusions.
//!
//! The cost of normalising is stated rather than hidden: a divergence that
//! scales every cgroup by the same factor moves no share and is invisible to the
//! share tests. [`GROSS_TOTAL_RATIO`] is a deliberately loose backstop for that
//! case — it catches a run that used the wrong duration or the wrong topology.
//! It is a wiring check, not a fidelity one, and it is not sensitive enough to
//! substitute for a real throughput oracle. There is no throughput oracle here.
//!
//! # The bound, and where it came from
//!
//! The three constants below were **pre-registered before any cross-backend
//! number was read** — before the recorded baselines were opened and before the
//! simulator was run — and derived from what would change a decision, not from
//! what currently passes. A tolerance chosen after seeing the gap is fitted to
//! the gap, and the check becomes decorative.
//!
//! * [`SHARE_ABS_TOLERANCE`] = 0.05. The floor exists for the starvation
//!   decision — "did this cgroup get approximately nothing". Relative error is
//!   meaningless near zero, so without a floor a near-idle cgroup fails on
//!   jitter. 5 percentage points sits below the smallest share anyone draws a
//!   conclusion from and above per-run noise in a spin-wait workload.
//!
//! * [`SHARE_REL_TOLERANCE`] = 0.25. Set by a separation property against the
//!   smallest distinction these schedulers are routinely asked to make, which is
//!   a 2x weight step. Within this band two true shares a factor of two apart
//!   can never be confused: the highest reading of the smaller (1.25s) stays
//!   clearly below the lowest reading of the larger (1.6s). Bands looser than
//!   roughly 40% start to blur adjacent 2x steps, so 25% is the round number
//!   comfortably inside the requirement.
//!
//! * [`GROSS_TOTAL_RATIO`] = 2.0. Loose on purpose; see above.
//!
//! The relative test is applied only where a share is large enough for a ratio
//! to mean anything ([`SHARE_REL_FLOOR`]); below that the absolute test governs.
//! Both tests must pass.
//!
//! # This compares against a recording, not against the VM today
//!
//! The VM side is a committed baseline under `baselines/`, produced by
//! `scripts/record_vm_baseline.py` from a ktstr stats sidecar. So the check
//! answers "does the simulator still agree with the VM run we recorded", not
//! "does it agree with the VM right now". That is the trade that makes it a
//! committed check runnable in a normal `cargo test` rather than a nightly that
//! needs a kernel and a VM.
//!
//! The baseline therefore carries its own provenance — kernel, ktstr commit,
//! topology, recording date — so a reader can judge whether it is still the
//! right reference, and re-recording is one script invocation.

use std::collections::{BTreeMap, BTreeSet};

/// Absolute tolerance on a cgroup's share of total CPU time, in share units
/// (0.05 = 5 percentage points). See the module docs for the derivation.
pub const SHARE_ABS_TOLERANCE: f64 = 0.05;

/// Relative tolerance on a cgroup's share, as a fraction (0.25 = ±25%).
pub const SHARE_REL_TOLERANCE: f64 = 0.25;

/// Shares below this are governed by the absolute test alone: a ratio between
/// two tiny shares is dominated by noise and would fail for no useful reason.
pub const SHARE_REL_FLOOR: f64 = 0.10;

/// Gross-sanity band on total CPU time across all cgroups, as a ratio. Catches
/// wrong-duration and wrong-topology wiring, not fidelity.
pub const GROSS_TOTAL_RATIO: f64 = 2.0;

/// One cgroup's two readings and how far apart they are.
#[derive(Debug, Clone, PartialEq)]
pub struct CgroupComparison {
    /// The cgroup, as both backends name it.
    pub cgroup: String,
    /// CPU time the VM baseline recorded, in nanoseconds.
    pub vm_ns: u64,
    /// CPU time the simulator produced, in nanoseconds.
    pub sim_ns: u64,
    /// The VM's share of its run's total CPU time.
    pub vm_share: f64,
    /// The simulator's share of its run's total CPU time.
    pub sim_share: f64,
    /// `sim_share - vm_share`, signed so the direction of a divergence is
    /// visible rather than only its size.
    pub share_delta: f64,
    /// `sim_share / vm_share`, or `None` where the VM share is zero and the
    /// ratio is undefined.
    pub share_ratio: Option<f64>,
}

impl CgroupComparison {
    /// Whether this cgroup is inside the bound. Both tests must pass; the
    /// relative one applies only above [`SHARE_REL_FLOOR`].
    #[must_use]
    pub fn agrees(&self) -> bool {
        if self.share_delta.abs() > SHARE_ABS_TOLERANCE {
            return false;
        }
        if self.vm_share.max(self.sim_share) < SHARE_REL_FLOOR {
            return true;
        }
        match self.share_ratio {
            // A zero VM share paired with a sim share above the floor is a
            // disagreement by construction, not an undefined ratio to wave
            // through: one backend ran this cgroup and the other did not.
            None => false,
            Some(r) => (1.0 - SHARE_REL_TOLERANCE..=1.0 + SHARE_REL_TOLERANCE).contains(&r),
        }
    }
}

/// Why two backends were judged not to agree. Separated by kind because the
/// remedies differ: a cgroup-set mismatch is a wiring or naming fault, an
/// out-of-band share is a behavioural divergence.
#[derive(Debug, Clone, PartialEq)]
pub enum Mismatch {
    /// A cgroup one backend reported and the other did not. Named rather than
    /// skipped: dropping it would let the surviving shares add up to something
    /// plausible while describing a different workload.
    CgroupSet {
        /// Present in the VM baseline, absent from the simulator run.
        vm_only: BTreeSet<String>,
        /// Present in the simulator run, absent from the VM baseline.
        sim_only: BTreeSet<String>,
    },
    /// A cgroup whose share is outside the bound.
    Share(CgroupComparison),
    /// Total CPU time outside the gross-sanity band.
    GrossTotal {
        /// Total CPU time in the VM baseline, nanoseconds.
        vm_ns: u64,
        /// Total CPU time in the simulator run, nanoseconds.
        sim_ns: u64,
        /// `sim_ns / vm_ns`.
        ratio: f64,
    },
    /// A backend reported no CPU time at all, so shares cannot be formed. This
    /// is a failure, not a vacuous pass — it is what a broken extraction looks
    /// like, and it is the shape most likely to make a comparison check silently
    /// stop comparing.
    NoCpuTime {
        /// Whether the VM baseline was the empty side.
        vm_empty: bool,
        /// Whether the simulator run was the empty side.
        sim_empty: bool,
    },
}

/// The full result of holding one scenario's two backends to the same number.
#[derive(Debug, Clone, PartialEq)]
pub struct Comparison {
    /// Every cgroup both backends reported, in name order.
    pub cgroups: Vec<CgroupComparison>,
    /// Everything that put this outside the bound. Empty means agreement.
    pub mismatches: Vec<Mismatch>,
    /// Total CPU time in the VM baseline, nanoseconds.
    pub vm_total_ns: u64,
    /// Total CPU time in the simulator run, nanoseconds.
    pub sim_total_ns: u64,
}

impl Comparison {
    /// Whether the two backends agree within the pre-registered bound.
    #[must_use]
    pub fn agrees(&self) -> bool {
        self.mismatches.is_empty()
    }

    /// The largest absolute share delta over all compared cgroups, which is the
    /// single number worth quoting when describing how far apart two backends
    /// are. `0.0` when there is nothing to compare.
    #[must_use]
    pub fn worst_share_delta(&self) -> f64 {
        self.cgroups
            .iter()
            .map(|c| c.share_delta.abs())
            .fold(0.0, f64::max)
    }

    /// A table fit for a test failure message: every cgroup, both readings, and
    /// a marker on the rows that broke the bound.
    #[must_use]
    pub fn report(&self) -> String {
        use std::fmt::Write as _;
        let mut s = String::new();
        let _ = writeln!(
            s,
            "  {:<14} {:>12} {:>12} {:>9} {:>9} {:>9} {:>8}",
            "cgroup", "vm (ms)", "sim (ms)", "vm share", "sim share", "delta", "ratio"
        );
        for c in &self.cgroups {
            let ratio = match c.share_ratio {
                Some(r) => format!("{r:.3}"),
                None => "  n/a".to_string(),
            };
            let _ = writeln!(
                s,
                "  {:<14} {:>12.1} {:>12.1} {:>9.4} {:>9.4} {:>+9.4} {:>8} {}",
                c.cgroup,
                c.vm_ns as f64 / 1e6,
                c.sim_ns as f64 / 1e6,
                c.vm_share,
                c.sim_share,
                c.share_delta,
                ratio,
                if c.agrees() { "" } else { "  <-- OUT OF BOUND" },
            );
        }
        let _ = writeln!(
            s,
            "  {:<14} {:>12.1} {:>12.1}   (total; ratio {:.3})",
            "TOTAL",
            self.vm_total_ns as f64 / 1e6,
            self.sim_total_ns as f64 / 1e6,
            ratio_or_nan(self.sim_total_ns, self.vm_total_ns),
        );
        let _ = write!(
            s,
            "  bound: |delta| <= {SHARE_ABS_TOLERANCE}, and ratio within \
             +/-{:.0}% where either share >= {SHARE_REL_FLOOR}",
            SHARE_REL_TOLERANCE * 100.0,
        );
        s
    }
}

fn ratio_or_nan(num: u64, den: u64) -> f64 {
    if den == 0 {
        f64::NAN
    } else {
        num as f64 / den as f64
    }
}

/// Compare a VM baseline against a simulator run.
///
/// Both maps are cgroup name to total CPU time in nanoseconds — the VM side
/// from `stats.cgroups[].total_cpu_time_ns` in a ktstr sidecar, the simulator
/// side from [`crate::cgroup_cpu_time`].
///
/// A cgroup present in only one backend is reported as a
/// [`Mismatch::CgroupSet`] and excluded from the share table, because a share
/// computed against a different set of participants is not comparable. It is
/// still a failure — see the variant's own note.
#[must_use]
pub fn compare(vm: &BTreeMap<String, u64>, sim: &BTreeMap<String, u64>) -> Comparison {
    let vm_total: u64 = vm.values().sum();
    let sim_total: u64 = sim.values().sum();

    let mut mismatches = Vec::new();

    let vm_names: BTreeSet<&String> = vm.keys().collect();
    let sim_names: BTreeSet<&String> = sim.keys().collect();
    let vm_only: BTreeSet<String> = vm_names
        .difference(&sim_names)
        .map(|s| (*s).clone())
        .collect();
    let sim_only: BTreeSet<String> = sim_names
        .difference(&vm_names)
        .map(|s| (*s).clone())
        .collect();
    if !vm_only.is_empty() || !sim_only.is_empty() {
        mismatches.push(Mismatch::CgroupSet { vm_only, sim_only });
    }

    if vm_total == 0 || sim_total == 0 {
        mismatches.push(Mismatch::NoCpuTime {
            vm_empty: vm_total == 0,
            sim_empty: sim_total == 0,
        });
        // Shares are undefined; return what is known rather than dividing by
        // zero into a table of NaNs that reads like data.
        return Comparison {
            cgroups: Vec::new(),
            mismatches,
            vm_total_ns: vm_total,
            sim_total_ns: sim_total,
        };
    }

    let total_ratio = sim_total as f64 / vm_total as f64;
    if !(1.0 / GROSS_TOTAL_RATIO..=GROSS_TOTAL_RATIO).contains(&total_ratio) {
        mismatches.push(Mismatch::GrossTotal {
            vm_ns: vm_total,
            sim_ns: sim_total,
            ratio: total_ratio,
        });
    }

    let mut cgroups = Vec::new();
    for (name, vm_ns) in vm {
        let Some(sim_ns) = sim.get(name) else {
            continue;
        };
        let vm_share = *vm_ns as f64 / vm_total as f64;
        let sim_share = *sim_ns as f64 / sim_total as f64;
        let c = CgroupComparison {
            cgroup: name.clone(),
            vm_ns: *vm_ns,
            sim_ns: *sim_ns,
            vm_share,
            sim_share,
            share_delta: sim_share - vm_share,
            share_ratio: if vm_share == 0.0 {
                None
            } else {
                Some(sim_share / vm_share)
            },
        };
        if !c.agrees() {
            mismatches.push(Mismatch::Share(c.clone()));
        }
        cgroups.push(c);
    }

    Comparison {
        cgroups,
        mismatches,
        vm_total_ns: vm_total,
        sim_total_ns: sim_total,
    }
}

/// A VM baseline as committed under `baselines/`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct VmBaseline {
    /// The ktstr scenario this records.
    pub scenario: String,
    /// Per-cgroup total CPU time in nanoseconds, as the VM measured it.
    pub per_cgroup_cpu_time_ns: BTreeMap<String, u64>,
    /// Where this recording came from, so a reader can judge whether it is
    /// still the right reference.
    pub provenance: BaselineProvenance,
}

/// Enough about a recording to decide whether to trust it.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct BaselineProvenance {
    /// Kernel the VM ran.
    pub kernel: String,
    /// ktstr revision that produced the sidecar.
    pub project_commit: Option<String>,
    /// ktstr's topology string for the run.
    pub topology: Option<String>,
    /// Scheduler the VM backend ran. Note this is ktstr's own BPF scheduler,
    /// not one of the simulator's — see the caveat in `tests/cross_backend.rs`.
    pub scheduler: Option<String>,
    /// When the VM run happened, UTC.
    pub recorded_utc: Option<String>,
    /// Sidecar filename the extract came from.
    pub sidecar: Option<String>,
    /// Whether the VM run passed. A baseline recorded from a failing run would
    /// make the simulator agree with a broken reference.
    pub passed: bool,
}

/// Read a committed VM baseline.
///
/// # Errors
///
/// Returns the underlying read or parse failure. A missing baseline is an error
/// rather than an empty default: a comparison against nothing would pass.
pub fn load_baseline(path: &std::path::Path) -> Result<VmBaseline, ReadBaselineError> {
    let bytes = std::fs::read(path).map_err(ReadBaselineError::Read)?;
    serde_json::from_slice(&bytes).map_err(ReadBaselineError::Parse)
}

/// Why a baseline could not be read.
#[derive(Debug)]
pub enum ReadBaselineError {
    /// The file could not be read.
    Read(std::io::Error),
    /// The file is not a [`VmBaseline`].
    Parse(serde_json::Error),
}

impl std::fmt::Display for ReadBaselineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadBaselineError::Read(e) => write!(f, "read VM baseline: {e}"),
            ReadBaselineError::Parse(e) => write!(
                f,
                "VM baseline is not the expected shape: {e}. Re-record with \
                 scripts/record_vm_baseline.py rather than hand-editing."
            ),
        }
    }
}

impl std::error::Error for ReadBaselineError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pairs: &[(&str, u64)]) -> BTreeMap<String, u64> {
        pairs.iter().map(|(k, v)| ((*k).to_string(), *v)).collect()
    }

    #[test]
    fn identical_runs_agree() {
        let a = m(&[("cg_0", 6_000_000_000), ("cg_1", 6_000_000_000)]);
        let c = compare(&a, &a);
        assert!(c.agrees(), "{}", c.report());
        assert_eq!(c.worst_share_delta(), 0.0);
    }

    /// Different absolute magnitudes with the same split agree, which is the
    /// deliberate consequence of comparing shares. Documented as a test so the
    /// blind spot is visible rather than discovered.
    #[test]
    fn same_split_at_different_scale_agrees() {
        let vm = m(&[("cg_0", 6_000_000_000), ("cg_1", 6_000_000_000)]);
        let sim = m(&[("cg_0", 9_000_000_000), ("cg_1", 9_000_000_000)]);
        assert!(compare(&vm, &sim).agrees());
    }

    /// ...but only up to the gross-sanity band.
    #[test]
    fn same_split_at_wildly_different_scale_is_caught() {
        let vm = m(&[("cg_0", 1_000_000_000), ("cg_1", 1_000_000_000)]);
        let sim = m(&[("cg_0", 3_000_000_000), ("cg_1", 3_000_000_000)]);
        let c = compare(&vm, &sim);
        assert!(!c.agrees());
        assert!(matches!(c.mismatches[0], Mismatch::GrossTotal { .. }));
    }

    /// The property the relative bound was chosen for: two shares a factor of
    /// two apart must never be confusable. Same total, so the gross-total band
    /// cannot be what catches it.
    #[test]
    fn a_two_x_share_divergence_is_caught() {
        let vm = m(&[("cg_0", 4_000_000_000), ("cg_1", 8_000_000_000)]);
        let sim = m(&[("cg_0", 8_000_000_000), ("cg_1", 4_000_000_000)]);
        let c = compare(&vm, &sim);
        assert!(!c.agrees(), "{}", c.report());
    }

    /// Just inside the bound, at a share where the relative test governs. Pins
    /// the bound from below so a future widening is a visible edit to a test,
    /// not a silent constant change.
    ///
    /// Deliberately 0.04 rather than exactly 0.05: a value on the boundary is
    /// decided by floating-point representation, not by the bound (0.55 - 0.50
    /// is 0.050000000000000044, which is genuinely over). Pinning a bound with
    /// a case whose verdict depends on binary64 rounding would be pinning the
    /// rounding.
    #[test]
    fn just_inside_the_relative_bound_agrees() {
        // vm 0.5/0.5; sim 0.54/0.46 -> ratio 1.08, delta 0.04.
        let vm = m(&[("cg_0", 500), ("cg_1", 500)]);
        let sim = m(&[("cg_0", 540), ("cg_1", 460)]);
        let c = compare(&vm, &sim);
        assert!(c.agrees(), "{}", c.report());
    }

    /// And just outside it.
    #[test]
    fn just_outside_the_relative_bound_is_caught() {
        // vm 0.5/0.5; sim 0.6/0.4 -> ratio 1.20 (inside) but delta 0.10 (outside).
        let vm = m(&[("cg_0", 500), ("cg_1", 500)]);
        let sim = m(&[("cg_0", 600), ("cg_1", 400)]);
        let c = compare(&vm, &sim);
        assert!(!c.agrees(), "{}", c.report());
    }

    /// Below the relative floor the absolute test governs, so a large ratio on
    /// a tiny share is not a failure.
    #[test]
    fn a_large_ratio_on_a_tiny_share_is_governed_by_the_absolute_test() {
        // cg_1 is 1% on one side and 3% on the other: ratio 3.0, delta 0.02.
        let vm = m(&[("cg_0", 9_900), ("cg_1", 100)]);
        let sim = m(&[("cg_0", 9_700), ("cg_1", 300)]);
        let c = compare(&vm, &sim);
        assert!(c.agrees(), "{}", c.report());
    }

    /// A cgroup one backend never reported is a failure, not a skipped row.
    #[test]
    fn a_missing_cgroup_is_a_mismatch() {
        let vm = m(&[("cg_0", 6_000_000_000), ("cg_1", 6_000_000_000)]);
        let sim = m(&[("cg_0", 6_000_000_000)]);
        let c = compare(&vm, &sim);
        assert!(!c.agrees());
        assert!(c
            .mismatches
            .iter()
            .any(|m| matches!(m, Mismatch::CgroupSet { .. })));
    }

    /// An empty side must fail rather than pass vacuously. This is the shape a
    /// broken extraction takes, and a comparison check that goes green when it
    /// has nothing to compare is worse than no check.
    #[test]
    fn an_empty_side_fails_rather_than_passing_vacuously() {
        let vm = m(&[("cg_0", 6_000_000_000)]);
        let empty = BTreeMap::new();
        let c = compare(&vm, &empty);
        assert!(!c.agrees());
        assert!(c.mismatches.iter().any(|m| matches!(
            m,
            Mismatch::NoCpuTime {
                sim_empty: true,
                ..
            }
        )));
    }

    /// Both sides all-zero is still a failure, for the same reason.
    #[test]
    fn two_empty_sides_still_fail() {
        let z = m(&[("cg_0", 0)]);
        let c = compare(&z, &z);
        assert!(!c.agrees(), "{}", c.report());
    }
}
