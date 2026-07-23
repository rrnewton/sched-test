//! Tests for simulator CONFIGURATION options and their effects.
//!
//! Focused on config surface not owned by other suites: invalid-configuration
//! rejection (the `ScenarioBuilder::build()` / builder validation asserts),
//! default-configuration sanity, and config→effect relationships. Watchdog
//! config lives in `errors.rs`, noise/timing config in `noise.rs`,
//! cross-scheduler determinism in `determinism.rs`, and topology sweeps in
//! `topology.rs` — this file deliberately does not duplicate those.
//!
//! All runs use the deterministic serial engine.

use scx_simulator::*;

mod common;

fn run_once(run_ns: u64) -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(run_ns)],
        repeat: RepeatMode::Once,
    }
}

fn forever_run(run_ns: u64) -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(run_ns)],
        repeat: RepeatMode::Forever,
    }
}

// ---------------------------------------------------------------------------
// Invalid configuration rejection (builder / build() validation).
// ---------------------------------------------------------------------------

#[test]
#[should_panic(expected = "at least one task")]
fn test_reject_no_tasks() {
    let _lock = common::setup_test();
    // No task added.
    let _ = Scenario::builder().cpus(1).build();
}

#[test]
#[should_panic(expected = "at least one CPU")]
fn test_reject_zero_cpus() {
    let _lock = common::setup_test();
    let _ = Scenario::builder()
        .cpus(0)
        .add_task("t", 0, run_once(1_000_000))
        .build();
}

#[test]
#[should_panic(expected = "smt_threads_per_core must be at least 1")]
fn test_reject_zero_smt() {
    let _lock = common::setup_test();
    let _ = Scenario::builder()
        .cpus(2)
        .add_task("t", 0, run_once(1_000_000))
        .smt(0)
        .build();
}

#[test]
#[should_panic(expected = "divisible by smt_threads_per_core")]
fn test_reject_cpus_not_divisible_by_smt() {
    let _lock = common::setup_test();
    // 3 CPUs cannot be split into cores of 2 SMT threads.
    let _ = Scenario::builder()
        .cpus(3)
        .add_task("t", 0, run_once(1_000_000))
        .smt(2)
        .build();
}

#[test]
#[should_panic(expected = "divisible by cpus_per_llc")]
fn test_reject_cpus_not_divisible_by_llc() {
    let _lock = common::setup_test();
    // 6 CPUs cannot be split into LLCs of 4.
    let _ = Scenario::builder()
        .cpus(6)
        .add_task("t", 0, run_once(1_000_000))
        .cpus_per_llc(4)
        .build();
}

#[test]
#[should_panic(expected = "mutually exclusive")]
fn test_reject_preemptive_and_native_concurrent() {
    let _lock = common::setup_test();
    // Two contradictory execution modes at once.
    let _ = Scenario::builder()
        .cpus(2)
        .add_task("t", 0, forever_run(10_000_000))
        .preemptive(PreemptiveConfig::cooperative_only())
        .native_concurrent(NativeConcurrentConfig::default())
        .build();
}

#[test]
#[should_panic]
fn test_reject_cpu_preempt_acquire_not_after_release() {
    let _lock = common::setup_test();
    // acquire must be strictly after release; this asserts in the builder.
    let _ = Scenario::builder()
        .cpus(1)
        .add_task("t", 0, forever_run(10_000_000))
        .cpu_preempt(CpuId(0), 100_000_000, 50_000_000);
}

// ---------------------------------------------------------------------------
// Default configuration is sane.
// ---------------------------------------------------------------------------

/// The builder's documented defaults (1 CPU, no SMT, single LLC, seed 42,
/// 100ms duration) must hold, and a default-configured run of a trivial
/// workload must complete Normally.
#[test]
fn test_default_config_values_and_sane_run() {
    let _lock = common::setup_test();
    let scenario = Scenario::builder()
        .add_task("worker", 0, run_once(5_000_000))
        .build();

    // Documented defaults.
    assert_eq!(scenario.nr_cpus, 1, "default nr_cpus");
    assert_eq!(scenario.smt_threads_per_core, 1, "default smt");
    assert_eq!(
        scenario.cpus_per_llc, 0,
        "default cpus_per_llc (single LLC)"
    );
    assert_eq!(scenario.seed, 42, "default seed (DEFAULT_SEED)");
    assert_eq!(scenario.duration_ns, 100_000_000, "default duration 100ms");

    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario);
    trace.dump();
    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    assert!(
        trace.schedule_count(Pid(1)) > 0,
        "default run scheduled nothing"
    );
    // Under the DEFAULT config, noise + overhead are ON (unlike instant_timing),
    // so accounted runtime is slightly below the 5ms of requested work — assert
    // the task ran and completed rather than an exact figure.
    assert!(trace.total_runtime(Pid(1)) > 0, "task got no runtime");
    assert!(
        trace
            .events()
            .iter()
            .any(|e| matches!(e.kind, TraceKind::TaskCompleted { pid } if pid == Pid(1))),
        "default-config task did not complete"
    );
}

// ---------------------------------------------------------------------------
// Config -> effect.
// ---------------------------------------------------------------------------

/// CPU-count is a config knob with a real effect: with more CPUs, independent
/// CPU-bound tasks run in parallel, so aggregate runtime over the same wall
/// window is substantially higher.
#[test]
fn test_cpu_count_increases_parallelism() {
    let _lock = common::setup_test();
    let nr_tasks = 4u32;
    let agg = |nr_cpus: u32| -> u64 {
        let mut b = Scenario::builder().cpus(nr_cpus).seed(42).instant_timing();
        for i in 1..=nr_tasks {
            b = b.add_task(&format!("t{i}"), 0, forever_run(100_000_000));
        }
        let trace = Simulator::new(DynamicScheduler::simple()).run(b.duration_ms(200).build());
        assert_eq!(trace.exit_kind(), &ExitKind::Normal);
        (1..=nr_tasks as i32)
            .map(|p| trace.total_runtime(Pid(p)))
            .sum()
    };

    let one = agg(1);
    let four = agg(4);
    eprintln!("aggregate runtime: 1 CPU={one}ns, 4 CPUs={four}ns");
    // Four CPUs should complete clearly more aggregate work than one.
    assert!(
        four >= one * 2,
        "expected 4 CPUs to yield much more aggregate runtime than 1: 4cpu={four} 1cpu={one}"
    );
}

/// The RNG seed is a config knob: the same seed reproduces a byte-identical
/// trace, and varying the seed still produces valid (Normal) runs. Uses the
/// `simple` scheduler (no cgroup_bw global state) for stable determinism.
#[test]
fn test_seed_config_determinism_and_variation() {
    let _lock = common::setup_test();
    let build = |seed: u32| {
        Scenario::builder()
            .cpus(2)
            .seed(seed)
            .instant_timing()
            .add_task("a", 0, forever_run(7_000_000))
            .add_task("b", -3, forever_run(9_000_000))
            .add_task("c", 3, forever_run(5_000_000))
            .duration_ms(150)
            .build()
    };

    // Same seed -> identical trace.
    let t1 = Simulator::new(DynamicScheduler::simple()).run(build(42));
    let t2 = Simulator::new(DynamicScheduler::simple()).run(build(42));
    assert_eq!(
        t1.events().len(),
        t2.events().len(),
        "same seed differs in length"
    );
    for (i, (e1, e2)) in t1.events().iter().zip(t2.events().iter()).enumerate() {
        assert_eq!(
            (e1.time_ns, e1.cpu, &e1.kind),
            (e2.time_ns, e2.cpu, &e2.kind),
            "same seed: event {i} differs"
        );
    }

    // Varying the seed must not break anything.
    for seed in [1u32, 7, 42, 99, 123_456] {
        let trace = Simulator::new(DynamicScheduler::simple()).run(build(seed));
        assert_eq!(
            trace.exit_kind(),
            &ExitKind::Normal,
            "seed {seed} did not exit Normal"
        );
        for p in 1..=3i32 {
            assert!(
                trace.schedule_count(Pid(p)) > 0,
                "seed {seed}: task {p} never ran"
            );
        }
    }
}

/// Scheduler-specific knobs: LAVD's `lavd_configure(per_cpu_dsq, pinned_slice_ns,
/// mig_delta_pct)` combinations and every power mode must be accepted and
/// produce a valid Normal run (config-robustness matrix). `pinned_slice_ns` is
/// LAVD's configurable timeslice knob.
#[test]
fn test_lavd_config_knob_matrix() {
    let _lock = common::setup_test();
    let scenario = || {
        Scenario::builder()
            .cpus(4)
            .seed(42)
            .instant_timing()
            .add_task("h1", 0, forever_run(50_000_000))
            .add_task("h2", 0, forever_run(50_000_000))
            .add_task("h3", -2, forever_run(50_000_000))
            .duration_ms(150)
            .build()
    };

    for per_cpu_dsq in [false, true] {
        for pinned_slice_ns in [0u64, 2_000_000] {
            for mig_delta_pct in [0u8, 20] {
                let sched = DynamicScheduler::lavd(4);
                sched.lavd_configure(per_cpu_dsq, pinned_slice_ns, mig_delta_pct);
                let trace = Simulator::new(sched).run(scenario());
                assert_eq!(
                    trace.exit_kind(),
                    &ExitKind::Normal,
                    "lavd_configure({per_cpu_dsq}, {pinned_slice_ns}, {mig_delta_pct}) did not exit Normal"
                );
                assert!(!trace.has_error());
            }
        }
    }

    // Power-mode knob: all three modes must run cleanly.
    for mode in [
        LavdPowerMode::Performance,
        LavdPowerMode::Balanced,
        LavdPowerMode::Powersave,
    ] {
        let sched = DynamicScheduler::lavd(4);
        sched.lavd_set_power_mode(mode);
        let trace = Simulator::new(sched).run(scenario());
        assert_eq!(
            trace.exit_kind(),
            &ExitKind::Normal,
            "power mode {mode:?} did not exit Normal"
        );
    }
}
