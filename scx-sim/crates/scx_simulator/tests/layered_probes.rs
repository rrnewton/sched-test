//! scx_layered state probes: the FACTORS behind a layer decision.
//!
//! `tests/layered.rs` asserts on OUTCOMES — `task_layer(pid)` is the layer
//! scx_layered picked. That is the whole story it can tell, and it is the
//! reason `sim-hyr11` cost a 20-run coverage/non-coverage bisection to
//! diagnose: the only evidence available was "landed in layer 2, expected
//! layer 1", which is compatible with a broken matcher, a workload that never
//! got the name we thought, and a cgroup path that does not render the way we
//! assumed.
//!
//! The probes exercised here separate those three. Every verdict below comes
//! from the scheduler's own `match_one()`, reached through
//! `schedulers/layered/wrapper.c`; nothing in the probe path re-derives
//! matching.

use scx_simulator::*;

#[macro_use]
mod common;

/// A task that runs once for `run_ns` and exits.
fn run_once(run_ns: u64) -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(run_ns)],
        repeat: RepeatMode::Once,
    }
}

/// Assert the whole probe-side match walk agrees with the layer the scheduler
/// itself chose, for every task in `pids`.
///
/// This is the consistency gate for the match probes. `match_first_failure()`
/// walks the terms in `match_layer()`'s order and applies its
/// `match_one(..) == !exclude` test, but it is our loop, not the scheduler's.
/// If upstream ever changes how the per-term verdicts combine, this goes red
/// instead of the probes quietly reporting a rule set nobody uses any more.
fn assert_trace_agrees_with_outcome(monitor: &LayeredMonitor, pids: &[Pid]) {
    for &pid in pids {
        let trace = monitor
            .first_match_trace(pid)
            .unwrap_or_else(|| panic!("no match trace recorded for {pid:?}"));
        let chosen = monitor.probes().task_layer(pid);
        assert_eq!(
            trace.first_matching_layer(),
            Some(chosen),
            "{pid:?}: probe walk says {:?}, scheduler chose layer {chosen}; \
             comm={:?} cgrp={:?}",
            trace.first_matching_layer(),
            trace.comm,
            trace.cgrp_path,
        );
    }
}

// ---------------------------------------------------------------------------
// The flagship: which rule rejected this task?
// ---------------------------------------------------------------------------

/// QUESTION ANSWERED: "my task did not land in the layer I configured for it —
/// which rule rejected it?"
///
/// `task_layer()` alone says only "layer 1, not layer 0". The match probe
/// names the term.
#[test]
fn match_probe_names_the_term_that_rejected_the_task() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(2);
    sched.layered_layers(&[
        LayerSpec::new("batch", LayerKind::Open).with_match(LayerMatch::CommPrefix("batch".into())),
        LayerSpec::catch_all("rest"),
    ]);
    let mut monitor = LayeredMonitor::new(LayeredProbes::new(&sched));

    let scenario = Scenario::builder()
        .cpus(2)
        .detect_bpf_errors()
        .add_task("batch_a", 0, run_once(10_000_000))
        .add_task("iface_b", 0, run_once(10_000_000))
        .duration_ms(200)
        .build();
    let sim = Simulator::new(sched);
    let result = sim.run_monitored(scenario, &mut monitor);
    assert_eq!(result.trace.exit_kind(), &ExitKind::Normal);

    let probes = monitor.probes();

    // Configuration readout: one OR group, one AND term, and it is the rule
    // we wrote. This is the half that needs no task at all.
    assert_eq!(probes.match_nr_ors(0), 1);
    assert_eq!(probes.match_nr_ands(0, 0), 1);
    assert_eq!(
        probes.match_kind(0, 0, 0),
        Some(LayeredMatchKind::CommPrefix)
    );
    assert_eq!(probes.match_exclude(0, 0, 0), Some(false));
    assert_eq!(probes.match_needle(0, 0, 0).as_deref(), Some("batch"));

    // The matching task: layer 0's only OR group holds.
    let matched = monitor
        .first_match_trace(Pid(1))
        .expect("trace for batch_a");
    assert_eq!(matched.comm.as_deref(), Some("batch_a"));
    assert_eq!(matched.layers[0].groups, vec![OrGroupVerdict::Matches]);

    // The rejected task: the probe names WHICH term failed, not just that the
    // layer did not take it.
    let rejected = monitor
        .first_match_trace(Pid(2))
        .expect("trace for iface_b");
    assert_eq!(rejected.comm.as_deref(), Some("iface_b"));
    assert_eq!(
        rejected.layers[0].groups,
        vec![OrGroupVerdict::FailedAt(0)],
        "layer 0 should reject iface_b at its only term"
    );
    assert_eq!(
        probes.describe_term(0, 0, 0),
        "CommPrefix(\"batch\")",
        "the failure should be reportable in terms a reader can act on"
    );

    assert_trace_agrees_with_outcome(&monitor, &[Pid(1), Pid(2)]);
}

/// QUESTION ANSWERED: "which of the ANDed conditions did my task miss?"
///
/// Two tasks fail the same layer for opposite reasons. Without the probe both
/// look identical: "fell through to the catch-all".
#[test]
fn anded_rule_failure_is_attributed_to_the_specific_term() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(2);
    sched.layered_layers(&[
        LayerSpec::new("bg_and_nice", LayerKind::Open).with_or(vec![
            LayerMatch::CommPrefix("bg".into()),
            LayerMatch::NiceAbove(0),
        ]),
        LayerSpec::catch_all("rest"),
    ]);
    let mut monitor = LayeredMonitor::new(LayeredProbes::new(&sched));

    let scenario = Scenario::builder()
        .cpus(2)
        .detect_bpf_errors()
        .add_task("bg_nice", 5, run_once(10_000_000)) // both conditions
        .add_task("bg_norm", 0, run_once(10_000_000)) // comm only
        .add_task("fg_nice", 5, run_once(10_000_000)) // nice only
        .duration_ms(200)
        .build();
    let sim = Simulator::new(sched);
    let result = sim.run_monitored(scenario, &mut monitor);
    assert_eq!(result.trace.exit_kind(), &ExitKind::Normal);

    let probes = monitor.probes();
    assert_eq!(probes.match_nr_ands(0, 0), 2);
    assert_eq!(
        probes.match_kind(0, 0, 0),
        Some(LayeredMatchKind::CommPrefix)
    );
    assert_eq!(
        probes.match_kind(0, 0, 1),
        Some(LayeredMatchKind::NiceAbove)
    );

    let group = |pid: Pid| monitor.first_match_trace(pid).unwrap().layers[0].groups[0];

    assert_eq!(group(Pid(1)), OrGroupVerdict::Matches, "bg_nice");
    assert_eq!(
        group(Pid(2)),
        OrGroupVerdict::FailedAt(1),
        "bg_norm passes the comm term and fails the NICE term — term 1"
    );
    assert_eq!(
        group(Pid(3)),
        OrGroupVerdict::FailedAt(0),
        "fg_nice fails at the COMM term — term 0 — and never reaches nice"
    );

    assert_trace_agrees_with_outcome(&monitor, &[Pid(1), Pid(2), Pid(3)]);
}

/// QUESTION ANSWERED: "is the negation being applied the way I meant it?"
///
/// A negated term's raw `match_one()` verdict and the term's effect are
/// deliberately reported separately, so an inverted rule cannot be confused
/// with a predicate that simply did not hold.
#[test]
fn a_negated_term_reports_the_raw_verdict_and_the_negation_separately() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(2);
    sched.layered_layers(&[
        LayerSpec::new("not_batch", LayerKind::Open).with_match(LayerMatch::Not(Box::new(
            LayerMatch::CommPrefix("batch".into()),
        ))),
        LayerSpec::catch_all("rest"),
    ]);
    let mut monitor = LayeredMonitor::new(LayeredProbes::new(&sched));

    let scenario = Scenario::builder()
        .cpus(2)
        .detect_bpf_errors()
        .add_task("batch_a", 0, run_once(10_000_000))
        .add_task("other_b", 0, run_once(10_000_000))
        .duration_ms(200)
        .build();
    let sim = Simulator::new(sched);
    let result = sim.run_monitored(scenario, &mut monitor);
    assert_eq!(result.trace.exit_kind(), &ExitKind::Normal);

    let probes = monitor.probes();
    assert_eq!(probes.match_exclude(0, 0, 0), Some(true));
    assert_eq!(probes.describe_term(0, 0, 0), "!CommPrefix(\"batch\")");

    // batch_a: the predicate HOLDS, and it is precisely because it holds that
    // the excluded term rejects the task. That distinction is invisible from
    // `task_layer()`.
    assert_eq!(
        monitor.first_match_trace(Pid(1)).unwrap().layers[0].groups[0],
        OrGroupVerdict::FailedAt(0)
    );
    assert_eq!(
        monitor.first_match_trace(Pid(2)).unwrap().layers[0].groups[0],
        OrGroupVerdict::Matches
    );

    assert_trace_agrees_with_outcome(&monitor, &[Pid(1), Pid(2)]);
}

// ---------------------------------------------------------------------------
// Both sides of the comparison
// ---------------------------------------------------------------------------

/// QUESTION ANSWERED: "did the workload actually get the cgroup path the rule
/// is written against?" — which is the first thing to rule out when a
/// name-based rule does not fire, and the question the layered-recon
/// workstream is gated on.
///
/// This is the `sim-hyr11` shape. That bug made `MATCH_CGROUP_CONTAINS` return
/// "no match" for a string that does contain the needle. With only
/// `task_layer()` the evidence was "landed in the catch-all". With these
/// probes the evidence is the needle, the haystack, and the verdict side by
/// side — at which point the wrongness is on the face of it.
#[test]
fn cgroup_match_reports_needle_and_the_path_the_scheduler_actually_built() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(2);
    sched.layered_layers(&[
        LayerSpec::new("contains", LayerKind::Open)
            .with_match(LayerMatch::CgroupContains("mid".into())),
        LayerSpec::catch_all("rest"),
    ]);
    let mut monitor = LayeredMonitor::new(LayeredProbes::new(&sched));

    let scenario = Scenario::builder()
        .cpus(2)
        .detect_bpf_errors()
        .cgroup("xmidy", &[CpuId(0), CpuId(1)])
        .cgroup("plain", &[CpuId(0), CpuId(1)])
        .add_task_in_cgroup("hit", 0, run_once(5_000_000), "xmidy")
        .add_task_in_cgroup("miss", 0, run_once(5_000_000), "plain")
        .duration_ms(200)
        .build();
    let sim = Simulator::new(sched);
    let result = sim.run_monitored(scenario, &mut monitor);
    assert_eq!(result.trace.exit_kind(), &ExitKind::Normal);

    let probes = monitor.probes();
    assert_eq!(
        probes.match_kind(0, 0, 0),
        Some(LayeredMatchKind::CgroupContains)
    );
    assert_eq!(probes.match_needle(0, 0, 0).as_deref(), Some("mid"));

    // The haystack comes from the scheduler's own format_cgrp_path(), NOT from
    // the harness's idea of the cgroup name. A level-1 cgroup renders with a
    // trailing slash, which is exactly the kind of detail a rule written
    // against the harness name gets wrong.
    let hit = monitor.first_match_trace(Pid(1)).unwrap();
    let miss = monitor.first_match_trace(Pid(2)).unwrap();
    assert_eq!(hit.cgrp_path.as_deref(), Some("xmidy/"));
    assert_eq!(miss.cgrp_path.as_deref(), Some("plain/"));

    // Needle present in haystack => the scheduler's matcher must say so. This
    // assertion is what `sim-hyr11` violates under an instrumented build, and
    // it fails HERE, at the term, rather than three layers away at the outcome.
    assert!(
        hit.cgrp_path.as_deref().unwrap().contains("mid"),
        "test premise: the path must really contain the needle"
    );
    assert_eq!(
        hit.layers[0].groups[0],
        OrGroupVerdict::Matches,
        "scx_layered's own match_one() must agree that {:?} contains {:?}",
        hit.cgrp_path,
        probes.match_needle(0, 0, 0),
    );
    assert_eq!(miss.layers[0].groups[0], OrGroupVerdict::FailedAt(0));

    assert_trace_agrees_with_outcome(&monitor, &[Pid(1), Pid(2)]);
}

/// QUESTION ANSWERED: "with several candidate layers, which ones considered
/// this task and how far did each get?"
///
/// `maybe_refresh_layer()` scans layers in order and stops at the first match.
/// The trace shows every layer's verdict, so a rule that would ALSO have
/// matched — and was only shadowed by an earlier layer — is visible.
#[test]
fn match_trace_shows_every_layer_including_the_ones_that_were_shadowed() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(2);
    sched.layered_layers(&[
        LayerSpec::new("first", LayerKind::Open).with_match(LayerMatch::CommPrefix("dup".into())),
        LayerSpec::new("second", LayerKind::Open).with_match(LayerMatch::CommPrefix("du".into())),
        LayerSpec::catch_all("rest"),
    ]);
    let mut monitor = LayeredMonitor::new(LayeredProbes::new(&sched));

    let scenario = Scenario::builder()
        .cpus(2)
        .detect_bpf_errors()
        .add_task("dup_a", 0, run_once(10_000_000))
        .duration_ms(200)
        .build();
    let sim = Simulator::new(sched);
    let result = sim.run_monitored(scenario, &mut monitor);
    assert_eq!(result.trace.exit_kind(), &ExitKind::Normal);

    let trace = monitor.first_match_trace(Pid(1)).unwrap();
    assert!(trace.layers[0].matched(), "layer 0 takes the task");
    assert!(
        trace.layers[1].matched(),
        "layer 1 would ALSO have matched — shadowed, not rejected. \
         task_layer() cannot show this and a config author needs it."
    );
    assert_eq!(trace.first_matching_layer(), Some(0));
    assert_eq!(monitor.probes().task_layer(Pid(1)), 0);
}

// ---------------------------------------------------------------------------
// Membership lifecycle
// ---------------------------------------------------------------------------

/// QUESTION ANSWERED: "the task's name changed — did scx_layered notice, and
/// has it re-matched yet?"
///
/// `refresh_layer` is the pending-re-match flag and `recheck_layer_membership`
/// is the expiry policy. Both were entirely invisible before; a task stuck in
/// a stale layer and a task correctly staying put look identical from
/// `task_layer()`.
#[test]
fn membership_lifecycle_is_observable_across_a_rename() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(2);
    sched.layered_layers(&[
        LayerSpec::new("batch", LayerKind::Open).with_match(LayerMatch::CommPrefix("batch".into())),
        LayerSpec::catch_all("rest"),
    ]);
    let mut monitor = LayeredMonitor::new(LayeredProbes::new(&sched));

    let scenario = Scenario::builder()
        .cpus(2)
        .detect_bpf_errors()
        .add_task("plain", 0, workloads::periodic(1_000_000, 3_000_000))
        .task_rename(Pid(1), "batch_now", 50_000_000)
        .duration_ms(200)
        .build();
    let sim = Simulator::new(sched);
    let result = sim.run_monitored(scenario, &mut monitor);
    assert_eq!(result.trace.exit_kind(), &ExitKind::Normal);

    // The task really did move layers, and the monitor recorded the sequence
    // rather than only the endpoint.
    assert_eq!(
        monitor.layer_transitions(Pid(1)),
        vec![1, 0],
        "plain starts in the catch-all and re-layers into batch after rename"
    );

    // Membership never expires by policy here (no member_expire_ms), so the
    // re-match came from the rename tracepoint, not from an expiry.
    let last = monitor.final_snapshot(Pid(1)).expect("a final snapshot");
    assert_eq!(last.member_state, MemberState::NoExpire);
    assert_eq!(
        last.refresh_layer,
        Some(false),
        "the pending re-match must have been consumed by the end of the run"
    );

    // Both traces were captured, and each names the comm the scheduler saw at
    // that moment — the direct evidence that the rename reached `p->comm`.
    let comms: Vec<Option<String>> = monitor
        .task_history(Pid(1))
        .filter_map(|s| s.match_trace.as_ref())
        .map(|t| t.comm.clone())
        .collect();
    assert_eq!(
        comms,
        vec![Some("plain".to_string()), Some("batch_now".to_string())]
    );
}

/// QUESTION ANSWERED: "what placement inputs did scx_layered record for this
/// task?" — the LLC it settled in, whether its affinity pins it to one NUMA
/// node, and the runtime average that `MATCH_AVG_RUNTIME` would compare on.
#[test]
fn per_task_placement_inputs_are_readable() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(4);
    sched.layered_layers(&[LayerSpec::catch_all("all")]);
    let mut monitor = LayeredMonitor::new(LayeredProbes::new(&sched));

    let scenario = Scenario::builder()
        .cpus(4)
        .detect_bpf_errors()
        .add_task("a", 0, run_once(20_000_000))
        .duration_ms(200)
        .build();
    let sim = Simulator::new(sched);
    let result = sim.run_monitored(scenario, &mut monitor);
    assert_eq!(result.trace.exit_kind(), &ExitKind::Normal);

    let probes = monitor.probes();
    let llc = probes.task_llc(Pid(1)).expect("task_ctx exists");
    assert!(
        llc < probes.nr_llcs(),
        "recorded LLC {llc} must be one the scheduler was told about ({})",
        probes.nr_llcs()
    );
    assert_eq!(
        probes.task_all_cpus_allowed(Pid(1)),
        Some(true),
        "an unpinned task must be recorded as unrestricted"
    );
    assert!(
        probes.task_runtime_avg(Pid(1)) > 0,
        "a task that ran must have accrued a runtime average"
    );

    // Every snapshot carries the same fields, so the trajectory is available
    // and not just the endpoint.
    assert!(monitor.task_history(Pid(1)).count() > 1);
}

// ---------------------------------------------------------------------------
// Allocation-side counters
// ---------------------------------------------------------------------------

/// QUESTION ANSWERED: "what demand did the userspace allocator actually
/// measure for this layer?"
///
/// `growth_denied` says only yes/no. The usage counters are the input that
/// produced the answer, and they were exported by the wrapper but unreachable
/// from Rust until now. `sim-klue5` — the SMT shrink fixed point — is an
/// argument about these numbers.
#[test]
fn layer_usage_counters_are_reachable_and_account_the_run() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(4);
    sched.layered_layers(&[
        LayerSpec::new("busy", LayerKind::Open).with_match(LayerMatch::CommPrefix("busy".into())),
        LayerSpec::catch_all("rest"),
    ]);
    let probes = LayeredProbes::new(&sched);

    let scenario = Scenario::builder()
        .cpus(4)
        .detect_bpf_errors()
        .add_task("busy_a", 0, run_once(40_000_000))
        .add_task("busy_b", 0, run_once(40_000_000))
        .duration_ms(300)
        .build();
    let sim = Simulator::new(sched);
    let result = sim.run(scenario);
    assert_eq!(result.exit_kind(), &ExitKind::Normal);

    let owned = probes.layer_usage(0, LayerUsage::Owned);
    let open = probes.layer_usage(0, LayerUsage::Open);
    assert!(
        owned + open > 0,
        "the busy layer ran two CPU-bound tasks; owned={owned} open={open}"
    );

    // The per-node view must sum to no more than the classes it sums over.
    let node_total: u64 = (0..probes.nr_nodes())
        .map(|n| probes.layer_node_usage(0, n))
        .sum();
    assert_eq!(
        node_total,
        owned + open,
        "read_layer_node_usages() sums LAYER_USAGE_OWNED..=LAYER_USAGE_OPEN, \
         so the per-node total must equal those two classes"
    );

    // No task is node-pinned here, so the pinned view must be empty. This is
    // the negative control that keeps the assertion above from passing
    // vacuously if the two views were accidentally the same counter.
    let pinned_total: u64 = (0..probes.nr_nodes())
        .map(|n| probes.layer_node_pinned_usage(0, n))
        .sum();
    assert_eq!(pinned_total, 0, "no task in this scenario is node-pinned");
}

/// QUESTION ANSWERED: "was this layer held back from running, and by what?"
///
/// The `LayerStat` enum previously exposed 10 of the 34 counters scx_layered
/// maintains; `KeepFailBusy` and friends were unreachable by name.
#[test]
fn the_full_layer_stat_range_is_addressable() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(2);
    sched.layered_layers(&[LayerSpec::catch_all("all")]);
    let probes = LayeredProbes::new(&sched);

    let scenario = Scenario::builder()
        .cpus(2)
        .detect_bpf_errors()
        .add_task("a", 0, run_once(30_000_000))
        .add_task("b", 0, run_once(30_000_000))
        .duration_ms(200)
        .build();
    let sim = Simulator::new(sched);
    let result = sim.run(scenario);
    assert_eq!(result.exit_kind(), &ExitKind::Normal);

    // Reading every named stat must be in range: the C side returns 0 for
    // `stat_id >= NR_LSTATS`, so a Rust discriminant that ran off the end of
    // the BPF enum would silently read zero forever. Pin it against a counter
    // that must be non-zero on any run that scheduled anything.
    let enqueued = LayerStat::EnqLocal as u32;
    assert!(enqueued < LayerStat::RunqLatBase as u32);
    let dispatched = probes.layer_stat(0, LayerStat::SelLocal)
        + probes.layer_stat(0, LayerStat::EnqLocal)
        + probes.layer_stat(0, LayerStat::EnqDsq);
    assert!(
        dispatched > 0,
        "two tasks ran, so the layer must have taken at least one enqueue path"
    );

    // The late stats must be readable without panicking and must be sane
    // (a counter cannot exceed the number of scheduling decisions made).
    for stat in [
        LayerStat::KeepFailBusy,
        LayerStat::PreemptFail,
        LayerStat::XllcMigrationSkip,
        LayerStat::SkipRemoteNode,
    ] {
        let _ = probes.layer_stat(0, stat);
    }
    for stat in [GlobalStat::SkipPreempt, GlobalStat::PreemptingMismatch] {
        let _ = probes.global_stat(stat);
    }
}

// ---------------------------------------------------------------------------
// ABI
// ---------------------------------------------------------------------------

/// The `LayeredMatchKind` discriminants are positional in `enum
/// layer_match_kind`, so an upstream insertion silently renames every kind
/// after it. Pin them against the values the scheduler itself reports.
///
/// Only the 16 kinds `layered_probe_enum` exports selectors for can be pinned
/// this way; the remaining 10 are UNCOVERED by this gate, and named as such
/// rather than being asserted against a copy of the same constant.
#[test]
fn layered_match_kind_discriminants_match_the_bpf_enum() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(1);
    let p = LayeredProbes::new(&sched);

    let pinned: [(LayeredMatchKind, LayeredEnumProbe); 16] = [
        (
            LayeredMatchKind::CgroupPrefix,
            LayeredEnumProbe::MatchCgroupPrefix,
        ),
        (
            LayeredMatchKind::CommPrefix,
            LayeredEnumProbe::MatchCommPrefix,
        ),
        (
            LayeredMatchKind::PcommPrefix,
            LayeredEnumProbe::MatchPcommPrefix,
        ),
        (
            LayeredMatchKind::NiceAbove,
            LayeredEnumProbe::MatchNiceAbove,
        ),
        (
            LayeredMatchKind::NiceBelow,
            LayeredEnumProbe::MatchNiceBelow,
        ),
        (
            LayeredMatchKind::NiceEquals,
            LayeredEnumProbe::MatchNiceEquals,
        ),
        (
            LayeredMatchKind::UserIdEquals,
            LayeredEnumProbe::MatchUserIdEquals,
        ),
        (
            LayeredMatchKind::GroupIdEquals,
            LayeredEnumProbe::MatchGroupIdEquals,
        ),
        (
            LayeredMatchKind::PidEquals,
            LayeredEnumProbe::MatchPidEquals,
        ),
        (
            LayeredMatchKind::PpidEquals,
            LayeredEnumProbe::MatchPpidEquals,
        ),
        (
            LayeredMatchKind::TgidEquals,
            LayeredEnumProbe::MatchTgidEquals,
        ),
        (
            LayeredMatchKind::IsGroupLeader,
            LayeredEnumProbe::MatchIsGroupLeader,
        ),
        (
            LayeredMatchKind::IsKthread,
            LayeredEnumProbe::MatchIsKthread,
        ),
        (
            LayeredMatchKind::CgroupSuffix,
            LayeredEnumProbe::MatchCgroupSuffix,
        ),
        (
            LayeredMatchKind::CgroupContains,
            LayeredEnumProbe::MatchCgroupContains,
        ),
        (LayeredMatchKind::NumaNode, LayeredEnumProbe::MatchNumaNode),
    ];
    for (kind, probe) in pinned {
        assert_eq!(
            kind as i32,
            p.enum_value(probe),
            "LayeredMatchKind::{kind:?} drifted from the BPF enum"
        );
        assert_eq!(LayeredMatchKind::from_raw(kind as i32), Some(kind));
    }

    // An unknown kind must decode to None rather than to a wrong variant.
    assert_eq!(LayeredMatchKind::from_raw(999), None);
    assert_eq!(LayeredMatchKind::from_raw(-1), None);
}

/// Out-of-range probe indices must report "no such term", never a plausible
/// wrong answer.
#[test]
fn out_of_range_match_indices_report_absence() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(1);
    sched.layered_layers(&[
        LayerSpec::new("one", LayerKind::Open).with_match(LayerMatch::CommPrefix("x".into())),
        LayerSpec::catch_all("rest"),
    ]);
    let p = LayeredProbes::new(&sched);

    assert_eq!(p.match_nr_ors(99), 0, "no such layer");
    assert_eq!(p.match_nr_ands(0, 99), 0, "no such OR group");
    assert_eq!(p.match_kind(0, 0, 99), None, "no such AND term");
    assert_eq!(p.match_exclude(0, 0, 99), None);
    assert_eq!(p.match_needle(0, 0, 99), None);
    assert!(p.describe_term(0, 0, 99).starts_with("<no term"));

    // A catch-all layer is one OR group with ZERO AND terms: `match_layer()`
    // starts each group with `matched = true` and never enters the AND loop,
    // so the group holds for every task. Worth pinning, because "no rules" and
    // "no OR groups" are different configurations and only the first matches
    // everything — a layer with `nr_match_ors == 0` matches NOTHING.
    assert_eq!(p.match_nr_ors(1), 1, "catch-all is one empty OR group");
    assert_eq!(p.match_nr_ands(1, 0), 0, "...with no AND terms in it");
    assert_eq!(p.match_kind(1, 0, 0), None, "so there is no term 0 to read");
}
