//! V5-A: cbw accounting timer MIN-bound activation regression test.
//!
//! tg `add-bug1-v5-d-min-bound-active-integration-test`. Promotes V5's
//! `D_16cg_2each` fixture into the standard scxsim test suite as a
//! guardrail against future regressions silently dropping MIN-bound
//! firing of the cgroup_bw accounting timer.
//!
//! # Background — the V4-B finding this test guards against
//!
//! V4-B (`scxsim-investigate-cbw-accounting-timer-fire-rate`)
//! discovered that `bug1_canonical` (single-cgroup fixture) produces
//! ONLY 3 MIN-bound (1ms) fires across a 5s run — all transitional, at
//! the first throttle/refill boundary (~t=100ms). After that the timer
//! locks at MAX=20ms for the entire stall. Root cause: the
//! `cbw_throttle_cgroups()` library function (lib/cgroup_bw.bpf.c:1339)
//! skips already-throttled cgroups when computing
//! `min_time_to_throttle`. With ONE cgroup, once it throttles → loop
//! has nothing → `U64_MAX/4` clamps to `CBW_ACCOUNTING_PERIOD_MAX`
//! (20ms) for the rest of the run. Structural lock-out, not transient.
//!
//! V5 (`scxsim-design-fixture-to-reproduce-live-pr3521-mechanism`)
//! escaped that lock-out by designing multi-cgroup fixtures.
//! `D_16cg_2each` (16 cgroups × 2 tasks each, cpu.max=10000/100000 per
//! cgroup) fires 469 MIN-bound events distributed across all 5 seconds
//! of a 5s run — the timer adaptive-mode is engaged 78% of the time.
//! Empirical headline data is in
//! `experiments/v5_multi_cgroup_fixture_20260514/REPORT.md`.
//!
//! # What this test asserts
//!
//! 1. **MIN-bound fires happen continuously.** With the V5-D shape
//!    (16 cgroups × 2 tasks each, 10% quota per cgroup), the cbw
//!    accounting_timer must fire at MIN=1ms at least N times across a
//!    5s run, where N is well below the empirical 469 to leave headroom
//!    for minor scheduler-timing variation.
//!
//! 2. **MIN-bound fires are spread across the run** — at least 4 of 5
//!    seconds must contain at least one MIN-bound fire. (If MIN fires
//!    cluster only at run start, the V4-B lock-out has effectively
//!    re-emerged, just on a longer transient.)
//!
//! 3. **Determinism** — across 3 reps the MIN-bound count and per-second
//!    spread are byte-identical. This is the same determinism contract
//!    enforced by `bug1_canonical_repro::test_bug1_canonical_subprocess_deterministic_10_reps`.
//!
//! # Validation use case
//!
//! When a candidate fix lands in `scx/lib/cgroup_bw.bpf.c` targeting
//! the timer logic (e.g. raising `CBW_ACCOUNTING_PERIOD_MIN`,
//! changing `CBW_ACCOUNTING_PERIOD_DIVISOR`, or fixing the
//! skip-throttled logic), this test will FAIL and direct the author
//! to revisit the fixture or update the threshold.
//!
//! # Why a multi-cgroup fixture and not bug1_canonical
//!
//! `bug1_canonical` exercises the lib only in MAX-locked mode (3
//! transitional MIN fires). It cannot validate timer-rate behavior
//! changes in any meaningful way. V5-D structurally exercises the
//! adaptive-mode code path; that's what V5-A's test guards.

use scx_simulator::*;

#[macro_use]
mod common;

// ---------------------------------------------------------------------------
// Constants. Mirror the V5 D_16cg_2each fixture exactly so this test
// matches the JSON fixture at tests/fixtures/h6/bug1_v5_d_min_bound_active.json
// (kept in lockstep — see the README in tests/fixtures/h6/).
// ---------------------------------------------------------------------------

/// Number of cgroups (each with finite quota). 16 cgroups → 16 throttle
/// streams to keep cbw_throttle_cgroups() finding non-throttled work.
const N_CGROUPS: usize = 16;
/// Tasks per cgroup. 2 tasks × 16 cgroups = 32 tasks total.
const TASKS_PER_CG: usize = 2;
/// Number of CPUs.
const N_CPUS: u32 = 4;
/// Per-cgroup `cpu.max` quota in microseconds (10ms).
const QUOTA_US: u64 = 10_000;
/// Per-cgroup `cpu.max` period in microseconds (100ms = lib's
/// `CBW_REPLENISH_PERIOD`).
const PERIOD_US: u64 = 100_000;
/// Simulated wall duration (5 seconds = 50 lib replenishment periods).
const DURATION_MS: u64 = 5_000;
/// Workload behavior: each task is a long-running CPU-bound burst far
/// in excess of what its 10% cgroup quota can finish in 5s. Forces
/// continuous throttle/refill cycling on every cgroup.
const TASK_BURST_NS: u64 = 2_000_000_000;
/// CBW_ACCOUNTING_PERIOD_MIN from `scx/lib/cgroup_bw.bpf.c`.
/// `cbw_throttle_cgroups()` clamps the next-fire interval to
/// `[MIN, MAX]`; the test verifies the timer reaches this MIN bound
/// continuously, not just transiently.
const CBW_ACCOUNTING_PERIOD_MIN_NS: u64 = 1_000_000;
/// Slot index where LAVD's wrapper.c maps the `&accounting_timer` BPF
/// timer in `lavd_timer_table` (initialization-order dependent: slot 0
/// = LAVD update_timer, slot 1 = cbw replenish_timer, slot 2 = cbw
/// accounting_timer). Empirically validated by V4-B's slot enumeration
/// of the canonical fixture (`experiments/v4b_cbw_accounting_timer_fire_rate_20260513/REPORT.md`).
const ACCOUNTING_TIMER_SLOT: u8 = 2;

/// Lower bound on MIN-bound fire count. Empirical V5-D measurement was
/// 469 across 5s. The threshold leaves wide headroom (~3× margin) so
/// minor scheduler-timing variation cannot trip the test, while still
/// catching any regression that drops the count by an order of
/// magnitude (e.g. back to V4-B's 3 transitional fires only).
const MIN_FIRES_THRESHOLD: usize = 150;

/// Number of 1-second buckets that must contain at least one MIN-bound
/// fire. The 5-second run has 5 buckets; the test demands fires in at
/// least 4 of 5. Empirical V5-D measurement: all 5/5 buckets contained
/// MIN fires (counts: 145, 118, 80, 52, 74). 4-of-5 leaves headroom
/// for the bucket-boundary edge case.
const MIN_BUCKETS_WITH_MIN_FIRES: usize = 4;

/// Determinism rep count. 3 reps catches non-deterministic behavior
/// without inflating wall time.
const N_DETERMINISM_REPS: usize = 3;

/// LAVD `enable_cpu_bw` boolean global setter (mirrors helpers in
/// `bug1_canonical_consume_ns_bound.rs` and
/// `cgroup_bw_replenish_smoking_gun.rs` — kept local-private here to
/// avoid pulling another shared module).
///
/// # Safety
/// Caller must ensure `name` is the literal name of a `bool` global
/// in the loaded LAVD `.so`. The scheduler must outlive this write.
unsafe fn lavd_set_bool(sched: &DynamicScheduler, name: &str, val: bool) {
    let sym: libloading::Symbol<'_, *mut bool> = sched
        .get_symbol(name.as_bytes())
        .unwrap_or_else(|| panic!("symbol {name} not found"));
    std::ptr::write_volatile(*sym, val);
}

/// Build the V5-D scenario in-process. Matches the JSON fixture at
/// `tests/fixtures/h6/bug1_v5_d_min_bound_active.json` cell-for-cell.
fn build_v5d_scenario() -> Scenario {
    let cpuset: Vec<CpuId> = (0..N_CPUS).map(CpuId).collect();
    let mut b = Scenario::builder().cpus(N_CPUS);
    for cg_idx in 0..N_CGROUPS {
        let cg_name = format!("v5d_cg{cg_idx}");
        b = b.cgroup_with_bandwidth(&cg_name, &cpuset, PERIOD_US, QUOTA_US, 0);
        for t in 0..TASKS_PER_CG {
            let task_name = format!("yes_{}", cg_idx * TASKS_PER_CG + t);
            b = b.add_task_in_cgroup(&task_name, 0, workloads::cpu_bound(TASK_BURST_NS), &cg_name);
        }
    }
    b.duration_ms(DURATION_MS).build()
}

/// Filter trace events to slot-2 (cbw accounting_timer) MIN-bound fires.
/// Returns (total_fires, per_second_buckets) where per_second_buckets[i]
/// counts MIN-bound fires whose timestamp falls in [i*1s, (i+1)*1s).
fn extract_min_bound_fires(trace: &Trace) -> (usize, Vec<usize>) {
    let n_secs = ((DURATION_MS + 999) / 1000) as usize;
    let mut buckets = vec![0usize; n_secs];
    let mut total = 0usize;
    for ev in trace.events() {
        if let TraceKind::CbwAccountingTimerFired {
            slot,
            requested_period_ns,
            ..
        } = &ev.kind
        {
            if *slot == ACCOUNTING_TIMER_SLOT
                && *requested_period_ns == CBW_ACCOUNTING_PERIOD_MIN_NS
            {
                total += 1;
                let idx = (ev.time_ns / 1_000_000_000) as usize;
                if idx < n_secs {
                    buckets[idx] += 1;
                }
            }
        }
    }
    (total, buckets)
}

/// Single rep: build scenario, set enable_cpu_bw=true, run, extract
/// MIN-bound fire stats. Returns (total_min_fires, per_second_bucket_counts).
fn run_one_rep() -> (usize, Vec<usize>) {
    let sched = DynamicScheduler::lavd(N_CPUS);
    // SAFETY: `enable_cpu_bw` is a bool global declared in LAVD's
    // main.bpf.c; the symbol is present in the loaded `.so`. The
    // scheduler outlives this assignment — moves into the Simulator.
    unsafe {
        lavd_set_bool(&sched, "enable_cpu_bw", true);
    }
    let scenario = build_v5d_scenario();
    let trace = Simulator::new(sched).run(scenario);
    extract_min_bound_fires(&trace)
}

// ---------------------------------------------------------------------------
// Test 1: MIN-bound fires happen + are spread across the run
// ---------------------------------------------------------------------------

#[test]
fn test_v5d_min_bound_fires_continuously() {
    let _lock = common::setup_test();

    let (total, buckets) = run_one_rep();
    eprintln!("[v5d_min_bound] total MIN-bound fires = {total}, per-second buckets = {buckets:?}");

    assert!(
        total >= MIN_FIRES_THRESHOLD,
        "V4-B regression: V5-D fixture should fire MIN-bound (1ms) at least {MIN_FIRES_THRESHOLD} \
         times in {DURATION_MS}ms; got {total}. Empirical baseline was 469. If the count has \
         dropped near zero (e.g. < 10), the V4-B structural lock-out has effectively re-emerged \
         (cbw_throttle_cgroups now skips all cgroups, returning U64_MAX/4 = MAX). per-second \
         buckets = {buckets:?}"
    );

    let buckets_with_fires = buckets.iter().filter(|&&c| c > 0).count();
    assert!(
        buckets_with_fires >= MIN_BUCKETS_WITH_MIN_FIRES,
        "V5-D fixture should fire MIN-bound (1ms) in at least {MIN_BUCKETS_WITH_MIN_FIRES} of \
         {} 1-second buckets; got {buckets_with_fires} (buckets = {buckets:?}). If MIN fires \
         cluster only at run start, the V4-B lock-out has re-emerged on a longer transient. \
         Total MIN fires = {total}.",
        buckets.len()
    );
}

// ---------------------------------------------------------------------------
// Test 2: determinism — repeated reps produce identical fire counts
// ---------------------------------------------------------------------------

#[test]
fn test_v5d_min_bound_deterministic() {
    let _lock = common::setup_test();

    let mut first: Option<(usize, Vec<usize>)> = None;
    for rep in 0..N_DETERMINISM_REPS {
        let result = run_one_rep();
        match &first {
            None => first = Some(result),
            Some(prev) => {
                assert_eq!(
                    &result, prev,
                    "rep {rep}: nondeterministic V5-D MIN-bound fire count or per-second spread. \
                     Got {result:?}, expected {prev:?}. Determinism is part of the V5 acceptance \
                     contract — see experiments/v5_multi_cgroup_fixture_20260514/REPORT.md."
                );
            }
        }
    }
    eprintln!("[v5d_min_bound] {N_DETERMINISM_REPS} reps deterministic; result = {first:?}");
}
