//! The first calibration against real dual-backend data.
//!
//! `sched_basic_proportional` executes on both backends: on a real guest
//! (source-built 6.14.11, ktstr's `scx-ktstr`) and on the simulator, from ONE
//! scenario definition lowered through `ktstr ops -> IR -> Scenario`. This
//! applies the pre-registered rejection rule in [`scxsim_calibration`] to the
//! two runs and reports a verdict per metric.
//!
//! # The rule is applied as written
//!
//! Every tolerance comes from [`Metric::spec`], which was committed before any
//! of this data existed. Nothing here re-derives, widens or special-cases a
//! bound. Where a metric fails, the failure is the finding and is recorded as
//! one.
//!
//! # What this comparison is NOT
//!
//! **The two backends run different schedulers.** The guest runs `scx-ktstr`;
//! the simulator runs `simple`, because `scx-ktstr` is ktstr's own BPF
//! scheduler and scx-sim does not have it. Both are minimal global-DSQ
//! schedulers, so a fairness question is meaningful on either, but any
//! discrepancy found here is attributable to the scheduler difference at least
//! as much as to simulator infidelity. Until scx-sim can run scx-ktstr, this is
//! a SCENARIO-level calibration and the numbers below must not be cited as
//! "the simulator is N% off".
//!
//! That is also why the interesting result is not the metrics that agree.

use scxsim_calibration::{
    report::{mean_slice_result, MetricResult},
    CalibrationRun, Metric, NegativeControl, Quantity, RunOutcome, SampleCount, SimRun, Verdict,
    VmRun,
};
use scxsim_workload_ir::{
    lower, to_scenario, DurationNs as IrDuration, SourceCgroupDef, SourceHold, SourceScenario,
    SourceStep, SourceTopology, SourceWorkSpec, SourceWorkType,
};

use scxsim_calibration::units::{DurationNs, Ratio};

/// The live run, committed verbatim. See `vm_runs/README.md` for provenance.
const VM_SIDECAR: &str =
    include_str!("../vm_runs/sched_basic_proportional-6.14.11-85c72e1.ktstr.json");

/// ktstr's `sched_basic_proportional`, as ktstr declares it.
///
/// Transcribed from `ktstr/tests/ktstr_sched_tests.rs`: 1 numa / 1 llc / 2 cores
/// / 1 thread = 2 CPUs, two cgroups at ktstr's default one worker each, the
/// default `SpinWait` work type, no ops, held for the whole of ktstr's default
/// 12s. "Proportional" names the expectation that two equally-weighted cgroups
/// get comparable CPU — neither sets a `cpu.weight`.
fn scenario_source() -> SourceScenario {
    SourceScenario {
        duration: IrDuration::from_secs(12),
        topology: SourceTopology {
            numa_nodes: 1,
            llcs: 1,
            cores: 2,
            threads: 1,
        },
        default_workers_per_cgroup: 1,
        ..SourceScenario::new("sched_basic_proportional")
    }
    .step(SourceStep::new(
        vec![
            SourceCgroupDef::named("cg_0").work(SourceWorkSpec::new(SourceWorkType::SpinWait)),
            SourceCgroupDef::named("cg_1").work(SourceWorkSpec::new(SourceWorkType::SpinWait)),
        ],
        SourceHold::FULL,
    ))
}

/// Run the scenario on the simulator and calibrate the result against the
/// committed guest run.
///
/// Returns the finished run so the assertions below can interrogate it, and
/// prints the full report either way — a calibration whose output only appears
/// on failure is one nobody reads.
fn calibrate() -> CalibrationRun {
    use scx_simulator::{DynamicScheduler, ExitKind, Simulator};

    let _guard = scx_simulator::SIM_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let vm = VmRun::from_json(VM_SIDECAR).expect("the committed sidecar parses");
    assert!(
        vm.passed,
        "calibrating against a FAILED guest run is invalid"
    );

    let ir = lower(&scenario_source()).expect("the scenario lowers");
    assert!(
        ir.fidelity.is_exact(),
        "this scenario must lower exactly, else a discrepancy below could be the \
         lowering rather than the simulator: {:?}",
        ir.fidelity.approximations()
    );
    let scenario = to_scenario(&ir).expect("the IR ingests");

    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario.clone());
    assert_eq!(
        *trace.exit_kind(),
        ExitKind::Normal,
        "the simulation must terminate normally"
    );
    // Named once, and threaded into the mean-slice guard below. The guest's
    // scheduler comes off its own sidecar (`vm.scheduler`), so neither side is
    // asserted by this file — they are reported by the runs themselves.
    const SIM_SCHEDULER: &str = "simple";
    let sim = SimRun::new(&scenario, &trace);
    let wall = sim.elapsed();

    // One run on each side. Every aggregate clears MinSamples::AGGREGATE (1);
    // nothing here can clear MinSamples::PERCENTILE (100).
    let n = SampleCount(1);

    let mut run = CalibrationRun::new(
        "sched_basic_proportional",
        // Recorded so a later reader can rebuild both halves exactly.
        "feat/scxsim-calibration-first-run",
        &vm.project_commit,
    );

    // --- Occupancy: dimensionless, tightest bound (5%). ---
    run.record(MetricResult::evaluate(
        Metric::Occupancy,
        None,
        Some(Quantity::Ratio(sim.occupancy())),
        Some(Quantity::Ratio(vm.occupancy(wall))),
        n,
    ));

    // --- CPU time and off-CPU time: per cgroup, so a fairness gap between the
    // two cgroups cannot hide inside a total. ---
    for cg in &vm.stats.cgroups {
        let name = cg.cgroup_name.as_str();
        run.record(MetricResult::evaluate(
            Metric::CpuTime,
            Some(name.to_string()),
            Some(Quantity::Duration(sim.cpu_time(name))),
            Some(Quantity::Duration(cg.cpu_time())),
            n,
        ));
        // Off-CPU: RECORDED, NOT EVALUATED. Both sides compute
        // (wall - cpu)/wall correctly and the two results are not the same
        // physical quantity — the guest's is dominated by virtualization
        // overhead the simulator has no concept of. See Metric::OffCpuTime for
        // the decomposition. The values are still carried so the gap stays
        // visible; what stops is subtracting them and calling it fidelity.
        run.record(MetricResult::not_comparable(
            Metric::OffCpuTime,
            Some(name.to_string()),
            sim.off_cpu_fraction(name).map(Quantity::Ratio),
            Some(Quantity::Ratio(cg.off_cpu_fraction())),
            n,
        ));
        // Scheduling delay: the quantity off-CPU time was MEANT to test, and
        // unlike off-CPU time the two sides are the same physical thing — total
        // runnable-but-not-running per worker, averaged over workers. The two
        // definitions are set out in `VmCgroup::run_delay` and
        // `SimRun::run_delay`, which also name the two residual differences
        // (direct dispatch, absence of interference) and, below, measure the
        // first one rather than assuming its size.
        //
        // The tolerance is `Metric::spec`'s, committed before either side could
        // compute the quantity. It is applied here as written.
        run.record(MetricResult::evaluate(
            Metric::SchedulingDelay,
            Some(name.to_string()),
            sim.run_delay(name).map(Quantity::Duration),
            cg.run_delay().map(Quantity::Duration),
            n,
        ));
    }

    // --- Migrations. Note the estimator difference documented in
    // `scxsim_calibration::sim`: ktstr samples sched_getcpu from userspace, the
    // simulator counts every placement change. ---
    run.record(MetricResult::evaluate(
        Metric::Migrations,
        None,
        Some(Quantity::Count(sim.migrations())),
        Some(Quantity::Count(vm.stats.total_migrations)),
        n,
    ));

    // --- Context switches: NOT COMPARED, and the None on the live side is why.
    // ktstr's sidecar carries no per-task count; its schedstat total is VM-wide,
    // a different population from the simulator's per-task count. Feeding that
    // total in would be comparing two different things and calling the result
    // fidelity. ---
    run.record(MetricResult::evaluate(
        Metric::ContextSwitches,
        None,
        Some(Quantity::Count(sim.context_switches())),
        None,
        n,
    ));

    // --- Wake latency: the guest recorded `wake_measured: false`, so the live
    // side is None. The zeros in its wake fields are placeholders. ---
    let sim_wake = sim.wake_latencies();
    run.record(MetricResult::evaluate(
        Metric::WakeLatency,
        Some("p99".into()),
        sim_wake.percentile(99.0).map(Quantity::Duration),
        vm.cgroup("cg_0")
            .and_then(|c| c.wake_latency_p99())
            .map(Quantity::Duration),
        sim_wake.count(),
    ));

    // --- Mean slice length. The guard runs BEFORE either measurement is
    // consulted: the guest runs scx-ktstr and the simulator runs `simple`, so
    // this must abstain rather than compare two different schedulers' slice
    // policies and call the difference simulator infidelity.
    //
    // The VM side is None because no wprof slice extraction exists in this
    // crate yet — a second, independent reason to abstain, and one that would
    // still hold if the schedulers were made to match. Both are reported.
    let sim_slices = sim.slice_durations();
    run.record(mean_slice_result(
        SIM_SCHEDULER,
        &vm.scheduler,
        sim.mean_slice(),
        None,
        sim_slices.count(),
    ));

    // --- The negative control. A 3x-wrong occupancy must be rejected by the
    // SAME tolerance the real comparison used, or nothing above is citable. ---
    let run = run.with_control(NegativeControl::perturbed(
        Metric::Occupancy,
        Quantity::Ratio(vm.occupancy(wall)),
        3.0,
        n,
    ));

    println!(
        "\n=== sched_basic_proportional: simulator vs live guest ===\n\
         wall {wall}  sim cpus {}  guest vcpus {}  guest kernel 6.14.11 / {}\n\
         SCHEDULERS DIFFER: guest ran `{}`, simulator ran `simple`\n\n{}",
        scenario.nr_cpus,
        vm.vcpus,
        vm.project_commit,
        vm.scheduler,
        run.render()
    );
    // The sample-floor caveat on Metric::MeanSliceLength is only valid while
    // the slice distribution stays tight; report the dispersion so the floor
    // can be raised on evidence rather than assumed adequate.
    match (sim_slices.mean(), sim_slices.coefficient_of_variation()) {
        (Some(m), Some(cv)) => println!(
            "sim slice distribution: {} mean={m} CV={cv:.3}{}",
            sim_slices.count(),
            if cv > 0.33 {
                "  <-- CV > 0.33: MinSamples::PERCENTILE is NOT sufficient here; \
                 raise to TAIL_PERCENTILE"
            } else {
                "  (CV <= 0.33, so PERCENTILE is adequate)"
            }
        ),
        _ => println!("sim slice distribution: not computable"),
    }
    println!(
        "supporting detail not in the table:\n  \
         sim context switches {} (live side has no per-task counterpart)\n  \
         sim wake-latency samples {}\n  \
         sim migrations {} (kernel-exact) vs guest {} (userspace-sampled)\n  \
         scheduling delay, direct dispatches charged nothing by the simulator \
         and a real wakeup path by the guest: cg_0 {} of {} dispatches, \
         cg_1 {} of {}\n  \
         live between-worker spread on an IDENTICAL workload: cg_0 {} vs cg_1 \
         {} — the live side's own variation, {:.2}x, is larger than either \
         side's gap to the other\n",
        sim.context_switches(),
        sim_wake.count(),
        sim.migrations(),
        vm.stats.total_migrations,
        sim.direct_dispatches("cg_0"),
        sim.dispatches("cg_0"),
        sim.direct_dispatches("cg_1"),
        sim.dispatches("cg_1"),
        vm.cgroup("cg_0").and_then(|c| c.run_delay()).unwrap(),
        vm.cgroup("cg_1").and_then(|c| c.run_delay()).unwrap(),
        vm.cgroup("cg_1").unwrap().mean_run_delay_us / vm.cgroup("cg_0").unwrap().mean_run_delay_us,
    );

    run
}

/// THE deliverable: apply the rule, and report what it says.
///
/// The assertions pin the run's STRUCTURE — that the control rejected, that
/// nothing unmeasured was scored as agreement — rather than the individual
/// numbers, which belong to the simulator and are expected to move.
#[test]
fn the_pre_registered_rule_applied_to_both_backends() {
    let run = calibrate();
    let outcome = run.finish();

    // 1. The control must have rejected, or no metric in the run is citable.
    let control = run
        .negative_control
        .as_ref()
        .expect("the run must carry a control");
    assert!(
        control.rejected_as_required(),
        "negative control did not reject a 3x error, so the tolerances cannot \
         distinguish a matching simulator from a wrong one: {:?}",
        control.result
    );
    assert!(
        !matches!(outcome, RunOutcome::Void { .. }),
        "run is VOID: {outcome}"
    );

    // 2. Report the comparable-vs-unmeasured split. A verdict computed over two
    //    of six metrics is a mostly-unmeasured run, not a passing calibration,
    //    and the count is the only thing that says which.
    let total = run.results.len();
    let unmeasured = run
        .results
        .iter()
        .filter(|r| r.verdict == Verdict::NotMeasured)
        .count();
    println!(
        "comparable {}/{total}; NotMeasured {unmeasured}/{total}",
        total - unmeasured
    );
    assert!(
        total - unmeasured >= 1,
        "nothing was comparable at all; this is not a calibration"
    );
}

/// The findings, pinned.
///
/// A calibration whose result is only ever printed is a calibration nobody
/// notices changing. These are the verdicts as of the first run; if the
/// simulator's fidelity moves — in either direction — this fails and the change
/// has to be described rather than absorbed.
///
/// **Updating this test is expected and fine. Doing it silently is not.** A
/// verdict that flips is a claim about the simulator, so say which way and why
/// in the commit message.
#[test]
fn the_findings_as_first_measured() {
    let run = calibrate();

    let verdict = |m: Metric, at: Option<&str>| -> Verdict {
        run.results
            .iter()
            .find(|r| r.metric == m && r.at.as_deref() == at)
            .unwrap_or_else(|| panic!("no result for {m} {at:?}"))
            .verdict
    };

    // Agreed. Both are near-unfalsifiable on this workload — two saturated
    // spinners on two dedicated CPUs total ~24s of CPU under any scheduler that
    // is not broken — so their agreement is the weakest evidence in the run.
    assert_eq!(verdict(Metric::Occupancy, None), Verdict::Agree);
    assert_eq!(verdict(Metric::CpuTime, Some("cg_0")), Verdict::Agree);
    assert_eq!(verdict(Metric::CpuTime, Some("cg_1")), Verdict::Agree);

    // FINDING 1: the simulator models far too little off-CPU time. It has no
    // IRQs, no timer ticks and no competing guest work, so a task that never
    // sleeps is never off-CPU; the live spinners lose ~0.4% to interference.
    for cg in ["cg_0", "cg_1"] {
        assert_eq!(
            verdict(Metric::OffCpuTime, Some(cg)),
            Verdict::Inconclusive,
            "{cg}: off-CPU time is no longer Inconclusive. If it became Agree, \
             the gap closed. If it became Disagree, a real divergence appeared. \
             KNOWN-GAP TEST: this going red means the gap CLOSED. Invert this \
             assertion to assert the property now holds. Do not delete it, and \
             do not loosen the bound."
        );
    }

    // FINDING 2: zero migrations against the guest's 16. `simple` places each
    // task once and never rebalances. Attributable to the scheduler difference
    // as much as to the simulator — see the module header.
    assert_eq!(
        verdict(Metric::Migrations, None),
        Verdict::Disagree,
        "migrations now AGREE with the guest — `simple` has started rebalancing, \
         or the metric changed. \
         KNOWN-GAP TEST: this going red means the gap CLOSED. Invert this \
         assertion to assert the property now holds. Do not delete it, and do \
         not loosen the bound."
    );

    // FINDING 3: the simulator models 25-58x too little scheduling delay.
    // 150us simulated against 3.694ms / 8.683ms live. cg_1 fails BOTH arms of
    // the pre-registered bound — 98.3% against the 20% relative arm, and
    // 8.533ms against the 4ms absolute arm, which it exceeds by 2.1x. cg_0
    // survives only because its live value is small enough that the absolute
    // arm still covers the gap, at 89% of that arm.
    //
    // Same missing-interference story as FINDING 1, but on a quantity that IS
    // comparable across backends, so it cannot be set aside as a definitional
    // artefact the way off-CPU time was.
    assert_eq!(
        verdict(Metric::SchedulingDelay, Some("cg_0")),
        Verdict::Agree
    );
    assert_eq!(
        verdict(Metric::SchedulingDelay, Some("cg_1")),
        Verdict::Disagree
    );

    // Not comparable, for two different reasons — see the dedicated tests.
    assert_eq!(verdict(Metric::ContextSwitches, None), Verdict::NotMeasured);
    assert_eq!(
        verdict(Metric::WakeLatency, Some("p99")),
        Verdict::NotMeasured
    );

    assert_eq!(
        run.finish(),
        RunOutcome::Gap(Verdict::Disagree),
        "a run with disagreements is a GAP: interpretable, and not a pass"
    );
}

/// Wake latency must be NotMeasured, because the guest run says so.
///
/// This is the guard against the most seductive failure available here: the
/// simulator DOES emit wake latencies, the sidecar DOES have wake-latency
/// fields, and they are zeros. Comparing the two would produce a confident
/// verdict about a quantity the guest never sampled.
#[test]
fn wake_latency_is_not_measured_because_the_guest_did_not_measure_it() {
    let vm = VmRun::from_json(VM_SIDECAR).expect("parses");
    assert!(
        !vm.wake_latency_measured(),
        "premise: this guest run did not measure wake latency"
    );

    let run = calibrate();
    let wake: Vec<_> = run
        .results
        .iter()
        .filter(|r| r.metric == Metric::WakeLatency)
        .collect();
    assert!(!wake.is_empty(), "wake latency must appear in the report");
    for r in wake {
        assert_eq!(
            r.verdict,
            Verdict::NotMeasured,
            "wake latency must be NotMeasured, not scored against a placeholder zero"
        );
        assert_eq!(r.vm, None, "the live value must be absent, not 0");
    }
}

/// Context switches must be NotMeasured for a stated reason, not compared.
#[test]
fn context_switches_are_not_measured_because_the_populations_differ() {
    let run = calibrate();
    let cs = run
        .results
        .iter()
        .find(|r| r.metric == Metric::ContextSwitches)
        .expect("context switches must appear in the report");
    assert_eq!(cs.verdict, Verdict::NotMeasured);
    assert_eq!(
        cs.vm, None,
        "the guest's schedstat total is VM-wide and must not be passed off as a \
         per-task count"
    );
    assert!(
        matches!(cs.sim, Some(Quantity::Count(n)) if n > 0),
        "the simulator side is real and should be recorded even when uncomparable"
    );
}

/// The simulator's migration count of ZERO must be a real observation, not an
/// artefact of the derivation.
///
/// `SimRun::migrations` compares `event.cpu` between consecutive dispatches of
/// a task. If the trace reported every event on CPU 0 — because the field was
/// unpopulated, or because the scenario collapsed to one CPU — the derivation
/// would return 0 for any workload whatsoever, and the DISAGREE against the
/// guest's 16 would be a bug in this crate being reported as a fidelity gap.
///
/// So: prove the trace really does span both CPUs, and that each task really
/// did stay put on one of them.
#[test]
fn the_zero_migration_count_is_an_observation_not_an_unpopulated_field() {
    use scx_simulator::{DynamicScheduler, Simulator, TraceKind};
    use std::collections::{HashMap, HashSet};

    let _guard = scx_simulator::SIM_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let ir = lower(&scenario_source()).expect("lowers");
    let scenario = to_scenario(&ir).expect("ingests");
    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario.clone());

    let mut per_pid: HashMap<i32, HashSet<u32>> = HashMap::new();
    let mut dispatches = 0usize;
    for e in trace.events() {
        if let TraceKind::TaskScheduled { pid } = e.kind {
            per_pid.entry(pid.0).or_default().insert(e.cpu.0);
            dispatches += 1;
        }
    }
    println!("dispatches {dispatches}; per-pid CPU sets {per_pid:?}");

    assert!(
        dispatches > 1_000,
        "too few dispatches to conclude anything"
    );
    assert_eq!(per_pid.len(), 2, "both tasks must have been dispatched");

    // The field is populated and the run genuinely used both CPUs: the union
    // over tasks spans more than one CPU, so a migration WOULD have been seen.
    let spanned: HashSet<u32> = per_pid.values().flatten().copied().collect();
    assert_eq!(
        spanned.len(),
        2,
        "the run must span both CPUs, else 'no migrations' is vacuous; saw {spanned:?}"
    );

    // And each individual task stayed on exactly one CPU — which is what makes
    // the count zero.
    for (pid, cpus) in &per_pid {
        assert_eq!(
            cpus.len(),
            1,
            "pid {pid} used {cpus:?}; if a task really did move, migrations must \
             not report 0"
        );
    }

    let sim = SimRun::new(&scenario, &trace);
    assert_eq!(sim.migrations(), 0, "consistent with the per-pid CPU sets");
}

/// A sanity floor on the derivation itself: if the simulator's CPU time were
/// zero, every duration comparison above would be meaningless.
#[test]
fn the_simulated_side_actually_ran() {
    let run = calibrate();
    let cpu: Vec<_> = run
        .results
        .iter()
        .filter(|r| r.metric == Metric::CpuTime)
        .collect();
    assert_eq!(cpu.len(), 2, "one per cgroup");
    for r in cpu {
        match r.sim {
            Some(Quantity::Duration(d)) => assert!(
                d.as_nanos() > 0,
                "cgroup {:?} got no simulated CPU time",
                r.at
            ),
            other => panic!("expected a duration, got {other:?}"),
        }
    }
}

/// THE ACCEPTANCE TEST FOR `MeanSliceLength`, and it is the NEGATIVE case.
///
/// The guest runs `scx-ktstr`; the simulator runs `simple`. Those are different
/// schedulers, and different schedulers legitimately choose different slice
/// lengths — so a mean-slice comparison between them is a category error and
/// the metric MUST report `NotMeasured` rather than a number.
///
/// A number here would be worse than having no metric at all. It would look
/// like a comparison, it would sit in the calibration output beside real
/// results, and nothing would tell a reader it compared two unlike things.
/// That is the same defect as an `is_exact()` returning true while fabricating
/// a value: a report confidently about the wrong thing.
///
/// Written before the wiring and watched to fail, so that it is known to be
/// capable of failing.
#[test]
fn mean_slice_abstains_when_the_two_sides_run_different_schedulers() {
    let run = calibrate();
    let r = run
        .results
        .iter()
        .find(|r| r.metric == Metric::MeanSliceLength)
        .expect("mean slice must APPEAR in the report, even when it abstains");

    assert_eq!(
        r.verdict,
        Verdict::NotMeasured,
        "guest and simulator run different schedulers, so this must abstain \
         rather than produce a number: {r:?}",
    );
    assert!(
        r.at.as_deref().is_some_and(|a| a.contains("scheduler")),
        "the abstention must say WHY, so a reader is not left guessing: {:?}",
        r.at,
    );
}

/// Percentile metrics cannot clear their floor from a single run, whatever the
/// numbers look like. Pinned so that a later change which starts reporting a
/// percentile as Agree from N=1 fails here.
#[test]
fn one_run_cannot_support_a_percentile_verdict_on_the_live_side() {
    use scxsim_calibration::MinSamples;
    assert_eq!(MinSamples::PERCENTILE.0, 100);
    let vm = VmRun::from_json(VM_SIDECAR).expect("parses");
    // One sidecar is one run; there is no second live observation in the tree.
    assert_eq!(SampleCount(1).0, 1, "N=1 on the live side");
    assert!(!vm.wake_latency_measured());

    let r = MetricResult::evaluate(
        Metric::WakeLatency,
        Some("p99".into()),
        Some(Quantity::Duration(DurationNs(1_000))),
        Some(Quantity::Duration(DurationNs(1_000))),
        SampleCount(1),
    );
    assert_eq!(
        r.verdict,
        Verdict::Inconclusive,
        "identical numbers from one sample are still not evidence"
    );
    let _ = Ratio(0.0);
}

/// The evidence for ruling `OffCpuTime` not-comparable, asserted rather than
/// asserted-in-a-comment.
///
/// Reclassifying a `Disagree` is the single most dangerous edit in this crate:
/// done wrongly it is indistinguishable from making an inconvenient result go
/// away. So the reason is encoded as a test over the same fixture, and it can
/// fail. If the guest's off-CPU time ever stops being dominated by
/// non-scheduling time, the premise of the reclassification is gone and this
/// goes red, pointing at the classification rather than at the workload.
///
/// The claim: the live off-CPU number is mostly NOT runqueue waiting, so it is
/// not measuring what a scheduler-fidelity comparison needs it to measure.
#[test]
fn off_cpu_is_dominated_by_non_scheduling_time() {
    let vm = VmRun::from_json(VM_SIDECAR).expect("the committed sidecar parses");

    for cg in &vm.stats.cgroups {
        assert!(
            cg.run_delay_measured,
            "{}: schedstat run_delay must be measured for this argument to \
             hold; without it there is no scheduler-attributable baseline to \
             compare against",
            cg.cgroup_name,
        );

        // Recover the wall the guest actually used, by inverting its own
        // off_cpu fraction against its own CPU time. Both cgroups must land on
        // the same wall — they are independently reported workers in one run,
        // so agreement here is what makes the inversion trustworthy.
        let cpu = cg.total_cpu_time_ns as f64;
        let off_frac = cg.off_cpu_fraction().get();
        let wall = cpu / (1.0 - off_frac);
        let off_cpu_ns = wall - cpu;
        let run_delay_ns = cg.mean_run_delay_us * 1_000.0;

        assert!(
            (wall - 12.0e9) / 12.0e9 > 0.0,
            "{}: the worker's wall window ({:.4}s) should exceed the 12s \
             scenario; if it does not, the inversion below is measuring \
             something else",
            cg.cgroup_name,
            wall / 1e9,
        );

        // The load-bearing assertion. 3x is deliberately far below the measured
        // 5.9x (cg_1) and 11.7x (cg_0) — the claim is "dominated", not a
        // particular ratio, and a bound hugging the observed value would be
        // fitting a number to the outcome.
        assert!(
            off_cpu_ns > 3.0 * run_delay_ns,
            "{}: off_cpu {:.2} ms is not >3x run_delay {:.2} ms. The \
             not-comparable classification of Metric::OffCpuTime rests on the \
             live number being mostly non-scheduling time; if that is no longer \
             true, re-evaluate the classification rather than this bound.",
            cg.cgroup_name,
            off_cpu_ns / 1e6,
            run_delay_ns / 1e6,
        );

        // And no single long stall, which would be a scheduling event and
        // would undercut the "virtualization overhead" reading.
        assert!(
            cg.max_gap_ms <= 2,
            "{}: max_gap_ms {} suggests a real stall, not finely distributed \
             overhead — the reclassification's reasoning would need revisiting",
            cg.cgroup_name,
            cg.max_gap_ms,
        );
    }
}

/// The pre-registered scheduling-delay bound REJECTS on cg_1, and this pins
/// exactly which arm each cgroup fails.
///
/// This test previously asserted the opposite — that both cgroups agreed, on
/// the absolute arm alone. That was measured against a lowering defect: ktstr's
/// `SpinWait` was lowered to repeating 500us chunks, so the phase ended every
/// slice and the scheduler's 20ms `SCX_SLICE_DFL` never bound. 24029 phase-
/// bound dispatches per task each contributed a fraction of a microsecond of
/// queueing, summing to a spurious 6.007ms that happened to land between the
/// two live values and read as agreement. `676b42f` fixed the lowering;
/// dispatches fell 39.8x to 603 and the simulated delay fell 39.9x with them —
/// the same factor, which is the mechanism, not a coincidence.
///
/// The bound was NOT touched in response, in either direction. It was derived
/// from what a policy comparison needs and committed before either side could
/// compute the quantity; it agreed when the data was wrong and rejects now that
/// the data is right, which is the whole reason for fixing it in advance.
#[test]
fn the_pre_registered_bound_rejects_scheduling_delay_on_cg_1() {
    let run = calibrate();
    let result = |cg: &str| {
        run.results
            .iter()
            .find(|r| r.metric == Metric::SchedulingDelay && r.at.as_deref() == Some(cg))
            .expect("scheduling delay was recorded per cgroup")
    };

    // Both cgroups fail the RELATIVE arm; that is not what separates them.
    for cg in ["cg_0", "cg_1"] {
        let gap = result(cg)
            .relative_gap()
            .expect("both sides supplied a value");
        assert!(
            gap > 0.20,
            "{cg}: relative gap {:.1}% is now inside the 20% arm — the simulator \
             would have become accurate on this quantity, which is a bigger \
             result than this test is guarding and should be read as one",
            gap * 100.0
        );
    }

    // What separates them is the absolute arm, and only because cg_0's live
    // value is small enough for a 4ms floor to still cover a 24.55x error.
    let delta = |cg: &str| {
        let r = result(cg);
        (r.sim.unwrap().value() - r.vm.unwrap().value()).abs()
    };
    assert!(
        delta("cg_0") <= 4_000_000.0,
        "cg_0: absolute gap {:.3}ms now exceeds the 4ms arm too",
        delta("cg_0") / 1e6
    );
    assert!(
        delta("cg_1") > 4_000_000.0,
        "cg_1: absolute gap {:.3}ms no longer exceeds the 4ms arm, so the \
         rejection below is not the one this test describes",
        delta("cg_1") / 1e6
    );

    assert_eq!(result("cg_0").verdict, Verdict::Agree);
    assert_eq!(result("cg_1").verdict, Verdict::Disagree);
}

/// The fixture CAN discriminate: the sim-vs-live gaps now exceed the live
/// side's own reproducibility.
///
/// cg_0 and cg_1 run the identical workload — one spinner each, two cgroups,
/// two CPUs — and the guest measured 3.694ms against 8.683ms, a 2.35x
/// between-worker spread. That spread is the floor on what any verdict here can
/// mean: a gap smaller than the reference's own variation is not resolvable.
///
/// The earlier version of this test asserted the spread EXCEEDED both gaps, and
/// concluded the fixture had no discriminating power. Against the fixed
/// lowering the gaps are 24.55x and 57.79x — an order of magnitude clear of the
/// spread. The rejection above is therefore a statement about the simulator and
/// not about fixture noise, and that distinction is what this test defends.
#[test]
fn the_gaps_now_exceed_the_live_sides_own_spread() {
    let vm = VmRun::from_json(VM_SIDECAR).expect("the committed sidecar parses");
    let a = vm.cgroup("cg_0").unwrap().mean_run_delay_us;
    let b = vm.cgroup("cg_1").unwrap().mean_run_delay_us;
    let live_spread = b.max(a) / b.min(a);

    let run = calibrate();
    let sim_gap = |cg: &str| -> f64 {
        let r = run
            .results
            .iter()
            .find(|r| r.metric == Metric::SchedulingDelay && r.at.as_deref() == Some(cg))
            .unwrap();
        let (s, v) = (r.sim.unwrap().value(), r.vm.unwrap().value());
        s.max(v) / s.min(v)
    };

    for cg in ["cg_0", "cg_1"] {
        assert!(
            sim_gap(cg) > 3.0 * live_spread,
            "{cg}: sim-vs-live gap {:.2}x is no longer comfortably clear of the \
             live side's own {live_spread:.2}x spread, so the discrepancy can no \
             longer be separated from fixture noise at N=1. The 3x margin is a \
             deliberately loose separation check, not a fidelity bound.",
            sim_gap(cg)
        );
    }
}

/// Direct dispatch is a real definitional difference and a negligible one here.
///
/// A simulator dispatch that skipped the enqueue path contributes zero delay,
/// where the kernel would still charge its wakeup-to-switch cost. That is the
/// one place the two definitions genuinely part company, so its size decides
/// whether it can explain anything. It cannot: 1 dispatch in 603. Even valued
/// at a generous 10us of wakeup path, that is 10us against a 3.5-8.5ms gap.
#[test]
fn direct_dispatch_cannot_explain_the_scheduling_delay_gap() {
    use scx_simulator::{DynamicScheduler, Simulator};

    fn gap_ns(run: &CalibrationRun, cg: &str) -> f64 {
        let r = run
            .results
            .iter()
            .find(|r| r.metric == Metric::SchedulingDelay && r.at.as_deref() == Some(cg))
            .unwrap();
        (r.sim.unwrap().value() - r.vm.unwrap().value()).abs()
    }
    let run = calibrate();

    let scenario = to_scenario(&lower(&scenario_source()).unwrap()).unwrap();
    let _guard = scx_simulator::SIM_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario.clone());
    let sim = SimRun::new(&scenario, &trace);
    for cg in ["cg_0", "cg_1"] {
        let (direct, total) = (sim.direct_dispatches(cg), sim.dispatches(cg));
        // Sample-size sanity, NOT a tolerance. The 68x lowering fix cut
        // dispatches per task from 24029 to ~603 by letting the scheduler's
        // 20ms slice bind, so the previous floor of 1000 would now fail on a
        // correct run. Lowered to 100, which still catches a scenario that
        // barely dispatched at all. Nothing about the metric's bound moves.
        assert!(total > 100, "{cg}: only {total} dispatches to reason about");
        // Bound the effect in TIME, not as a ratio of dispatches. The ratio
        // was the wrong proxy: the lowering fix cut the denominator 39.8x
        // without changing the numerator, so a fixed percentage threshold
        // started failing on a run where the absolute effect had not moved at
        // all. What the test actually claims is that this difference cannot
        // account for the observed gap, so measure that directly.
        //
        // 10us per wakeup path is deliberately generous — a real
        // wakeup-to-context-switch is nearer 1-5us — so the bound is
        // conservative in the direction that would refute the claim.
        const GENEROUS_WAKEUP_NS: u64 = 10_000;
        let unmodelled = direct * GENEROUS_WAKEUP_NS;
        let gap = gap_ns(&run, cg);
        assert!(
            (unmodelled as f64) < 0.05 * gap,
            "{cg}: {direct} skipped enqueue paths at a generous 10us each is \
             {}us, which is no longer negligible against the {:.0}us \
             scheduling-delay gap. Direct dispatch may now be part of the \
             explanation rather than a rounding error.",
            unmodelled / 1_000,
            gap / 1e3
        );
    }
}

/// The calibration's scheduling-delay definition is the SAME rule the simulator
/// already validates elsewhere, not a second one that can drift from it.
///
/// `SimRun::run_delay` reimplements the enqueue-to-dispatch accumulation rather
/// than calling `TraceStats`, to keep this crate's accessors uniform (every one
/// of them derives from the raw trace) and free of the warmup window
/// `TraceStats` applies. That is a duplicated definition, and duplicated
/// definitions drift. This pins them together: any future change to either
/// rule that does not change both will fail here.
///
/// `crates/scx_simulator/tests/rundelay_tracking.rs` is what validates the rule
/// itself — that it accumulates across preempt/re-enqueue cycles, that it
/// matches an independent recomputation from raw events, and that it grows with
/// load. This test inherits all of that by agreeing with it.
#[test]
fn the_calibrations_run_delay_is_the_simulators_own_rule() {
    use scx_simulator::{DynamicScheduler, Simulator, TraceStats};

    let scenario = to_scenario(&lower(&scenario_source()).unwrap()).unwrap();
    let _guard = scx_simulator::SIM_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario.clone());
    assert_eq!(
        trace.warmup_ns(),
        0,
        "a warmup window would legitimately \
        separate the two figures, and this test could not tell that apart from \
        a real divergence"
    );

    let stats = TraceStats::from_trace(&trace);
    let sim = SimRun::new(&scenario, &trace);
    for cg in ["cg_0", "cg_1"] {
        let pids: Vec<_> = sim.pids_in(cg).collect();
        let via_trace_stats: u64 = pids
            .iter()
            .map(|p| {
                stats
                    .tasks
                    .get(p)
                    .map(|t| t.sched_latencies.iter().sum::<u64>())
                    .unwrap_or(0)
            })
            .sum::<u64>()
            / pids.len() as u64;
        assert!(
            via_trace_stats > 0,
            "{cg}: no run delay from either rule, so agreement between them \
             would be vacuous"
        );
        assert_eq!(
            sim.run_delay(cg),
            Some(DurationNs(via_trace_stats)),
            "{cg}: the calibration accessor and TraceStats::sched_latencies \
             disagree, so one of the two rules has changed"
        );
    }
}
