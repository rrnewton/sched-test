//! Scheduler lifecycle / "switching" tests (tg test-scheduler-switching).
//!
//! **scxsim does not hot-swap schedulers mid-run.** `Simulator::new(sched)`
//! takes ownership of a single scheduler for the entire `run()` (see
//! `engine.rs`), and there is no `swap`/`switch`/`set_scheduler` API. Scheduler
//! "switching" is therefore modeled as SEQUENTIAL runs. Each `DynamicScheduler`
//! owns its `.so` via `libloading`: on construction it `dlopen`s the library
//! and calls `{prefix}_setup()` to (re)initialize every `const volatile` global
//! and BPF map; on `Drop` it `dlclose`s the library (`ffi.rs`). So each run is
//! meant to be fully self-contained.
//!
//! These tests verify that lifecycle is correct and leak-free:
//!   1. Different schedulers run back-to-back on the same workload, each clean.
//!   2. Global / BPF-map state set on one instance does NOT leak into a later
//!      fresh instance of the same scheduler.
//!   3. Many load→run→drop cycles produce byte-identical results (no drift /
//!      state accumulation / resource leak that would perturb the outcome).
//!   4. Interleaving different schedulers keeps each independent (loading and
//!      tearing down scheduler B between two A runs does not corrupt A).
//!
//! Determinism per run is a prerequisite for (2)-(4); it is guaranteed by the
//! seeded engine (see `determinism.rs`), so every scenario here pins an
//! explicit seed and the signatures are compared for exact equality.

use scx_simulator::*;

#[macro_use]
mod common;

/// Fixed seed so every run of the same (scheduler, scenario) is byte-identical,
/// making cross-run equality a valid leak/drift probe.
const SEED: u32 = 0x5eed_1234;

/// A saturating bursty workload (demand > CPUs) so scheduler placement /
/// queueing paths run and produce a rich, discriminating signature.
fn workload(nr_cpus: u32, nr_tasks: u32) -> Scenario {
    let mut b = Scenario::builder().cpus(nr_cpus).seed(SEED);
    for i in 0..nr_tasks {
        b = b.add_task(
            &format!("t{i}"),
            0,
            TaskBehavior {
                phases: vec![Phase::Run(8_000_000), Phase::Sleep(1_000_000)],
                repeat: RepeatMode::Forever,
            },
        );
    }
    b.duration_ms(200).build()
}

/// A compact, fully-deterministic fingerprint of a run's observable outcome.
/// Contains only logical quantities (runtimes, schedule counts, DSQ dispatch
/// split, exit status) — no addresses — so it is stable across load/drop cycles
/// for a fixed seed and is a faithful proxy for "did this run behave the same".
#[derive(PartialEq, Eq, Debug, Clone)]
struct RunSig {
    runtimes: Vec<u64>,
    sched_counts: Vec<usize>,
    dsq: (usize, usize),
    normal_exit: bool,
    had_error: bool,
}

fn signature(trace: &Trace, nr_tasks: u32) -> RunSig {
    RunSig {
        runtimes: (1..=nr_tasks as i32)
            .map(|p| trace.total_runtime(Pid(p)))
            .collect(),
        sched_counts: (1..=nr_tasks as i32)
            .map(|p| trace.schedule_count(Pid(p)))
            .collect(),
        dsq: trace.dsq_dispatch_counts(),
        normal_exit: *trace.exit_kind() == ExitKind::Normal,
        had_error: trace.has_error(),
    }
}

/// Assert a signature reflects a clean, productive run.
fn assert_healthy(sig: &RunSig, ctx: &str) {
    assert!(sig.normal_exit, "[{ctx}] did not exit Normal");
    assert!(!sig.had_error, "[{ctx}] scheduler raised scx_bpf_error");
    assert!(
        sig.runtimes.iter().all(|&r| r > 0),
        "[{ctx}] some task got no runtime: {:?}",
        sig.runtimes
    );
}

// ============================================================================
// 1. Sequential different schedulers on the same workload
// ============================================================================

/// Load each supported scheduler in turn, run it on the *same* workload spec,
/// and drop it before loading the next. This is scxsim's stand-in for
/// "switching schedulers": a clean load → `{prefix}_setup()` → run → `Drop`
/// (dlclose) for every scheduler, back-to-back in one process. Each must
/// complete cleanly with every task making progress.
#[test]
fn test_sequential_schedulers_same_workload() {
    let _lock = common::setup_test();
    let nr_cpus = 4u32;
    let nr_tasks = 8u32;

    // simple() is always single-CPU-configured but runs on any scenario.
    type MakeSched = fn(u32) -> DynamicScheduler;
    let cases: &[(&str, MakeSched)] = &[
        ("simple", |_n| DynamicScheduler::simple()),
        ("lavd", DynamicScheduler::lavd),
        ("cosmos", DynamicScheduler::cosmos),
        ("mitosis", DynamicScheduler::mitosis),
        ("tickless", DynamicScheduler::tickless),
    ];

    for &(name, make) in cases {
        let sched = make(nr_cpus);
        let trace = Simulator::new(sched).run(workload(nr_cpus, nr_tasks));
        // `sched` is moved into the Simulator and dropped at end of iteration,
        // dlclosing the .so before the next scheduler loads.
        let sig = signature(&trace, nr_tasks);
        assert_healthy(&sig, name);
    }
}

// ============================================================================
// 2. No global / BPF-map state leak between runs
// ============================================================================

/// A run's scheduler-side state must not bleed into the next run. We drive
/// COSMOS into deadline mode by populating `cpu_util_map` (via
/// `cosmos_set_cpu_util`) so `is_cpu_busy()` is true and tasks route through the
/// global (vtime) DSQ — an observable behavior change (global dispatch count
/// jumps). A fresh COSMOS loaded *after* that knobbed run must behave exactly
/// like the baseline (global dispatch count back to the default), proving
/// `cosmos_setup()` + `Drop`/dlclose fully re-initialize the BPF map / globals.
#[test]
fn test_no_state_leak_between_cosmos_runs() {
    let _lock = common::setup_test();
    let (nr_cpus, nr_tasks) = (2u32, 8u32);

    let run_default = || {
        let s = DynamicScheduler::cosmos(nr_cpus);
        signature(
            &Simulator::new(s).run(workload(nr_cpus, nr_tasks)),
            nr_tasks,
        )
    };

    let baseline = run_default();

    let knobbed = {
        let s = DynamicScheduler::cosmos(nr_cpus);
        s.cosmos_set_cpu_util(nr_cpus, 1024); // report saturated → deadline mode
        signature(
            &Simulator::new(s).run(workload(nr_cpus, nr_tasks)),
            nr_tasks,
        )
    };

    let after = run_default();

    assert_healthy(&baseline, "cosmos baseline");
    assert_healthy(&knobbed, "cosmos knobbed");
    assert_healthy(&after, "cosmos after-knob");

    // The knob must genuinely change behavior, otherwise the leak test is vacuous.
    assert_ne!(
        knobbed.dsq, baseline.dsq,
        "cpu_util knob did not change the global/local DSQ split — probe is not discriminating"
    );
    assert!(
        baseline.dsq.0 == 0 && knobbed.dsq.0 > 0,
        "expected baseline global-DSQ=0 and knobbed global-DSQ>0, got baseline={:?} knobbed={:?}",
        baseline.dsq,
        knobbed.dsq
    );

    // The core guarantee: a fresh run after the knobbed one is identical to the
    // pristine baseline — no leaked cpu_util_map / global state.
    assert_eq!(
        after, baseline,
        "fresh cosmos run inherited state from the prior knobbed run (state leak)"
    );
}

// ============================================================================
// 3. Repeated load/drop cycles are stable (no drift / resource leak)
// ============================================================================

/// A resource or state leak across the load→run→drop cycle would show up as
/// either a crash after many cycles or as the result drifting from the first
/// run. Run the same scheduler through many cycles and assert every cycle
/// reproduces the first run's signature exactly.
#[test]
fn test_repeated_load_drop_no_drift() {
    let _lock = common::setup_test();
    let (nr_cpus, nr_tasks) = (4u32, 8u32);
    const CYCLES: usize = 25;

    let first = signature(
        &Simulator::new(DynamicScheduler::cosmos(nr_cpus)).run(workload(nr_cpus, nr_tasks)),
        nr_tasks,
    );
    assert_healthy(&first, "cosmos cycle 0");

    for cycle in 1..=CYCLES {
        let sig = signature(
            &Simulator::new(DynamicScheduler::cosmos(nr_cpus)).run(workload(nr_cpus, nr_tasks)),
            nr_tasks,
        );
        assert_eq!(
            sig, first,
            "cosmos load/drop cycle {cycle} drifted from cycle 0 (state leak or nondeterminism)"
        );
    }
}

// ============================================================================
// 4. Interleaved different schedulers stay independent
// ============================================================================

/// Loading and tearing down scheduler B between two runs of scheduler A must
/// not perturb A (each `.so` is an independent mapping, and each run re-inits
/// via `{prefix}_setup()`). Establish per-scheduler baselines, then interleave
/// cosmos and lavd runs and assert each still matches its own baseline.
#[test]
fn test_interleaved_schedulers_independent() {
    let _lock = common::setup_test();
    let (nr_cpus, nr_tasks) = (4u32, 8u32);

    let cosmos_sig = || {
        signature(
            &Simulator::new(DynamicScheduler::cosmos(nr_cpus)).run(workload(nr_cpus, nr_tasks)),
            nr_tasks,
        )
    };
    let lavd_sig = || {
        signature(
            &Simulator::new(DynamicScheduler::lavd(nr_cpus)).run(workload(nr_cpus, nr_tasks)),
            nr_tasks,
        )
    };

    let cosmos_base = cosmos_sig();
    let lavd_base = lavd_sig();
    assert_healthy(&cosmos_base, "cosmos base");
    assert_healthy(&lavd_base, "lavd base");
    // Different schedulers should produce different outcomes on this workload,
    // confirming the signatures actually discriminate between schedulers.
    assert_ne!(
        cosmos_base, lavd_base,
        "cosmos and lavd produced identical signatures — signature not discriminating"
    );

    for round in 0..4 {
        assert_eq!(
            cosmos_sig(),
            cosmos_base,
            "cosmos perturbed after interleaving with lavd (round {round})"
        );
        assert_eq!(
            lavd_sig(),
            lavd_base,
            "lavd perturbed after interleaving with cosmos (round {round})"
        );
    }
}
