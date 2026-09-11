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
    lower, lower_with_options, to_scenario, DurationNs, IoCalibrationScheduler, IoModelBacking,
    IoModelFlush, IoModelOpenMode, IoModelOperation, IoModelSpec, LoweringOptions, Phase, Repeat,
    ResolvedIoProfile, SourceCgroupDef, SourceHold, SourceScenario, SourceStep, SourceTopology,
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

/// A resolved storage profile is calibrated under scx-ktstr and must not run
/// under another scheduler. scx-sim has no scx-ktstr implementation today, so
/// the production path must fail closed rather than normalize a `simple` run as
/// in-domain evidence. A separate unbound Scenario below checks only the generic
/// SystemCpu/Sleep/Park plumbing.
#[test]
fn resolved_storage_profile_requires_the_calibration_scheduler() {
    use scx_simulator::{
        DynamicScheduler, ExitKind, Gid, Phase as SimPhase, Pid, RepeatMode, Scenario, Simulator,
        TaskBehavior, TaskDef, TraceKind, Uid,
    };

    let _guard = scx_simulator::SIM_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let source = SourceScenario {
        duration: DurationNs::from_millis(100),
        topology: SourceTopology {
            numa_nodes: 1,
            llcs: 1,
            cores: 4,
            threads: 1,
        },
        ..SourceScenario::new("resolved_io_sync_write")
    }
    .step(SourceStep::new(
        vec![SourceCgroupDef::named("io").work(
            SourceWorkSpec::new(SourceWorkType::IoModelV1 {
                spec: IoModelSpec {
                    operation: IoModelOperation::SequentialWrite,
                    backing: IoModelBacking::FreshRawUnthrottledBlockDevice,
                    backing_capacity_bytes: 256 * 1024 * 1024,
                    open_mode: IoModelOpenMode::OSync,
                    write_size_bytes: 4096,
                    writes_per_cycle: 16,
                    flush: IoModelFlush::FdatasyncPerCycle,
                    queue_depth: 1,
                    calibration_scheduler: IoCalibrationScheduler::ScxKtstr,
                    declared_bytes_per_worker: 2 * 1024 * 1024,
                },
            })
            .workers(1),
        )],
        SourceHold::FULL,
    ));
    let spec = match &source.steps[0].setup[0].works[0].work_type {
        SourceWorkType::IoModelV1 { spec } => spec.clone(),
        other => panic!("expected typed I/O model source, got {other:?}"),
    };
    let profile = ResolvedIoProfile::frozen_v1(spec).expect("spec is inside frozen v1");
    let options = LoweringOptions::default().with_io_profile(profile.clone());
    let ir = lower_with_options(&source, &options).expect("resolved profile lowers");
    assert_eq!(ir.tasks[0].repeat, Repeat::Once);
    // v2 emits the alternation; both estimands must survive as SUMS, and the
    // terminal Park must still be the last phase.
    assert_eq!(ir.tasks[0].phases.last(), Some(&Phase::Park));
    let ir_system: u64 = ir.tasks[0]
        .phases
        .iter()
        .filter_map(|p| match p {
            Phase::SystemCpu(d) => Some(d.as_nanos()),
            _ => None,
        })
        .sum();
    let ir_nonrunning: u64 = ir.tasks[0]
        .phases
        .iter()
        .filter_map(|p| match p {
            Phase::NonRunning(d) => Some(d.as_nanos()),
            _ => None,
        })
        .sum();
    assert_eq!(ir_system, profile.system_cpu_per_worker.as_nanos());
    assert_eq!(ir_nonrunning, profile.nonrunning_per_worker.as_nanos());

    let scenario = to_scenario(&ir).expect("resolved profile ingests");
    assert_eq!(scenario.required_scheduler_identity(), Some("ktstr"));
    // The same two sums must survive INGESTION into the simulator scenario, not
    // just the lowering. This is the check that would catch the ingest path
    // rescaling, reordering or dropping part of the alternation, and it is why
    // it is asserted separately from the IR-level one above.
    let sim = scenario.tasks[0].behavior.phases.as_slice();
    assert!(
        matches!(sim.last(), Some(SimPhase::Park)),
        "terminal Park must survive ingestion: {sim:?}"
    );
    let sim_system: u64 = sim
        .iter()
        .filter_map(|p| match p {
            SimPhase::SystemCpu(ns) => Some(*ns),
            _ => None,
        })
        .sum();
    let sim_sleep: u64 = sim
        .iter()
        .filter_map(|p| match p {
            SimPhase::Sleep(ns) => Some(*ns),
            _ => None,
        })
        .sum();
    assert_eq!(sim_system, profile.system_cpu_per_worker.as_nanos());
    assert_eq!(sim_sleep, profile.nonrunning_per_worker.as_nanos());
    assert!(sim.len() > 3, "the alternation must not be collapsed");
    let rejected = Simulator::new(DynamicScheduler::simple()).run(scenario);
    assert_eq!(
        rejected.exit_kind(),
        &ExitKind::ErrorSchedulerIdentityMismatch {
            required: "ktstr".into(),
            actual: "simple".into(),
        }
    );
    assert!(
        rejected.events().is_empty(),
        "a scheduler mismatch must reject before simulation execution"
    );

    // Generic phase-semantics plumbing, deliberately not an IoModelV1
    // Scenario and therefore not evidence about the calibrated profile.
    let mut scenario = Scenario::builder()
        .cpus(4)
        .duration_ns(100_000_000)
        .task(TaskDef {
            name: "generic_phase_plumbing".into(),
            pid: Pid(1),
            nice: 0,
            behavior: TaskBehavior {
                phases: vec![
                    SimPhase::SystemCpu(profile.system_cpu_per_worker.as_nanos()),
                    SimPhase::Sleep(profile.nonrunning_per_worker.as_nanos()),
                    SimPhase::Park,
                ],
                repeat: RepeatMode::Once,
            },
            start_time_ns: 0,
            mm_id: None,
            allowed_cpus: None,
            parent_pid: None,
            cgroup_name: None,
            task_flags: 0,
            migration_disabled: 0,
            thread_group_leader: None,
            uid: Uid(0),
            gid: Gid(0),
        })
        .build();
    // Isolate the tagged phase invariant from optional simulator timing
    // extras. The default-noise path is separately covered by ingestion: a
    // SystemCpu tag cannot enter the generic Phase::Run jitter branch.
    scenario.noise.enabled = false;
    scenario.overhead.enabled = false;
    scenario.sched_overhead_rbc_ns = None;
    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario);
    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    assert_eq!(
        trace.total_runtime(Pid(1)),
        profile.system_cpu_per_worker.as_nanos(),
        "SystemCpu must be scheduled exactly, without generic compute jitter"
    );
    assert!(
        trace
            .events()
            .iter()
            .any(|event| matches!(event.kind, TraceKind::TaskSlept { pid } if pid == Pid(1))),
        "NonRunning must make the task unrunnable"
    );
    assert!(
        trace
            .events()
            .iter()
            .any(|event| matches!(event.kind, TraceKind::TaskParked { pid } if pid == Pid(1))),
        "the trace must prove that the fixed volume reached its terminal park"
    );
    assert!(
        trace
            .events()
            .iter()
            .all(|event| !matches!(event.kind, TraceKind::TaskCompleted { pid } if pid == Pid(1))),
        "the measured VM worker parks after its fixed volume; it must not become Exited"
    );

    let mut truncated = Scenario::builder()
        .cpus(4)
        .duration_ns(1_000_000)
        .task(TaskDef {
            name: "generic_truncated_phase_plumbing".into(),
            pid: Pid(1),
            nice: 0,
            behavior: TaskBehavior {
                phases: vec![
                    SimPhase::SystemCpu(profile.system_cpu_per_worker.as_nanos()),
                    SimPhase::Sleep(profile.nonrunning_per_worker.as_nanos()),
                    SimPhase::Park,
                ],
                repeat: RepeatMode::Once,
            },
            start_time_ns: 0,
            mm_id: None,
            allowed_cpus: None,
            parent_pid: None,
            cgroup_name: None,
            task_flags: 0,
            migration_disabled: 0,
            thread_group_leader: None,
            uid: Uid(0),
            gid: Gid(0),
        })
        .build();
    truncated.noise.enabled = false;
    truncated.overhead.enabled = false;
    truncated.sched_overhead_rbc_ns = None;
    let truncated_trace = Simulator::new(DynamicScheduler::simple()).run(truncated);
    assert_eq!(
        truncated_trace.exit_kind(),
        &ExitKind::ErrorTerminalParkNotReached { pid: Pid(1) },
        "a run that never reaches the fixed-volume Park must not report Normal"
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

/// A cgroup cpuset must actually confine its member tasks.
///
/// INVERTED FROM A CHARACTERIZATION TEST, deliberately not deleted. The
/// exact-arm audit wrote this to assert the OPPOSITE — that cgroup cpusets
/// reached `CgroupDef` and then confined nothing, because a task's
/// `allowed_cpus` was populated only from its own affinity. That gap was
/// sim-4qlh5; it is fixed (`Scenario::effective_cpuset`, PR #79), so the
/// original assertion fired its own "KNOWN GAP CLOSED?" message.
///
/// The gap-asserting form was replaced with the confinement assertion it was
/// standing in for, rather than removed. Deleting it would have discarded
/// exactly the coverage it existed to protect: this is the only test in the
/// crate that checks a declared cpuset against where tasks were OBSERVED to
/// run, and nothing else would notice the gap reopening.
///
/// THE TRAP THE ORIGINAL AUTHOR LEFT, preserved because it is the whole reason
/// this test is written the way it is: two tasks on four CPUs land apart by
/// luck often enough that "are the two tasks disjoint from each other" passes
/// while confinement is entirely absent. Each task is therefore checked
/// against ITS OWN cgroup's declared set, never against the other task.
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

    // The cpuset must reach the task's EFFECTIVE cpumask. Note it does NOT
    // reach `TaskDef::allowed_cpus`, and asserting that it does would be
    // wrong: `allowed_cpus` stays the task's OWN declared affinity (None
    // here), and the fix intersects it with the cgroup's cpuset at task
    // construction via `Scenario::effective_cpuset`. Checking `allowed_cpus`
    // would test an implementation that does not exist.
    for task in &scenario.tasks {
        let effective = scenario.effective_cpuset(task).unwrap_or_else(|| {
            panic!(
                "task {:?} is in a cpuset-confined cgroup, so its effective \
                 cpuset must be Some; got None. If this fires, the cgroup \
                 cpuset has stopped reaching task placement (sim-4qlh5 \
                 reopened).",
                task.pid,
            )
        });
        let expected = declared
            .get(&task.pid.0)
            .expect("both pids are in the declared map");
        let got: HashSet<u32> = effective.iter().map(|c| c.0).collect();
        assert_eq!(
            &got, expected,
            "task {:?} effective cpuset must be exactly its cgroup's declared \
             set",
            task.pid,
        );
    }

    // And it must hold in the RUN, not just in the scenario: a populated
    // allowed_cpus that the engine ignored would satisfy the check above.
    assert!(
        violations.is_empty(),
        "a cgroup confined by cpuset.cpus must not run outside it — in the \
         kernel this is impossible, since cpuset.cpus narrows every member \
         task's effective cpumask. Violations: {}",
        violations.join("; ")
    );
    println!("confinement holds: every task ran only within its cgroup's declared cpuset");
}
