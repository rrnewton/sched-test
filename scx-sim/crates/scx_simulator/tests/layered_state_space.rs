//! Does scx_layered do what its own configuration says?
//!
//! `tests/layered.rs` asserts outcomes and `tests/layered_probes.rs` asserts
//! the factors behind one decision. This file asks a third question: across a
//! range of configurations, workloads and machine shapes, does the scheduler
//! ever make a decision that CONTRADICTS the configuration it was given?
//!
//! Each check names the contradiction it hunts, and reads the evidence out of
//! the scheduler's own state (`LayeredProbes`) plus the engine's trace of
//! which CPU actually ran which task.
//!
//! UNCOVERED IS NOT STUBBED. Everything here runs the real BPF. Where a
//! decision is unreachable under scxsim today, or where upstream itself has
//! the same blind spot, the test says so by name and cites the evidence,
//! rather than being weakened until it passes.

use scx_simulator::*;
use std::collections::{BTreeMap, BTreeSet};

#[macro_use]
mod common;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// scx_layered's DSQ id encoding (`scx_layered/src/bpf/intf.h:52`).
const HI_FB_DSQ_BASE: u64 = 0x4000_0000;

fn run_sleep_loop(run_ns: u64, sleep_ns: u64) -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(run_ns), Phase::Sleep(sleep_ns)],
        repeat: RepeatMode::Forever,
    }
}

fn hog() -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(1_000_000_000)],
        repeat: RepeatMode::Forever,
    }
}

/// Which CPUs each task actually ran on, from the engine's trace.
///
/// The scheduler's cpumask says where a task is ALLOWED to run; this says
/// where it DID. A gap between the two is the scheduler disobeying its own
/// configuration.
fn cpus_per_task(trace: &Trace) -> BTreeMap<Pid, BTreeSet<u32>> {
    let mut out: BTreeMap<Pid, BTreeSet<u32>> = BTreeMap::new();
    for e in trace.events() {
        if let TraceKind::TaskScheduled { pid } = e.kind {
            out.entry(pid).or_default().insert(e.cpu.0);
        }
    }
    out
}

/// The CPUs the scheduler itself says belong to `layer_id`, read back out of
/// `struct layer`'s cpumask rather than recomputed on our side.
fn layer_cpus(probes: &LayeredProbes, layer_id: u32, nr_cpus: u32) -> BTreeSet<u32> {
    (0..nr_cpus)
        .filter(|&c| probes.layer_has_cpu(layer_id, CpuId(c)))
        .collect()
}

/// A two-node machine: 16 CPUs, 4 per LLC (so 4 LLCs), 2 nodes, no SMT.
/// LLCs 0-1 are on node 0 (CPUs 0-7); LLCs 2-3 on node 1 (CPUs 8-15).
fn two_node_16() -> MachineTopology {
    MachineTopology::uniform(16, 4, 2, 1)
}

/// Run one restricted layer plus one catch-all against `two_node_16()` and
/// report the CPUs the restricted layer was granted.
fn affinity_run(
    kind: LayerKind,
    restrict: impl FnOnce(LayerSpec) -> LayerSpec,
    control_loop: bool,
    duration_ms: u64,
) -> BTreeSet<u32> {
    let topo = two_node_16();
    let sched = DynamicScheduler::layered_for_topology(&topo);
    let spec = restrict(
        LayerSpec::new("restricted", kind)
            .with_match(LayerMatch::CommPrefix("r".into()))
            // The Tier-3 control loop refuses a non-Open layer without one.
            .with_util_range(0.1, 0.9),
    );
    sched.layered_layers(&[spec, LayerSpec::catch_all("rest")]);
    if control_loop {
        sched.layered_enable_control_loop(50_000_000);
    }
    let probes = LayeredProbes::new(&sched);

    let mut b = Scenario::builder()
        .cpus(16)
        .topology(topo)
        .detect_bpf_errors();
    for i in 0..8 {
        b = b.add_task(&format!("r{i}"), 0, run_sleep_loop(2_000_000, 500_000));
        b = b.add_task(&format!("o{i}"), 0, run_sleep_loop(2_000_000, 500_000));
    }
    let sim = Simulator::new(sched);
    let trace = sim.run(b.duration_ms(duration_ms).build());
    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    layer_cpus(&probes, 0, 16)
}

// ---------------------------------------------------------------------------
// A1. `nodes` / `llcs` are a restriction, and must be obeyed
// ---------------------------------------------------------------------------

/// CONTRADICTION HUNTED: a layer declares `nodes: [1]` or `llcs: [3]`, and the
/// allocator hands it a CPU somewhere else.
///
/// This is upstream's documented contract. Its README: "Layer affinities can
/// be defined using the `nodes` or `llcs` layer configs. This allows for
/// **restricting** a layer to a NUMA node or LLC."
/// `layer_core_growth.rs::node_order` says the same in code terms — "spec_nodes
/// if set (hard limit)". Upstream enforces it with `Layer::allowed_cpus`
/// (`main.rs:1447`, built in `Layer::new`), intersected at every allocation
/// site: `main.rs:3250`, `3619`, `3657`, `3925` and `4073`.
///
/// It was NOT enforced here before 2026-09-11. `nodes`/`llcs` never reached
/// `schedulers/layered/wrapper.c` at all, so every layer's slice came off the
/// front of the machine: a `nodes: [1]` layer on this topology was granted
/// CPUs 0-7 — all eight forbidden, none of the eight it asked for. Enabling
/// the control loop made it {0..15} rather than repairing it, because the
/// shrink path (faithfully) mirrors upstream's `next_to_free(cands,
/// core_order[n].iter().rev())`, which finds `core_order[0]` empty for a
/// forbidden node and cannot hand the CPUs back. Upstream never needs that
/// path: its layers start with `cpus: Cpumask::new()`.
///
/// The matrix is exhaustive on purpose — the defect was present in all three
/// layer kinds, on both axes, under both allocators, and the three sub-causes
/// were in three different files.
#[test]
fn a_layer_is_never_granted_a_cpu_its_affinity_forbids() {
    let _lock = common::setup_test();
    let topo = two_node_16();
    let node1: BTreeSet<u32> = topo.cpus_of_node(1).into_iter().map(|c| c.0).collect();
    let llc3: BTreeSet<u32> = topo.cpus_of_llc(3).into_iter().map(|c| c.0).collect();

    for kind in [LayerKind::Confined, LayerKind::Grouped, LayerKind::Open] {
        for control in [false, true] {
            let got = affinity_run(kind, |s| s.with_nodes(vec![1]), control, 400);
            let bad: Vec<u32> = got.difference(&node1).copied().collect();
            assert!(
                bad.is_empty(),
                "{kind:?} nodes=[1] control={control}: granted {got:?}, \
                 forbidden CPUs held: {bad:?} (allowed: {node1:?})"
            );

            let got = affinity_run(kind, |s| s.with_llcs(vec![3]), control, 400);
            let bad: Vec<u32> = got.difference(&llc3).copied().collect();
            assert!(
                bad.is_empty(),
                "{kind:?} llcs=[3] control={control}: granted {got:?}, \
                 forbidden CPUs held: {bad:?} (allowed: {llc3:?})"
            );
        }
    }
}

/// The other half of A1: obeying the restriction must not be achieved by
/// granting nothing. The static split gives a restricted layer a real share of
/// its allowed set.
///
/// Without this, `a_layer_is_never_granted_a_cpu_its_affinity_forbids` would
/// pass trivially if the allocator simply stopped allocating.
#[test]
fn a_restricted_layer_still_gets_its_allowed_cpus() {
    let _lock = common::setup_test();
    let topo = two_node_16();
    let node1: BTreeSet<u32> = topo.cpus_of_node(1).into_iter().map(|c| c.0).collect();
    let llc3: BTreeSet<u32> = topo.cpus_of_llc(3).into_iter().map(|c| c.0).collect();

    for kind in [LayerKind::Confined, LayerKind::Grouped, LayerKind::Open] {
        assert_eq!(
            affinity_run(kind, |s| s.with_nodes(vec![1]), false, 400),
            node1,
            "{kind:?} nodes=[1] under the static split should get exactly node 1"
        );
        assert_eq!(
            affinity_run(kind, |s| s.with_llcs(vec![3]), false, 400),
            llc3,
            "{kind:?} llcs=[3] under the static split should get exactly LLC 3"
        );
    }
}

/// KNOWN GAP, and it is UPSTREAM'S, not ours: an `llcs`-only restriction is
/// invisible to the per-node CPU budget, so under the control loop such a
/// layer can end up with no CPUs at all.
///
/// WHY EXPECTED: the budget comes from `unified_alloc(.., &node_groups)`, and
/// `node_groups` is built from `spec.nodes()` alone — `main.rs:3791-3797`
/// passes `self.layer_specs[idx].nodes()`, and `safe/layered_control.rs` passes
/// `spec.nodes()` to the same upstream function. With `nodes: []` the layer's
/// whole budget can land on node 0, measured here as
/// `node_target=[N,0]` for a layer whose only allowed CPUs are LLC 3 on node 1.
/// Upstream's grow loop then calls `alloc_cpus(&node_allowed, ..)` with
/// `node_allowed = allowed_cpus.and(node_span)` empty (`main.rs:3925-3931`),
/// gets `None`, and breaks — reaching the same zero. Inventing a
/// `node_groups` that consults `llcs` would be modelling the scheduler rather
/// than the kernel, which this project forbids.
///
/// WHEN THIS GOES RED: upstream taught its node budget about `llcs`, or we
/// vendored a version that does. Invert it — assert the layer gets LLC 3 —
/// and keep the test.
///
/// DO NOT: delete it, and do not "fix" it locally by diverging from upstream's
/// allocator.
#[test]
fn known_gap_an_llc_only_restriction_is_invisible_to_the_node_budget() {
    let _lock = common::setup_test();
    let topo = two_node_16();
    let llc3: BTreeSet<u32> = topo.cpus_of_llc(3).into_iter().map(|c| c.0).collect();

    let got = affinity_run(LayerKind::Confined, |s| s.with_llcs(vec![3]), true, 400);
    assert!(
        got.is_empty(),
        "KNOWN-GAP TEST: this going red means the gap CLOSED. Invert this \
         assertion to assert the property now holds. Do not delete it, and do \
         not loosen the bound. (expected no CPUs for an llcs-only layer under \
         the control loop, got {got:?}; allowed set is {llc3:?})"
    );
    // The static split does get it right, so the gap is specifically the
    // node-budget path and not the affinity plumbing.
    assert_eq!(
        affinity_run(LayerKind::Confined, |s| s.with_llcs(vec![3]), false, 400),
        llc3,
        "the static split should still place this layer on LLC 3"
    );
}

/// GUARD: the affinity work must be a no-op for a layer that declares no
/// affinity. This pins the contiguous weight-proportional windows the
/// pre-affinity allocation produced, so a future change to the skip logic
/// cannot quietly reshape every existing scenario.
#[test]
fn an_unrestricted_allocation_is_the_same_contiguous_split_as_before() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(8);
    sched.layered_layers(&[
        LayerSpec::new("a", LayerKind::Confined).with_match(LayerMatch::CommPrefix("a".into())),
        LayerSpec::new("b", LayerKind::Confined).with_match(LayerMatch::CommPrefix("b".into())),
        LayerSpec::catch_all("rest"),
    ]);
    let probes = LayeredProbes::new(&sched);
    let mut b = Scenario::builder().cpus(8).detect_bpf_errors();
    for i in 0..4 {
        b = b.add_task(&format!("a{i}"), 0, run_sleep_loop(1_000_000, 500_000));
        b = b.add_task(&format!("b{i}"), 0, run_sleep_loop(1_000_000, 500_000));
    }
    let sim = Simulator::new(sched);
    let trace = sim.run(b.duration_ms(100).build());
    assert_eq!(trace.exit_kind(), &ExitKind::Normal);

    // Three equal-weight layers over 8 CPUs: share = 8*100/300 = 2 each,
    // taken as contiguous windows from CPU 0. The open catch-all then absorbs
    // the unallocated remainder.
    assert_eq!(layer_cpus(&probes, 0, 8), BTreeSet::from([0, 1]));
    assert_eq!(layer_cpus(&probes, 1, 8), BTreeSet::from([2, 3]));
    assert_eq!(layer_cpus(&probes, 2, 8), BTreeSet::from([4, 5, 6, 7]));
}

// ---------------------------------------------------------------------------
// A2. Confined layers, and why they do not confine yet
// ---------------------------------------------------------------------------

/// KNOWN GAP (mb sim-6mheb, blocked on mb sim-zwypg): **a `Confined` layer
/// does not confine anything under scxsim.** Its tasks run on every CPU on the
/// machine, and scx_layered counts each escape as an affinity violation.
///
/// WHY EXPECTED: this is not an independent scx_layered defect, it is the
/// measurable consequence of the `is_scheduler_task()` masking, and the
/// attribution is asserted below rather than argued.
///
/// * `is_scheduler_task(p)` is `(u32)p->tgid == layered_root_tgid`
///   (`main.bpf.c:203-206`). The wrapper never runs
///   `initialize_pid_namespace`, so `layered_root_tgid` stays 0, and
///   `sim_task_alloc()` leaves `p->tgid` 0. Both sides 0, so it is true for
///   every simulated task.
/// * `LSTAT_AFFN_VIOL` has exactly two increment sites in `main.bpf.c`. The
///   one at 1378 requires `p->nr_cpus_allowed == 1`, which no task here is.
///   The one at 2051 is inside that `is_scheduler_task(p)` arm.
/// * The arm then inserts on `hi_fb_dsq_id`, a fallback DSQ any CPU consumes,
///   so confinement is structurally impossible from there whatever the layer's
///   cpumask says. The test asserts the tasks ARE on that DSQ, which is what
///   ties the escape to this cause and not another.
///
/// WHEN THIS GOES RED: `p->tgid = pid` landed. Invert it — assert the tasks
/// stayed inside the layer's cpumask, which is what a Confined layer means.
///
/// DO NOT: delete it, or relax it to "some tasks escaped".
#[test]
fn known_gap_confined_layers_do_not_confine_because_every_task_takes_the_daemon_path() {
    let _lock = common::setup_test();
    const NR_CPUS: u32 = 8;

    let sched = DynamicScheduler::layered(NR_CPUS);
    sched.layered_layers(&[
        // Pin explicitly so "which CPUs did it ask for?" is not in question.
        LayerSpec::new("pinned", LayerKind::Confined)
            .with_match(LayerMatch::CommPrefix("pinned".into()))
            .with_cpus(vec![CpuId(0), CpuId(1)]),
        LayerSpec::catch_all("rest"),
    ]);
    let probes = LayeredProbes::new(&sched);

    let mut b = Scenario::builder().cpus(NR_CPUS).detect_bpf_errors();
    for i in 0..4 {
        b = b.add_task(&format!("pinned{i}"), 0, run_sleep_loop(2_000_000, 500_000));
    }
    for i in 0..8 {
        b = b.add_task(&format!("other{i}"), 0, run_sleep_loop(2_000_000, 500_000));
    }
    let sim = Simulator::new(sched);
    let trace = sim.run(b.duration_ms(300).build());
    assert_eq!(trace.exit_kind(), &ExitKind::Normal);

    let allowed = layer_cpus(&probes, 0, NR_CPUS);
    assert_eq!(
        allowed,
        BTreeSet::from([0, 1]),
        "the layer's own cpumask should be the two CPUs the config named"
    );

    let members: Vec<Pid> = (1..=12)
        .map(Pid)
        .filter(|p| probes.task_layer(*p) == 0)
        .collect();
    assert!(!members.is_empty(), "no task landed in layer 0");
    for pid in &members {
        assert_ne!(
            probes.task_dsq(*pid) & HI_FB_DSQ_BASE,
            0,
            "{pid:?} is in the Confined layer but not on hi_fb; if this fires, \
             the attribution no longer holds and the escape has another cause"
        );
    }

    let ran = cpus_per_task(&trace);
    let escaped: BTreeSet<u32> = members
        .iter()
        .filter_map(|p| ran.get(p))
        .flat_map(|s| s.difference(&allowed).copied())
        .collect();
    assert!(
        !escaped.is_empty(),
        "KNOWN-GAP TEST: this going red means the gap CLOSED. Invert this \
         assertion to assert the property now holds (Confined tasks stay on \
         the layer's CPUs). Do not delete it, and do not loosen the bound."
    );
    assert!(
        probes.layer_stat(0, LayerStat::AffnViol) > 0,
        "the scheduler should be counting these as affinity violations"
    );
}

/// KNOWN GAP (mb sim-6mheb): no task is ever placed on a LAYER DSQ, so the
/// core of what scx_layered does with a task — LLC selection,
/// `maybe_update_task_llc()`, vtime clamping, `scx_bpf_dsq_insert_vtime()` —
/// does not execute under scxsim.
///
/// WHY EXPECTED: same `is_scheduler_task()` masking as above. `LSTAT_ENQ_DSQ`
/// is incremented at exactly one site, `main.bpf.c:2193`, immediately after
/// the insert onto `layer_dsq_id(layer_id, llc_id)`. It stays at zero even
/// with the machine oversubscribed 4:1.
///
/// WHEN THIS GOES RED: `p->tgid = pid` landed and the real dispatch path is
/// running. Invert it — assert `EnqDsq > 0` — and then go and look at
/// everything `state_space_sweep` reports, because a large part of the
/// scheduler will have started executing for the first time.
///
/// DO NOT: delete it. It is the single most load-bearing coverage fact about
/// scx_layered under scxsim.
#[test]
fn known_gap_no_task_ever_reaches_a_layer_dsq() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(4);
    sched.layered_layers(&[
        LayerSpec::new("hot", LayerKind::Open).with_match(LayerMatch::CommPrefix("hot".into())),
        LayerSpec::catch_all("rest"),
    ]);
    let probes = LayeredProbes::new(&sched);

    let mut b = Scenario::builder().cpus(4).detect_bpf_errors();
    for i in 0..8 {
        b = b.add_task(&format!("hot{i}"), 0, run_sleep_loop(2_000_000, 1_000_000));
        b = b.add_task(&format!("cold{i}"), 0, run_sleep_loop(2_000_000, 1_000_000));
    }
    let sim = Simulator::new(sched);
    let trace = sim.run(b.duration_ms(200).build());
    assert_eq!(trace.exit_kind(), &ExitKind::Normal);

    let enq_dsq: u64 = (0..probes.nr_layers())
        .map(|l| probes.layer_stat(l, LayerStat::EnqDsq))
        .sum();
    let hi_fb = probes.global_stat(GlobalStat::HiFbEvents);
    assert!(
        hi_fb > 0,
        "the daemon fast path should be busy; got HiFbEvents={hi_fb}"
    );
    assert_eq!(
        enq_dsq, 0,
        "KNOWN-GAP TEST: this going red means the gap CLOSED. Invert this \
         assertion to assert the property now holds (tasks reach layer DSQs). \
         Do not delete it, and do not loosen the bound. (HiFbEvents={hi_fb})"
    );
}

// ---------------------------------------------------------------------------
// Measurement: which of scx_layered's decisions ever happen?
// ---------------------------------------------------------------------------

/// 31 of `LayerStat`'s 33 variants. `MinExecNs` and `RunqLatBase` are
/// deliberately absent: they are nanosecond accumulators, not decision
/// counters, so "did it move" says nothing about which branch was taken.
const ALL_LAYER_STATS: &[(LayerStat, &str)] = &[
    (LayerStat::SelLocal, "SelLocal"),
    (LayerStat::EnqLocal, "EnqLocal"),
    (LayerStat::EnqWakeup, "EnqWakeup"),
    (LayerStat::EnqExpire, "EnqExpire"),
    (LayerStat::EnqReenq, "EnqReenq"),
    (LayerStat::EnqDsq, "EnqDsq"),
    (LayerStat::Keep, "Keep"),
    (LayerStat::MinExec, "MinExec"),
    (LayerStat::OpenIdle, "OpenIdle"),
    (LayerStat::AffnViol, "AffnViol"),
    (LayerStat::KeepFailMaxExec, "KeepFailMaxExec"),
    (LayerStat::KeepFailBusy, "KeepFailBusy"),
    (LayerStat::Preempt, "Preempt"),
    (LayerStat::PreemptFirst, "PreemptFirst"),
    (LayerStat::PreemptXllc, "PreemptXllc"),
    (LayerStat::PreemptXnuma, "PreemptXnuma"),
    (LayerStat::PreemptIdle, "PreemptIdle"),
    (LayerStat::PreemptFail, "PreemptFail"),
    (LayerStat::ExclCollision, "ExclCollision"),
    (LayerStat::ExclPreempt, "ExclPreempt"),
    (LayerStat::Yield, "Yield"),
    (LayerStat::YieldIgnore, "YieldIgnore"),
    (LayerStat::Migration, "Migration"),
    (LayerStat::XnumaMigration, "XnumaMigration"),
    (LayerStat::XllcMigration, "XllcMigration"),
    (LayerStat::XllcMigrationSkip, "XllcMigrationSkip"),
    (LayerStat::XlayerWake, "XlayerWake"),
    (LayerStat::XlayerRewake, "XlayerRewake"),
    (LayerStat::LlcDrainTry, "LlcDrainTry"),
    (LayerStat::LlcDrain, "LlcDrain"),
    (LayerStat::SkipRemoteNode, "SkipRemoteNode"),
];

/// All 11 `GlobalStat` variants.
const ALL_GLOBAL_STATS: &[(GlobalStat, &str)] = &[
    (GlobalStat::ExclIdle, "ExclIdle"),
    (GlobalStat::ExclWakeup, "ExclWakeup"),
    (GlobalStat::HiFbEvents, "HiFbEvents"),
    (GlobalStat::HiFbUsage, "HiFbUsage"),
    (GlobalStat::LoFbEvents, "LoFbEvents"),
    (GlobalStat::LoFbUsage, "LoFbUsage"),
    (GlobalStat::FbCpuUsage, "FbCpuUsage"),
    (GlobalStat::Antistall, "Antistall"),
    (GlobalStat::SkipPreempt, "SkipPreempt"),
    (GlobalStat::FixupVtime, "FixupVtime"),
    (GlobalStat::PreemptingMismatch, "PreemptingMismatch"),
];

type Reached = (BTreeSet<&'static str>, BTreeSet<&'static str>);

fn nonzero_stats(probes: &LayeredProbes) -> Reached {
    let nr_layers = probes.nr_layers();
    let mut l = BTreeSet::new();
    for (stat, name) in ALL_LAYER_STATS {
        if (0..nr_layers).any(|i| probes.layer_stat(i, *stat) > 0) {
            l.insert(*name);
        }
    }
    let mut g = BTreeSet::new();
    for (stat, name) in ALL_GLOBAL_STATS {
        if probes.global_stat(*stat) > 0 {
            g.insert(*name);
        }
    }
    (l, g)
}

/// MEASUREMENT: which of scx_layered's 31 per-layer and 10 global decision
/// counters ever move, across a sweep of shapes?
///
/// A counter that stays at zero across everything is a decision the scheduler
/// never makes under scxsim. That is a coverage statement in the scheduler's
/// own vocabulary rather than in line counts, and it is how this file reports
/// what it did NOT cover.
///
/// Run with:
/// `cargo test -p scx_simulator --test layered_state_space -- --ignored
///  --nocapture state_space_sweep`
#[test]
#[ignore = "measurement: prints the reached/unreached decision table"]
fn state_space_sweep() {
    let _lock = common::setup_test();
    let mut all_l: BTreeSet<&'static str> = BTreeSet::new();
    let mut all_g: BTreeSet<&'static str> = BTreeSet::new();

    for (name, (l, g)) in sweep_cases() {
        println!("--- {name}\n      layer:  {l:?}\n      global: {g:?}");
        all_l.extend(l);
        all_g.extend(g);
    }

    let unreached_l: Vec<_> = ALL_LAYER_STATS
        .iter()
        .map(|(_, n)| *n)
        .filter(|n| !all_l.contains(n))
        .collect();
    let unreached_g: Vec<_> = ALL_GLOBAL_STATS
        .iter()
        .map(|(_, n)| *n)
        .filter(|n| !all_g.contains(n))
        .collect();
    println!("\n=== UNION ===");
    println!(
        "LayerStat  reached {}/{}: {all_l:?}",
        all_l.len(),
        ALL_LAYER_STATS.len()
    );
    println!(
        "LayerStat  UNREACHED {}: {unreached_l:?}",
        unreached_l.len()
    );
    println!(
        "GlobalStat reached {}/{}: {all_g:?}",
        all_g.len(),
        ALL_GLOBAL_STATS.len()
    );
    println!(
        "GlobalStat UNREACHED {}: {unreached_g:?}",
        unreached_g.len()
    );
}

fn sweep_cases() -> Vec<(String, Reached)> {
    vec![
        ("flat-8cpu/catch-all/oversub".into(), case_flat_oversub()),
        ("flat-8cpu/preempt-over-hogs".into(), case_preempt()),
        ("16cpu/4-per-llc/2-node".into(), case_two_node()),
        // BOTH arms, because they reach DISJOINT parts of the scheduler and
        // keeping only one silently loses coverage. Non-preempting reaches
        // `sib_keep_idle` (ExclIdle / ExclWakeup); preempting reaches the
        // sibling-kick inside `try_preempt_cpu` (ExclPreempt) and, because the
        // preempt block sits ABOVE the `is_scheduler_task` arm in
        // `layered_enqueue`, stops reaching the hi-fallback DSQ at all.
        ("16cpu/smt2/exclusive".into(), case_exclusive(false)),
        ("16cpu/smt2/exclusive+preempt".into(), case_exclusive(true)),
        ("16cpu/2-node/node-restricted".into(), case_restricted()),
        ("flat-8cpu/yielding".into(), case_yield()),
    ]
}

/// `layered_yield` is only reached when a task actually yields, which none of
/// the other shapes do. `yield_ignore` splits the two counters: a layer that
/// ignores yields increments `YieldIgnore`, one that honours them `Yield`.
fn case_yield() -> Reached {
    let sched = DynamicScheduler::layered(8);
    let mut ignoring =
        LayerSpec::new("ignoring", LayerKind::Open).with_match(LayerMatch::CommPrefix("yi".into()));
    ignoring.set_yield_ignore(1.0);
    sched.layered_layers(&[
        ignoring,
        LayerSpec::new("honouring", LayerKind::Open)
            .with_match(LayerMatch::CommPrefix("yh".into())),
        LayerSpec::catch_all("rest"),
    ]);
    let probes = LayeredProbes::new(&sched);
    let yielder = TaskBehavior {
        phases: vec![Phase::Run(500_000), Phase::Yield],
        repeat: RepeatMode::Forever,
    };
    let mut b = Scenario::builder().cpus(8).detect_bpf_errors();
    for i in 0..8 {
        b = b.add_task(&format!("yi{i}"), 0, yielder.clone());
        b = b.add_task(&format!("yh{i}"), 0, yielder.clone());
        b = b.add_task(&format!("bg{i}"), 0, hog());
    }
    let sim = Simulator::new(sched);
    let _ = sim.run(b.duration_ms(300).build());
    nonzero_stats(&probes)
}

fn case_flat_oversub() -> Reached {
    let sched = DynamicScheduler::layered(8);
    sched.layered_layers(&[LayerSpec::catch_all("all")]);
    let probes = LayeredProbes::new(&sched);
    let mut b = Scenario::builder().cpus(8).detect_bpf_errors();
    for i in 0..32 {
        b = b.add_task(&format!("w{i}"), 0, run_sleep_loop(2_000_000, 500_000));
    }
    let sim = Simulator::new(sched);
    let _ = sim.run(b.duration_ms(300).build());
    nonzero_stats(&probes)
}

fn case_preempt() -> Reached {
    let sched = DynamicScheduler::layered(8);
    sched.layered_layers(&[
        LayerSpec::new("rt", LayerKind::Open)
            .with_match(LayerMatch::CommPrefix("rt".into()))
            .with_preempt(true),
        LayerSpec::catch_all("bulk"),
    ]);
    let probes = LayeredProbes::new(&sched);
    let mut b = Scenario::builder().cpus(8).detect_bpf_errors();
    for i in 0..16 {
        b = b.add_task(&format!("bulk{i}"), 0, hog());
    }
    for i in 0..4 {
        b = b.add_task(&format!("rt{i}"), 0, run_sleep_loop(500_000, 3_000_000));
    }
    let sim = Simulator::new(sched);
    let _ = sim.run(b.duration_ms(300).build());
    nonzero_stats(&probes)
}

fn case_two_node() -> Reached {
    let topo = two_node_16();
    let sched = DynamicScheduler::layered_for_topology(&topo);
    sched.layered_layers(&[
        LayerSpec::new("a", LayerKind::Grouped).with_match(LayerMatch::CommPrefix("a".into())),
        LayerSpec::catch_all("rest"),
    ]);
    let probes = LayeredProbes::new(&sched);
    let mut b = Scenario::builder()
        .cpus(16)
        .topology(topo)
        .detect_bpf_errors();
    for i in 0..32 {
        b = b.add_task(&format!("a{i}"), 0, run_sleep_loop(2_000_000, 500_000));
        b = b.add_task(&format!("b{i}"), 0, run_sleep_loop(2_000_000, 500_000));
    }
    let sim = Simulator::new(sched);
    let _ = sim.run(b.duration_ms(300).build());
    nonzero_stats(&probes)
}

fn case_exclusive(preempting: bool) -> Reached {
    let topo = MachineTopology::uniform(16, 8, 1, 2);
    let sched = DynamicScheduler::layered_for_topology(&topo);
    sched.layered_layers(&[
        // scx_layered's sibling-kick — the half that makes room for an
        // exclusive task — is inside `try_preempt_cpu`'s `preempt:` block
        // (main.bpf.c:1820-1838), so a non-preempting exclusive layer never
        // reaches it and `ExclPreempt` / `ExclCollision` stay at zero.
        // Measured on an 8-CPU SMT2 machine, same workload: 0 and 0 without
        // `preempt`, 44 and 3 with it. Hence both arms in the sweep.
        LayerSpec::new("excl", LayerKind::Open)
            .with_match(LayerMatch::CommPrefix("x".into()))
            .with_exclusive(true)
            .with_preempt(preempting),
        LayerSpec::catch_all("rest"),
    ]);
    let probes = LayeredProbes::new(&sched);
    let mut b = Scenario::builder()
        .cpus(16)
        .topology(topo)
        .detect_bpf_errors();
    for i in 0..16 {
        b = b.add_task(&format!("x{i}"), 0, hog());
        b = b.add_task(&format!("y{i}"), 0, hog());
    }
    let sim = Simulator::new(sched);
    let _ = sim.run(b.duration_ms(300).build());
    nonzero_stats(&probes)
}

fn case_restricted() -> Reached {
    let topo = two_node_16();
    let sched = DynamicScheduler::layered_for_topology(&topo);
    sched.layered_layers(&[
        LayerSpec::new("n1", LayerKind::Confined)
            .with_match(LayerMatch::CommPrefix("n1".into()))
            .with_util_range(0.1, 0.9)
            .with_nodes(vec![1]),
        LayerSpec::catch_all("rest"),
    ]);
    let probes = LayeredProbes::new(&sched);
    let mut b = Scenario::builder()
        .cpus(16)
        .topology(topo)
        .detect_bpf_errors();
    for i in 0..16 {
        b = b.add_task(&format!("n1_{i}"), 0, run_sleep_loop(2_000_000, 500_000));
        b = b.add_task(&format!("z{i}"), 0, hog());
    }
    let sim = Simulator::new(sched);
    let _ = sim.run(b.duration_ms(300).build());
    nonzero_stats(&probes)
}

// ---------------------------------------------------------------------------
// A3. Per-layer policy scalars
// ---------------------------------------------------------------------------

/// CONTRADICTION HUNTED: a layer sets `slice_us`, and its tasks get a
/// different timeslice.
///
/// `slice_ns` is the most direct number a layer config states about runtime
/// behaviour, and it is measurable end-to-end from the engine's trace without
/// any probe: the interval from `TaskScheduled` to the task's next stop event
/// IS the slice it was given. Two layers three orders of magnitude apart make
/// the comparison unambiguous.
///
/// Result at integration@1e8a2be: honoured exactly. 1 ms layer, 176 episodes,
/// p10 = median = p90 = 1.001 ms; 20 ms layer, 90 episodes, p10 = median =
/// p90 = 20.001 ms. The 1 us excess is the modelled context-switch overhead.
#[test]
fn per_layer_slice_us_shapes_how_long_its_tasks_actually_run() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(4);
    sched.layered_layers(&[
        LayerSpec::new("shortslice", LayerKind::Open)
            .with_match(LayerMatch::CommPrefix("s".into()))
            .with_slice_ns(1_000_000),
        LayerSpec::new("longslice", LayerKind::Open)
            .with_match(LayerMatch::CommPrefix("l".into()))
            .with_slice_ns(20_000_000),
        LayerSpec::catch_all("rest"),
    ]);
    let probes = LayeredProbes::new(&sched);
    let mut b = Scenario::builder().cpus(4).detect_bpf_errors();
    for i in 0..4 {
        b = b.add_task(&format!("s{i}"), 0, hog());
        b = b.add_task(&format!("l{i}"), 0, hog());
    }
    let sim = Simulator::new(sched);
    let trace = sim.run(b.duration_ms(500).build());
    assert_eq!(trace.exit_kind(), &ExitKind::Normal);

    let by_layer = on_cpu_episodes_by_layer(&trace, &probes);
    for (layer, want_ns) in [(0u32, 1_000_000u64), (1, 20_000_000)] {
        let ds = by_layer
            .get(&layer)
            .unwrap_or_else(|| panic!("layer {layer} ran no episodes"));
        assert!(
            ds.len() >= 20,
            "layer {layer}: only {} episodes, too few to characterise",
            ds.len()
        );
        let median = ds[ds.len() / 2];
        // Generous: the point is 1 ms vs 20 ms, not the exact overhead.
        let lo = want_ns;
        let hi = want_ns + want_ns / 10 + 100_000;
        assert!(
            (lo..=hi).contains(&median),
            "layer {layer} configured slice_ns={want_ns} but median on-CPU \
             episode was {median} ns over {} episodes (p10={} p90={})",
            ds.len(),
            ds[ds.len() / 10],
            ds[ds.len() * 9 / 10],
        );
    }
}

/// Sorted per-layer on-CPU episode durations, from the engine's trace.
fn on_cpu_episodes_by_layer(trace: &Trace, probes: &LayeredProbes) -> BTreeMap<u32, Vec<u64>> {
    let mut starts: BTreeMap<Pid, u64> = BTreeMap::new();
    let mut by_layer: BTreeMap<u32, Vec<u64>> = BTreeMap::new();
    for e in trace.events() {
        match e.kind {
            TraceKind::TaskScheduled { pid } => {
                starts.insert(pid, e.time_ns);
            }
            TraceKind::TaskPreempted { pid }
            | TraceKind::TaskSlept { pid }
            | TraceKind::TaskYielded { pid } => {
                if let Some(t0) = starts.remove(&pid) {
                    by_layer
                        .entry(probes.task_layer(pid))
                        .or_default()
                        .push(e.time_ns - t0);
                }
            }
            _ => {}
        }
    }
    for ds in by_layer.values_mut() {
        ds.sort_unstable();
    }
    by_layer
}

/// MEASUREMENT, deliberately not an assertion: how often does an `exclusive`
/// layer's task share an SMT core with a non-exclusive one?
///
/// **There is no hard guarantee here to contradict, so this does not assert
/// one.** scx_layered's exclusivity has two halves and both are qualified:
///
/// * `sib_keep_idle` (`main.bpf.c:2683-2699`) stops a CPU picking work up when
///   its sibling already runs exclusive — the "non-exclusive starts second"
///   direction only.
/// * The sibling-kick that makes room for an exclusive task starting second is
///   inside `try_preempt_cpu`'s `preempt:` block (`main.bpf.c:1820-1838`), so a
///   layer that is `exclusive` but not `preempt` never reaches it. Its own
///   comment calls the test "inaccurate and racy but should be good enough for
///   best-effort optimization".
///
/// Measured, 8 CPUs / 4 cores / SMT2, 4 exclusive hogs + 8 others, 300 ms:
///
/// | `preempt` | intervals | overlapping | of those, non-excl started second | `ExclPreempt` |
/// |---|---|---|---|---|
/// | false | 31 | 20 | 2 | 0 |
/// | true  | 91 | 75 | 5 | 44 |
///
/// A second confound makes any stronger claim unsafe today: per
/// `known_gap_no_task_ever_reaches_a_layer_dsq`, placement is not going through
/// the ordinary path at all. Revisit once mb sim-6mheb lands — at that point
/// the "started second" column is the one to look at.
#[test]
#[ignore = "measurement: exclusivity is best-effort upstream; prints, does not assert"]
fn measure_exclusive_layer_sibling_sharing() {
    let _lock = common::setup_test();
    for preempting in [false, true] {
        let topo = MachineTopology::uniform(8, 8, 1, 2);
        let sched = DynamicScheduler::layered_for_topology(&topo);
        sched.layered_layers(&[
            LayerSpec::new("excl", LayerKind::Open)
                .with_match(LayerMatch::CommPrefix("x".into()))
                .with_exclusive(true)
                .with_preempt(preempting),
            LayerSpec::catch_all("rest"),
        ]);
        let probes = LayeredProbes::new(&sched);
        let mut b = Scenario::builder()
            .cpus(8)
            .topology(topo)
            .detect_bpf_errors();
        for i in 0..4 {
            b = b.add_task(&format!("x{i}"), 0, hog());
        }
        for i in 0..8 {
            b = b.add_task(&format!("y{i}"), 0, hog());
        }
        let sim = Simulator::new(sched);
        let trace = sim.run(b.duration_ms(300).build());

        let mut open: BTreeMap<Pid, (u64, u32)> = BTreeMap::new();
        let mut ivals: Vec<(u32, u64, u64, Pid)> = Vec::new();
        for e in trace.events() {
            match e.kind {
                TraceKind::TaskScheduled { pid } => {
                    open.insert(pid, (e.time_ns, e.cpu.0));
                }
                TraceKind::TaskPreempted { pid }
                | TraceKind::TaskSlept { pid }
                | TraceKind::TaskYielded { pid } => {
                    if let Some((t0, cpu)) = open.remove(&pid) {
                        if e.time_ns > t0 {
                            ivals.push((cpu, t0, e.time_ns, pid));
                        }
                    }
                }
                _ => {}
            }
        }
        let is_excl = |p: Pid| probes.task_layer(p) == 0;
        let (mut checked, mut overlaps, mut started_second, mut worst) = (0, 0, 0, 0u64);
        for (cpu, a0, a1, pa) in &ivals {
            if !is_excl(*pa) {
                continue;
            }
            let sib = probes.sibling_cpu(CpuId(*cpu));
            if sib < 0 {
                continue;
            }
            checked += 1;
            for (cpu_b, b0, b1, pb) in &ivals {
                if *cpu_b != sib as u32 || is_excl(*pb) {
                    continue;
                }
                let (lo, hi) = ((*a0).max(*b0), (*a1).min(*b1));
                if hi > lo {
                    overlaps += 1;
                    worst = worst.max(hi - lo);
                    if *b0 > *a0 {
                        started_second += 1;
                    }
                }
            }
        }
        println!(
            "preempt={preempting:<5} intervals={checked:<4} overlaps={overlaps:<4} \
non-excl-started-second={started_second:<3} worst={:.3}ms  \
ExclIdle={} ExclWakeup={} ExclCollision={} ExclPreempt={}",
            worst as f64 / 1e6,
            probes.global_stat(GlobalStat::ExclIdle),
            probes.global_stat(GlobalStat::ExclWakeup),
            probes.layer_stat(0, LayerStat::ExclCollision),
            probes.layer_stat(0, LayerStat::ExclPreempt),
        );
    }
}
