//! Which model advanced simulated time during a run.
//!
//! # Why this exists
//!
//! The simulated clock is advanced by one of three different models, and the
//! one used is chosen at RUNTIME from what the HOST provides (see
//! [`crate::engine`]'s `charge_sched_time` and the `rbc_counter` selection).
//! A bare-metal box with a usable PMU takes [`ClockMode::Pmu`]; a VM without
//! one silently takes [`ClockMode::Fallback`]. Those are different clocks, so
//! they can produce different simulated timelines from identical declared
//! inputs.
//!
//! That is not hypothetical. The `bug1_canonical` fixture, at one commit and
//! one set of flags, yields `is_throttled 0` under [`ClockMode::Pmu`] and
//! `is_throttled 1` under [`ClockMode::Fallback`] — both reproducibly, on the
//! same machine, differing only in this. It read as a dev-box-versus-CI
//! discrepancy for a day because nothing in the output said which clock had
//! run.
//!
//! So a scxsim result is only interpretable alongside its clock mode, and two
//! results are only comparable if their modes match. This type exists to make
//! that fact travel WITH the result rather than being reconstructed from the
//! machine it happened to run on.
//!
//! # Where it must appear
//!
//! Anywhere a result is compared FROM, not merely the console. A number in a
//! fixture, a baseline or a sidecar outlives the terminal it was printed in,
//! and a number without its environment is what made the PMU-only coverage
//! baseline meaningless on a PMU-less runner.

use serde::{Deserialize, Serialize};
use std::fmt;

/// The model that advanced simulated time for a run.
///
/// Serialised in lowercase (`"pmu"`, `"e9patch"`, `"fallback"`, `"off"`) so it
/// reads the same in JSON, CSV and a log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClockMode {
    /// e9patch software branch counter. Deterministic and host-independent:
    /// the instrumented `.so` decrements a counter at every conditional
    /// branch, so the count depends on the code, not the CPU.
    E9patch,
    /// Hardware PMU retired-conditional-branch counter.
    ///
    /// **Not reproducible across machines.** Simulated time becomes a function
    /// of a real measurement taken on this host's microarchitecture, so a run
    /// here and a run on a different CPU are not comparable even when both
    /// have a working PMU.
    Pmu,
    /// Accumulated kfunc cost with a minimum per-callback floor. Deterministic
    /// and host-independent, but a coarser model than either counter.
    Fallback,
    /// Scheduler overhead accounting disabled entirely; callbacks are free and
    /// simulated time is not advanced by scheduler execution.
    Off,
}

impl ClockMode {
    /// The stable short name used in output, JSON and CSV.
    pub const fn as_str(self) -> &'static str {
        match self {
            ClockMode::E9patch => "e9patch",
            ClockMode::Pmu => "pmu",
            ClockMode::Fallback => "fallback",
            ClockMode::Off => "off",
        }
    }

    /// Whether results from this mode can be compared against results produced
    /// on a different machine.
    ///
    /// Only [`ClockMode::Pmu`] is host-dependent: its clock is driven by a
    /// hardware measurement of this CPU. The others are computed from the code
    /// under simulation and reproduce anywhere.
    pub const fn is_host_dependent(self) -> bool {
        matches!(self, ClockMode::Pmu)
    }

    /// One line naming the mode and what it means for comparability, for the
    /// run header.
    pub const fn describe(self) -> &'static str {
        match self {
            ClockMode::E9patch => {
                "e9patch software branch counter — deterministic, host-independent"
            }
            ClockMode::Pmu => {
                "hardware PMU branch counter — HOST-DEPENDENT, not comparable across machines"
            }
            ClockMode::Fallback => "kfunc-cost model — deterministic, host-independent",
            ClockMode::Off => "scheduler overhead accounting disabled",
        }
    }
}

impl fmt::Display for ClockMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the caller asked for, and what the host could actually supply.
///
/// Split out as a pure function so every combination can be tested without a
/// machine that has (or lacks) a PMU. The engine cannot be asked to run
/// without one on a box where `perf_event_open` succeeds, so the decision has
/// to be testable away from the hardware.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockDecision {
    /// The model that will advance simulated time.
    pub mode: ClockMode,
    /// Set when the PMU was asked for and could not be supplied.
    pub downgrade: Option<Downgrade>,
}

/// A run asked for the PMU clock and got a different one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Downgrade {
    /// ns-per-branch that was requested.
    pub requested_rbc_ns: u64,
    /// The clock actually used instead.
    pub actual: ClockMode,
    /// Whether the request was explicit (`--rbc-ns` / `SCX_SIM_RBC_NS`) rather
    /// than the `Some(10)` default. Explicit requests are a hard error;
    /// the default is a warning, because otherwise every PMU-less host —
    /// including all of CI — would fail.
    pub explicit: bool,
}

/// Decide which clock a run will use, and whether that is a downgrade.
///
/// Mirrors the three arms of the engine's `charge_sched_time`. Keep them in
/// step: a mode here that the engine does not implement would mislabel runs.
pub fn decide(
    is_e9: bool,
    rbc_ns: Option<u64>,
    pmu_counter_available: bool,
    overhead_enabled: bool,
    explicit: bool,
) -> ClockDecision {
    if is_e9 {
        // e9patch supersedes the PMU deliberately, so it is not a downgrade
        // even when the PMU path was requested.
        return ClockDecision {
            mode: ClockMode::E9patch,
            downgrade: None,
        };
    }
    let wanted_pmu = rbc_ns.is_some_and(|ns| ns > 0);
    if wanted_pmu && pmu_counter_available {
        return ClockDecision {
            mode: ClockMode::Pmu,
            downgrade: None,
        };
    }
    let mode = if overhead_enabled {
        ClockMode::Fallback
    } else {
        ClockMode::Off
    };
    let downgrade = if wanted_pmu {
        Some(Downgrade {
            requested_rbc_ns: rbc_ns.unwrap_or(0),
            actual: mode,
            explicit,
        })
    } else {
        None
    };
    ClockDecision { mode, downgrade }
}

impl Downgrade {
    /// The operator-facing explanation. Deliberately says the clock is
    /// DIFFERENT rather than merely less precise: the two models produce
    /// different simulated timelines, not the same one at different fidelity.
    pub fn message(&self) -> String {
        format!(
            "scxsim: PMU scheduler-overhead accounting was requested \
             (sched_overhead_rbc_ns = {} ns/branch) but no PMU counter could be \
             created on this host, so simulated time was advanced by the '{}' \
             model instead.\n\
             This is a DIFFERENT CLOCK, not a slower one: results from this run \
             are not comparable with results produced under '{}'.",
            self.requested_rbc_ns,
            self.actual,
            ClockMode::Pmu,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pmu_when_asked_and_available() {
        let d = decide(false, Some(10), true, true, false);
        assert_eq!(d.mode, ClockMode::Pmu);
        assert_eq!(d.downgrade, None);
    }

    #[test]
    fn asked_for_pmu_but_unavailable_is_a_downgrade_not_a_silent_swap() {
        let d = decide(false, Some(10), false, true, false);
        assert_eq!(d.mode, ClockMode::Fallback);
        let dg = d.downgrade.expect("a swapped clock must be reported");
        assert_eq!(dg.requested_rbc_ns, 10);
        assert_eq!(dg.actual, ClockMode::Fallback);
        assert!(
            !dg.explicit,
            "the Some(10) default is not an explicit request"
        );
        assert!(
            dg.message().contains("DIFFERENT CLOCK"),
            "the message must not read as a fidelity tweak: {}",
            dg.message()
        );
    }

    #[test]
    fn an_explicit_request_is_marked_explicit_so_it_can_hard_fail() {
        let d = decide(false, Some(25), false, true, true);
        assert!(d.downgrade.expect("still a downgrade").explicit);
    }

    #[test]
    fn not_asking_for_the_pmu_is_never_a_downgrade() {
        // --no-rbc (Some(0)) and "disabled" (None) both decline the PMU, so
        // neither can be downgraded from it.
        for rbc in [None, Some(0)] {
            let d = decide(false, rbc, false, true, false);
            assert_eq!(d.mode, ClockMode::Fallback);
            assert_eq!(d.downgrade, None, "rbc_ns={rbc:?} did not ask for the PMU");
        }
    }

    #[test]
    fn e9patch_wins_and_is_not_a_downgrade() {
        // e9patch deliberately supersedes the PMU; reporting that as a
        // downgrade would cry wolf on every instrumented run.
        let d = decide(true, Some(10), true, true, true);
        assert_eq!(d.mode, ClockMode::E9patch);
        assert_eq!(d.downgrade, None);
    }

    #[test]
    fn overhead_disabled_yields_off_not_fallback() {
        let d = decide(false, None, false, false, false);
        assert_eq!(d.mode, ClockMode::Off);
    }

    #[test]
    fn downgrade_to_off_is_still_reported() {
        let d = decide(false, Some(10), false, false, false);
        assert_eq!(d.mode, ClockMode::Off);
        assert_eq!(d.downgrade.expect("reported").actual, ClockMode::Off);
    }

    #[test]
    fn only_pmu_is_host_dependent() {
        assert!(ClockMode::Pmu.is_host_dependent());
        for m in [ClockMode::E9patch, ClockMode::Fallback, ClockMode::Off] {
            assert!(
                !m.is_host_dependent(),
                "{m} is computed from the code under simulation, not the host, \
                 so it must not be flagged host-dependent"
            );
        }
    }

    #[test]
    fn names_are_stable_and_lowercase() {
        // These strings land in committed artifacts; changing one silently
        // breaks comparison against every previously recorded result.
        assert_eq!(ClockMode::E9patch.as_str(), "e9patch");
        assert_eq!(ClockMode::Pmu.as_str(), "pmu");
        assert_eq!(ClockMode::Fallback.as_str(), "fallback");
        assert_eq!(ClockMode::Off.as_str(), "off");
    }

    #[test]
    fn serde_roundtrips_as_the_same_lowercase_name() {
        for m in [
            ClockMode::E9patch,
            ClockMode::Pmu,
            ClockMode::Fallback,
            ClockMode::Off,
        ] {
            let json = serde_json::to_string(&m).unwrap();
            assert_eq!(json, format!("\"{}\"", m.as_str()));
            let back: ClockMode = serde_json::from_str(&json).unwrap();
            assert_eq!(back, m);
        }
    }

    #[test]
    fn describe_warns_for_the_host_dependent_mode_only() {
        assert!(ClockMode::Pmu.describe().contains("HOST-DEPENDENT"));
        for m in [ClockMode::E9patch, ClockMode::Fallback, ClockMode::Off] {
            assert!(!m.describe().contains("HOST-DEPENDENT"));
        }
    }
}
