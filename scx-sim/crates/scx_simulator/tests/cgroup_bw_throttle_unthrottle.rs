//! Tests for cgroup CPU-bandwidth (cpu.max) THROTTLE / UNTHROTTLE cycles
//! under LAVD — the only scheduler that compiles in the real
//! `scx/lib/cgroup_bw.bpf.c` enforcement library.
//!
//! These complement — do NOT duplicate — the existing cgroup_bw tests:
//!
//!  * `cgroup_bw_replenish_smoking_gun.rs` asserts the V4-C engine-fix
//!    "healthy oscillation" invariant via `CgroupBwReplenish`.
//!  * `cgroup_hierarchy.rs` asserts multi-level hierarchy enforcement
//!    (tight-vs-loose, deep nesting, runtime migration) but does NOT turn
//!    on LAVD's enqueue-time throttle path (`enable_cpu_bw`).
//!
//! Here the focus is the THROTTLE/UNTHROTTLE STATE MACHINE itself, observed
//! through the trace surface the library and engine expose:
//!
//!  1. Throttle fires when a cgroup exhausts its quota
//!     (`CbwThrottleCgroups{throttled=true}` + `CgroupBwReplenish{debt>0}`).
//!  2. Unthrottle happens at the replenish-period boundary
//!     (`CbwThrottleCgroups{throttled=false}` aligned to the period).
//!  3. The cycle repeats several times and STRICTLY alternates (regression
//!     guard against the cpu-bw-stall-bug "stuck throttled" shape).
//!  4. A task put aside while throttled (`LavdBailOnCgroupThrottle`) is
//!     re-dispatched on unthrottle (`CgroupBwReenqueueOnReplenish`) and
//!     makes forward progress afterwards.
//!  5. The same throttle/unthrottle cycle is observed for a bandwidth-limited
//!     cgroup that lives BELOW the root (a limited nested child directly
//!     holding the task under an unlimited parent).
//!  6. The throttle state reported across the two trace surfaces is
//!     internally consistent (`keep_throttled == (period_budget_out <= 0)`;
//!     transitions alternate; a cgroup starts unthrottled).
//!
//! No-Stub / model-the-kernel: every assertion is on trace events emitted
//! by the real linked-in `cgroup_bw.bpf.c`, never a Rust approximation.
//! All scenarios use the deterministic serial engine (`seed` +
//! `instant_timing`) so the throttle timeline is reproducible.

use scx_simulator::*;
use std::collections::BTreeMap;

mod common;

/// Replenish period used by every scenario here: 100ms, matching the
/// library's `CBW_REPLENISH_PERIOD` and the canonical H6 reproducer.
const PERIOD_US: u64 = 100_000;
const PERIOD_NS: u64 = PERIOD_US * 1_000;

/// Set a `bool` global (e.g. `enable_cpu_bw`) in the loaded LAVD `.so`.
///
/// # Safety
/// Caller must ensure `name` (NUL-terminated) is the literal name of a
/// `bool` global present in the loaded LAVD `.so`.
unsafe fn lavd_set_bool(sched: &DynamicScheduler, name: &str, val: bool) {
    let sym: libloading::Symbol<'_, *mut bool> = sched
        .get_symbol(name.as_bytes())
        .unwrap_or_else(|| panic!("symbol {name} not found"));
    std::ptr::write_volatile(*sym, val);
}

/// LAVD with cgroup-bandwidth enforcement actually turned ON.
///
/// `lavd_setup` hard-codes `enable_cpu_bw = false` ("disable complex
/// features for initial simulation"), which gates LAVD's enqueue-time
/// `cgroup_throttled()` check — without flipping it the throttle/replenish
/// path (`CgroupBwReplenish`, `LavdBailOnCgroupThrottle`,
/// `CbwThrottleCgroups`) never runs. Flip it on, matching the
/// `build_h6_scenario` tests and the `bug1_canonical.toml` fixture
/// (`[scheduler.bool_globals] enable_cpu_bw = true`).
fn lavd_cpu_bw(nr_cpus: u32) -> DynamicScheduler {
    let sched = DynamicScheduler::lavd(nr_cpus);
    sched.lavd_set_cgroup_bw_max(64);
    // SAFETY: `enable_cpu_bw` is a `bool` global in LAVD's main.bpf.c; the
    // symbol is present in the loaded `.so` and the scheduler outlives this
    // write.
    unsafe {
        lavd_set_bool(&sched, "enable_cpu_bw\0", true);
    }
    sched
}

fn forever_run(run_ns: u64) -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(run_ns)],
        repeat: RepeatMode::Forever,
    }
}

/// Count trace events whose `TraceKind` matches `pred`.
fn count_kind(trace: &Trace, pred: impl Fn(&TraceKind) -> bool) -> usize {
    trace.events().iter().filter(|e| pred(&e.kind)).count()
}

/// Collect `(time_ns, throttled)` `CbwThrottleCgroups` transitions per cgid,
/// in ascending time order (events are already time-ordered in the trace).
fn throttle_transitions(trace: &Trace) -> BTreeMap<u64, Vec<(u64, bool)>> {
    let mut per_cg: BTreeMap<u64, Vec<(u64, bool)>> = BTreeMap::new();
    for ev in trace.events() {
        if let TraceKind::CbwThrottleCgroups { cgid, throttled } = &ev.kind {
            per_cg
                .entry(cgid.0)
                .or_default()
                .push((ev.time_ns, *throttled));
        }
    }
    per_cg
}

/// The cgid whose throttle transitions we care about: the one with the most
/// `CbwThrottleCgroups` events (the contended, quota-limited cgroup).
fn busiest_throttled_cgid(trace: &Trace) -> Option<(u64, Vec<(u64, bool)>)> {
    throttle_transitions(trace)
        .into_iter()
        .max_by_key(|(_, v)| v.len())
}

fn assert_identical(t1: &Trace, t2: &Trace, ctx: &str) {
    assert_eq!(
        t1.events().len(),
        t2.events().len(),
        "{ctx}: trace lengths differ ({} vs {})",
        t1.events().len(),
        t2.events().len()
    );
    for (i, (e1, e2)) in t1.events().iter().zip(t2.events().iter()).enumerate() {
        assert_eq!(e1.time_ns, e2.time_ns, "{ctx}: event {i} time differs");
        assert_eq!(e1.cpu, e2.cpu, "{ctx}: event {i} cpu differs");
        assert_eq!(e1.kind, e2.kind, "{ctx}: event {i} kind differs");
    }
}

/// Canonical single-cgroup scenario: one tight-quota cgroup (10ms per 100ms
/// period = 10% of one CPU) holding a single CPU-bound task on a 4-CPU
/// machine, run for 600ms so the library's replenish timer fires ~6 times
/// and the throttle/unthrottle cycle repeats several times. Mirrors the H6
/// reproducer constants.
fn tight_quota_scenario() -> Scenario {
    Scenario::builder()
        .cpus(4)
        .seed(42)
        .instant_timing()
        .cgroup_with_bandwidth(
            "tight",
            &[CpuId(0), CpuId(1), CpuId(2), CpuId(3)],
            PERIOD_US,
            10_000, // quota_us = 10ms
            0,      // burst_us
        )
        .add_task_in_cgroup("hog", 0, forever_run(2_000_000_000), "tight")
        .duration_ms(600)
        .build()
}

// ---------------------------------------------------------------------------
// 1. Throttle triggers when quota is exhausted.
// ---------------------------------------------------------------------------

#[test]
fn test_throttle_triggers_when_quota_exhausted() {
    let _lock = common::setup_test();
    let trace = Simulator::new(lavd_cpu_bw(4)).run(tight_quota_scenario());

    assert_eq!(trace.exit_kind(), &ExitKind::Normal, "run should not stall");
    assert!(
        !trace.has_error(),
        "unexpected error: {:?}",
        trace.exit_kind()
    );

    // The cpu.max quota was actually configured on the cgroup.
    assert!(
        count_kind(&trace, |k| matches!(
            k,
            TraceKind::CgroupSetBandwidth { .. }
        )) >= 1,
        "expected the tight cgroup's cpu.max to be configured"
    );

    // The real cgroup_bw library marked at least one cgroup throttled after
    // it crossed its 10ms/100ms quota.
    let transitions = throttle_transitions(&trace);
    let throttled_fired = transitions.values().any(|trs| trs.iter().any(|(_, t)| *t));
    assert!(
        throttled_fired,
        "expected >=1 cgroup to be throttled (CbwThrottleCgroups{{throttled=true}}) \
         after exhausting its 10ms/100ms quota; transitions={transitions:?}"
    );

    // Replenish records confirm the CAUSE: at least one period ended with the
    // cgroup having overspent its budget (debt > 0), which is exactly what
    // drives the throttle decision inside cgroup_bw.bpf.c.
    let over_quota = trace
        .events()
        .iter()
        .any(|e| matches!(&e.kind, TraceKind::CgroupBwReplenish { debt, .. } if *debt > 0));
    assert!(
        over_quota,
        "expected >=1 CgroupBwReplenish period with debt>0 (quota exhausted)"
    );
}

// ---------------------------------------------------------------------------
// 2. Unthrottle happens when the replenish period resets.
// ---------------------------------------------------------------------------

#[test]
fn test_unthrottle_when_period_resets() {
    let _lock = common::setup_test();
    let trace = Simulator::new(lavd_cpu_bw(4)).run(tight_quota_scenario());

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    assert!(
        !trace.has_error(),
        "unexpected error: {:?}",
        trace.exit_kind()
    );

    let transitions = throttle_transitions(&trace);
    let mut unthrottles = 0usize;
    for (cg, trs) in &transitions {
        for (t, throttled) in trs {
            if !*throttled {
                unthrottles += 1;
                // The library clears `is_throttled` inside its per-period
                // replenish timer, so every unthrottle lands exactly on a
                // replenish-period boundary.
                assert_eq!(
                    t % PERIOD_NS,
                    0,
                    "unthrottle for cgid={cg} at t={t} is not aligned to the \
                     replenish period ({PERIOD_NS}ns); transitions={transitions:?}"
                );
            }
        }
    }
    assert!(
        unthrottles >= 1,
        "expected >=1 unthrottle (throttled->false) transition at a period \
         boundary; transitions={transitions:?}"
    );
}

// ---------------------------------------------------------------------------
// 3. Multiple throttle/unthrottle cycles that strictly alternate.
// ---------------------------------------------------------------------------

#[test]
fn test_multiple_throttle_unthrottle_cycles() {
    let _lock = common::setup_test();
    let trace = Simulator::new(lavd_cpu_bw(4)).run(tight_quota_scenario());

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    assert!(
        !trace.has_error(),
        "unexpected error: {:?}",
        trace.exit_kind()
    );

    let (cg, trs) = busiest_throttled_cgid(&trace).expect("expected some throttle transitions");

    let n_throttle = trs.iter().filter(|(_, t)| *t).count();
    let n_unthrottle = trs.iter().filter(|(_, t)| !*t).count();
    assert!(
        n_throttle >= 2 && n_unthrottle >= 2,
        "expected multiple throttle/unthrottle CYCLES on cgid={cg} \
         (>=2 each); got throttle={n_throttle} unthrottle={n_unthrottle}: {trs:?}"
    );

    // Strict alternation: the cgroup never records two identical states in a
    // row. A run of consecutive `throttled=true` would be the cpu-bw-stall-bug
    // "stuck throttled" shape; this is the regression guard against it.
    for w in trs.windows(2) {
        assert_ne!(
            w[0].1, w[1].1,
            "throttle state failed to alternate on cgid={cg}: {trs:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. A task queued (put aside) during throttle is dispatched on unthrottle.
// ---------------------------------------------------------------------------

#[test]
fn test_queued_task_dispatched_on_unthrottle() {
    let _lock = common::setup_test();
    let trace = Simulator::new(lavd_cpu_bw(4)).run(tight_quota_scenario());

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    assert!(
        !trace.has_error(),
        "unexpected error: {:?}",
        trace.exit_kind()
    );

    // While throttled, LAVD bails the task in `lavd_enqueue` and the library
    // puts it aside in its BTQ (LAVD bails earlier than the engine admission
    // gate, so `CgroupBwDequeueOnThrottle` does not fire under LAVD — the
    // `LavdBailOnCgroupThrottle` path does).
    let bails: Vec<(u64, Pid)> = trace
        .events()
        .iter()
        .filter_map(|e| match &e.kind {
            TraceKind::LavdBailOnCgroupThrottle { pid, .. } => Some((e.time_ns, *pid)),
            _ => None,
        })
        .collect();
    assert!(
        !bails.is_empty(),
        "expected the throttled task to be put aside (LavdBailOnCgroupThrottle)"
    );

    // On replenish/unthrottle the library re-enqueues the parked task.
    let reenqueues: Vec<(u64, Pid)> = trace
        .events()
        .iter()
        .filter_map(|e| match &e.kind {
            TraceKind::CgroupBwReenqueueOnReplenish { pid, .. } => Some((e.time_ns, *pid)),
            _ => None,
        })
        .collect();
    assert!(
        !reenqueues.is_empty(),
        "expected the parked task to be re-enqueued on replenish \
         (CgroupBwReenqueueOnReplenish)"
    );

    // Concretely, for the single hog (pid 1): it was bailed during throttle,
    // then re-enqueued at a LATER time that coincides with an unthrottle, and
    // it actually ran afterwards.
    let pid = Pid(1);
    let first_bail = bails
        .iter()
        .filter(|(_, p)| *p == pid)
        .map(|(t, _)| *t)
        .min()
        .expect("hog (pid 1) should be bailed at least once while throttled");
    let reenq_after = reenqueues
        .iter()
        .filter(|(t, p)| *p == pid && *t > first_bail)
        .map(|(t, _)| *t)
        .min()
        .unwrap_or_else(|| {
            panic!(
                "expected pid {} re-enqueued after its throttle-bail at {first_bail}",
                pid.0
            )
        });

    // The re-enqueue coincides with an unthrottle transition (period reset).
    let unthrottle_at_reenq = throttle_transitions(&trace).values().any(|trs| {
        trs.iter()
            .any(|(t, throttled)| !*throttled && *t == reenq_after)
    });
    assert!(
        unthrottle_at_reenq,
        "re-enqueue at {reenq_after} should coincide with an unthrottle transition"
    );

    // The task made forward progress once dispatched on unthrottle.
    let ran_after = trace.events().iter().any(|e| {
        e.time_ns >= reenq_after
            && matches!(&e.kind, TraceKind::TaskScheduled { pid: p } if *p == pid)
    });
    assert!(
        ran_after,
        "task (pid {}) did not run after being dispatched on unthrottle at {reenq_after}",
        pid.0
    );

    // Sanity: the task ran overall but was bandwidth-limited (nonzero, finite).
    assert!(
        trace.total_runtime(pid) > 0,
        "throttled task should still accrue some runtime across periods"
    );
}

// ---------------------------------------------------------------------------
// 5. Throttle/unthrottle cycle across a nested cgroup hierarchy.
// ---------------------------------------------------------------------------

/// A bandwidth-limited cgroup that lives BELOW the root of the hierarchy
/// (a limited `inner` child under an unlimited `outer` parent) and directly
/// holds the CPU-bound task. The throttle/unthrottle cycle must be observed
/// exactly as for a flat limited cgroup — proving the throttle machinery is
/// independent of the limited cgroup's depth — and the throttled nested task
/// must run strictly less than an unlimited control task.
///
/// NOTE: the complementary topology (limit on an ANCESTOR, task in an
/// unlimited descendant) does NOT throttle under `enable_cpu_bw` today —
/// the descendant's runtime is not rolled up to the limited ancestor's
/// cgroup_bw accounting (`runtime_total_last==0`). That fidelity gap is
/// tracked in mb sim-9e9273; this test deliberately puts the limit on the
/// cgroup that directly holds the task to stay faithful to what the real
/// library actually does under this path.
#[test]
fn test_throttle_unthrottle_in_nested_hierarchy() {
    let _lock = common::setup_test();
    let scenario = Scenario::builder()
        .cpus(4)
        .seed(42)
        .instant_timing()
        // Unlimited outer parent; limited inner child (10ms/100ms) holds the task.
        .cgroup("outer", &[CpuId(0), CpuId(1), CpuId(2), CpuId(3)])
        .cgroup_nested_bw("inner", "outer", PERIOD_US, 10_000, 0)
        .add_task_in_cgroup("deep", 0, forever_run(2_000_000_000), "inner")
        // Unlimited control cgroup with an equivalent task.
        .cgroup("free", &[CpuId(0), CpuId(1), CpuId(2), CpuId(3)])
        .add_task_in_cgroup("ctl", 0, forever_run(2_000_000_000), "free")
        .duration_ms(600)
        .build();

    let trace = Simulator::new(lavd_cpu_bw(4)).run(scenario);

    assert_eq!(
        trace.exit_kind(),
        &ExitKind::Normal,
        "nested hierarchy run should not stall"
    );
    assert!(
        !trace.has_error(),
        "unexpected error: {:?}",
        trace.exit_kind()
    );

    // Both throttle AND unthrottle transitions were observed even though the
    // limit is on the parent and the task is in the nested child.
    let transitions = throttle_transitions(&trace);
    let any_throttle = transitions.values().any(|trs| trs.iter().any(|(_, t)| *t));
    let any_unthrottle = transitions.values().any(|trs| trs.iter().any(|(_, t)| !*t));
    assert!(
        any_throttle && any_unthrottle,
        "expected both throttle and unthrottle transitions across the nested \
         hierarchy; transitions={transitions:?}"
    );

    // The alternation invariant holds for the nested case too.
    if let Some((cg, trs)) = busiest_throttled_cgid(&trace) {
        for w in trs.windows(2) {
            assert_ne!(
                w[0].1, w[1].1,
                "throttle state failed to alternate on nested cgid={cg}: {trs:?}"
            );
        }
    }

    // The limited nested task runs strictly less than the unlimited control.
    let deep_rt = trace.total_runtime(Pid(1));
    let ctl_rt = trace.total_runtime(Pid(2));
    eprintln!("nested-throttle: deep(10% limit)={deep_rt}ns ctl(unlimited)={ctl_rt}ns");
    assert!(
        deep_rt > 0 && ctl_rt > deep_rt,
        "the throttled nested task should run less than the unlimited control: \
         deep={deep_rt} ctl={ctl_rt}"
    );
}

// ---------------------------------------------------------------------------
// 6. Throttle state is internally consistent across the trace surfaces.
// ---------------------------------------------------------------------------

#[test]
fn test_throttle_state_correct_in_trace() {
    let _lock = common::setup_test();
    let trace = Simulator::new(lavd_cpu_bw(4)).run(tight_quota_scenario());

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    assert!(
        !trace.has_error(),
        "unexpected error: {:?}",
        trace.exit_kind()
    );

    // (a) Every replenish record's throttle decision matches its budget:
    // keep_throttled == (period_budget_out <= 0), and the derived fields are
    // non-negative (mirrors cgroup_bw.bpf.c's own invariants).
    let mut replenishes = 0usize;
    for e in trace.events() {
        if let TraceKind::CgroupBwReplenish {
            cgid,
            runtime_total_last,
            debt,
            burst_credit,
            period_budget_out,
            keep_throttled,
            ..
        } = &e.kind
        {
            replenishes += 1;
            assert_eq!(
                *keep_throttled,
                *period_budget_out <= 0,
                "cgid={} keep_throttled={keep_throttled} disagrees with \
                 period_budget_out={period_budget_out}",
                cgid.0
            );
            assert!(*debt >= 0, "cgid={} negative debt {debt}", cgid.0);
            assert!(
                *burst_credit >= 0,
                "cgid={} negative burst_credit {burst_credit}",
                cgid.0
            );
            assert!(
                *runtime_total_last >= 0,
                "cgid={} negative runtime_total_last {runtime_total_last}",
                cgid.0
            );
        }
    }
    assert!(
        replenishes > 0,
        "expected >=1 CgroupBwReplenish record to validate throttle state against"
    );

    // (b) Per cgid: transitions strictly alternate, and the FIRST observed
    // transition is `throttled=true` — a cgroup starts unthrottled and only
    // becomes throttled after crossing quota (a 0->1 edge, never 1->0 first).
    let transitions = throttle_transitions(&trace);
    assert!(
        !transitions.is_empty(),
        "expected at least one cgroup with throttle transitions"
    );
    for (cg, trs) in &transitions {
        assert!(
            trs.first().expect("nonempty by construction").1,
            "first throttle transition for cgid={cg} should be throttled=true \
             (cgroup starts unthrottled): {trs:?}"
        );
        for w in trs.windows(2) {
            assert_ne!(
                w[0].1, w[1].1,
                "non-alternating throttle state on cgid={cg}: {trs:?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 7. Determinism guard — the throttle timeline is fully reproducible.
// ---------------------------------------------------------------------------

#[test]
fn test_throttle_unthrottle_determinism() {
    let _lock = common::setup_test();
    let t1 = Simulator::new(lavd_cpu_bw(4)).run(tight_quota_scenario());
    let t2 = Simulator::new(lavd_cpu_bw(4)).run(tight_quota_scenario());
    assert_identical(&t1, &t2, "throttle/unthrottle timeline");
    assert_eq!(t1.exit_kind(), &ExitKind::Normal);

    // The two runs must agree on the full throttle-transition timeline.
    assert_eq!(
        throttle_transitions(&t1),
        throttle_transitions(&t2),
        "throttle transitions differ between identical runs"
    );
}
