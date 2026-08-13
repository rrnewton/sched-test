//! Every `#[ktstr_scenario]` test, executed on the simulator.
//!
//! The records under `records/` are not hand-written. They are the output of
//! ktstr's `export_registered_scenarios` test, which walks the real
//! `KTSTR_SCENARIOS` registry and its paired `KtstrTestEntry`, so the workloads
//! here are the ones the VM backend runs — not restatements of them.
//!
//! # What this file is for
//!
//! Not collecting green ticks. The point is to find where the pipeline stops
//! being faithful, so:
//!
//! * a non-`Exact` fidelity report is a **success** — the lowering identified
//!   something it cannot express instead of approximating it silently;
//! * a scenario that cannot compile is a **result** — it names a missing
//!   capability;
//! * and a scenario that runs while quietly dropping the property it exists to
//!   test is the **worst** outcome, because nothing in the fidelity machinery
//!   reports it.
//!
//! `sched_cpuset_split` is that third case today (sim-4qlh5), which is why
//! [`cpuset_violations`] measures where tasks actually ran instead of trusting
//! the report. The lowering carries its disjoint cpusets faithfully all the way
//! into `Scenario.cgroups[].cpuset` and reports `Exact` — correctly, as far as
//! it can see. The simulator then does not enforce them, which is downstream of
//! anything the IR knows about.
//!
//! # The VM side, for comparison
//!
//! `sched_basic_proportional` is green on a source-built 6.14.11:
//!
//! ```text
//! PASS [19.088s] (1/1) ktstr::ktstr_sched_tests ktstr/sched_basic_proportional
//! cg_0  num_workers 1  total_cpu_time_ns 12_010_668_525
//! cg_1  num_workers 1  total_cpu_time_ns 12_002_274_341   -> spread 0.07%
//! ```
//!
//! # What is NOT claimed
//!
//! The two backends run different schedulers. The VM runs `scx-ktstr`; the
//! simulator runs `simple`, because `scx-ktstr` is ktstr's own BPF scheduler and
//! is not one of the simulator's. Both are minimal global-DSQ schedulers, so the
//! fairness question is meaningful on either, but this is a scenario-level
//! comparison and not a scheduler-level one. A reader who assumed scheduler
//! parity would over-read any agreement.
//!
//! # Why expectations are per scenario
//!
//! An earlier version asserted one global "cgroups should get comparable CPU
//! time" bound. That is wrong for `sched_dynamic_add`, whose second cgroup
//! starts half-way through and *correctly* gets half the CPU. A single bound
//! loose enough to pass that would be unable to fail for the scenarios where
//! equality genuinely is the invariant. Each scenario states its own expected
//! share.

use std::collections::BTreeMap;
use std::path::PathBuf;

use ktstr_scenario_replay::{
    cgroup_cpu_time, compile, cpu_time_spread, cpuset_violations, observed_placement, Compiled,
};
use scx_simulator::prelude::*;

/// What we expect of one scenario's per-cgroup CPU-time split.
enum Share {
    /// Every cgroup should get comparable CPU time, within the given fraction.
    Even { tolerance: f64 },
    /// `larger` should get roughly `ratio` times `smaller`'s CPU time — the
    /// staged-start shape.
    Ratio {
        larger: &'static str,
        smaller: &'static str,
        ratio: f64,
        tolerance: f64,
    },
    /// A single cgroup: the split question is not meaningful, and asserting
    /// anything about it would be asserting a tautology.
    SingleCgroup,
}

struct Case {
    name: &'static str,
    cgroups: &'static [&'static str],
    /// CPUs the scenario declares, checked against the ingested topology.
    nr_cpus: u32,
    share: Share,
    /// What part of the pipeline this scenario is the first to exercise.
    exercises: &'static str,
}

const CASES: &[Case] = &[
    Case {
        name: "sched_basic_proportional",
        cgroups: &["cg_0", "cg_1"],
        nr_cpus: 2,
        share: Share::Even { tolerance: 0.25 },
        exercises: "baseline: two spinners, two cgroups, two CPUs",
    },
    Case {
        name: "sched_cpuset_split",
        cgroups: &["cg_0", "cg_1"],
        nr_cpus: 4,
        // Note this passes whether or not the cpusets are honoured — equal
        // spinners get equal CPU either way. That is exactly why the cpuset
        // check below cannot be folded into the share check.
        share: Share::Even { tolerance: 0.25 },
        exercises: "disjoint cgroup cpusets on a 4-CPU machine",
    },
    Case {
        name: "sched_dynamic_add",
        cgroups: &["cg_0", "cg_1"],
        nr_cpus: 2,
        // cg_1 is created at the half-way point, so it can only accrue half of
        // cg_0's CPU time. Tight tolerance: this is arithmetic, not a race.
        share: Share::Ratio {
            larger: "cg_0",
            smaller: "cg_1",
            ratio: 2.0,
            tolerance: 0.05,
        },
        exercises: "staged start: a second step adds a cgroup mid-run",
    },
    Case {
        name: "sched_verifier_stats_populated",
        cgroups: &["cg_0"],
        nr_cpus: 2,
        share: Share::SingleCgroup,
        exercises: "single cgroup, short (2s) duration",
    },
    Case {
        name: "sched_perf_positive",
        cgroups: &["cg_0"],
        nr_cpus: 2,
        share: Share::SingleCgroup,
        exercises: "single cgroup; its Assert override is dropped at export",
    },
];

fn record(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("records")
        .join(format!("{name}.json"))
}

fn run(compiled: &Compiled) -> Trace {
    let so = format!("{}/libscx_simple.so", env!("REPLAY_SO_DIR"));
    let def = SchedulerDefinition::new("simple")
        .with_strip_const(false)
        .with_scx_bpf_dir(false);
    let sched = DynamicScheduler::load_with_definition(&so, &def, compiled.scenario.nr_cpus);
    Simulator::new(sched).run(compiled.scenario.clone())
}

fn check_share(case: &Case, by_cgroup: &BTreeMap<String, u64>) {
    match case.share {
        Share::SingleCgroup => {
            assert_eq!(
                by_cgroup.len(),
                1,
                "{}: expected a single cgroup; got {by_cgroup:#?}",
                case.name,
            );
            assert!(
                cpu_time_spread(by_cgroup).is_none(),
                "{}: a one-cgroup spread must be None, not a 0.0 that reads as a pass",
                case.name,
            );
        }
        Share::Even { tolerance } => {
            let spread = cpu_time_spread(by_cgroup)
                .unwrap_or_else(|| panic!("{}: need two cgroups to compare", case.name));
            assert!(
                spread < tolerance,
                "{}: equal-weight cgroups running identical work should get \
                 comparable CPU time; spread was {:.4}% over {by_cgroup:#?}",
                case.name,
                spread * 100.0,
            );
        }
        Share::Ratio {
            larger,
            smaller,
            ratio,
            tolerance,
        } => {
            let big = *by_cgroup
                .get(larger)
                .unwrap_or_else(|| panic!("{}: no cgroup {larger}", case.name));
            let small = *by_cgroup
                .get(smaller)
                .unwrap_or_else(|| panic!("{}: no cgroup {smaller}", case.name));
            assert!(small > 0, "{}: {smaller} got no CPU time", case.name);
            let actual = big as f64 / small as f64;
            assert!(
                (actual - ratio).abs() / ratio < tolerance,
                "{}: expected {larger}:{smaller} ~= {ratio}:1, got {actual:.4}:1 \
                 over {by_cgroup:#?}",
                case.name,
            );
        }
    }
}

/// Compile and run every exported scenario, and report what each one cost in
/// fidelity.
///
/// A compile failure fails the test — that is a bridge regression. A non-`Exact`
/// fidelity report does NOT: it is the lowering doing its job, and the report is
/// printed so the loss is on the record rather than in a log nobody reads.
#[test]
fn every_ktstr_scenario_executes_on_the_simulator() {
    let _guard = scx_simulator::SIM_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let mut summary: Vec<String> = Vec::new();

    for case in CASES {
        let compiled = compile(&record(case.name))
            .unwrap_or_else(|e| panic!("{}: compile the exported record: {e}", case.name));

        assert_eq!(
            compiled.scenario.nr_cpus, case.nr_cpus,
            "{}: topology came from the record",
            case.name,
        );
        let mut got: Vec<&str> = compiled
            .scenario
            .tasks
            .iter()
            .filter_map(|t| t.cgroup_name.as_deref())
            .collect();
        got.sort_unstable();
        got.dedup();
        assert_eq!(got, case.cgroups, "{}: cgroup set", case.name);

        let approximations = compiled.ir.fidelity.approximations().len();
        println!("\n=== {} — {} ===", case.name, case.exercises);
        println!("{}", scxsim_workload_ir::pretty(&compiled.ir));
        if approximations > 0 {
            println!("--- {approximations} approximation(s) ---");
            println!("{:#?}", compiled.ir.fidelity);
        }

        let trace = run(&compiled);
        assert_eq!(
            *trace.exit_kind(),
            ExitKind::Normal,
            "{}: simulation must terminate normally; got {:?}",
            case.name,
            trace.exit_kind(),
        );

        let by_cgroup = cgroup_cpu_time(&compiled.scenario, &trace);
        for (cg, ns) in &by_cgroup {
            assert!(*ns > 0, "{}: cgroup {cg} got no CPU time", case.name);
        }
        println!("per-cgroup CPU time (ns): {by_cgroup:?}");
        println!(
            "placement: {:?}",
            observed_placement(&compiled.scenario, &trace)
        );
        check_share(case, &by_cgroup);

        let violations = cpuset_violations(&compiled.scenario, &trace);
        let cpuset_note = if violations.is_empty() {
            "cpuset ok".to_string()
        } else {
            for v in &violations {
                println!(
                    "CPUSET VIOLATION (sim-4qlh5): {} declared {:?} but ran on \
                     {:?}, escaping to {:?}",
                    v.cgroup, v.declared, v.observed, v.escaped_to,
                );
            }
            format!("{} CPUSET VIOLATION(S) — sim-4qlh5", violations.len())
        };

        summary.push(format!(
            "  {:<32} {:>2} approx  {}",
            case.name, approximations, cpuset_note,
        ));
    }

    println!("\n=== fidelity summary ===");
    for line in &summary {
        println!("{line}");
    }
}

/// Correct-behaviour guard for sim-4qlh5: a cgroup confined by `cpuset.cpus`
/// must not run outside it.
///
/// Was `#[ignore]`d as a failing acceptance test for sim-4qlh5; the fix landed
/// (`Scenario::effective_cpuset` narrows a task's cpumask by its cgroup's
/// cpuset at `engine.rs:1499`) and this now runs by default.
///
/// Measured before and after, from `TaskScheduled` events on the same 4-CPU
/// scenario with cg_0 declared on [0,1] and cg_1 on [2,3]:
///
/// ```text
/// before  placement: {"cg_0": {1}, "cg_1": {0}}   cg_1 escaped to CPU 0
/// after   placement: {"cg_0": {0}, "cg_1": {2}}   each inside its own half
/// ```
///
/// The assertion is known to discriminate: reverting the intersection to the
/// old `def.allowed_cpus` turns this red, and restoring it turns it green
/// again. It is not a test that always passes.
///
/// Separate from the sweep above rather than an assertion inside it, so that a
/// regression here names cpuset enforcement specifically instead of failing
/// mid-sweep and masking the scenarios after it.
#[test]
fn cgroup_cpusets_are_enforced() {
    let _guard = scx_simulator::SIM_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let compiled = compile(&record("sched_cpuset_split")).expect("compile");
    let trace = run(&compiled);
    let violations = cpuset_violations(&compiled.scenario, &trace);
    assert!(
        violations.is_empty(),
        "a cgroup confined by cpuset.cpus must not run outside it. In the \
         kernel this is impossible — cpuset.cpus narrows every member task's \
         effective cpumask, so the scheduler cannot place the task elsewhere. \
         Violations: {violations:#?}",
    );

    // Containment is necessary but not sufficient: both cgroups sharing one
    // CPU inside a shared cpuset would satisfy it. What this scenario exists
    // to test is that the halves are DISJOINT, so assert that directly.
    let placement = observed_placement(&compiled.scenario, &trace);
    let cg0 = placement.get("cg_0").expect("cg_0 ran");
    let cg1 = placement.get("cg_1").expect("cg_1 ran");
    assert!(
        cg0.is_disjoint(cg1),
        "cg_0 and cg_1 are declared on disjoint cpuset halves, so the CPUs they \
         actually ran on must not overlap; got cg_0={cg0:?} cg_1={cg1:?}",
    );
}
