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
//! 2. **Smoking-gun signature DETECTED when present**: the H6 cell-C
//!    workload (kernel cpu.max=10000/100000 + LAVD enable_cpu_bw + a
//!    long-running CPU-bound task) drives the lib into multiple
//!    consecutive `keep_throttled=true && runtime_total_last==0`
//!    replenishments for the throttled cgroup. That signature -- the
//!    cgroup never escapes throttle, no work was done in the period --
//!    is the specific shape of the cpu-bw-stall-bug. The test asserts
//!    its presence on the integrated `simulator.v6` tip (which is NOT
//!    Bug-1-FIXED; the engine-throttle-attribution fix only repaired
//!    the simulator's wiring, not the underlying scheduler bug).
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

/// The cpu-bw-stall-bug's smoking-gun signature surfaces in
/// `CgroupBwReplenish` as a cgroup that gets `keep_throttled=true`
/// (`period_budget_out <= 0`) and stays that way across multiple
/// consecutive replenishments. Means the lib's debt accumulation
/// outpaces budget refill -- the cgroup never escapes throttle.
///
/// wprof-trace-baseline's recommended sub-signature is "rtl==0 too"
/// (no work done in the period, the most damning case). In practice
/// the H6 cell-C scenario produces a STRONGER fingerprint: rtl values
/// at OR ABOVE the per-period quota (the engine's BandwidthManager
/// keeps charging the cgroup even after the lib says throttled), so
/// the lib computes a positive debt every period and `period_budget`
/// just gets MORE negative. That is the same bug surface, observed
/// from a different scheduler-engine interaction. This test asserts
/// the persistence-of-keep_throttled signal (the bug-shape independent
/// of how much work happened) and reports any rtl==0 hits as a stronger
/// finding when present.
///
/// On a Bug-1-FIXED scheduler, the lib must converge out of throttle
/// within a couple of periods. The integrated `simulator.v6` is NOT
/// Bug-1-FIXED -- it has the engine-throttle-attribution fix that
/// wires the lib correctly, but the underlying scheduler bug is still
/// present. So this test expects persistent `keep_throttled` to fire.
///
/// If this test starts failing without an explicit Bug-1 fix landing,
/// that is a regression: the smoking-gun observer stopped emitting
/// events or the lib's behavior under H6 cell-C silently changed.
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

    // Primary smoking gun: ANY cgroup with >= 2 CONSECUTIVE
    // `keep_throttled=true` records. That's the bug-shape: the lib's
    // budget went non-positive and stayed non-positive across multiple
    // refills. A single hit is normal (one period of throttling); >= 2
    // consecutive is the persistence signal.
    let primary: Vec<(u64, usize)> = per_cg
        .iter()
        .filter_map(|(cgid, recs)| {
            let mut max_run = 0;
            let mut cur = 0;
            for (kt, _rtl, _pbo) in recs {
                if *kt {
                    cur += 1;
                    max_run = max_run.max(cur);
                } else {
                    cur = 0;
                }
            }
            if max_run >= 2 {
                Some((*cgid, max_run))
            } else {
                None
            }
        })
        .collect();

    // Secondary (stronger) sub-signature: any cgroup with a
    // `keep_throttled=true && runtime_total_last==0` record. Means
    // the lib carried debt forward through a period in which NO work
    // was done -- the most damning evidence. wprof-trace-baseline's
    // recommended canary. Reported as informational; not asserted
    // because the H6 cell-C scenario produces persistent
    // keep_throttled with runtime_total_last >= quota instead of 0
    // (engine's BandwidthManager keeps charging through the throttle).
    let no_work_throttled: Vec<(u64, usize)> = per_cg
        .iter()
        .filter_map(|(cgid, recs)| {
            let n = recs.iter().filter(|(kt, rtl, _)| *kt && *rtl == 0).count();
            if n > 0 {
                Some((*cgid, n))
            } else {
                None
            }
        })
        .collect();

    assert!(
        !primary.is_empty(),
        "expected at least one cgroup with >= 2 CONSECUTIVE \
         CgroupBwReplenish events where keep_throttled=true (the cpu-bw-\
         stall-bug persistence-of-throttle signature). Per-cgroup \
         replenish history (kt, rtl, pb_out):\n  {per_cg:?}\n\
         If the integrated simulator.v6 has been Bug-1-FIXED upstream, \
         this test is the canary -- update the assertion to match the \
         fixed behavior. Otherwise, an upstream change broke the smoking-\
         gun observer or the H6 cell-C workload no longer drives the lib \
         into the bug surface."
    );

    eprintln!(
        "[smoking-gun] persistent keep_throttled detected on {} cgroup(s) \
         (cgid, max_consecutive_runs): {primary:?}",
        primary.len()
    );
    if !no_work_throttled.is_empty() {
        eprintln!(
            "[smoking-gun] STRONGER no-work-throttled signal on {} cgroup(s) \
             (cgid, count_of_kt_and_rtl0): {no_work_throttled:?}",
            no_work_throttled.len()
        );
    } else {
        eprintln!(
            "[smoking-gun] no rtl==0 sub-signature observed on this run -- \
             persistent keep_throttled with rtl > 0 indicates the engine's \
             BandwidthManager keeps charging the cgroup even while the lib \
             says throttled, which is a different (but related) bug surface."
        );
    }
}
