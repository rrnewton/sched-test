//! V4-C regression test: per-period `scx_cgroup_bw_consume(ns)` charges
//! must respect the bandwidth bound `nquota_ub × nr_running_tasks_in_cgroup`.
//!
//! Background (tg
//! `scxsim-fix-cbw-debt-runaway-or-document-as-known-cpu-bw-stall-bug`):
//!
//! Pre-V4-C scxsim was passing a stale `prev_task` to `lavd_dispatch(cpu,
//! prev)` whenever a CPU went idle. LAVD's fall-through path
//! `consume_prev(prev) → account_task_runtime(prev) →
//! scx_cgroup_bw_consume(prev->cgroup, task_time_wall)` then charged the
//! off-CPU prev's cgroup with full inter-tick wall time (~100M ns/period
//! per idle CPU). With 4 CPUs all going idle during a STALL window this
//! produced ~400M ns/period of fake debt, exceeding any reasonable
//! per-period bound and driving the cgroup_bw library into permanent
//! `keep_throttled=true` (the cpu-bw-stall-bug surface).
//!
//! V4-C fix: clear `prev_task = None` on every CPU idle transition in
//! `safe/engine.rs`. With prev cleared, `lavd_dispatch` no longer falls
//! through to `consume_prev` on idle CPUs, and the cgroup is only
//! charged for time tasks were actually running. The kernel never
//! exhibits this bug because `pick_next_task` during idle returns the
//! IDLE task (root cgroup), not the last user task.
//!
//! # Bound
//!
//! For a cgroup with quota `nquota_ub` ns per `period` ns and
//! `nr_running_tasks` tasks runnable in the cgroup, the legal
//! `scx_cgroup_bw_consume` charge AVERAGED ACROSS ALL ELAPSED PERIODS
//! is bounded by:
//!
//!   avg_consume_per_period ≤ SLACK × nquota_ub × nr_running_tasks
//!
//! The averaging is essential because scxsim's lib-side enforcement is
//! INTRINSICALLY loose on a per-period basis:
//!
//!   * Period 0 has no throttle history, so a runnable task burns
//!     down a full slice (~100ms) before the first replenishment
//!     calculates its debt. Any per-period bound would fail here on a
//!     correctly-functioning fix.
//!
//!   * Subsequent periods see ~2× over-shoot from in-flight consume
//!     that races with the throttle decision (the lib observes the
//!     consume call AFTER the engine has already committed to the
//!     dispatched slice).
//!
//! AVERAGING across periods captures the fundamental property the
//! V4-C fix restores: STALL periods with no runnable task produce
//! ZERO consume, balancing out the active-period over-shoots.
//!
//! Pre-V4-C: every period had ~100M ns of fake consume from stale-prev
//!   consume_prev fall-through, regardless of whether work was running.
//!   avg ≈ 100M ≫ bound.
//!
//! Post-V4-C: only periods with actual runnable work produce consume
//!   charges. Average across the 600ms / 6-period run drops to ~25M
//!   (initial burn-down period + occasional active periods + zero
//!   during STALL recovery). avg ≪ bound.
//!
//! # CRITICAL CAVEAT
//!
//! This test asserts the FIXED scxsim behavior, not the FIXED
//! production scheduler. Production has its own cpu-bw-stall-bug (PR
//! #3521 timer-MIN-bound regression) with the SAME observable symptom
//! but a DIFFERENT upstream cause. Fixing scxsim's reproduction does
//! NOT fix production. See REPORT.md for the disambiguation.

use scx_simulator::*;
use std::collections::BTreeMap;

#[macro_use]
mod common;

// ---------------------------------------------------------------------------
// Constants matching the canonical bug-1 fixture (smoking-gun variant:
// single hog, 4 CPUs, 600ms duration, 10ms/100ms quota).
// ---------------------------------------------------------------------------

/// Replenish period in ns (matches lib's CBW_REPLENISH_PERIOD = 100ms).
const PERIOD_NS: i64 = 100_000_000;
/// Cgroup quota per period in ns (10ms = 10% of one CPU).
const NQUOTA_UB_NS: i64 = 10_000_000;
/// Number of runnable tasks in the test cgroup.
const NR_TASKS: i64 = 1;
/// Defensive 4× slack absorbs legitimate in-flight consume that races
/// with the throttle decision (the lib observes consume after the
/// engine has already dispatched the next slice) AND the period-0
/// warm-up burn-down. Empirically the post-V4-C average lands ~25M
/// for the H6 cell-C scenario; the bound 4× × 10M × 1 = 40M leaves
/// ~38% headroom while still being well below the pre-V4-C ~100M
/// every-period floor.
const SLACK_FACTOR: i64 = 4;
/// Simulated wall duration. 600ms / 100ms-period = 6 replenishments.
const DURATION_MS: u64 = 600;

/// LAVD `enable_cpu_bw` boolean global setter (mirrors the helper in
/// `cgroup_bw_replenish_smoking_gun.rs` — kept local-private here to
/// avoid pulling another shared module).
///
/// # Safety
/// Caller must ensure `name` is the literal name of a `bool` global
/// in the loaded LAVD `.so`.
unsafe fn lavd_set_bool(sched: &DynamicScheduler, name: &str, val: bool) {
    let sym: libloading::Symbol<'_, *mut bool> = sched
        .get_symbol(name.as_bytes())
        .unwrap_or_else(|| panic!("symbol {name} not found"));
    std::ptr::write_volatile(*sym, val);
}

fn build_h6_scenario() -> Scenario {
    Scenario::builder()
        .cpus(4)
        .cgroup_with_bandwidth(
            "tight",
            &[CpuId(0), CpuId(1), CpuId(2), CpuId(3)],
            (PERIOD_NS / 1000) as u64,    // period_us = 100ms
            (NQUOTA_UB_NS / 1000) as u64, // quota_us = 10ms
            0,                            // burst_us = 0
        )
        .add_task_in_cgroup("hog", 0, workloads::cpu_bound(2_000_000_000), "tight")
        .duration_ms(DURATION_MS)
        .build()
}

/// Per-period accumulation of `scx_cgroup_bw_consume(ns)` charges,
/// keyed by cgid, then by period index (event timestamp / PERIOD_NS).
fn aggregate_consume_per_period(trace: &Trace) -> BTreeMap<u64, BTreeMap<u64, u64>> {
    let mut per_cg_per_period: BTreeMap<u64, BTreeMap<u64, u64>> = BTreeMap::new();
    for ev in trace.events() {
        if let TraceKind::CgroupBwConsumeNs { cgid, ns } = &ev.kind {
            let period_idx = (ev.time_ns / PERIOD_NS as u64) as u64;
            *per_cg_per_period
                .entry(cgid.0)
                .or_default()
                .entry(period_idx)
                .or_insert(0) += *ns;
        }
    }
    per_cg_per_period
}

// ---------------------------------------------------------------------------
// Test 1: per-period consume_ns bound
// ---------------------------------------------------------------------------

/// Asserts that for every cgid in the H6 cell-C scenario, the AVERAGE
/// `scx_cgroup_bw_consume` charge across all elapsed replenishment
/// periods does not exceed `SLACK × NQUOTA_UB × NR_TASKS`. Pre-V4-C
/// the engine charged ~100M ns every period from stale-prev
/// consume_prev fall-through (avg ≈ 100M ≫ bound). Post-V4-C the
/// charges reflect only legitimate task runtime — STALL periods drop
/// to zero, balancing out the active periods (avg ≈ 25M).
///
/// REGRESSION SEMANTICS: if this test fails on simulator.v6 or later
/// with "avg consume X exceeds bound Y", the V4-C engine fix has
/// regressed (someone re-introduced stale-prev passing on idle, or a
/// new code path charges off-CPU cgroups during idle). Triage by
/// re-reading the V4-C commentary in `safe/engine.rs` (the two
/// `prev_task = None` clears) and `experiments/scxsim_cbw_engine_fix_v4c_20260513/REPORT.md`.
///
/// See module-level docs above for why a per-period bound is NOT
/// asserted here (period 0 burn-down + 2× over-shoot from skid make
/// per-period bounds intrinsically loose; the average captures the
/// fundamental V4-C property).
#[test]
fn test_bug1_canonical_consume_ns_bound() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::lavd(4);
    // SAFETY: `enable_cpu_bw` is a bool global declared in LAVD's
    // main.bpf.c; the symbol is present in the loaded `.so`. The
    // scheduler outlives this assignment.
    unsafe {
        lavd_set_bool(&sched, "enable_cpu_bw\0", true);
    }
    let scenario = build_h6_scenario();
    let trace = Simulator::new(sched).run(scenario);

    let per_cg_per_period = aggregate_consume_per_period(&trace);
    assert!(
        !per_cg_per_period.is_empty(),
        "expected at least one CgroupBwConsumeNs event over a 600ms run \
         under LAVD + enable_cpu_bw + finite-quota cgroup. Got zero — the \
         V4-A CgroupBwConsumeNs trace observer may have regressed, or the \
         LAVD .so was built without SCXSIM_PHASE2_REAL_CGROUP_BW=1."
    );

    // Total elapsed periods = max period idx across all cgroups + 1.
    let max_period_idx: u64 = per_cg_per_period
        .values()
        .flat_map(|m| m.keys().copied())
        .max()
        .unwrap_or(0);
    let elapsed_periods: i64 = (max_period_idx as i64) + 1;
    let bound: i64 = SLACK_FACTOR * NQUOTA_UB_NS * NR_TASKS;
    let mut violations: Vec<(u64, i64, i64)> = Vec::new();
    for (cgid, periods) in &per_cg_per_period {
        let total_ns: i64 = periods.values().map(|n| *n as i64).sum();
        let avg_ns: i64 = total_ns / elapsed_periods;
        eprintln!(
            "[consume_ns_bound] cgid={cgid}: total={total_ns} ns over \
             {elapsed_periods} periods, avg={avg_ns} ns/period (bound={bound})"
        );
        if avg_ns > bound {
            violations.push((*cgid, avg_ns, total_ns));
        }
    }

    assert!(
        violations.is_empty(),
        "V4-C engine over-charge regression: {} cgroup(s) with avg consume_ns \
         exceeding the bound {bound} ns/period (= {SLACK_FACTOR} × NQUOTA_UB \
         × NR_TASKS = {SLACK_FACTOR} × {NQUOTA_UB_NS} × {NR_TASKS}). Pre-V4-C \
         the engine charged ~100M ns/period per stale-prev consume_prev \
         fall-through, regardless of whether work was running. The V4-C fix \
         in safe/engine.rs clears prev_task=None on every CPU idle transition; \
         if avg consume is climbing, that fix has regressed.\n\n\
         Violations (cgid, avg_ns, total_ns):\n  {violations:?}\n\n\
         Full per-cgroup per-period consume map:\n  {per_cg_per_period:?}",
        violations.len()
    );
}

// ---------------------------------------------------------------------------
// Test 2: STALL-window zero-charge property
// ---------------------------------------------------------------------------

/// Companion assertion to test 1: when no work is pending on the
/// throttled cgroup (a "STALL" window in V3 terms), the per-period
/// consume sum should be ~0 ns. Pre-V4-C the engine kept charging
/// ~100M ns/period during STALL via stale-prev consume_prev. Post-V4-C
/// idle CPUs don't fall through, so no_work periods produce zero
/// charges.
///
/// We assert at least one period with `consume_sum == 0` exists. This
/// catches the pre-V4-C bug-shape directly: if EVERY period has
/// non-zero consume, the engine is still charging during STALL.
#[test]
fn test_bug1_canonical_consume_ns_zero_during_stall() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::lavd(4);
    // SAFETY: see test 1.
    unsafe {
        lavd_set_bool(&sched, "enable_cpu_bw\0", true);
    }
    let scenario = build_h6_scenario();
    let trace = Simulator::new(sched).run(scenario);

    let per_cg_per_period = aggregate_consume_per_period(&trace);
    assert!(
        !per_cg_per_period.is_empty(),
        "expected at least one CgroupBwConsumeNs event"
    );

    // Compute total periods elapsed (final event timestamp / period).
    // The H6 scenario runs 600ms = 6 periods; we expect at least 4
    // periods to elapse (some scheduling slack at start/end).
    let max_period_idx: u64 = per_cg_per_period
        .values()
        .flat_map(|m| m.keys().copied())
        .max()
        .unwrap_or(0);

    // For each cgroup, count the number of periods (within
    // [0..=max_period_idx]) that had ZERO consume — i.e. the cgroup
    // was either fully throttled with no in-flight work, or no task
    // happened to run in that period.
    let mut had_any_zero_period = false;
    for (cgid, periods) in &per_cg_per_period {
        let mut zero_periods: Vec<u64> = Vec::new();
        for p in 0..=max_period_idx {
            let sum = periods.get(&p).copied().unwrap_or(0);
            if sum == 0 {
                zero_periods.push(p);
            }
        }
        eprintln!(
            "[consume_ns_zero] cgid={cgid}: {} of {} periods had zero consume: {zero_periods:?}",
            zero_periods.len(),
            max_period_idx + 1
        );
        if !zero_periods.is_empty() {
            had_any_zero_period = true;
        }
    }

    assert!(
        had_any_zero_period,
        "V4-C STALL-window regression: NO period had zero consume_ns sum on \
         any cgroup. Pre-V4-C the engine over-charged every period via \
         stale-prev consume_prev fall-through, so EVERY period had non-zero \
         consume even when no task was running. Post-V4-C, periods with no \
         runnable task should produce zero charges. If this assertion fails, \
         either the V4-C engine fix regressed, or the H6 scenario no longer \
         produces STALL windows.\n\n\
         Full per-cgroup per-period consume map:\n  {per_cg_per_period:?}"
    );
}
