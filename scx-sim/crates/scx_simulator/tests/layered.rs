//! scx_layered integration tests.
//!
//! scx_layered partitions tasks into *layers* selected by comm / cgroup /
//! nice / pid rules. Each layer has its own CPU set, slice, preemption
//! policy, and a DSQ per (layer, LLC); tasks that cannot be placed fall back
//! to per-LLC hi/lo fallback DSQs.
//!
//! Everything asserted here is read back out of the scheduler's own state
//! (`task_ctx.layer_id`, `layer->nr_cpus`, the `lstats`/`gstats` counters) via
//! [`LayeredProbes`], or out of the engine's trace — never re-derived.

use scx_simulator::probes::{GlobalStat, LayerStat, LayeredEnumProbe, LayeredProbes};
use scx_simulator::*;

#[macro_use]
mod common;

/// A CPU-bound task that runs for the whole simulation.
fn hog() -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(500_000_000)],
        repeat: RepeatMode::Forever,
    }
}

/// A task that runs once for `run_ns` and exits.
fn run_once(run_ns: u64) -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(run_ns)],
        repeat: RepeatMode::Once,
    }
}

/// Count trace events matching `pred`.
fn count<F: Fn(&TraceKind) -> bool>(trace: &Trace, pred: F) -> usize {
    trace.events().iter().filter(|e| pred(&e.kind)).count()
}

/// A task pinned to `cpus` (the builder has no affinity shorthand).
fn pinned_task(name: &str, pid: Pid, behavior: TaskBehavior, cpus: Vec<CpuId>) -> TaskDef {
    TaskDef {
        name: name.to_string(),
        pid,
        nice: 0,
        behavior,
        start_time_ns: 0,
        mm_id: None,
        allowed_cpus: Some(cpus),
        parent_pid: None,
        cgroup_name: None,
        task_flags: 0,
        migration_disabled: 0,
    }
}

// ---------------------------------------------------------------------------
// ABI guard
// ---------------------------------------------------------------------------

/// The Rust `LayerKind` / `LayerMatch` / `LayerGrowthAlgo` values are the
/// intf.h enum values. Compare them against what the compiled scheduler
/// reports, so an upstream reordering of `enum layer_match_kind` fails here
/// instead of silently mis-configuring every layer.
#[test]
fn layer_enum_abi_matches_bpf() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(1);
    let p = LayeredProbes::new(&sched);

    assert_eq!(
        p.enum_value(LayeredEnumProbe::KindOpen),
        LayerKind::Open as i32
    );
    assert_eq!(
        p.enum_value(LayeredEnumProbe::KindGrouped),
        LayerKind::Grouped as i32
    );
    assert_eq!(
        p.enum_value(LayeredEnumProbe::KindConfined),
        LayerKind::Confined as i32
    );

    assert_eq!(
        p.enum_value(LayeredEnumProbe::GrowthSticky),
        LayerGrowthAlgo::Sticky as i32
    );
    assert_eq!(
        p.enum_value(LayeredEnumProbe::GrowthLinear),
        LayerGrowthAlgo::Linear as i32
    );
    assert_eq!(
        p.enum_value(LayeredEnumProbe::GrowthReverse),
        LayerGrowthAlgo::Reverse as i32
    );
    assert_eq!(
        p.enum_value(LayeredEnumProbe::GrowthTopo),
        LayerGrowthAlgo::Topo as i32
    );
    assert_eq!(
        p.enum_value(LayeredEnumProbe::GrowthRoundRobin),
        LayerGrowthAlgo::RoundRobin as i32
    );

    // Every LayerMatch variant's lowered kind must equal the BPF enum value.
    let cases: [(LayerMatch, LayeredEnumProbe); 16] = [
        (
            LayerMatch::CgroupPrefix("x".into()),
            LayeredEnumProbe::MatchCgroupPrefix,
        ),
        (
            LayerMatch::CommPrefix("x".into()),
            LayeredEnumProbe::MatchCommPrefix,
        ),
        (
            LayerMatch::PcommPrefix("x".into()),
            LayeredEnumProbe::MatchPcommPrefix,
        ),
        (LayerMatch::NiceAbove(0), LayeredEnumProbe::MatchNiceAbove),
        (LayerMatch::NiceBelow(0), LayeredEnumProbe::MatchNiceBelow),
        (LayerMatch::NiceEquals(0), LayeredEnumProbe::MatchNiceEquals),
        (
            LayerMatch::UserIdEquals(0),
            LayeredEnumProbe::MatchUserIdEquals,
        ),
        (
            LayerMatch::GroupIdEquals(0),
            LayeredEnumProbe::MatchGroupIdEquals,
        ),
        (LayerMatch::PidEquals(0), LayeredEnumProbe::MatchPidEquals),
        (LayerMatch::PpidEquals(0), LayeredEnumProbe::MatchPpidEquals),
        (LayerMatch::TgidEquals(0), LayeredEnumProbe::MatchTgidEquals),
        (
            LayerMatch::IsGroupLeader(true),
            LayeredEnumProbe::MatchIsGroupLeader,
        ),
        (
            LayerMatch::IsKthread(true),
            LayeredEnumProbe::MatchIsKthread,
        ),
        (
            LayerMatch::CgroupSuffix("x".into()),
            LayeredEnumProbe::MatchCgroupSuffix,
        ),
        (
            LayerMatch::CgroupContains("x".into()),
            LayeredEnumProbe::MatchCgroupContains,
        ),
        (LayerMatch::NumaNode(0), LayeredEnumProbe::MatchNumaNode),
    ];
    for (m, probe) in cases {
        assert_eq!(
            m.match_kind(),
            p.enum_value(probe),
            "match kind mismatch for {m:?}"
        );
    }

    assert_eq!(
        p.enum_value(LayeredEnumProbe::DefaultLayerWeight),
        DEFAULT_LAYER_WEIGHT as i32
    );
    // Tests below assume MAX_LAYERS == 16 (the LAYERED_NO_LAYER sentinel).
    assert_eq!(p.enum_value(LayeredEnumProbe::MaxLayers), 16);
}

// ---------------------------------------------------------------------------
// Default (single catch-all layer) configuration
// ---------------------------------------------------------------------------

/// The bare constructor must produce a runnable scheduler: it loads, ops.init
/// succeeds, tasks run to completion and the run exits normally.
#[test]
fn default_config_loads_and_runs() {
    let _lock = common::setup_test();
    let scenario = Scenario::builder()
        .cpus(4)
        .detect_bpf_errors()
        .add_task("a", 0, run_once(20_000_000))
        .add_task("b", 0, run_once(20_000_000))
        .add_task("c", 0, run_once(20_000_000))
        .duration_ms(200)
        .build();
    let t = Simulator::new(DynamicScheduler::layered(4)).run(scenario);

    assert_eq!(
        t.exit_kind(),
        &ExitKind::Normal,
        "layered did not exit cleanly"
    );
    assert!(
        count(&t, |k| matches!(k, TraceKind::TaskScheduled { .. })) > 0,
        "no task was ever scheduled"
    );
    assert_eq!(
        count(&t, |k| matches!(k, TraceKind::TaskCompleted { .. })),
        3,
        "all three tasks should have completed"
    );
}

/// With the default configuration there is exactly one layer, it owns every
/// CPU (it is an OPEN layer), and every task ends up in it.
#[test]
fn default_config_is_one_catch_all_layer_owning_all_cpus() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(4);
    let probes = LayeredProbes::new(&sched);
    let scenario = Scenario::builder()
        .cpus(4)
        .detect_bpf_errors()
        .add_task("a", 0, run_once(20_000_000))
        .add_task("b", 0, run_once(20_000_000))
        .duration_ms(200)
        .build();
    let sim = Simulator::new(sched);
    let t = sim.run(scenario);
    assert_eq!(t.exit_kind(), &ExitKind::Normal);

    assert_eq!(probes.nr_layers(), 1);
    assert_eq!(probes.layer_nr_cpus(0), 4);
    for cpu in 0..4 {
        assert!(
            probes.layer_has_cpu(0, CpuId(cpu)),
            "catch-all layer must own cpu {cpu}"
        );
    }
    for pid in [Pid(1), Pid(2)] {
        assert_eq!(
            probes.task_layer(pid),
            0,
            "{pid:?} should be in the catch-all layer"
        );
    }
}

/// A saturated run must make forward progress on every task — no layer may
/// starve, and the watchdog must not fire.
#[test]
fn oversubscribed_run_starves_nobody() {
    let _lock = common::setup_test();
    let mut b = Scenario::builder()
        .cpus(2)
        .detect_bpf_errors()
        .duration_ms(300);
    for i in 0..6 {
        b = b.add_task(&format!("hog{i}"), 0, hog());
    }
    let t = Simulator::new(DynamicScheduler::layered(2)).run(b.build());
    assert_eq!(t.exit_kind(), &ExitKind::Normal);

    for pid in 1..=6i32 {
        let ran = t
            .events()
            .iter()
            .any(|e| matches!(e.kind, TraceKind::TaskScheduled { pid: p } if p == Pid(pid)));
        assert!(ran, "pid {pid} never ran under layered");
    }
}

// ---------------------------------------------------------------------------
// Topology
// ---------------------------------------------------------------------------

/// The scheduler must observe the same LLC layout the engine simulates.
/// `layered_with_topology` is the only way it learns the topology, so a
/// mismatch here means every LLC-affinity decision layered makes is wrong.
#[test]
fn topology_seen_by_scheduler_matches_the_engine() {
    let _lock = common::setup_test();
    const NR_CPUS: u32 = 8;
    const CPUS_PER_LLC: u32 = 4;

    let sched = DynamicScheduler::layered_with_topology(NR_CPUS, CPUS_PER_LLC, 2, 1);
    let probes = LayeredProbes::new(&sched);
    let scenario = Scenario::builder()
        .cpus(NR_CPUS)
        .detect_bpf_errors()
        .cpus_per_llc(CPUS_PER_LLC)
        .add_task("a", 0, run_once(20_000_000))
        .duration_ms(100)
        .build();

    // Cross-check against the engine's own per-CPU llc_id before running.
    for cpu in 0..NR_CPUS {
        assert_eq!(
            probes.cpu_llc(CpuId(cpu)),
            cpu / CPUS_PER_LLC,
            "scheduler disagrees with the engine about cpu {cpu}'s LLC"
        );
    }
    assert_eq!(probes.nr_llcs(), NR_CPUS / CPUS_PER_LLC);
    assert_eq!(probes.nr_nodes(), 2);
    assert_eq!(probes.cpu_node(CpuId(0)), 0);
    assert_eq!(probes.cpu_node(CpuId(4)), 1);

    let sim = Simulator::new(sched);
    let t = sim.run(scenario);
    assert_eq!(t.exit_kind(), &ExitKind::Normal);
}

/// With SMT enabled the scheduler must know each CPU's sibling; without it,
/// `__sibling_cpu` must be -1 so layered's exclusive-layer logic short-circuits.
#[test]
fn smt_siblings_are_published_only_when_smt_is_on() {
    let _lock = common::setup_test();

    let flat = DynamicScheduler::layered_with_topology(4, 0, 1, 1);
    let flat_probes = LayeredProbes::new(&flat);
    for cpu in 0..4 {
        assert_eq!(
            flat_probes.sibling_cpu(CpuId(cpu)),
            -1,
            "cpu {cpu} should have no sibling with SMT off"
        );
    }
    drop(flat);

    let smt = DynamicScheduler::layered_with_topology(4, 0, 1, 2);
    let smt_probes = LayeredProbes::new(&smt);
    // The engine lays siblings out as consecutive pairs.
    assert_eq!(smt_probes.sibling_cpu(CpuId(0)), 1);
    assert_eq!(smt_probes.sibling_cpu(CpuId(1)), 0);
    assert_eq!(smt_probes.sibling_cpu(CpuId(2)), 3);
    assert_eq!(smt_probes.sibling_cpu(CpuId(3)), 2);
}

/// A multi-LLC, multi-node, SMT run must complete cleanly — this is the
/// topology on which layered's per-(layer, LLC) DSQs and cross-LLC migration
/// logic actually run.
#[test]
fn runs_on_multi_llc_numa_smt_topology() {
    let _lock = common::setup_test();
    const NR_CPUS: u32 = 8;

    let sched = DynamicScheduler::layered_with_topology(NR_CPUS, 4, 2, 2);
    let mut b = Scenario::builder()
        .cpus(NR_CPUS)
        .detect_bpf_errors()
        .cpus_per_llc(4)
        .smt(2)
        .duration_ms(300);
    for i in 0..8 {
        b = b.add_task(
            &format!("t{i}"),
            0,
            workloads::periodic(2_000_000, 6_000_000),
        );
    }
    let t = Simulator::new(sched).run(b.build());
    assert_eq!(t.exit_kind(), &ExitKind::Normal);
    assert!(count(&t, |k| matches!(k, TraceKind::TaskScheduled { .. })) > 0);
}

// ---------------------------------------------------------------------------
// Multi-layer configuration and matching
// ---------------------------------------------------------------------------

/// A two-layer config with a comm-prefix rule: tasks named `batch*` must land
/// in the batch layer, everything else in the catch-all. This is the first
/// test in which layered's actual layer-selection policy runs.
#[test]
fn comm_prefix_match_routes_tasks_to_the_right_layer() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(4);
    sched.layered_layers(&[
        LayerSpec::new("batch", LayerKind::Open)
            .with_match(LayerMatch::CommPrefix("batch".into()))
            .with_weight(50),
        LayerSpec::catch_all("normal").with_weight(100),
    ]);
    let probes = LayeredProbes::new(&sched);

    let scenario = Scenario::builder()
        .cpus(4)
        .detect_bpf_errors()
        .add_task("batch_a", 0, run_once(20_000_000))
        .add_task("batch_b", 0, run_once(20_000_000))
        .add_task("iface_c", 0, run_once(20_000_000))
        .duration_ms(200)
        .build();
    let sim = Simulator::new(sched);
    let t = sim.run(scenario);
    assert_eq!(t.exit_kind(), &ExitKind::Normal);

    assert_eq!(probes.nr_layers(), 2);
    assert_eq!(probes.task_layer(Pid(1)), 0, "batch_a should match layer 0");
    assert_eq!(probes.task_layer(Pid(2)), 0, "batch_b should match layer 0");
    assert_eq!(
        probes.task_layer(Pid(3)),
        1,
        "iface_c should fall through to the catch-all"
    );
    // `layer->nr_tasks` is back to zero here: every task has exited and
    // layered_disable() dropped its membership. That teardown accounting is
    // asserted by `layered_disable_drops_layer_membership`; what matters here
    // is the routing above.
}

/// A negated rule (`LayerMatch::Not`) must invert the match: everything
/// *except* `batch*` goes to layer 0.
#[test]
fn negated_match_inverts_layer_selection() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(2);
    sched.layered_layers(&[
        LayerSpec::new("not_batch", LayerKind::Open).with_match(LayerMatch::Not(Box::new(
            LayerMatch::CommPrefix("batch".into()),
        ))),
        LayerSpec::catch_all("rest"),
    ]);
    let probes = LayeredProbes::new(&sched);

    let scenario = Scenario::builder()
        .cpus(2)
        .detect_bpf_errors()
        .add_task("batch_a", 0, run_once(10_000_000))
        .add_task("other_b", 0, run_once(10_000_000))
        .duration_ms(200)
        .build();
    let sim = Simulator::new(sched);
    let t = sim.run(scenario);
    assert_eq!(t.exit_kind(), &ExitKind::Normal);

    assert_eq!(
        probes.task_layer(Pid(1)),
        1,
        "batch_a is excluded from layer 0"
    );
    assert_eq!(probes.task_layer(Pid(2)), 0, "other_b matches !batch");
}

/// Two ANDed rules in one OR group must both hold. `nice > 0 AND comm ~ bg*`
/// matches only the task satisfying both.
#[test]
fn anded_rules_require_every_condition() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(2);
    sched.layered_layers(&[
        LayerSpec::new("bg_and_nice", LayerKind::Open).with_or(vec![
            LayerMatch::CommPrefix("bg".into()),
            LayerMatch::NiceAbove(0),
        ]),
        LayerSpec::catch_all("rest"),
    ]);
    let probes = LayeredProbes::new(&sched);

    let scenario = Scenario::builder()
        .cpus(2)
        .detect_bpf_errors()
        .add_task("bg_nice", 5, run_once(10_000_000)) // both conditions
        .add_task("bg_norm", 0, run_once(10_000_000)) // comm only
        .add_task("fg_nice", 5, run_once(10_000_000)) // nice only
        .duration_ms(200)
        .build();
    let sim = Simulator::new(sched);
    let t = sim.run(scenario);
    assert_eq!(t.exit_kind(), &ExitKind::Normal);

    assert_eq!(probes.task_layer(Pid(1)), 0, "bg_nice matches both ANDs");
    assert_eq!(probes.task_layer(Pid(2)), 1, "bg_norm fails the nice AND");
    assert_eq!(probes.task_layer(Pid(3)), 1, "fg_nice fails the comm AND");
}

/// Separate OR groups are alternatives: a task matching either lands in the
/// layer.
#[test]
fn or_groups_are_alternatives() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(2);
    sched.layered_layers(&[
        LayerSpec::new("either", LayerKind::Open)
            .with_match(LayerMatch::CommPrefix("aaa".into()))
            .with_match(LayerMatch::CommPrefix("bbb".into())),
        LayerSpec::catch_all("rest"),
    ]);
    let probes = LayeredProbes::new(&sched);

    let scenario = Scenario::builder()
        .cpus(2)
        .detect_bpf_errors()
        .add_task("aaa_1", 0, run_once(10_000_000))
        .add_task("bbb_2", 0, run_once(10_000_000))
        .add_task("ccc_3", 0, run_once(10_000_000))
        .duration_ms(200)
        .build();
    let sim = Simulator::new(sched);
    let t = sim.run(scenario);
    assert_eq!(t.exit_kind(), &ExitKind::Normal);

    assert_eq!(probes.task_layer(Pid(1)), 0);
    assert_eq!(probes.task_layer(Pid(2)), 0);
    assert_eq!(probes.task_layer(Pid(3)), 1);
}

/// A layer pinned to an explicit CPU set must be published with exactly that
/// set, and its tasks must run only on those CPUs. This is the confinement
/// property that makes CONFINED layers meaningful.
#[test]
fn confined_layer_runs_only_on_its_own_cpus() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(4);
    sched.layered_layers(&[
        LayerSpec::new("pinned", LayerKind::Confined)
            .with_match(LayerMatch::CommPrefix("pin".into()))
            .with_cpus(vec![CpuId(0), CpuId(1)]),
        LayerSpec::catch_all("rest").with_cpus(vec![CpuId(2), CpuId(3)]),
    ]);
    let probes = LayeredProbes::new(&sched);

    let scenario = Scenario::builder()
        .cpus(4)
        .detect_bpf_errors()
        .add_task("pin_a", 0, workloads::periodic(2_000_000, 4_000_000))
        .add_task("pin_b", 0, workloads::periodic(2_000_000, 4_000_000))
        .add_task("other_c", 0, workloads::periodic(2_000_000, 4_000_000))
        .duration_ms(300)
        .build();
    let sim = Simulator::new(sched);
    let t = sim.run(scenario);
    assert_eq!(t.exit_kind(), &ExitKind::Normal);

    assert_eq!(probes.layer_nr_cpus(0), 2);
    assert!(probes.layer_has_cpu(0, CpuId(0)));
    assert!(probes.layer_has_cpu(0, CpuId(1)));
    assert!(!probes.layer_has_cpu(0, CpuId(2)));
    assert!(!probes.layer_has_cpu(0, CpuId(3)));

    // The confined layer's tasks must never be scheduled outside {0, 1}.
    for e in t.events() {
        if let TraceKind::TaskScheduled { pid } = e.kind {
            if pid == Pid(1) || pid == Pid(2) {
                let cpu = e.cpu;
                assert!(
                    cpu == CpuId(0) || cpu == CpuId(1),
                    "confined-layer {pid:?} ran on {cpu:?}, outside its layer CPUs"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ops.yield / ops.set_weight / ops.disable, scheduler side
// ---------------------------------------------------------------------------

/// scx_layered implements `ops.yield`, so the engine must deliver the call
/// and must NOT apply the kernel's no-ops.yield fallback of zeroing the
/// slice. `layered_yield()` always returns false and, with the default
/// `yield_step_ns == 0`, counts every call in `LSTAT_YIELD_IGNORE` — its
/// documented "yielding is completely ignored" configuration.
#[test]
fn layered_handles_ops_yield() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(1);
    let probes = LayeredProbes::new(&sched);
    let scenario = Scenario::builder()
        .cpus(1)
        .detect_bpf_errors()
        .add_task(
            "y",
            0,
            TaskBehavior {
                phases: vec![Phase::Run(1_000_000), Phase::Yield],
                repeat: RepeatMode::Count(5),
            },
        )
        .duration_ms(200)
        .build();
    let sim = Simulator::new(sched);
    let t = sim.run(scenario);
    assert_eq!(t.exit_kind(), &ExitKind::Normal);

    let handled = t
        .events()
        .iter()
        .filter(|e| matches!(e.kind, TraceKind::TaskYield { handled: true, .. }))
        .count();
    assert_eq!(
        handled, 5,
        "every sched_yield() should have reached scx_layered's own ops.yield"
    );

    // layered_yield() bumps exactly one of these two per call, and with
    // yield_step_ns == 0 it is always LSTAT_YIELD_IGNORE.
    assert_eq!(
        probes.layer_stat(0, LayerStat::YieldIgnore),
        5,
        "yield_step_ns == 0 means every yield is counted as ignored"
    );
    assert_eq!(probes.layer_stat(0, LayerStat::Yield), 0);
}

/// `ops.disable` and `ops.exit_task` must both reach scx_layered, and
/// `layered_disable()` must drop the task's layer membership so
/// `layer->nr_tasks` returns to zero at teardown.
#[test]
fn layered_disable_drops_layer_membership() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(2);
    let probes = LayeredProbes::new(&sched);
    let scenario = Scenario::builder()
        .cpus(2)
        .detect_bpf_errors()
        .add_task("a", 0, run_once(10_000_000))
        .add_task("b", 0, run_once(10_000_000))
        .duration_ms(200)
        .build();
    let sim = Simulator::new(sched);
    let t = sim.run(scenario);
    assert_eq!(t.exit_kind(), &ExitKind::Normal);

    assert_eq!(count(&t, |k| matches!(k, TraceKind::Disable { .. })), 2);
    assert_eq!(
        probes.layer_nr_tasks(0),
        0,
        "layered_disable should have dropped every task's layer membership"
    );
}

// ---------------------------------------------------------------------------
// Fallback DSQs and antistall
// ---------------------------------------------------------------------------

/// A task whose affinity excludes every CPU of its (confined) layer cannot be
/// placed on a layer DSQ, so layered must route it to a fallback DSQ rather
/// than losing it. Forward progress is the property that matters.
#[test]
fn task_outside_its_layer_cpus_still_makes_progress() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(4);
    sched.layered_layers(&[
        LayerSpec::new("pinned", LayerKind::Confined)
            .with_match(LayerMatch::CommPrefix("pin".into()))
            .with_cpus(vec![CpuId(0), CpuId(1)]),
        LayerSpec::catch_all("rest"),
    ]);
    let probes = LayeredProbes::new(&sched);

    let scenario = Scenario::builder()
        .cpus(4)
        .detect_bpf_errors()
        // Affinity {2,3} but matched into the layer that owns {0,1}.
        .task(pinned_task(
            "pin_offside",
            Pid(1),
            workloads::periodic(1_000_000, 3_000_000),
            vec![CpuId(2), CpuId(3)],
        ))
        .add_task("other", 0, workloads::periodic(1_000_000, 3_000_000))
        .duration_ms(300)
        .build();
    let sim = Simulator::new(sched);
    let t = sim.run(scenario);
    assert_eq!(t.exit_kind(), &ExitKind::Normal);

    assert!(
        t.events()
            .iter()
            .any(|e| matches!(e.kind, TraceKind::TaskScheduled { pid, .. } if pid == Pid(1))),
        "the off-side task never ran — it was lost rather than sent to a fallback DSQ"
    );
    // It is in the confined layer but had to use a fallback path.
    assert_eq!(probes.task_layer(Pid(1)), 0);
    assert!(
        probes.global_stat(GlobalStat::HiFbEvents) + probes.global_stat(GlobalStat::LoFbEvents) > 0,
        "expected the fallback DSQs to have been used"
    );
}

// ---------------------------------------------------------------------------
// Determinism
// ---------------------------------------------------------------------------

/// Two identical runs must produce identical traces. scx_layered has several
/// sources of potential nondeterminism (per-CPU layer scan orders, proximity
/// maps, `bpf_get_prandom_u32`); this pins them all down at once.
#[test]
fn layered_runs_are_deterministic() {
    let _lock = common::setup_test();
    let build = || {
        Scenario::builder()
            .cpus(4)
            .detect_bpf_errors()
            .cpus_per_llc(2)
            .add_task("batch_a", 0, workloads::periodic(2_000_000, 5_000_000))
            .add_task("batch_b", 5, workloads::periodic(1_000_000, 4_000_000))
            .add_task("iface_c", -5, workloads::periodic(500_000, 2_000_000))
            .duration_ms(200)
            .build()
    };
    let make = || {
        let s = DynamicScheduler::layered_with_topology(4, 2, 2, 1);
        s.layered_layers(&[
            LayerSpec::new("batch", LayerKind::Grouped)
                .with_match(LayerMatch::CommPrefix("batch".into()))
                .with_weight(50),
            LayerSpec::catch_all("normal").with_weight(200),
        ]);
        s
    };

    let a = Simulator::new(make()).run(build());
    let b = Simulator::new(make()).run(build());
    assert_eq!(a.exit_kind(), &ExitKind::Normal);
    assert_eq!(b.exit_kind(), &ExitKind::Normal);
    assert_eq!(
        a.events().len(),
        b.events().len(),
        "two identical layered runs produced different event counts"
    );
    for (x, y) in a.events().iter().zip(b.events().iter()) {
        assert_eq!(
            (x.time_ns, x.cpu, &x.kind),
            (y.time_ns, y.cpu, &y.kind),
            "two identical layered runs diverged"
        );
    }
}
