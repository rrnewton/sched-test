//! Explicit capability gating for tests that need real hardware or tools.
//!
//! # Why this module exists
//!
//! Some tests can only assert anything on a machine that actually has the
//! thing under test: a working PMU, usable debug registers, Perfetto's
//! `trace_processor_shell`. The historical way to handle that was:
//!
//! ```ignore
//! let counter = match RbcCounter::new(&config) {
//!     Ok(c) => c,
//!     Err(e) => {
//!         eprintln!("skipping test: {e}");
//!         return;            // <-- reports PASS, asserts nothing
//!     }
//! };
//! ```
//!
//! That is a green test that cannot fail. It inflates the pass count and
//! hides regressions, and it is invisible in the runner's "skipped" number —
//! strictly worse than an honest skip. `scx-sim/CLAUDE.md` ("No Silent
//! Failures") forbids exactly this shape: "A skipped check is a lie."
//!
//! # The policy
//!
//! A missing capability is now **loud by default**: [`absent`] panics with a
//! message naming the capability and how to opt out. An environment that
//! genuinely lacks the capability must say so explicitly, by listing it in
//! [`ALLOW_MISSING_ENV`]. `validate.sh` probes the machine and sets that
//! variable automatically, so CI on a capability-less VM stays green while a
//! developer on real hardware still gets a hard failure if the capability
//! silently disappears.
//!
//! Note the deliberate split, which is the whole point of the design:
//!
//! - **Capability absent** (no PMU, `perf_event_open` denied, tool not
//!   installed) is a legitimate reason not to test. It routes through
//!   [`absent`] and must be declared.
//! - **Capability present but degenerate** (the kernel handed us a counter
//!   and it reads 0) is *not* a capability gap — it is a real defect, and the
//!   test must assert and fail. Do not route that through this module.

/// Environment variable listing capabilities the current machine lacks.
///
/// Comma- or space-separated capability names, or `all` to permit any.
/// Example: `SCXSIM_ALLOW_MISSING_CAPS=pmu,hw_breakpoint`.
pub const ALLOW_MISSING_ENV: &str = "SCXSIM_ALLOW_MISSING_CAPS";

/// Prefix printed when a test is skipped because a capability was declared
/// missing. Greppable so a run's skips can be audited after the fact.
pub const SKIP_MARKER: &str = "SCXSIM-CAPABILITY-SKIP";

/// PMU counters (`perf_event_open` with a hardware branch event).
pub const PMU: &str = "pmu";
/// Hardware execution breakpoints (CPU debug registers).
pub const HW_BREAKPOINT: &str = "hw_breakpoint";
/// Perfetto's `trace_processor_shell` binary.
pub const TRACE_PROCESSOR: &str = "trace_processor";

/// Every capability this module knows how to gate on.
pub const ALL: &[&str] = &[PMU, HW_BREAKPOINT, TRACE_PROCESSOR];

/// Whether the environment has explicitly declared `cap` as missing.
pub fn allowed_missing(cap: &str) -> bool {
    let Ok(raw) = std::env::var(ALLOW_MISSING_ENV) else {
        return false;
    };
    raw.split([',', ' '])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .any(|entry| entry.eq_ignore_ascii_case("all") || entry.eq_ignore_ascii_case(cap))
}

/// Handle a genuinely-absent capability.
///
/// Panics unless the environment declared `cap` missing via
/// [`ALLOW_MISSING_ENV`]. When it did, prints a greppable [`SKIP_MARKER`]
/// line and returns, so the caller can `return capability::absent(..)`.
///
/// `detail` should explain how the absence was detected (the underlying
/// error, or which probe failed).
pub fn absent(cap: &str, detail: &str) {
    if allowed_missing(cap) {
        eprintln!(
            "{SKIP_MARKER}: {cap} unavailable ({detail}); declared missing via {ALLOW_MISSING_ENV}"
        );
        return;
    }
    panic!(
        "required capability `{cap}` is unavailable: {detail}\n\
         \n\
         This test asserts real {cap} behaviour; passing it without that \
         capability would be a green test that asserts nothing.\n\
         If this machine genuinely lacks `{cap}`, declare it explicitly:\n\
         \n    {ALLOW_MISSING_ENV}={cap} cargo nextest run ...\n\
         \n\
         `validate.sh` probes for this and sets the variable automatically."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests mutate a process-global env var, so they must not run
    // concurrently with each other.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Run `f` with `ALLOW_MISSING_ENV` set to `value` (or unset for `None`),
    /// restoring the previous value afterwards.
    fn with_env<R>(value: Option<&str>, f: impl FnOnce() -> R) -> R {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var(ALLOW_MISSING_ENV).ok();
        match value {
            Some(v) => std::env::set_var(ALLOW_MISSING_ENV, v),
            None => std::env::remove_var(ALLOW_MISSING_ENV),
        }
        let out = f();
        match prev {
            Some(p) => std::env::set_var(ALLOW_MISSING_ENV, p),
            None => std::env::remove_var(ALLOW_MISSING_ENV),
        }
        out
    }

    #[test]
    fn unset_env_allows_nothing() {
        with_env(None, || {
            assert!(!allowed_missing(PMU));
            assert!(!allowed_missing(HW_BREAKPOINT));
        });
    }

    #[test]
    fn exact_name_matches_only_itself() {
        with_env(Some("pmu"), || {
            assert!(allowed_missing(PMU));
            assert!(!allowed_missing(HW_BREAKPOINT));
        });
    }

    #[test]
    fn comma_and_space_separated_lists_both_parse() {
        with_env(Some("pmu,hw_breakpoint"), || {
            assert!(allowed_missing(PMU));
            assert!(allowed_missing(HW_BREAKPOINT));
            assert!(!allowed_missing(TRACE_PROCESSOR));
        });
        with_env(Some("pmu hw_breakpoint"), || {
            assert!(allowed_missing(PMU));
            assert!(allowed_missing(HW_BREAKPOINT));
        });
    }

    #[test]
    fn all_permits_every_capability() {
        with_env(Some("all"), || {
            for cap in ALL {
                assert!(allowed_missing(cap), "`all` should permit {cap}");
            }
        });
    }

    #[test]
    fn matching_is_case_insensitive_and_trims() {
        with_env(Some("  PMU , Hw_Breakpoint "), || {
            assert!(allowed_missing(PMU));
            assert!(allowed_missing(HW_BREAKPOINT));
        });
    }

    #[test]
    fn empty_entries_do_not_permit_everything() {
        with_env(Some(",, ,"), || {
            assert!(!allowed_missing(PMU));
        });
    }

    #[test]
    fn absent_returns_quietly_when_declared() {
        with_env(Some("pmu"), || absent(PMU, "probe says no"));
    }

    #[test]
    #[should_panic(expected = "required capability `pmu` is unavailable")]
    fn absent_panics_when_not_declared() {
        with_env(None, || absent(PMU, "probe says no"));
    }

    #[test]
    #[should_panic(expected = "required capability `hw_breakpoint` is unavailable")]
    fn absent_panics_for_capability_not_in_the_list() {
        with_env(Some("pmu"), || absent(HW_BREAKPOINT, "probe says no"));
    }
}
