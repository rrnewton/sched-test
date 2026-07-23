//! Task weight / priority / nice-value mapping tests
//! (tg test-weight-priority-mapping).
//!
//! Two layers:
//!   A. Unit tests of the pure conversion functions `nice_to_weight` and
//!      `sched_weight_to_cgroup` against the kernel `sched_prio_to_weight`
//!      table — these had no direct test.
//!   B. Behavioral tests that the mapped weight actually influences scheduling:
//!      weight *ordering* (heavier nice gets more CPU) across simple/lavd/cosmos,
//!      quantitative *proportionality* on the fair scheduler (simple), and
//!      *starvation prevention* for an extreme-low-priority task under all three.
//!
//! Per-scheduler weighted-fairness ratios are already covered in `simple.rs` /
//! `tickless.rs` / `mitosis.rs` / `lavd.rs`; this file adds the missing
//! conversion unit tests, the cross-scheduler ordering guarantee, the
//! conversion->behavior linkage (expected ratios derived from
//! `nice_to_weight`), and explicit anti-starvation coverage.
//!
//! Priority inheritance (PI): scxsim does NOT model priority inheritance /
//! futex priority-boost. LAVD's futex-boost machinery (`lock.bpf.c`) runs 0%
//! under simulation because the substrate delivers no futex
//! tracepoints/fexit hooks (see the coverage audit + related scxsim-infra
//! notes). There is therefore no supported PI path to test here; task item 4
//! ("if supported") is not applicable until the futex substrate exists.

use scx_simulator::*;

#[macro_use]
mod common;

// ============================================================================
// A. Conversion-function unit tests
// ============================================================================

/// The full kernel `sched_prio_to_weight` table (kernel/sched/core.c), indexed
/// by `nice + 20`. `nice_to_weight` must reproduce it exactly.
const KERNEL_PRIO_TO_WEIGHT: [(i8, u32); 40] = [
    (-20, 88761),
    (-19, 71755),
    (-18, 56483),
    (-17, 46273),
    (-16, 36291),
    (-15, 29154),
    (-14, 23254),
    (-13, 18705),
    (-12, 14949),
    (-11, 11916),
    (-10, 9548),
    (-9, 7620),
    (-8, 6100),
    (-7, 4904),
    (-6, 3906),
    (-5, 3121),
    (-4, 2501),
    (-3, 1991),
    (-2, 1586),
    (-1, 1277),
    (0, 1024),
    (1, 820),
    (2, 655),
    (3, 526),
    (4, 423),
    (5, 335),
    (6, 272),
    (7, 215),
    (8, 172),
    (9, 137),
    (10, 110),
    (11, 87),
    (12, 70),
    (13, 56),
    (14, 45),
    (15, 36),
    (16, 29),
    (17, 23),
    (18, 18),
    (19, 15),
];

#[test]
fn test_nice_to_weight_matches_kernel_table() {
    for (nice, expected) in KERNEL_PRIO_TO_WEIGHT {
        assert_eq!(
            nice_to_weight(nice),
            expected,
            "nice_to_weight({nice}) should be {expected}"
        );
    }
    // Spot-check the canonical anchors.
    assert_eq!(
        nice_to_weight(0),
        1024,
        "nice 0 is the 1024 reference weight"
    );
    assert_eq!(nice_to_weight(-20), 88761, "nice -20 is the max weight");
    assert_eq!(nice_to_weight(19), 15, "nice 19 is the min weight");
}

#[test]
fn test_nice_to_weight_strictly_decreasing() {
    // Weight decreases monotonically as nice increases, and each step is the
    // kernel's ~1.25x multiplier (10% CPU per nice level).
    for nice in -20..19i8 {
        let heavier = nice_to_weight(nice);
        let lighter = nice_to_weight(nice + 1);
        assert!(
            heavier > lighter,
            "weight must strictly decrease: nice {nice}={heavier} !> nice {}={lighter}",
            nice + 1
        );
        let step = heavier as f64 / lighter as f64;
        assert!(
            (1.15..=1.40).contains(&step),
            "per-nice step ratio {step:.3} at nice {nice} outside kernel ~1.25x band"
        );
    }
}

#[test]
#[should_panic(expected = "out of range")]
fn test_nice_to_weight_rejects_too_high() {
    let _ = nice_to_weight(20);
}

#[test]
#[should_panic(expected = "out of range")]
fn test_nice_to_weight_rejects_too_low() {
    let _ = nice_to_weight(-21);
}

#[test]
fn test_sched_weight_to_cgroup() {
    // The nice-0 reference weight (1024) maps to the cgroup default (100).
    assert_eq!(
        sched_weight_to_cgroup(nice_to_weight(0)),
        100,
        "nice-0 weight (1024) must map to cgroup default 100"
    );
    // Monotonic and clamped to [1, 10000].
    for nice in -20..=19i8 {
        let cg = sched_weight_to_cgroup(nice_to_weight(nice));
        assert!(
            (1..=10000).contains(&cg),
            "cgroup weight {cg} for nice {nice} out of [1,10000]"
        );
    }
    // Heaviest nice maps well above default; lightest floors at >= 1.
    assert!(
        sched_weight_to_cgroup(nice_to_weight(-20)) > 100,
        "max weight should exceed cgroup default"
    );
    assert!(
        sched_weight_to_cgroup(nice_to_weight(19)) >= 1,
        "min weight must clamp to at least 1"
    );
    // Extreme inputs clamp to the ends.
    assert_eq!(
        sched_weight_to_cgroup(u32::MAX),
        10000,
        "over-max clamps to 10000"
    );
    assert_eq!(sched_weight_to_cgroup(0), 1, "zero clamps to 1");
}

// ============================================================================
// B. Behavioral tests (weight actually affects scheduling)
// ============================================================================

/// A perpetually-runnable CPU hog with a given nice value.
fn hog(pid: i32, nice: i8) -> TaskDef {
    TaskDef {
        name: format!("nice{nice}_p{pid}"),
        pid: Pid(pid),
        nice,
        behavior: TaskBehavior {
            phases: vec![Phase::Run(100_000_000)],
            repeat: RepeatMode::Forever,
        },
        start_time_ns: 0,
        mm_id: None,
        allowed_cpus: None,
        parent_pid: None,
        cgroup_name: None,
        task_flags: 0,
        migration_disabled: 0,
    }
}

/// Weight ordering must hold for EVERY scheduler: on a single contended CPU,
/// a heavier task (lower nice) gets strictly more CPU than a lighter one.
/// (Exact proportionality is scheduler-specific — lavd's vtime + latency
/// criticality dampen it — but the ordering is a universal invariant.)
#[test]
fn test_weight_ordering_all_schedulers() {
    let _lock = common::setup_test();
    let build = || {
        Scenario::builder()
            .cpus(1)
            .seed(1)
            .task(hog(1, -5)) // heaviest (weight 3121)
            .task(hog(2, 0)) // middle    (weight 1024)
            .task(hog(3, 5)) // lightest  (weight 335)
            .duration_ms(900)
            .build()
    };

    type Make = fn() -> DynamicScheduler;
    let makers: &[(&str, Make)] = &[
        ("simple", DynamicScheduler::simple),
        ("lavd", || DynamicScheduler::lavd(1)),
        ("cosmos", || DynamicScheduler::cosmos(1)),
    ];

    for &(name, make) in makers {
        let trace = Simulator::new(make()).run(build());
        assert_eq!(
            *trace.exit_kind(),
            ExitKind::Normal,
            "[{name}] abnormal exit"
        );
        assert!(!trace.has_error(), "[{name}] scheduler error");

        let heavy = trace.total_runtime(Pid(1));
        let mid = trace.total_runtime(Pid(2));
        let light = trace.total_runtime(Pid(3));
        assert!(
            heavy > mid && mid > light,
            "[{name}] weight ordering violated: nice-5={heavy} nice0={mid} nice+5={light}"
        );
        assert!(light > 0, "[{name}] lightest task starved (got 0 runtime)");
    }
}

/// Quantitative proportionality on the fair scheduler (simple): observed
/// runtime ratios must track the `nice_to_weight`-derived weight ratios. This
/// links the (unit-tested) conversion to real scheduling behavior — the
/// expected ratios are computed from `nice_to_weight`, not hardcoded.
#[test]
fn test_weight_proportionality_tracks_conversion_simple() {
    let _lock = common::setup_test();
    // nice -5 / 0 / +5 -> weights 3121 / 1024 / 335.
    let (n_heavy, n_mid, n_light) = (-5i8, 0i8, 5i8);
    let scenario = Scenario::builder()
        .cpus(1)
        .seed(1)
        .task(hog(1, n_heavy))
        .task(hog(2, n_mid))
        .task(hog(3, n_light))
        .duration_ms(900)
        .build();

    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario);
    let (heavy, mid, light) = (
        trace.total_runtime(Pid(1)) as f64,
        trace.total_runtime(Pid(2)) as f64,
        trace.total_runtime(Pid(3)) as f64,
    );

    let expected_hm = nice_to_weight(n_heavy) as f64 / nice_to_weight(n_mid) as f64; // ~3.05
    let expected_ml = nice_to_weight(n_mid) as f64 / nice_to_weight(n_light) as f64; // ~3.06
    let observed_hm = heavy / mid;
    let observed_ml = mid / light;

    // simple is EEVDF-like fair; observed tracks ideal to within ~35%.
    let within =
        |observed: f64, expected: f64| (0.65 * expected..=1.35 * expected).contains(&observed);
    assert!(
        within(observed_hm, expected_hm),
        "heavy/mid ratio {observed_hm:.3} not within 35% of expected {expected_hm:.3}"
    );
    assert!(
        within(observed_ml, expected_ml),
        "mid/light ratio {observed_ml:.3} not within 35% of expected {expected_ml:.3}"
    );
}

/// Starvation prevention: a single extreme-low-priority task (nice +19, weight
/// 15) sharing one CPU with four extreme-high-priority hogs (nice -20, weight
/// 88761 each — a ~23000x aggregate weight disadvantage) must still be
/// scheduled and make some progress under every scheduler. A weight-only
/// scheduler with no anti-starvation floor would let it run 0.
#[test]
fn test_no_starvation_extreme_priority_all_schedulers() {
    let _lock = common::setup_test();
    let build = || {
        let mut b = Scenario::builder().cpus(1).seed(1);
        for p in 1..=4 {
            b = b.task(hog(p, -20)); // four heaviest hogs
        }
        b = b.task(hog(5, 19)); // the lightest task
        b.duration_ms(1000).build()
    };

    type Make = fn() -> DynamicScheduler;
    let makers: &[(&str, Make)] = &[
        ("simple", DynamicScheduler::simple),
        ("lavd", || DynamicScheduler::lavd(1)),
        ("cosmos", || DynamicScheduler::cosmos(1)),
    ];

    for &(name, make) in makers {
        let trace = Simulator::new(make()).run(build());
        assert_eq!(
            *trace.exit_kind(),
            ExitKind::Normal,
            "[{name}] abnormal exit"
        );
        assert!(!trace.has_error(), "[{name}] scheduler error");

        let low_rt = trace.total_runtime(Pid(5));
        let low_sched = trace.schedule_count(Pid(5));
        assert!(
            low_rt > 0 && low_sched > 0,
            "[{name}] nice+19 task starved: runtime={low_rt}ns schedule_count={low_sched}"
        );
        // The heavy hogs should still dominate (sanity: this is not equal share).
        let heavy_sum: u64 = (1..=4).map(|p| trace.total_runtime(Pid(p))).sum();
        assert!(
            heavy_sum > low_rt,
            "[{name}] expected heavy hogs to dominate: heavy_sum={heavy_sum} low={low_rt}"
        );
    }
}
