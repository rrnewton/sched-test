//! The two backends must agree about a number, and this is where that is
//! enforced.
//!
//! `replay.rs` next door asserts that every ktstr scenario **executes** on the
//! simulator and that declared cpusets are honoured. `cgroup_cpusets_are_enforced`
//! asserts containment. Neither compares the simulator against the VM, which is
//! how a 2x per-cgroup disagreement on `sched_dynamic_add` sat in a green tree:
//! both suites were doing exactly what they said, and neither said this.
//!
//! # What "the VM" means here
//!
//! A committed baseline under `baselines/`, extracted by
//! `scripts/record_vm_baseline.py` from a ktstr stats sidecar. The check
//! therefore asks *"does the simulator still agree with the VM run we
//! recorded"*, not *"does it agree with the VM right now"* — the trade that
//! keeps this a normal `cargo test` instead of a nightly needing a kernel, a VM
//! and root. Each baseline carries the kernel, ktstr commit and recording date
//! so the reference can be judged, and re-recording is one script invocation.
//!
//! # The caveat that limits every green here
//!
//! **The two backends run different schedulers.** The VM runs `scx-ktstr`,
//! ktstr's own BPF scheduler; the simulator runs `simple`, because `scx-ktstr`
//! is not one of the simulator's. Both are minimal global-DSQ schedulers, so a
//! share question is meaningful on either — but this is a scenario-level
//! comparison, not a scheduler-level one, and a reader who took agreement here
//! as evidence that the simulator reproduces a *specific scheduler* would be
//! over-reading it.
//!
//! # Three outcomes, not two
//!
//! A scenario is declared [`Expect::Agree`] or [`Expect::KnownDivergence`].
//!
//! The second exists because `sched_dynamic_add` diverges today (sim-dk2st) and
//! the divergence is a real finding that is tracked, not something to
//! accommodate. Two wrong ways to handle that: widen the bound until it passes,
//! which destroys the check for every other scenario; or `#[ignore]` it, which
//! is a gate that cannot fail — the shape this whole task exists to remove.
//!
//! So a known divergence is asserted to **still be there, and still look the
//! same**. It fails if it disappears (go and promote it), and it fails if its
//! magnitude moves outside a tight drift band (something changed and nobody
//! noticed). The pinned magnitude is NOT a tolerance on correctness; it is a
//! tripwire on a number that is currently wrong, and it is deliberately much
//! tighter than the agreement bound.

use std::collections::BTreeMap;
use std::path::PathBuf;

use ktstr_scenario_replay::{
    cgroup_cpu_time, compare, compile, load_baseline, Comparison, Compiled, Mismatch,
};
use scx_simulator::prelude::*;

/// What this scenario's two backends are expected to do.
enum Expect {
    /// The backends agree within the pre-registered bound in
    /// `ktstr_scenario_replay::compare`.
    Agree,
    /// The backends are known to disagree, and the disagreement is tracked.
    KnownDivergence {
        /// The issue that owns it. A divergence without an owner is a bug
        /// somebody decided to live with silently.
        issue: &'static str,
        /// The worst absolute share delta observed when this expectation was
        /// written. See the module docs: a tripwire, not a tolerance.
        worst_share_delta: f64,
        /// How far that may move before this test fails, in share units.
        drift: f64,
        /// One line on what the divergence is, so a failure here does not send
        /// the reader to the issue tracker to find out what broke.
        what: &'static str,
    },
}

struct Case {
    name: &'static str,
    expect: Expect,
}

const CASES: &[Case] = &[
    Case {
        name: "sched_basic_proportional",
        expect: Expect::Agree,
    },
    Case {
        name: "sched_cpuset_split",
        expect: Expect::Agree,
    },
    Case {
        name: "sched_dynamic_add",
        expect: Expect::KnownDivergence {
            issue: "sim-dk2st",
            // Filled from a measured run; see the module docs for why this is
            // pinned rather than tolerated.
            worst_share_delta: 0.1663,
            drift: 0.02,
            what: "step teardown: the simulator keeps cg_0 running through \
                   step 1 while the VM stops it at the phase boundary, so the \
                   simulator's cg_0 gets the whole run and the VM's gets half. \
                   Which side is correct is NOT established — see the issue.",
        },
    },
    Case {
        name: "sched_perf_positive",
        expect: Expect::Agree,
    },
    Case {
        name: "sched_verifier_stats_populated",
        expect: Expect::Agree,
    },
];

fn record(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("records")
        .join(format!("{name}.json"))
}

fn baseline_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("baselines")
        .join(format!("{name}.vm.json"))
}

fn run(compiled: &Compiled) -> Trace {
    let so = format!("{}/libscx_simple.so", env!("REPLAY_SO_DIR"));
    let def = SchedulerDefinition::new("simple")
        .with_strip_const(false)
        .with_scx_bpf_dir(false);
    let sched = DynamicScheduler::load_with_definition(&so, &def, compiled.scenario.nr_cpus);
    Simulator::new(sched).run(compiled.scenario.clone())
}

/// Run one scenario on the simulator and compare it against its VM baseline.
fn compare_backends(name: &str) -> Comparison {
    let baseline = load_baseline(&baseline_path(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
    assert_eq!(
        baseline.scenario, name,
        "{name}: baseline file records a different scenario ({}); the baselines \
         directory has been shuffled",
        baseline.scenario,
    );
    assert!(
        baseline.provenance.passed,
        "{name}: the recorded VM run did not pass, so it is not a reference \
         anything should be held to",
    );

    let compiled =
        compile(&record(name)).unwrap_or_else(|e| panic!("{name}: compile the record: {e}"));
    let trace = run(&compiled);
    assert_eq!(
        *trace.exit_kind(),
        ExitKind::Normal,
        "{name}: the simulator must terminate normally before its numbers mean \
         anything; got {:?}",
        trace.exit_kind(),
    );

    let sim: BTreeMap<String, u64> = cgroup_cpu_time(&compiled.scenario, &trace);
    compare(&baseline.per_cgroup_cpu_time_ns, &sim)
}

/// The check. One ktstr definition, two backends, one number, held to a bound
/// fixed before the number was read.
#[test]
fn backends_agree_on_per_cgroup_cpu_time() {
    let _guard = scx_simulator::SIM_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let mut summary: Vec<String> = Vec::new();

    for case in CASES {
        let cmp = compare_backends(case.name);
        println!("\n=== {} ===\n{}", case.name, cmp.report());

        match case.expect {
            Expect::Agree => {
                assert!(
                    cmp.agrees(),
                    "{}: the VM and the simulator disagree beyond the bound.\n{}\n\n\
                     mismatches: {:#?}\n\n\
                     This is the check's whole purpose, so do not reach for the \
                     bound first. Either the simulator changed behaviour, or the \
                     baseline is stale for this kernel/ktstr revision (see its \
                     provenance block) — establish which before touching either.",
                    case.name,
                    cmp.report(),
                    cmp.mismatches,
                );
                summary.push(format!(
                    "  {:<32} AGREE     worst delta {:.4}",
                    case.name,
                    cmp.worst_share_delta()
                ));
            }
            Expect::KnownDivergence {
                issue,
                worst_share_delta,
                drift,
                what,
            } => {
                assert!(
                    !cmp.agrees(),
                    "{}: this scenario is recorded as a KNOWN DIVERGENCE ({issue}) \
                     and the backends now AGREE.\n{}\n\n\
                     What it was: {what}\n\n\
                     If {issue} was fixed, change this case to Expect::Agree and \
                     close the issue. A stale expected-failure is a gate that \
                     cannot fail.\n\n\
                     KNOWN-GAP TEST: this going red means the gap CLOSED. Invert \
                     this assertion to assert the property now holds. Do not \
                     delete it, and do not loosen the bound.",
                    case.name,
                    cmp.report(),
                );
                let observed = cmp.worst_share_delta();
                assert!(
                    (observed - worst_share_delta).abs() <= drift,
                    "{}: known divergence {issue} has MOVED. Pinned worst share \
                     delta {worst_share_delta:.4} +/-{drift}, observed \
                     {observed:.4}.\n{}\n\n\
                     What it was: {what}\n\n\
                     The magnitude is pinned so a tracked divergence cannot \
                     silently become a different one. Work out what changed \
                     before re-pinning.\n\n\
                     KNOWN-GAP TEST: this going red means the gap CLOSED. Invert \
                     this assertion to assert the property now holds. Do not \
                     delete it, and do not loosen the bound.",
                    case.name,
                    cmp.report(),
                );
                summary.push(format!(
                    "  {:<32} DIVERGE   worst delta {observed:.4}  ({issue}, pinned {worst_share_delta:.4} +/-{drift})",
                    case.name,
                ));
            }
        }
    }

    println!("\n=== cross-backend summary ===");
    for line in &summary {
        println!("{line}");
    }
}

/// KNOWN GAP: the VM and the simulator do not agree on `sched_dynamic_add`'s
/// per-cgroup CPU time (sim-dk2st).
///
/// WHY EXPECTED: step teardown differs. The simulator keeps `cg_0` running
/// through step 1 while the VM stops it at the phase boundary, so the
/// simulator's `cg_0` accrues the whole run and the VM's accrues half. The VM
/// sidecar corroborates it structurally — `phases[0].per_cgroup` holds only
/// `cg_0` and `phases[1].per_cgroup` only `cg_1`. Which side is CORRECT is not
/// established; see the issue before assuming the VM is the oracle.
///
/// WHEN THIS GOES RED: the gap has closed. Invert the assertion — assert the
/// backends now agree — flip the `sched_dynamic_add` case above to
/// [`Expect::Agree`], and close sim-dk2st.
///
/// DO NOT: delete it (silently drops the coverage), or loosen the bound to make
/// it pass (destroys the property that made it worth having).
///
/// This test does double duty, and the second job is why it is separate from
/// the sweep above. A comparison oracle that has never been seen red is
/// indistinguishable from one that CANNOT go red, and this repository has
/// removed several gates that turned out to be the latter. `sched_dynamic_add`
/// is the one real disagreement available, so asserting the bound fires on it
/// is a demonstration on real data rather than on a synthetic fixture. The unit
/// tests in `compare.rs` pin the bound's edges; this pins that it fires on the
/// disagreement we actually have.
///
/// SOUNDNESS OF THE DETECTOR: `!cmp.agrees()` alone would be satisfied by a
/// failure for an unrelated reason — a missing cgroup, an empty side — which
/// would keep this green while telling us nothing about the share bound. So it
/// additionally asserts the failure is a [`Mismatch::Share`] specifically. A
/// detector that can be satisfied by the wrong mechanism is the trap the
/// `known_gap_` convention in `scx-sim/CLAUDE.md` documents.
#[test]
fn known_gap_sched_dynamic_add_backends_disagree_on_cpu_time() {
    let _guard = scx_simulator::SIM_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let cmp = compare_backends("sched_dynamic_add");
    assert!(
        !cmp.agrees(),
        "sched_dynamic_add is the one scenario whose backends are known to \
         disagree (sim-dk2st). If the bound passes it, the bound is not \
         measuring anything and every green in this file is worthless.\n{}\n\n\
         KNOWN-GAP TEST: this going red means the gap CLOSED. Invert this \
         assertion to assert the property now holds. Do not delete it, and do \
         not loosen the bound.",
        cmp.report(),
    );

    let share_mismatches = cmp
        .mismatches
        .iter()
        .filter(|m| matches!(m, Mismatch::Share(_)))
        .count();
    assert!(
        share_mismatches > 0,
        "sched_dynamic_add must fail on the SHARE bound specifically, not \
         incidentally on cgroup-set or gross-total — otherwise this test is \
         green for the wrong reason and certifies nothing about the bound. \
         Got: {:#?}\n\n\
         KNOWN-GAP TEST: this going red means the gap CLOSED. Invert this \
         assertion to assert the property now holds. Do not delete it, and do \
         not loosen the bound.",
        cmp.mismatches,
    );

    println!(
        "the bound fires on sched_dynamic_add, as expected:\n{}",
        cmp.report()
    );
}

/// Every scenario with a replay record has a VM baseline.
///
/// Without this, adding a sixth scenario to `records/` and forgetting its
/// baseline would quietly reduce the cross-backend check's coverage while every
/// test stayed green — the same silent-narrowing failure the check exists to
/// stop, one level up.
#[test]
fn every_record_has_a_baseline_and_a_declared_expectation() {
    let records_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("records");
    let mut records: Vec<String> = std::fs::read_dir(&records_dir)
        .expect("records/ must exist")
        .filter_map(|e| {
            let p = e.ok()?.path();
            (p.extension()? == "json")
                .then(|| p.file_stem()?.to_str().map(str::to_string))
                .flatten()
        })
        .collect();
    records.sort();

    let mut declared: Vec<String> = CASES.iter().map(|c| c.name.to_string()).collect();
    declared.sort();

    assert_eq!(
        records, declared,
        "every scenario record must have a declared cross-backend expectation. \
         Records: {records:?}; declared: {declared:?}. Add the missing case (and \
         its baseline) rather than letting coverage shrink silently.",
    );

    for name in &records {
        let p = baseline_path(name);
        assert!(
            p.exists(),
            "{name}: no VM baseline at {}. Record one with \
             scripts/record_vm_baseline.py <ktstr-run-dir>.",
            p.display(),
        );
    }
}
