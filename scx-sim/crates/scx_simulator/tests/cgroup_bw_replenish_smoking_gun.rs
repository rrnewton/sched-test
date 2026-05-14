//! Smoking-gun observer for the cpu-bw-stall-bug: per-cgroup
//! `TraceKind::CgroupBwReplenish` events emitted by the engine after
//! every `fire_timer` invocation when LAVD compiles in the cgroup_bw
//! library.
//!
//! tg `wprof-r2-add-cgroup-bw-replenish-tracekind-smoking-gun` (R2 HIGH
//! from wprof-trace-baseline 2026-05-13).
//!
//! # What this test asserts
//!
//! 1. **Events fire**: under LAVD with `enable_cpu_bw=true` and a finite-
//!    quota cgroup running long enough to cross multiple replenishment
//!    periods, the trace contains at least one `CgroupBwReplenish`
//!    event. (Without this scaffold the bug's CAUSE -- `period_budget`
//!    debt accounting at lib/cgroup_bw.bpf.c:1679 -- has NO scxsim trace
//!    representation. Per wprof-trace-baseline, no kernel tracepoint
//!    either.)
//!
//! 2. **V4-C post-fix healthy oscillation**: the H6 cell-C workload
//!    (kernel cpu.max=10000/100000 + LAVD enable_cpu_bw + a long-
//!    running CPU-bound task) USED TO drive the lib into permanent
//!    `keep_throttled=true` (the cpu-bw-stall-bug surface, asserted
//!    by this test pre-V4-C). After the V4-C engine fix
//!    (agent/scxsim-cbw-engine-fix; clears stale `prev_task` on CPU
//!    idle to stop scxsim from charging off-CPU cgroups during idle),
//!    the cgroup throttles when it actually crosses quota and
//!    recovers within a period or two when no work is pending. The
//!    assertion now checks healthy oscillation: throttle fires AND
//!    recovers, max consecutive throttled periods <= 1. See REPORT.md
//!    for the scxsim-vs-production disambiguation: scxsim's bug and
//!    production's bug have the SAME observable symptom but
//!    DIFFERENT upstream causes. Fixing scxsim does NOT fix
//!    production (PR #3521 timer-MIN-bound regression).
//!
//! 3. **Computed fields are sane**: debt and burst_credit must be
//!    non-negative; period_budget_out matches keep_throttled.

use scx_simulator::*;

#[macro_use]
mod common;

/// LAVD `enable_cpu_bw` boolean global (mirrors `h6_matrix.rs`'s
/// helper). The smoking-gun observer only fires when the lib is
/// actually consulted -- this flag is the gate.
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

/// Single shared scenario builder for the two tests below: 4 CPUs, one
/// finite-quota cgroup ("tight" = 10% of one CPU), one CPU-bound task
/// in that cgroup, 600ms simulated duration so the lib's 100ms-period
/// replenish_timer fires ~6 times.
///
/// Mirrors the constants from `tests/fixtures/h6/bug1_canonical.json`
/// (the canonical reproducer fixture) but built in-process so we get
/// the live `Trace` object back, not a subprocess stderr stream.
fn build_h6_scenario() -> Scenario {
    Scenario::builder()
        .cpus(4)
        .cgroup_with_bandwidth(
            "tight",
            &[CpuId(0), CpuId(1), CpuId(2), CpuId(3)],
            100_000, // period_us = 100ms (matches lib's CBW_REPLENISH_PERIOD)
            10_000,  // quota_us  = 10ms (10% of one CPU)
            0,       // burst_us  = 0
        )
        .add_task_in_cgroup("hog", 0, workloads::cpu_bound(2_000_000_000), "tight")
        .duration_ms(600)
        .build()
}

// ---------------------------------------------------------------------------
// Test 1: events fire under LAVD + enable_cpu_bw on the H6 surface.
// ---------------------------------------------------------------------------

/// Under LAVD with `enable_cpu_bw=true` and a finite-quota cgroup that
/// crosses multiple 100ms replenish periods in 600ms of simulated time,
/// the trace MUST contain at least one `CgroupBwReplenish` event.
///
/// If this assertion fails, either:
///   * The LAVD `.so` was built without `SCXSIM_PHASE2_REAL_CGROUP_BW=1`
///     (the cgroup_bw library isn't compiled in), OR
///   * `scxsim_cbw_snapshot_all_cgroups` was not exported with default
///     visibility (dlsym lookup in `ffi.rs` returned `None`), OR
///   * The replenish_timerfn never fires (cgroup_bw lib init bailed out
///     before arming the timer).
#[test]
fn test_cgroup_bw_replenish_events_fire_under_lavd_with_cpu_bw() {
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

    let n_replenish = trace
        .events()
        .iter()
        .filter(|ev| matches!(ev.kind, TraceKind::CgroupBwReplenish { .. }))
        .count();

    assert!(
        n_replenish > 0,
        "expected at least one CgroupBwReplenish event under LAVD + \
         enable_cpu_bw + finite-quota cgroup over 600ms (>= 6 replenish \
         periods). Got {n_replenish}. Likely causes: (a) LAVD not built \
         with SCXSIM_PHASE2_REAL_CGROUP_BW=1, (b) wrapper.c's \
         scxsim_cbw_snapshot_all_cgroups not exported with default \
         visibility, (c) cgroup_bw lib's replenish_timerfn never fires \
         under this scenario."
    );

    // Sanity: every emitted event has internally consistent fields.
    // The lib's formulas at lines 1679-1706 guarantee debt >= 0 and
    // burst_credit >= 0; the diff in cgroup_bw_replenish::diff_snapshots
    // mirrors them. keep_throttled must equal (period_budget_out <= 0).
    for ev in trace.events() {
        if let TraceKind::CgroupBwReplenish {
            cgid,
            runtime_total_last,
            period_budget_in: _,
            debt,
            burst_credit,
            period_budget_out,
            keep_throttled,
        } = &ev.kind
        {
            assert!(
                *debt >= 0,
                "debt must be non-negative; got {debt} for cgid={}",
                cgid.0
            );
            assert!(
                *burst_credit >= 0,
                "burst_credit must be non-negative; got {burst_credit} for cgid={}",
                cgid.0
            );
            assert!(
                *runtime_total_last >= 0,
                "runtime_total_last must be non-negative; got {runtime_total_last} \
                 for cgid={}",
                cgid.0
            );
            assert_eq!(
                *keep_throttled,
                *period_budget_out <= 0,
                "keep_throttled must equal (period_budget_out <= 0); got \
                 keep_throttled={keep_throttled}, period_budget_out={period_budget_out} \
                 for cgid={}",
                cgid.0
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Test 2: smoking-gun signature is observable on the H6 cell-C workload.
// ---------------------------------------------------------------------------

/// HISTORICAL BUG FINGERPRINT (pre-V4-C, retained as documentation):
/// The cpu-bw-stall-bug's smoking-gun signature surfaced in
/// `CgroupBwReplenish` as a cgroup that got `keep_throttled=true`
/// (`period_budget_out <= 0`) and stayed that way across multiple
/// CONSECUTIVE replenishments. The H6 cell-C scenario produced a
/// STRONGER fingerprint pre-fix: rtl values at OR ABOVE the per-period
/// quota every period, with `period_budget_out` growing MORE negative
/// each period (engine over-charge → unbounded debt → permanent
/// throttle). The original assertion required at least one cgroup
/// with `>= 2 CONSECUTIVE keep_throttled=true` records.
///
/// V4-C FIX (this commit, agent/scxsim-cbw-engine-fix): the engine
/// over-charge was fixed by clearing `prev_task` on every CPU idle
/// transition in `engine.rs`. Previously scxsim was passing a stale
/// `prev_task` to `lavd_dispatch(cpu, prev)` on idle CPUs, causing
/// LAVD's fall-through `consume_prev(prev, ...) →
/// account_task_runtime(prev) → scx_cgroup_bw_consume(prev->cgroup,
/// task_time_wall)` chain to charge the off-CPU prev's cgroup with
/// full inter-tick wall time (~100M ns/period). With prev cleared on
/// idle, the scheduler sees the kernel's reality (no prev = no
/// consume_prev) and the lib sees only legitimate task runtime.
///
/// POST-FIX EXPECTATION (asserted below): healthy throttle/unthrottle
/// oscillation. The cgroup gets throttled when it actually exhausts
/// its quota (proves enforcement still works), but recovers within a
/// period or two when no work is pending (proves debt is bounded).
/// Concretely: `max_consecutive_keep_throttled <= 1`, AT LEAST ONE
/// `keep_throttled=true` period (proves enforcement still fires), AND
/// AT LEAST ONE period with `keep_throttled=false && rtl==0` (proves
/// the cgroup correctly exits throttle when no work is pending).
///
/// IMPORTANT CAVEAT: this test asserts the FIXED scxsim behavior, not
/// the FIXED scheduler. Production has its own cpu-bw-stall-bug
/// (PR #3521 timer-MIN-bound regression) with the SAME observable
/// symptom but a DIFFERENT upstream cause (kernel-side timer MIN-
/// clamping, not engine-side over-charge). Fixing scxsim's reproduction
/// does NOT fix production. See REPORT.md for the
/// scxsim-vs-production disambiguation.
///
/// REGRESSION SEMANTICS: if this test fails on simulator.v6 or later
/// with "max_consecutive_keep_throttled >= 2", the V4-C engine fix has
/// regressed (someone re-introduced stale-prev passing on idle, or a
/// new code path charges prev's cgroup during idle). If it fails with
/// "no keep_throttled=true at all", enforcement broke (the lib's
/// throttle decision is no longer being respected, or the H6 workload
/// no longer crosses quota). If it fails with "no kt=false && rtl=0
/// period", the cgroup never recovers (debt is still unbounded —
/// either the V4-C fix is incomplete or a new over-charge path was
/// added).
#[test]
fn test_cgroup_bw_replenish_smoking_gun_fires_on_h6_cell_c() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::lavd(4);
    // SAFETY: see test 1.
    unsafe {
        lavd_set_bool(&sched, "enable_cpu_bw\0", true);
    }
    let scenario = build_h6_scenario();
    let trace = Simulator::new(sched).run(scenario);

    // Group replenish events by cgid in time order.
    use std::collections::BTreeMap;
    let mut per_cg: BTreeMap<u64, Vec<(bool, i64, i64)>> = BTreeMap::new();
    for ev in trace.events() {
        if let TraceKind::CgroupBwReplenish {
            cgid,
            runtime_total_last,
            keep_throttled,
            period_budget_out,
            ..
        } = &ev.kind
        {
            per_cg.entry(cgid.0).or_default().push((
                *keep_throttled,
                *runtime_total_last,
                *period_budget_out,
            ));
        }
    }

    // V4-C post-fix assertions: healthy throttle/unthrottle oscillation.
    //
    // Find the longest consecutive `keep_throttled=true` run on any
    // cgroup that we observe (the cpu-bw-stall-bug pre-V4-C signature
    // would push this to "all true forever"; post-fix it must be <= 1).
    let max_consecutive_kt: usize = per_cg
        .values()
        .map(|recs| {
            let mut max_run: usize = 0;
            let mut cur: usize = 0;
            for (kt, _rtl, _pbo) in recs {
                if *kt {
                    cur += 1;
                    max_run = max_run.max(cur);
                } else {
                    cur = 0;
                }
            }
            max_run
        })
        .max()
        .unwrap_or(0);

    // Find any cgroup with at least one keep_throttled=true period
    // (proves enforcement still works -- the H6 workload still crosses
    // quota, the lib still detects it, and the engine still respects
    // the throttle).
    let any_throttle_fired: bool = per_cg
        .values()
        .any(|recs| recs.iter().any(|(kt, _rtl, _pbo)| *kt));

    // Find any cgroup with at least one period where we exited throttle
    // cleanly (kt=false AND rtl=0). This proves the cgroup recovers
    // when no work is pending, which is exactly the property the V4-C
    // engine fix restores: when the CPU is idle the prev's cgroup is
    // not charged, so debt drops to zero and the next replenishment
    // unthrottles.
    let any_clean_recovery: bool = per_cg
        .values()
        .any(|recs| recs.iter().any(|(kt, rtl, _pbo)| !*kt && *rtl == 0));

    // Primary post-fix assertion: the bug-shape (>= 2 CONSECUTIVE
    // keep_throttled=true) must NOT appear on any cgroup. This is the
    // V4-C regression guard.
    assert!(
        max_consecutive_kt <= 1,
        "V4-C engine fix regression: expected max_consecutive_keep_throttled <= 1 \
         (healthy oscillation), got {max_consecutive_kt}. The pre-V4-C bug-shape \
         is back: scxsim is over-charging cgroups during CPU idle (likely a stale \
         prev_task slipping through to lavd_dispatch -> consume_prev). Per-cgroup \
         replenish history (kt, rtl, pb_out):\n  {per_cg:?}\n\
         See V4-C commentary in safe/engine.rs for the rationale, and \
         experiments/scxsim_cbw_engine_fix_v4c_20260513/REPORT.md."
    );

    // Enforcement-still-works assertion: at least one throttle event
    // must fire. The H6 workload has a 10ms/100ms quota that one
    // CPU-bound task will always blow through; if NO throttle fired,
    // either the lib is no longer being consulted, or someone broke
    // the throttle-respect path (a too-eager fix that hides the canary
    // entirely).
    assert!(
        any_throttle_fired,
        "expected at least one keep_throttled=true period on the H6 \
         cell-C workload (10ms/100ms quota, CPU-bound task). Got zero. \
         Either the cgroup_bw lib is no longer being consulted (regression \
         in scxsim wiring), or the V4-C fix is OVER-aggressive and now \
         hides legitimate throttling. Per-cgroup history:\n  {per_cg:?}"
    );

    // Recovery assertion: at least one clean exit from throttle
    // (kt=false && rtl=0). This is the property the V4-C fix restores
    // -- without it, debt accumulates without bound and the cgroup
    // never escapes throttle (the cpu-bw-stall-bug surface).
    assert!(
        any_clean_recovery,
        "expected at least one keep_throttled=false && runtime_total_last=0 \
         period (clean recovery from throttle when no work pending). \
         Got zero, which means the cgroup never recovers cleanly -- debt \
         is still unbounded somewhere. The V4-C fix is incomplete or a \
         new over-charge path was added. Per-cgroup history:\n  {per_cg:?}"
    );

    eprintln!(
        "[smoking-gun] V4-C post-fix healthy oscillation observed: \
         max_consecutive_kt={max_consecutive_kt}, any_throttle_fired={any_throttle_fired}, \
         any_clean_recovery={any_clean_recovery}. Per-cgroup history:\n  {per_cg:?}"
    );
}
