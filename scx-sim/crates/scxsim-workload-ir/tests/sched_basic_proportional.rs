//! Execute ktstr's `sched_basic_proportional` scenario on the simulator.
//!
//! This is the simulator half of the one-test-both-backends milestone. The VM
//! half is green (that test passed in a real guest on kernel 6.14.11 running
//! scx-ktstr); this drives the *same scenario definition* through
//! `ktstr ops -> IR -> Scenario -> Simulator::run` and produces the metrics the
//! calibration crate consumes.
//!
//! The bar is execution, not wiring, so these tests run a real scheduler `.so`
//! and assert on the resulting `Trace`.
//!
//! # The scenario, taken from the ktstr source rather than paraphrased
//!
//! `ktstr/tests/ktstr_sched_tests.rs`:
//!
//! ```ignore
//! #[ktstr_test(scheduler = KTSTR_SCHED, llcs = 1, cores = 2, threads = 1,
//!              sustained_samples = 15, watchdog_timeout_s = 15)]
//! fn sched_basic_proportional(ctx: &Ctx) -> Result<AssertResult> {
//!     let steps = vec![Step {
//!         setup: vec![ctx.cgroup_def("cg_0"), ctx.cgroup_def("cg_1")].into(),
//!         ops: vec![],
//!         hold: HoldSpec::FULL,
//!     }];
//!     execute_steps(ctx, steps)
//! }
//! ```
//!
//! So: 1 LLC x 2 cores x 1 thread = 2 CPUs; two cgroups with ktstr's default
//! worker count (`CtxBuilder`'s default of 1) and its default work type
//! (`SpinWait`); no ops; hold for the whole run; ktstr's default 12s duration.
//! "Proportional" names the expectation — two equally-weighted cgroups of
//! equal load should get comparable CPU — not a `cpu.weight` setting. Neither
//! cgroup sets one, which is worth stating because "proportional" invites the
//! assumption that it does.
//!
//! # What this does NOT claim
//!
//! The VM runs **scx-ktstr**. scx-sim's scheduler set is simple / lavd / cosmos
//! / mitosis / tickless — it has no scx-ktstr. So the *workload* crossing
//! backends is demonstrated here, and the *scheduler under test* is not the same
//! one. That gap is named in the milestone notes rather than papered over,
//! because a calibration that compared scx-ktstr-in-VM against simple-in-sim
//! would be measuring the difference between two schedulers and reporting it as
//! simulator infidelity.

use scxsim_workload_ir::{
    lower, to_scenario, DurationNs, SourceCgroupDef, SourceHold, SourceScenario, SourceStep,
    SourceWorkSpec, SourceWorkType,
};

/// The scenario exactly as ktstr declares it.
fn sched_basic_proportional() -> SourceScenario {
    SourceScenario {
        // ktstr's #[ktstr_test] default duration_s = 12.
        duration: DurationNs::from_secs(12),
        topology: scxsim_workload_ir::SourceTopology {
            numa_nodes: 1,
            llcs: 1,
            cores: 2,
            threads: 1,
        },
        // ktstr CtxBuilder's default.
        default_workers_per_cgroup: 1,
        ..SourceScenario::new("sched_basic_proportional")
    }
    .step(SourceStep::new(
        vec![
            // ctx.cgroup_def(n) with no .work() => one default WorkSpec,
            // whose work type is SpinWait.
            SourceCgroupDef::named("cg_0").work(SourceWorkSpec::new(SourceWorkType::SpinWait)),
            SourceCgroupDef::named("cg_1").work(SourceWorkSpec::new(SourceWorkType::SpinWait)),
        ],
        SourceHold::FULL,
    ))
}

/// The scenario must lower with NO approximation at all.
///
/// This is the property that makes it the right first subject for calibration:
/// any sim-vs-VM discrepancy measured on it is attributable to the simulator's
/// model, not to something the lowering threw away.
#[test]
fn lowers_with_zero_approximations() {
    let ir = lower(&sched_basic_proportional()).expect("lowers");
    assert!(
        ir.fidelity.is_exact(),
        "expected an exact lowering, got: {:?}",
        ir.fidelity.approximations()
    );
    assert_eq!(ir.topology.total_cpus(), 2);
    assert_eq!(ir.cgroups.len(), 2);
    assert_eq!(ir.tasks.len(), 2, "one worker per cgroup");
    assert_eq!(ir.duration, DurationNs::from_secs(12));
}

/// And ingest cleanly — no yield, no non-Normal policy, no observation, so none
/// of the three `Scenario` gaps apply to this scenario.
#[test]
fn ingests_into_a_scenario() {
    let ir = lower(&sched_basic_proportional()).expect("lowers");
    let scenario = to_scenario(&ir).expect("ingests");
    assert_eq!(scenario.nr_cpus, 2);
    assert_eq!(scenario.tasks.len(), 2);
    assert_eq!(scenario.cgroups.len(), 2);
    assert_eq!(scenario.duration_ns, 12_000_000_000);
}

/// THE BAR: the scenario actually RUNS on the simulator and produces output.
///
/// Not "compiles", not "is wired up" — a real scheduler `.so` is loaded and the
/// engine executes the lowered scenario to completion.
#[test]
fn executes_on_the_simulator_and_produces_comparable_output() {
    use scx_simulator::{DynamicScheduler, ExitKind, Simulator};

    let ir = lower(&sched_basic_proportional()).expect("lowers");
    let scenario = to_scenario(&ir).expect("ingests");
    let nr_cpus = scenario.nr_cpus;

    // scx-sim has no scx-ktstr (see the module note); `simple` is a scheduler
    // it does have. The workload is the ktstr one; the scheduler is not.
    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario);

    assert_eq!(
        trace.exit_kind(),
        &ExitKind::Normal,
        "the lowered ktstr scenario must run to completion, not error out"
    );

    // Comparable output: the quantities scxsim-calibration compares against the
    // VM's WorkerReport. Assert they are actually populated — a trace that ran
    // but measured nothing would look like success.
    let pids: Vec<_> = ir
        .tasks
        .iter()
        .map(|t| scx_simulator::Pid(t.id.0 as i32 + 1))
        .collect();

    let mut total_runtime = 0u64;
    for pid in &pids {
        let rt = trace.total_runtime(*pid);
        assert!(
            rt > 0,
            "task {pid:?} got no CPU time; a spinning task on an idle CPU must run"
        );
        assert!(
            trace.schedule_count(*pid) > 0,
            "task {pid:?} was never scheduled"
        );
        total_runtime += rt;
    }

    // Both tasks spin for the whole run on 2 CPUs, so aggregate occupancy should
    // be substantial. Deliberately a loose sanity floor and not a calibration
    // assertion — calibration compares against the VM, and that comparison lives
    // in scxsim-calibration with its own pre-registered tolerances.
    let capacity = ir.duration.as_nanos() * nr_cpus as u64;
    let occupancy = total_runtime as f64 / capacity as f64;
    assert!(
        occupancy > 0.10,
        "two spinners on {nr_cpus} cpus for {} should occupy real CPU time; got {occupancy:.3}",
        ir.duration
    );

    eprintln!(
        "sched_basic_proportional on the simulator: exit={:?} tasks={} \
         total_runtime={}ns occupancy={:.3}",
        trace.exit_kind(),
        pids.len(),
        total_runtime,
        occupancy
    );
}

/// The proportional expectation the test is named for: two equally-loaded,
/// equally-weighted cgroups should receive comparable CPU.
///
/// Stated as a wide band, because this asserts the SIMULATOR is not doing
/// something absurd — it is not the calibration. The sim-vs-VM comparison, with
/// its pre-registered tolerance, is scxsim-calibration's job.
#[test]
fn two_equal_cgroups_receive_comparable_cpu() {
    use scx_simulator::{DynamicScheduler, Pid, Simulator};

    let ir = lower(&sched_basic_proportional()).expect("lowers");
    let scenario = to_scenario(&ir).expect("ingests");
    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario);

    let a = trace.total_runtime(Pid(1));
    let b = trace.total_runtime(Pid(2));
    assert!(a > 0 && b > 0, "both cgroups' workers must run: {a} vs {b}");

    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    let ratio = lo as f64 / hi as f64;
    assert!(
        ratio > 0.5,
        "two equally-weighted cgroups with identical load should get comparable \
         CPU; got {a} vs {b} (ratio {ratio:.3})"
    );
}
