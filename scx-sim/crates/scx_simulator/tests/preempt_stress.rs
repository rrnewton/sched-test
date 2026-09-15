//! Stress tests for PREEMPTION behavior across schedulers.
//!
//! Covers the five preemption stress dimensions from the task plan, using only
//! DETERMINISTIC, in-process mechanisms so the suite is CI-green and flake-free:
//!
//! 1. High-frequency preemption (simple + lavd) — many tasks contending for
//!    few CPUs, via default serial slice/tick preemption.
//! 2. Preemption at different points (simple + lavd) — cooperative-preemptive
//!    mode yields at kfunc boundaries, not only at slice expiry; verified
//!    deterministic per seed.
//! 3. Priority inversion (simple + lavd) — a higher-priority scheduling class
//!    (RT/DL) seizing the CPU via `cpu_preempt()`, plus nice/weight ordering
//!    under contention.
//! 4. Nested cgroup preemption (lavd) — bandwidth-limited parent with nested
//!    children under LAVD's real `cgroup_bw` library.
//! 5. Both `simple` and `lavd` are exercised throughout.
//!
//! Deterministic mechanisms only. We deliberately AVOID the flaky / build- or
//! hardware-dependent preemption backends (real-PMU `PreemptMode::Pmu`,
//! `PreemptMode::E9patch` which needs `_e9.so` variants, `native_concurrent`
//! OS-thread mode, and HW-breakpoint replay) — those cannot be asserted as
//! deterministic in CI. Preemption is verified via observable trace events
//! (`TaskPreempted`, `preempt_count`, cgroup-bw TraceKinds) only, per the
//! No-Stub / model-the-kernel rules.

use scx_simulator::*;

mod common;

/// Forever-running CPU-bound behavior.
fn forever_run(run_ns: u64) -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(run_ns)],
        repeat: RepeatMode::Forever,
    }
}

/// Count `TaskPreempted` events in a trace.
fn count_preemptions(trace: &Trace) -> usize {
    trace
        .events()
        .iter()
        .filter(|e| matches!(e.kind, TraceKind::TaskPreempted { .. }))
        .count()
}

/// Assert two traces are event-for-event identical (determinism / no-flake).
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

// ---------------------------------------------------------------------------
// 1. High-frequency preemption: many tasks, few CPUs (slice/tick preemption).
// ---------------------------------------------------------------------------

/// Build N forever-hog tasks on `nr_cpus` CPUs with deterministic timing.
fn high_freq_scenario(nr_cpus: u32, nr_tasks: u32, duration_ms: u64) -> Scenario {
    let mut builder = Scenario::builder().cpus(nr_cpus).seed(42).instant_timing();
    for i in 1..=nr_tasks {
        builder = builder.add_task(&format!("hog{i}"), 0, forever_run(100_000_000));
    }
    builder.duration_ms(duration_ms).build()
}

#[test]
fn stress_high_freq_preemption_simple() {
    let _lock = common::setup_test();
    let nr_tasks = 16u32;
    let scenario = high_freq_scenario(2, nr_tasks, 400);

    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario);
    trace.dump();

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    assert!(
        !trace.has_error(),
        "unexpected BPF error: {:?}",
        trace.exit_kind()
    );

    let preempts = count_preemptions(&trace);
    eprintln!("simple high-freq: {preempts} preemptions across {nr_tasks} tasks / 2 CPUs");
    // 16 tasks on 2 CPUs over 400ms with a 20ms slice => many slice-expiry
    // preemptions. Assert a healthy count with generous margin.
    assert!(
        preempts >= 10,
        "expected heavy slice preemption, got {preempts}"
    );

    // No task is starved out entirely.
    for pid in 1..=nr_tasks as i32 {
        assert!(
            trace.schedule_count(Pid(pid)) > 0,
            "task pid={pid} was never scheduled"
        );
    }
}

#[test]
fn stress_high_freq_preemption_lavd() {
    let _lock = common::setup_test();
    let nr_tasks = 12u32;
    let scenario = high_freq_scenario(2, nr_tasks, 400);

    let trace = Simulator::new(DynamicScheduler::lavd(2)).run(scenario);
    trace.dump();

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    assert!(
        !trace.has_error(),
        "unexpected BPF error: {:?}",
        trace.exit_kind()
    );

    let preempts = count_preemptions(&trace);
    eprintln!("lavd high-freq: {preempts} preemptions across {nr_tasks} tasks / 2 CPUs");
    // LAVD preempts via tick + slice + vtime; assert it happens under contention.
    assert!(
        preempts > 0,
        "expected LAVD preemption under contention, got {preempts}"
    );

    for pid in 1..=nr_tasks as i32 {
        assert!(
            trace.schedule_count(Pid(pid)) > 0,
            "task pid={pid} was never scheduled"
        );
    }
}

/// High-frequency preemption must be fully deterministic (no flakes): the same
/// scenario run twice yields byte-identical event streams.
#[test]
fn stress_high_freq_preemption_determinism() {
    let _lock = common::setup_test();

    let t1 = Simulator::new(DynamicScheduler::simple()).run(high_freq_scenario(2, 16, 300));
    let t2 = Simulator::new(DynamicScheduler::simple()).run(high_freq_scenario(2, 16, 300));
    assert_identical(&t1, &t2, "simple high-freq");

    let l1 = Simulator::new(DynamicScheduler::lavd(2)).run(high_freq_scenario(2, 12, 300));
    let l2 = Simulator::new(DynamicScheduler::lavd(2)).run(high_freq_scenario(2, 12, 300));
    assert_identical(&l1, &l2, "lavd high-freq");
}

// ---------------------------------------------------------------------------
// 2. Preemption at different points: cooperative-preemptive mode.
//    Yields at kfunc boundaries (not just slice expiry). Deterministic/seed.
// ---------------------------------------------------------------------------

fn cooperative_preemptive_scenario(
    nr_cpus: u32,
    nr_tasks: u32,
    seed: u32,
    duration_ms: u64,
) -> Scenario {
    let mut builder = Scenario::builder()
        .cpus(nr_cpus)
        .seed(seed)
        .fixed_priority(true)
        .instant_timing()
        .preemptive(PreemptiveConfig::cooperative_only());
    for i in 1..=nr_tasks {
        builder = builder.add_task(&format!("t{i}"), 0, forever_run(10_000_000));
    }
    builder.duration_ms(duration_ms).build()
}

#[test]
fn stress_cooperative_preemptive_determinism_simple() {
    let _lock = common::setup_test();
    // Determinism must hold for every seed (this is the no-flake guarantee for
    // the "preempt at arbitrary points" mode).
    for seed in [1u32, 7, 42, 1000] {
        let make = || cooperative_preemptive_scenario(2, 8, seed, 200);
        let t1 = Simulator::new(DynamicScheduler::simple()).run(make());
        let t2 = Simulator::new(DynamicScheduler::simple()).run(make());
        assert_identical(&t1, &t2, &format!("simple coop-preemptive seed={seed}"));
        assert_eq!(t1.exit_kind(), &ExitKind::Normal);
        for pid in 1..=8i32 {
            assert!(
                t1.schedule_count(Pid(pid)) > 0,
                "seed={seed}: pid={pid} never scheduled"
            );
        }
    }
}

#[test]
fn stress_cooperative_preemptive_lavd() {
    let _lock = common::setup_test();
    let make = || cooperative_preemptive_scenario(2, 6, 42, 200);

    let t1 = Simulator::new(DynamicScheduler::lavd(2)).run(make());
    t1.dump();
    assert_eq!(t1.exit_kind(), &ExitKind::Normal);
    assert!(
        !t1.has_error(),
        "lavd coop-preemptive error: {:?}",
        t1.exit_kind()
    );
    for pid in 1..=6i32 {
        assert!(
            t1.schedule_count(Pid(pid)) > 0,
            "lavd coop-preemptive: pid={pid} never scheduled"
        );
    }

    // Determinism under cooperative-preemptive mode with LAVD.
    let t2 = Simulator::new(DynamicScheduler::lavd(2)).run(make());
    assert_identical(&t1, &t2, "lavd coop-preemptive");
}

// ---------------------------------------------------------------------------
// 3. Priority inversion: a higher-priority class seizes the CPU via
//    cpu_preempt() (ops.cpu_release/cpu_acquire), plus nice ordering.
// ---------------------------------------------------------------------------

/// Two normal forever tasks on 1 CPU, with a higher-priority scheduling class
/// (modeled by `cpu_preempt`) repeatedly seizing the CPU. Each seize forces the
/// running normal task off-CPU (a preemption); tasks must resume and keep making
/// progress after the class releases the CPU.
fn cpu_preempt_scenario(nr_windows: u64) -> (Scenario, u64) {
    let period_ns = 40_000_000u64; // RT class grabs the CPU every 40ms ...
    let hold_ns = 10_000_000u64; //   ... and holds it for 10ms.
    let mut builder = Scenario::builder()
        .cpus(1)
        .seed(42)
        .instant_timing()
        .add_task("normal_a", 0, forever_run(100_000_000))
        .add_task("normal_b", 0, forever_run(100_000_000));
    for k in 0..nr_windows {
        let release = 20_000_000 + k * period_ns;
        let acquire = release + hold_ns;
        builder = builder.cpu_preempt(CpuId(0), release, acquire);
    }
    let duration_ms = (20 + nr_windows * (period_ns / 1_000_000)) + 40;
    (builder.duration_ms(duration_ms).build(), nr_windows)
}

#[test]
fn stress_priority_inversion_cpu_preempt_simple() {
    let _lock = common::setup_test();
    let nr_windows = 6u64;
    let (scenario, windows) = cpu_preempt_scenario(nr_windows);

    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario);
    trace.dump();

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    assert!(!trace.has_error());

    // Each higher-class seize (cpu_release) preempts the running task, so the
    // total preemption count must be at least the number of seize windows.
    let preempts = count_preemptions(&trace);
    eprintln!("simple cpu_preempt: {preempts} preemptions for {windows} RT-class windows");
    assert!(
        preempts as u64 >= windows,
        "expected >= {windows} preemptions from the higher-priority class, got {preempts}"
    );

    // Both normal tasks kept making progress (no starvation / priority inversion
    // deadlock) across the seize windows.
    assert!(trace.total_runtime(Pid(1)) > 0 && trace.total_runtime(Pid(2)) > 0);
}

#[test]
fn stress_priority_inversion_cpu_preempt_lavd() {
    let _lock = common::setup_test();
    let nr_windows = 6u64;
    let (scenario, windows) = cpu_preempt_scenario(nr_windows);

    let trace = Simulator::new(DynamicScheduler::lavd(1)).run(scenario);
    trace.dump();

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    assert!(!trace.has_error());

    let preempts = count_preemptions(&trace);
    eprintln!("lavd cpu_preempt: {preempts} preemptions for {windows} RT-class windows");
    assert!(
        preempts as u64 >= windows,
        "expected >= {windows} preemptions from the higher-priority class, got {preempts}"
    );
    assert!(trace.total_runtime(Pid(1)) > 0 && trace.total_runtime(Pid(2)) > 0);
}

/// Classic priority ordering under contention: a low-nice (high-priority) task
/// must not be starved by, and should out-run, a high-nice (low-priority) task
/// competing for the same CPU. Guards against priority-inversion where the
/// low-priority task hogs the CPU.
#[test]
fn stress_priority_inversion_weight_lavd() {
    let _lock = common::setup_test();
    let scenario = Scenario::builder()
        .cpus(1)
        .seed(42)
        .add_task("high_prio", -10, forever_run(100_000_000))
        .add_task("low_prio", 10, forever_run(100_000_000))
        .duration_ms(400)
        .build();

    let trace = Simulator::new(DynamicScheduler::lavd(1)).run(scenario);
    trace.dump();

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    let hi = trace.total_runtime(Pid(1));
    let lo = trace.total_runtime(Pid(2));
    eprintln!("lavd priority: high_prio(nice -10)={hi}ns, low_prio(nice 10)={lo}ns");
    assert!(hi > 0 && lo > 0, "both tasks should run (no starvation)");
    assert!(
        hi >= lo,
        "high-priority (nice -10) task must not be out-run by low-priority (nice 10): hi={hi} lo={lo}"
    );
}

// ---------------------------------------------------------------------------
// 4. Nested cgroup preemption interactions (LAVD cgroup_bw).
// ---------------------------------------------------------------------------

/// Nested cgroup hierarchy with a bandwidth-limited parent, run under LAVD.
/// Tasks in the nested children consume the parent's cpu.max quota; LAVD's real
/// `cgroup_bw` library must charge/enforce it. Verify the nested hierarchy is
/// accepted, preemption occurs, the cgroup-bw path actually runs, and the run
/// stays healthy (no stall/error).
#[test]
fn stress_nested_cgroup_preemption_lavd() {
    let _lock = common::setup_test();
    let scenario = Scenario::builder()
        .cpus(2)
        .seed(42)
        .instant_timing()
        // Bandwidth-limited parent (50% of one period across the cpuset) ...
        .cgroup_with_bandwidth("parent", &[CpuId(0), CpuId(1)], 100_000, 50_000, 0)
        // ... with two nested children under it.
        .cgroup_nested("child_a", "parent", None)
        .cgroup_nested("child_b", "parent", None)
        .add_task_in_cgroup("a1", 0, forever_run(100_000_000), "child_a")
        .add_task_in_cgroup("a2", 0, forever_run(100_000_000), "child_a")
        .add_task_in_cgroup("b1", 0, forever_run(100_000_000), "child_b")
        .duration_ms(300)
        .build();

    let sched = DynamicScheduler::lavd(2);
    sched.lavd_set_cgroup_bw_max(16);
    let trace = Simulator::new(sched).run(scenario);
    trace.dump();

    assert_eq!(
        trace.exit_kind(),
        &ExitKind::Normal,
        "nested-cgroup run should not stall"
    );
    assert!(
        !trace.has_error(),
        "unexpected BPF error: {:?}",
        trace.exit_kind()
    );

    // All tasks in the nested children were scheduled.
    for pid in 1..=3i32 {
        assert!(
            trace.schedule_count(Pid(pid)) > 0,
            "cgroup task pid={pid} was never scheduled"
        );
    }

    // Preemption occurred under the nested-cgroup workload.
    let preempts = count_preemptions(&trace);
    assert!(
        preempts > 0,
        "expected preemption under nested-cgroup contention, got {preempts}"
    );

    // LAVD's real cgroup_bw library actually ran (charge / consume / enforcement
    // events present) — proves we exercised the bandwidth path, not a no-op.
    let cbw_events = trace
        .events()
        .iter()
        .filter(|e| {
            matches!(
                e.kind,
                TraceKind::CgroupBwCharge { .. }
                    | TraceKind::CgroupBwConsumeNs { .. }
                    | TraceKind::CgroupBwReplenish { .. }
                    | TraceKind::CgroupBwDequeueOnThrottle { .. }
                    | TraceKind::CgroupBwReenqueueOnReplenish { .. }
                    | TraceKind::LavdBailOnCgroupThrottle { .. }
            )
        })
        .count();
    eprintln!("nested-cgroup lavd: {preempts} preemptions, {cbw_events} cgroup_bw events");
    assert!(
        cbw_events > 0,
        "expected LAVD cgroup_bw activity for a bandwidth-limited cgroup, got {cbw_events}"
    );
}

/// Determinism guard for the nested-cgroup preemption scenario (no flakes).
#[test]
fn stress_nested_cgroup_preemption_determinism() {
    let _lock = common::setup_test();
    let make = || {
        Scenario::builder()
            .cpus(2)
            .seed(42)
            .instant_timing()
            .cgroup_with_bandwidth("parent", &[CpuId(0), CpuId(1)], 100_000, 50_000, 0)
            .cgroup_nested("child_a", "parent", None)
            .cgroup_nested("child_b", "parent", None)
            .add_task_in_cgroup("a1", 0, forever_run(100_000_000), "child_a")
            .add_task_in_cgroup("b1", 0, forever_run(100_000_000), "child_b")
            .duration_ms(200)
            .build()
    };

    let s1 = DynamicScheduler::lavd(2);
    s1.lavd_set_cgroup_bw_max(16);
    let t1 = Simulator::new(s1).run(make());

    let s2 = DynamicScheduler::lavd(2);
    s2.lavd_set_cgroup_bw_max(16);
    let t2 = Simulator::new(s2).run(make());

    assert_identical(&t1, &t2, "nested-cgroup lavd");
}
