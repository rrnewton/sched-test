//! RBC (Retired Branch Conditional) scheduler-overhead / instruction-counting
//! tests.
//!
//! scxsim can charge scheduler-callback overhead to CPU time by measuring the
//! *retired conditional branches* (RBC) executed inside the BPF scheduler's C
//! code and multiplying by `sched_overhead_rbc_ns` (ns per branch). This is the
//! "instruction counting" side of RBC: the more branches a callback executes,
//! the more simulated CPU time that scheduling decision costs, which in turn
//! shifts the timeline and the scheduling decisions that follow.
//!
//! The complementary RBC feature — *preemptive interleaving* driven by PMU
//! branch-count timeslices — is exercised for determinism in
//! `determinism.rs` (`determinism_cooperative_preemptive`,
//! `record_replay_determinism`) and `interleave.rs`. This file deliberately
//! targets the previously-untested overhead/counting model
//! (`sched_overhead_rbc_ns`), not the interleaving token ring.
//!
//! ## Determinism caveat (mb sim-70abc8)
//!
//! Per CLAUDE.md, RBC counts ARE deterministic for a fixed instruction stream,
//! but raw PMU-RBC *reads* carry a documented ~10% run-to-run variation on some
//! hardware. So the PMU-driven overhead is asserted **directionally** (large
//! overhead measurably reduces scheduling) and with **tolerance** (repeated
//! runs stay close), never byte-exact — mirroring why `determinism.rs` keeps its
//! byte-exact PMU test `#[ignore]`. The byte-exact assertions here use only the
//! RBC-*disabled* path (`Some(0)`/`None`), which is pure deterministic engine.
//!
//! When the PMU is unavailable (VMs / containers / CI without perf access),
//! `sched_overhead_rbc_ns` charges nothing, so the PMU-dependent tests assert
//! graceful completion instead of the timing effect.

use scx_simulator::*;

#[macro_use]
mod common;

/// A named scheduler constructor: `(label, factory)`, so one test body can
/// sweep the same RBC config across multiple schedulers.
type NamedSchedFactory = (&'static str, fn(u32) -> DynamicScheduler);

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// True when a hardware RBC counter can actually be created and counts — the
/// same probe the engine uses to decide whether to charge RBC overhead.
fn pmu_available() -> bool {
    scx_simulator::perf::try_create_rbc_counter().is_some()
}

/// A saturated all-CPU-bound workload: `2 * nr_cpus` forever-running tasks,
/// with the given per-RBC overhead and seed. Oversubscription keeps every CPU
/// busy so scheduler callbacks (and their RBC overhead) fire continuously.
fn saturated(nr_cpus: u32, rbc_ns: Option<u64>, seed: u32) -> Scenario {
    let mut b = Scenario::builder()
        .cpus(nr_cpus)
        .seed(seed)
        .sched_overhead_rbc_ns(rbc_ns);
    for i in 0..(nr_cpus * 2) {
        b = b.add_task(
            &format!("t{i}"),
            0,
            TaskBehavior {
                phases: vec![Phase::Run(50_000_000)],
                repeat: RepeatMode::Forever,
            },
        );
    }
    b.duration_ms(200).build()
}

/// Sum of useful (task) runtime across PIDs `1..=ntasks`. RBC overhead is
/// charged to CPU time *outside* task runtime, so heavier overhead leaves less
/// wall-clock for useful task runtime in a fixed-duration simulation.
fn useful_runtime(trace: &Trace, ntasks: u32) -> u64 {
    (1..=ntasks as i32)
        .map(|p| trace.total_runtime(Pid(p)))
        .sum()
}

/// Number of `TaskScheduled` events — a proxy for how many scheduling
/// decisions the run made.
fn sched_event_count(trace: &Trace) -> usize {
    trace
        .events()
        .iter()
        .filter(|e| matches!(e.kind, TraceKind::TaskScheduled { .. }))
        .count()
}

/// Assert the run finished cleanly and every task in `1..=ntasks` progressed.
fn assert_progress(trace: &Trace, ntasks: u32, ctx: &str) {
    assert_eq!(
        trace.exit_kind(),
        &ExitKind::Normal,
        "{ctx}: simulation did not exit normally"
    );
    for pid in 1..=ntasks as i32 {
        assert!(
            trace.total_runtime(Pid(pid)) > 0,
            "{ctx}: task {pid} got no runtime"
        );
    }
}

// ---------------------------------------------------------------------------
// (1) RBC overhead is a valid, non-fatal config across schedulers.
// ---------------------------------------------------------------------------

/// Enabling RBC-based scheduler overhead must run cleanly under every
/// scheduler that goes through the callback path (the overhead is charged
/// generically from whatever branches the scheduler's C code executes).
#[test]
fn test_rbc_overhead_runs_all_schedulers() {
    let _lock = common::setup_test();
    let nr = 4;
    let scheds: [NamedSchedFactory; 3] = [
        ("simple", |_n| DynamicScheduler::simple()),
        ("lavd", DynamicScheduler::lavd),
        ("cosmos", DynamicScheduler::cosmos),
    ];
    for (name, make) in scheds {
        let trace = Simulator::new(make(nr)).run(saturated(nr, Some(50), 42));
        assert_progress(&trace, nr * 2, &format!("rbc-overhead {name}"));
    }
}

// ---------------------------------------------------------------------------
// (2) RBC threshold of 0 disables counting (byte-identical to None).
// ---------------------------------------------------------------------------

/// `sched_overhead_rbc_ns(Some(0))` is filtered to "disabled" by the engine
/// (`.filter(|&ns| ns > 0)`), so it must be byte-for-byte identical to `None`.
/// This runs entirely on the deterministic (RBC-disabled) engine path, so the
/// assertion is exact and never flaky — it pins the threshold boundary.
#[test]
fn test_rbc_zero_threshold_equals_disabled() {
    let _lock = common::setup_test();
    let nr = 4;
    let none = Simulator::new(DynamicScheduler::simple()).run(saturated(nr, None, 42));
    let zero = Simulator::new(DynamicScheduler::simple()).run(saturated(nr, Some(0), 42));

    assert_eq!(
        none.events().len(),
        zero.events().len(),
        "Some(0) should equal None (disabled): differing event counts"
    );
    for (i, (e1, e2)) in none.events().iter().zip(zero.events().iter()).enumerate() {
        assert!(
            e1.time_ns == e2.time_ns && e1.cpu == e2.cpu && e1.kind == e2.kind,
            "Some(0) diverged from None at event {i}: {:?} vs {:?}",
            e1.kind,
            e2.kind
        );
    }
    assert_eq!(
        useful_runtime(&none, nr * 2),
        useful_runtime(&zero, nr * 2),
        "Some(0) should not perturb useful runtime vs None"
    );
}

// ---------------------------------------------------------------------------
// (3) RBC instruction counting charges CPU time → fewer scheduling decisions.
// ---------------------------------------------------------------------------

/// A very large per-RBC cost makes scheduler overhead dominate: in a
/// fixed-duration run it consumes CPU time that would otherwise go to useful
/// task runtime, so both total useful runtime and the number of scheduling
/// decisions drop. This verifies that RBC counting actually feeds back into the
/// timeline and the scheduling decisions (requirement: "preemption timing
/// affects scheduling decisions correctly").
///
/// PMU-gated: the effect only exists when a real RBC counter is present. The
/// margin (huge ns/rbc) is far larger than the documented ~10% count noise, so
/// the directional assertion is robust.
#[test]
fn test_rbc_overhead_reduces_scheduling_under_pmu() {
    let _lock = common::setup_test();
    let nr = 4;
    let nt = nr * 2;
    let baseline = Simulator::new(DynamicScheduler::simple()).run(saturated(nr, None, 42));
    let heavy = Simulator::new(DynamicScheduler::simple()).run(saturated(nr, Some(100_000), 42));

    assert_progress(&baseline, nt, "baseline");
    assert_progress(&heavy, nt, "heavy-rbc");

    if pmu_available() {
        let rt_base = useful_runtime(&baseline, nt);
        let rt_heavy = useful_runtime(&heavy, nt);
        assert!(
            rt_heavy < rt_base,
            "heavy RBC overhead should reduce useful runtime: base={rt_base} heavy={rt_heavy}"
        );
        // Require a clear (>3%) reduction so ~10% RBC count noise cannot flip it.
        assert!(
            rt_heavy < rt_base - rt_base / 33,
            "heavy RBC overhead should cut useful runtime by a clear margin: \
             base={rt_base} heavy={rt_heavy}"
        );
        assert!(
            sched_event_count(&heavy) < sched_event_count(&baseline),
            "heavy RBC overhead should reduce scheduling decisions: base={} heavy={}",
            sched_event_count(&baseline),
            sched_event_count(&heavy)
        );
    } else {
        eprintln!(
            "PMU unavailable: RBC overhead charges nothing; asserted graceful completion only"
        );
    }
}

// ---------------------------------------------------------------------------
// (4) Different RBC thresholds (small / medium / large).
// ---------------------------------------------------------------------------

/// Sweep several per-RBC costs. Under PMU, larger thresholds impose more
/// overhead, so useful runtime is non-increasing as the threshold grows and the
/// largest threshold is clearly below the disabled baseline. A small tolerance
/// absorbs the documented ~10% RBC read noise at the low-overhead end.
#[test]
fn test_rbc_threshold_sweep() {
    let _lock = common::setup_test();
    let nr = 4;
    let nt = nr * 2;

    let rt = |rbc: Option<u64>| {
        let t = Simulator::new(DynamicScheduler::simple()).run(saturated(nr, rbc, 42));
        assert_progress(&t, nt, &format!("sweep {rbc:?}"));
        (useful_runtime(&t, nt), sched_event_count(&t))
    };

    let (rt_none, ev_none) = rt(None);
    let (rt_mid, _) = rt(Some(1_000));
    let (rt_big, ev_big) = rt(Some(100_000));

    if pmu_available() {
        // Moderate overhead must not *increase* useful runtime beyond noise.
        assert!(
            rt_mid <= rt_none + rt_none / 100,
            "moderate RBC overhead unexpectedly increased useful runtime: \
             none={rt_none} mid={rt_mid}"
        );
        // Large overhead is clearly below both the baseline and the mid point.
        assert!(
            rt_big < rt_mid && rt_big < rt_none - rt_none / 33,
            "large RBC overhead should clearly reduce useful runtime: \
             none={rt_none} mid={rt_mid} big={rt_big}"
        );
        assert!(
            ev_big < ev_none,
            "large RBC overhead should reduce scheduling events: none={ev_none} big={ev_big}"
        );
    } else {
        eprintln!("PMU unavailable: threshold sweep has no timing effect; completion asserted");
    }
}

// ---------------------------------------------------------------------------
// (5) RBC-driven runs are stable (deterministic) across runs with same seed.
// ---------------------------------------------------------------------------

/// Two runs of the same RBC-overhead scenario with the same seed must produce
/// stable results. The RBC-disabled path is asserted byte-exact (pure engine
/// determinism); the RBC-enabled path is asserted within a tight tolerance,
/// because raw PMU-RBC reads carry ~10% run-to-run noise on some hardware
/// (mb sim-70abc8) — asserting byte-equality there would be flaky by design.
#[test]
fn test_rbc_overhead_stable_across_runs() {
    let _lock = common::setup_test();
    let nr = 4;
    let nt = nr * 2;

    // RBC disabled → byte-exact determinism (the deterministic substrate).
    let d1 = Simulator::new(DynamicScheduler::simple()).run(saturated(nr, None, 7));
    let d2 = Simulator::new(DynamicScheduler::simple()).run(saturated(nr, None, 7));
    assert_eq!(
        d1.events().len(),
        d2.events().len(),
        "RBC-disabled runs must be byte-identical (event count)"
    );
    for (i, (e1, e2)) in d1.events().iter().zip(d2.events().iter()).enumerate() {
        assert!(
            e1.time_ns == e2.time_ns && e1.cpu == e2.cpu && e1.kind == e2.kind,
            "RBC-disabled runs diverged at event {i}"
        );
    }

    // RBC enabled → stable within tolerance across runs.
    let r1 = Simulator::new(DynamicScheduler::simple()).run(saturated(nr, Some(1_000), 7));
    let r2 = Simulator::new(DynamicScheduler::simple()).run(saturated(nr, Some(1_000), 7));
    assert_progress(&r1, nt, "rbc-stable run1");
    assert_progress(&r2, nt, "rbc-stable run2");

    let rt1 = useful_runtime(&r1, nt);
    let rt2 = useful_runtime(&r2, nt);
    let hi = rt1.max(rt2);
    let lo = rt1.min(rt2);
    // Within 3%: RBC read noise perturbs only the (~1% of total) overhead term,
    // so total useful runtime is far more stable than the raw counts.
    assert!(
        hi - lo <= hi / 33,
        "RBC-overhead useful runtime not stable across same-seed runs: {rt1} vs {rt2}"
    );
}

// ---------------------------------------------------------------------------
// (6) RBC overhead interacts correctly with voluntary yields and sleeps.
// ---------------------------------------------------------------------------

/// With RBC overhead enabled, tasks that voluntarily sleep/wake (and one that
/// voluntarily exits) must still be scheduled repeatedly and the run must finish
/// cleanly. Each wakeup re-enters enqueue/dispatch callbacks, so RBC overhead is
/// charged on every voluntary transition — this checks that charging that
/// overhead does not stall or starve sleepers/wakers.
#[test]
fn test_rbc_overhead_with_yields_and_sleeps() {
    let _lock = common::setup_test();
    let nr = 2;
    let sched = DynamicScheduler::simple();

    let scenario = Scenario::builder()
        .cpus(nr)
        .seed(42)
        .sched_overhead_rbc_ns(Some(1_000))
        // Waker/wakee pair: repeated voluntary sleeps + cross-task wakes.
        .add_task_with_mm(
            "waker",
            0,
            TaskBehavior {
                phases: vec![
                    Phase::Run(3_000_000),
                    Phase::Wake(Pid(2)),
                    Phase::Sleep(3_000_000),
                ],
                repeat: RepeatMode::Forever,
            },
            MmId(1),
        )
        .add_task_with_mm(
            "wakee",
            0,
            TaskBehavior {
                phases: vec![Phase::Run(3_000_000), Phase::Sleep(15_000_000)],
                repeat: RepeatMode::Forever,
            },
            MmId(1),
        )
        // A periodic sleeper (voluntary sleeps every cycle).
        .add_task(
            "sleeper",
            0,
            TaskBehavior {
                phases: vec![Phase::Run(2_000_000), Phase::Sleep(4_000_000)],
                repeat: RepeatMode::Forever,
            },
        )
        // A task that voluntarily exits after a few cycles (voluntary yield).
        .add_task(
            "finisher",
            0,
            TaskBehavior {
                phases: vec![Phase::Run(2_000_000), Phase::Sleep(2_000_000)],
                repeat: RepeatMode::Count(3),
            },
        )
        .duration_ms(200)
        .build();

    let trace = Simulator::new(sched).run(scenario);

    assert_eq!(
        trace.exit_kind(),
        &ExitKind::Normal,
        "sleep/yield workload with RBC overhead did not exit normally"
    );
    // Waker, wakee, and sleeper cycle repeatedly and must be scheduled many times.
    for (pid, name) in [(1, "waker"), (2, "wakee"), (3, "sleeper")] {
        assert!(
            trace.schedule_count(Pid(pid)) >= 3,
            "{name} scheduled only {} times under RBC overhead",
            trace.schedule_count(Pid(pid))
        );
    }
    // The finisher ran its 3 cycles and made progress (voluntary exit path).
    assert!(
        trace.total_runtime(Pid(4)) > 0,
        "finisher got no runtime under RBC overhead"
    );

    // Sanity: voluntary sleeps and wakes actually occurred in the trace.
    let s = trace.summary();
    assert!(s.total_sleeps > 0, "expected voluntary sleeps in the trace");
    assert!(s.total_wakes > 0, "expected wakeups in the trace");
}
