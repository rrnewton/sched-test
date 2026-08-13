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

/// THE REGRESSION GUARD for the 68x slice divergence.
///
/// `SpinWait` is a continuous busy loop. If it is ever lowered back to a short
/// repeating run chunk, the workload — not the scheduler — decides when every
/// slice ends, and the simulator's dispatch rate stops meaning anything.
///
/// Measured before the fix: a 500us chunk produced 48067 slices against the
/// live guest's 710, a 68x divergence entirely manufactured by the lowering,
/// while `cpu_time` and `occupancy` both still agreed to 0.23% and so could
/// not see it. See `ai_docs/SLICE_DIVERGENCE_ROOT_CAUSE_20260812.md`.
///
/// The bound below is deliberately loose — it is not a tolerance, it is a
/// tripwire. Anything shorter than the scheduler's own default slice means the
/// phase boundary is back in charge.
#[test]
fn spinwait_runs_continuously_so_the_scheduler_owns_the_slice() {
    /// `SCX_SLICE_DFL`, what a scheduler typically requests per dispatch.
    const SCX_SLICE_DFL_NS: u64 = 20_000_000;

    let ir = lower(&sched_basic_proportional()).expect("lowers");
    assert!(!ir.tasks.is_empty(), "premise: the scenario has tasks");

    for t in &ir.tasks {
        let phases = &t.phases;
        assert_eq!(
            phases.len(),
            1,
            "a continuous spinner has exactly one run phase and no yield point; got {phases:?}"
        );
        match &phases[0] {
            scxsim_workload_ir::Phase::Run(d) => assert!(
                d.as_nanos() >= SCX_SLICE_DFL_NS,
                "SpinWait's run phase is {d}, shorter than the scheduler's own                  {}ms slice — the workload would end every slice before the                  scheduler could, which is the 68x defect returning",
                SCX_SLICE_DFL_NS / 1_000_000,
            ),
            other => panic!("SpinWait must lower to a single Run phase, got {other:?}"),
        }
    }

    // And it must still claim exact — now truthfully, because nothing is
    // invented any more. Before the fix this assertion ALSO passed, while the
    // lowering was fabricating the scheduling quantum; that is why the phase
    // check above exists rather than relying on the fidelity report alone.
    assert!(ir.fidelity.is_exact(), "{:?}", ir.fidelity.approximations());
}

/// SCOPE-NARROWNESS CHECK: does a cgroup cpuset actually confine its tasks?
///
/// The second failure shape the exact-arm audit looked for: a value the
/// lowering carries correctly that something downstream then ignores. Cgroup
/// cpusets are the candidate — `to_scenario` resolves them onto `CgroupDef`,
/// but a task's `allowed_cpus` is populated only from its OWN affinity, so
/// nothing in the ingestion confines a task to its cgroup's CPUs.
///
/// This runs two cgroups pinned to disjoint halves of a 4-CPU box and reports
/// where the tasks actually ran. It asserts only what it can prove; the
/// interesting output is printed.
#[test]
fn cgroup_cpuset_confinement_is_observable_or_is_not() {
    use scx_simulator::{DynamicScheduler, Simulator, TraceKind};
    use scxsim_workload_ir::{SourceCpuset, SourceTopology};
    use std::collections::{HashMap, HashSet};

    let _guard = scx_simulator::SIM_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let src = SourceScenario {
        duration: DurationNs::from_secs(1),
        topology: SourceTopology {
            numa_nodes: 1,
            llcs: 1,
            cores: 4,
            threads: 1,
        },
        default_workers_per_cgroup: 1,
        ..SourceScenario::new("cpuset_split")
    }
    .step(SourceStep::new(
        vec![
            SourceCgroupDef::named("cg_0")
                .cpuset(SourceCpuset::Disjoint { index: 0, of: 2 })
                .work(SourceWorkSpec::new(SourceWorkType::SpinWait)),
            SourceCgroupDef::named("cg_1")
                .cpuset(SourceCpuset::Disjoint { index: 1, of: 2 })
                .work(SourceWorkSpec::new(SourceWorkType::SpinWait)),
        ],
        SourceHold::FULL,
    ));

    let ir = lower(&src).expect("lowers");
    let scenario = to_scenario(&ir).expect("ingests");

    // What the ingestion produced, before running anything.
    for t in &scenario.tasks {
        println!(
            "task {:?} cgroup {:?} allowed_cpus {:?}",
            t.pid, t.cgroup_name, t.allowed_cpus
        );
    }

    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario.clone());
    let mut used: HashMap<i32, HashSet<u32>> = HashMap::new();
    for e in trace.events() {
        if let TraceKind::TaskScheduled { pid } = e.kind {
            used.entry(pid.0).or_default().insert(e.cpu.0);
        }
    }
    println!("observed CPUs per task: {used:?}");

    // cg_0 declared CPUs 0-1 and cg_1 declared CPUs 2-3. Check each task
    // against ITS OWN cgroup's declared set — two tasks on four CPUs land apart
    // by luck often enough that "are they disjoint from each other" would pass
    // while confinement was entirely absent.
    let declared: HashMap<i32, HashSet<u32>> =
        HashMap::from([(1, HashSet::from([0, 1])), (2, HashSet::from([2, 3]))]);
    let mut violations = Vec::new();
    for (pid, cpus) in &used {
        if let Some(allowed) = declared.get(pid) {
            let outside: Vec<u32> = cpus.difference(allowed).copied().collect();
            if !outside.is_empty() {
                violations.push(format!("pid {pid} allowed {allowed:?} ran on {outside:?}"));
            }
        }
    }

    // THE FINDING, asserted rather than merely printed: the ingestion resolves
    // the cgroup cpuset onto CgroupDef but never onto the member tasks'
    // allowed_cpus, and nothing downstream confines them either.
    assert!(
        scenario.tasks.iter().all(|t| t.allowed_cpus.is_none()),
        "premise of this characterization test: cgroup cpusets do not reach \
         task allowed_cpus today",
    );
    assert!(
        !violations.is_empty(),
        "KNOWN GAP CLOSED? Tasks are now confined to their cgroup's cpuset. \
         That is the desired behaviour — delete this characterization test and \
         replace it with a real confinement assertion.",
    );
    println!(
        "CONFIRMED SCOPE-NARROWNESS GAP: cgroup cpusets do not confine tasks. {}",
        violations.join("; ")
    );
}
