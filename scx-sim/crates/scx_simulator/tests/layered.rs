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

/// Run the same asymmetric two-layer workload with the userspace control loop
/// either enabled or disabled, returning both serialized userspace masks and
/// the kptr cpumasks rebuilt by the real BPF refresh program.
fn run_reallocation_case(enable_control: bool) -> ((u32, u32), [Vec<bool>; 2]) {
    let sched = DynamicScheduler::layered(4);
    sched.layered_layers(&[
        LayerSpec::new("busy", LayerKind::Grouped)
            .with_match(LayerMatch::CommPrefix("busy".into()))
            .with_util_range(0.8, 0.9),
        LayerSpec::new("idle", LayerKind::Grouped)
            .with_or(Vec::new())
            .with_util_range(0.8, 0.9),
    ]);
    if enable_control {
        // Production's default scx_layered scheduling interval is 100ms.
        sched.layered_enable_control_loop(100_000_000);
    }
    let probes = LayeredProbes::new(&sched);
    let scenario = Scenario::builder()
        .cpus(4)
        .detect_bpf_errors()
        .add_task("busy_a", 0, hog())
        .add_task("busy_b", 0, hog())
        .add_task("busy_c", 0, hog())
        .add_task("busy_d", 0, hog())
        .add_task("idle", 0, workloads::periodic(1_000_000, 100_000_000))
        .duration_ms(700)
        .build();
    let sim = Simulator::new(sched);
    let trace = sim.run(scenario);
    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    let serialized = (probes.layer_nr_cpus(0), probes.layer_nr_cpus(1));
    let bpf_masks = [0, 1].map(|layer| {
        (0..4)
            .map(|cpu| probes.layer_bpf_has_cpu(layer, CpuId(cpu)))
            .collect()
    });
    (serialized, bpf_masks)
}

/// Run the same fully contended two-layer workload with the control cadence
/// enabled or disabled, and return the userspace growth-denial observations.
fn run_growth_denied_case(enable_control: bool) -> [(bool, u64); 2] {
    let sched = DynamicScheduler::layered(4);
    sched.layered_layers(&[
        LayerSpec::new("alpha", LayerKind::Grouped)
            .with_match(LayerMatch::CommPrefix("alpha".into()))
            .with_util_range(0.8, 0.9),
        LayerSpec::new("beta", LayerKind::Grouped)
            .with_or(Vec::new())
            .with_util_range(0.8, 0.9),
    ]);
    if enable_control {
        sched.layered_enable_control_loop(100_000_000);
    }
    let probes = LayeredProbes::new(&sched);
    let scenario = Scenario::builder()
        .cpus(4)
        .detect_bpf_errors()
        .add_task("alpha_a", 0, hog())
        .add_task("alpha_b", 0, hog())
        .add_task("beta_a", 0, hog())
        .add_task("beta_b", 0, hog())
        .duration_ms(800)
        .build();
    let sim = Simulator::new(sched);
    let trace = sim.run(scenario);
    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    [0, 1].map(|layer| {
        (
            probes.growth_denied(layer, 0),
            probes.growth_denied_count(layer, 0),
        )
    })
}

/// Force a single-core transfer and return the busy layer's final mask. The
/// workload and all targets are identical; only the upstream growth algorithm
/// differs.
fn run_core_order_case(growth_algo: LayerGrowthAlgo) -> Vec<bool> {
    let sched = DynamicScheduler::layered_with_topology(6, 2, 1, 1);
    sched.layered_layers(&[
        LayerSpec::new("busy", LayerKind::Grouped)
            .with_match(LayerMatch::CommPrefix("busy".into()))
            .with_util_range(0.8, 0.9)
            .with_cpus_range(3, 3)
            .with_growth_algo(growth_algo),
        LayerSpec::new("idle_a", LayerKind::Grouped)
            .with_match(LayerMatch::CommPrefix("idle_a".into()))
            .with_util_range(0.8, 0.9)
            .with_cpus_range(1, 1),
        LayerSpec::new("idle_b", LayerKind::Grouped)
            .with_or(Vec::new())
            .with_util_range(0.8, 0.9)
            .with_cpus_range(1, 1),
    ]);
    sched.layered_enable_control_loop(100_000_000);
    let probes = LayeredProbes::new(&sched);
    let scenario = Scenario::builder()
        .cpus(6)
        .cpus_per_llc(2)
        .detect_bpf_errors()
        .add_task("busy", 0, hog())
        .add_task("idle_a", 0, workloads::periodic(1_000_000, 100_000_000))
        .add_task("idle_b", 0, workloads::periodic(1_000_000, 100_000_000))
        .duration_ms(350)
        .build();
    let sim = Simulator::new(sched);
    let trace = sim.run(scenario);
    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    (0..6)
        .map(|cpu| probes.layer_bpf_has_cpu(0, CpuId(cpu)))
        .collect()
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
// Tier 3: periodic userspace CPU reallocation
// ---------------------------------------------------------------------------

/// The busy layer must take CPUs from the idle layer only when the real
/// userspace control cadence is enabled. The disabled arm is the negative
/// control: without it, a static allocator that happened to start with an
/// asymmetric split would make the enabled assertion pass vacuously.
#[test]
fn userspace_control_reallocates_cpus_from_idle_to_busy_layer() {
    let _lock = common::setup_test();
    let (disabled_counts, disabled_bpf_masks) = run_reallocation_case(false);
    let (enabled_counts, enabled_bpf_masks) = run_reallocation_case(true);

    assert_eq!(
        disabled_counts,
        (2, 2),
        "disabled loop must preserve the static split"
    );
    assert!(
        enabled_counts.0 > disabled_counts.0,
        "busy layer did not grow with control enabled: disabled={disabled_counts:?}, enabled={enabled_counts:?}"
    );
    assert!(
        enabled_counts.1 < disabled_counts.1,
        "idle layer did not shrink with control enabled: disabled={disabled_counts:?}, enabled={enabled_counts:?}"
    );
    for (result_name, serialized, bpf_masks) in [
        ("disabled", disabled_counts, disabled_bpf_masks),
        ("enabled", enabled_counts, enabled_bpf_masks),
    ] {
        let bpf_counts = (
            bpf_masks[0].iter().filter(|&&set| set).count() as u32,
            bpf_masks[1].iter().filter(|&&set| set).count() as u32,
        );
        assert_eq!(
            bpf_counts, serialized,
            "{result_name} serialized masks were not installed into BPF kptr cpumasks"
        );
    }
}

/// `growth_denied` must come from an actual allocation pass: under identical
/// contention it becomes observable only with the cadence enabled. Both
/// layers demand a third CPU but the real allocator can keep only the 2+2
/// split, so neither layer gains a CPU and both are denied.
#[test]
fn growth_denied_is_real_per_node_allocation_outcome() {
    let _lock = common::setup_test();
    let disabled = run_growth_denied_case(false);
    let enabled = run_growth_denied_case(true);

    assert_eq!(disabled, [(false, 0), (false, 0)]);
    for (layer, (current, count)) in enabled.into_iter().enumerate() {
        assert!(current, "layer {layer} denial was not current at run end");
        assert!(count > 0, "layer {layer} never recorded a denied pass");
    }
}

/// A passing reallocation count would not prove layer_core_growth executes.
/// With identical targets and topology, Linear must take freed core 3 while
/// Reverse must take freed core 5. These masks come from the real BPF kptrs
/// after the BPF_PROG_RUN refresh tail.
#[test]
fn upstream_linear_and_reverse_choose_different_freed_cores() {
    let _lock = common::setup_test();
    let linear = run_core_order_case(LayerGrowthAlgo::Linear);
    let reverse = run_core_order_case(LayerGrowthAlgo::Reverse);

    assert_eq!(linear.iter().filter(|&&cpu| cpu).count(), 3);
    assert_eq!(reverse.iter().filter(|&&cpu| cpu).count(), 3);
    assert!(
        linear[3] && !linear[5],
        "unexpected Linear mask: {linear:?}"
    );
    assert!(
        reverse[5] && !reverse[3],
        "unexpected Reverse mask: {reverse:?}"
    );
}

/// With SMT the allocator works in whole physical cores, and no layer may
/// ever hold half a core — a half-core allocation would let two layers share
/// an SMT pair, which is precisely what `excl` layers exist to prevent.
///
/// This test also pins down a NON-OBVIOUS upstream behaviour that an earlier
/// version of it got wrong by assuming the intuitive answer. With
/// `alloc_unit == 2`, a layer sitting at 4 CPUs (2 cores) whose target is
/// 2 CPUs can NEVER shrink:
///
///   dampened = 4 - ceil((4-2)/2) = 3 CPUs      (main.rs shrink dampening)
///   units    = ceil(3 / 2)       = 2 cores     (calc_raw_demands rounds UP)
///   allocated                    = 4 CPUs      -> unchanged, fixed point
///
/// So the 6/2 split the layer specs ask for is unreachable from an even 4/4
/// start, and the honest assertion is that the split stays 4/4 while every
/// core stays whole. Both halves matter: the first records real upstream
/// behaviour, the second is the invariant worth guarding. Confirmed against
/// `main.rs::refresh_cpumasks()`, which uses the same CPU-space dampening and
/// the same `target.div_ceil(au)`; filed as mb sim-klue5.
#[test]
fn smt_allocation_keeps_whole_cores_and_hits_the_shrink_fixed_point() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered_with_topology(8, 4, 1, 2);
    sched.layered_layers(&[
        LayerSpec::new("busy", LayerKind::Grouped)
            .with_match(LayerMatch::CommPrefix("busy".into()))
            .with_util_range(0.8, 0.9)
            .with_cpus_range(6, 6),
        LayerSpec::new("idle", LayerKind::Grouped)
            .with_or(Vec::new())
            .with_util_range(0.8, 0.9)
            .with_cpus_range(2, 2),
    ]);
    sched.layered_enable_control_loop(100_000_000);
    let probes = LayeredProbes::new(&sched);
    let scenario = Scenario::builder()
        .cpus(8)
        .cpus_per_llc(4)
        .smt(2)
        .detect_bpf_errors()
        .add_task("busy", 0, hog())
        .add_task("idle", 0, workloads::periodic(1_000_000, 100_000_000))
        .duration_ms(350)
        .build();
    let sim = Simulator::new(sched);
    let trace = sim.run(scenario);
    assert_eq!(trace.exit_kind(), &ExitKind::Normal);

    // The shrink fixed point, not the requested 6/2.
    assert_eq!(
        (probes.layer_nr_cpus(0), probes.layer_nr_cpus(1)),
        (4, 4),
        "expected the CPU-space-dampening / core-rounding fixed point"
    );

    // The invariant that must hold regardless: no half cores, in either the
    // serialized view or the BPF kptr mask the refresh tail rebuilt.
    for layer in 0..2 {
        for first in (0..8).step_by(2) {
            let serialized = (
                probes.layer_has_cpu(layer, CpuId(first)),
                probes.layer_has_cpu(layer, CpuId(first + 1)),
            );
            let bpf = (
                probes.layer_bpf_has_cpu(layer, CpuId(first)),
                probes.layer_bpf_has_cpu(layer, CpuId(first + 1)),
            );
            assert_eq!(
                serialized.0, serialized.1,
                "layer {layer} holds half of core {first} (serialized)"
            );
            assert_eq!(
                bpf.0, bpf.1,
                "layer {layer} holds half of core {first} (BPF kptr)"
            );
            assert_eq!(
                serialized, bpf,
                "serialized and BPF masks disagree on core {first} for layer {layer}"
            );
        }
    }
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

/// The topology the wrapper publishes must match the layout it was asked
/// for. This is an arithmetic check on the publication step only — it
/// compares against a re-derivation of the same inputs, so it would NOT
/// catch a `Scenario` and a `layered_with_topology` that were configured
/// inconsistently. `llc_topology_drives_dsq_selection` below is the test
/// that proves the published map actually reaches layered's decisions.
#[test]
fn topology_published_to_scheduler_matches_requested_layout() {
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

    // The engine assigns llc_id = cpu / cpus_per_llc (engine.rs build_cpus);
    // the wrapper must publish the same mapping for the same input.
    for cpu in 0..NR_CPUS {
        assert_eq!(
            probes.cpu_llc(CpuId(cpu)),
            cpu / CPUS_PER_LLC,
            "wrapper published the wrong LLC for cpu {cpu}"
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

/// The published LLC map must actually reach layered's DSQ decisions.
///
/// layered stamps each CPU's fallback DSQ id as `hi_fb_dsq_id(llc_id)` in
/// `create_llc()`, so a task's DSQ id carries the LLC of the CPU it was
/// placed on in its low bits. Tasks pinned into LLC 0 and LLC 1 must
/// therefore land on *different* DSQs.
///
/// The flat-topology arm is the control: with one LLC covering all 8 CPUs,
/// the identical workload must put every task on the SAME DSQ. Without it
/// this test would pass on any two DSQ ids that happened to differ.
#[test]
fn llc_topology_drives_dsq_selection() {
    let _lock = common::setup_test();

    // Six CPU-bound tasks, three pinned into each half of the machine.
    fn workload(cpus_per_llc: u32) -> Scenario {
        let mut b = Scenario::builder()
            .cpus(8)
            .detect_bpf_errors()
            .cpus_per_llc(cpus_per_llc);
        for (i, cpus) in [
            (1, vec![CpuId(0), CpuId(1)]),
            (2, vec![CpuId(0), CpuId(1)]),
            (3, vec![CpuId(0), CpuId(1)]),
            (4, vec![CpuId(4), CpuId(5)]),
            (5, vec![CpuId(4), CpuId(5)]),
            (6, vec![CpuId(4), CpuId(5)]),
        ] {
            b = b.task(pinned_task(
                &format!("t{i}"),
                Pid(i),
                workloads::cpu_bound(500_000_000),
                cpus,
            ));
        }
        b.duration_ms(300).build()
    }

    // Two LLCs of 4 CPUs: the two pinned groups sit in different LLCs.
    let sched = DynamicScheduler::layered_with_topology(8, 4, 1, 1);
    let probes = LayeredProbes::new(&sched);
    let sim = Simulator::new(sched);
    let t = sim.run(workload(4));
    assert_eq!(t.exit_kind(), &ExitKind::Normal);

    let llc0: Vec<u64> = (1..=3).map(|p| probes.task_dsq(Pid(p))).collect();
    let llc1: Vec<u64> = (4..=6).map(|p| probes.task_dsq(Pid(p))).collect();
    assert!(
        llc0.iter().all(|d| *d == llc0[0]),
        "tasks pinned within one LLC should share a DSQ, got {llc0:x?}"
    );
    assert!(
        llc1.iter().all(|d| *d == llc1[0]),
        "tasks pinned within one LLC should share a DSQ, got {llc1:x?}"
    );
    assert_ne!(
        llc0[0], llc1[0],
        "tasks in different LLCs landed on the same DSQ (0x{:x}) — the \
         published LLC map is not reaching layered's DSQ selection",
        llc0[0]
    );
    // The LLC index is the low bits of the DSQ id (DSQ_ID_LLC_MASK).
    assert_eq!(llc0[0] & 0xffff, 0, "expected LLC 0 in the DSQ id");
    assert_eq!(llc1[0] & 0xffff, 1, "expected LLC 1 in the DSQ id");
    drop(sim);

    // Control: one LLC over all 8 CPUs. Same workload, same pinning — every
    // task must now share a DSQ.
    let flat = DynamicScheduler::layered_with_topology(8, 8, 1, 1);
    let flat_probes = LayeredProbes::new(&flat);
    let flat_sim = Simulator::new(flat);
    let ft = flat_sim.run(workload(8));
    assert_eq!(ft.exit_kind(), &ExitKind::Normal);
    let all: Vec<u64> = (1..=6).map(|p| flat_probes.task_dsq(Pid(p))).collect();
    assert!(
        all.iter().all(|d| *d == all[0]),
        "with a single LLC every task must share a DSQ, got {all:x?}"
    );
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

// ---------------------------------------------------------------------------
// Cgroup-path matching
// ---------------------------------------------------------------------------

/// `MATCH_CGROUP_PREFIX` routes by the cgroup path scx_layered reconstructs
/// from `cgrp->kn->name` up the ancestor chain. This exercises
/// `util.bpf.c::format_cgrp_path()` and `match_prefix_suffix()` for real.
#[test]
fn cgroup_prefix_match_routes_tasks_by_cgroup_path() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(4);
    sched.layered_layers(&[
        LayerSpec::new("batchcg", LayerKind::Open)
            .with_match(LayerMatch::CgroupPrefix("batch".into())),
        LayerSpec::catch_all("rest"),
    ]);
    let probes = LayeredProbes::new(&sched);

    let scenario = Scenario::builder()
        .cpus(4)
        .detect_bpf_errors()
        .cgroup("batchgrp", &[CpuId(0), CpuId(1), CpuId(2), CpuId(3)])
        .cgroup("othergrp", &[CpuId(0), CpuId(1), CpuId(2), CpuId(3)])
        .add_task_in_cgroup("a", 0, run_once(10_000_000), "batchgrp")
        .add_task_in_cgroup("b", 0, run_once(10_000_000), "othergrp")
        .duration_ms(200)
        .build();
    let sim = Simulator::new(sched);
    let t = sim.run(scenario);
    assert_eq!(t.exit_kind(), &ExitKind::Normal);

    assert_eq!(
        probes.task_layer(Pid(1)),
        0,
        "task in /batchgrp should match the cgroup-prefix layer"
    );
    assert_eq!(
        probes.task_layer(Pid(2)),
        1,
        "task in /othergrp should fall through to the catch-all"
    );
}

/// `MATCH_CGROUP_SUFFIX` and `MATCH_CGROUP_CONTAINS` use different
/// `util.bpf.c` code paths (`match_prefix_suffix(.., true)` and
/// `match_substr()`); cover both.
#[test]
fn cgroup_suffix_and_contains_match() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(2);
    sched.layered_layers(&[
        LayerSpec::new("suffix", LayerKind::Open)
            // format_cgrp_path() renders a level-1 cgroup as "<name>/".
            .with_match(LayerMatch::CgroupSuffix("prod/".into())),
        LayerSpec::new("contains", LayerKind::Open)
            .with_match(LayerMatch::CgroupContains("mid".into())),
        LayerSpec::catch_all("rest"),
    ]);
    let probes = LayeredProbes::new(&sched);

    let scenario = Scenario::builder()
        .cpus(2)
        .detect_bpf_errors()
        .cgroup("appprod", &[CpuId(0), CpuId(1)])
        .cgroup("xmidy", &[CpuId(0), CpuId(1)])
        .cgroup("plain", &[CpuId(0), CpuId(1)])
        .add_task_in_cgroup("a", 0, run_once(5_000_000), "appprod")
        .add_task_in_cgroup("b", 0, run_once(5_000_000), "xmidy")
        .add_task_in_cgroup("c", 0, run_once(5_000_000), "plain")
        .duration_ms(200)
        .build();
    let sim = Simulator::new(sched);
    let t = sim.run(scenario);
    assert_eq!(t.exit_kind(), &ExitKind::Normal);

    assert_eq!(probes.task_layer(Pid(1)), 0, "appprod/ ends with prod/");
    assert_eq!(probes.task_layer(Pid(2)), 1, "xmidy/ contains mid");
    assert_eq!(probes.task_layer(Pid(3)), 2, "plain/ matches neither");
}

// ---------------------------------------------------------------------------
// Antistall timer
// ---------------------------------------------------------------------------

/// scx_layered arms one BPF timer (ANTISTALL_TIMER) from `ops.init` and
/// re-arms it from its own callback. Drive it with a shortened interval and
/// assert the callback actually ran repeatedly — the callback body *is*
/// `antistall_scan()`, so this proves the whole timer path executes.
#[test]
fn antistall_timer_fires_and_rearms() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(2);
    // 20ms scan period instead of the production 15s, so a 200ms run reaches
    // it. Purely a simulation accelerator; antistall_sec stays at production's
    // 3 seconds.
    sched.layered_set_antistall(true, 3, Some(20_000_000));
    let probes = LayeredProbes::new(&sched);

    let scenario = Scenario::builder()
        .cpus(2)
        .detect_bpf_errors()
        .add_task("a", 0, workloads::periodic(2_000_000, 5_000_000))
        .add_task("b", 0, workloads::periodic(2_000_000, 5_000_000))
        .duration_ms(200)
        .build();
    let sim = Simulator::new(sched);
    let t = sim.run(scenario);
    assert_eq!(t.exit_kind(), &ExitKind::Normal);

    let fires = probes.timer_fires();
    assert!(
        fires >= 5,
        "antistall timer should have fired repeatedly over 200ms at a 20ms \
         period, got {fires}"
    );
}

/// Firing the callback is not the same as antistall *doing* anything, so this
/// drives the mechanism to an observable effect.
///
/// A task whose affinity ({2,3}) excludes every CPU of its confined layer
/// ({0,1}) cannot be placed on a layer DSQ, so it queues on a fallback DSQ
/// that the layer's own CPUs are not draining — exactly the starvation
/// antistall exists to break. With eight competing hogs and
/// `--antistall-sec 0`, `antistall_set()` must flag a CPU for the delayed DSQ
/// and `antistall_consume()` must then drain it, bumping `GSTAT_ANTISTALL`.
///
/// The paired assertion is the point: the SAME workload with a one-hour
/// `antistall_sec` must leave the counter at zero. Without that control the
/// test would pass on a counter that increments unconditionally.
#[test]
fn antistall_consumes_a_delayed_dsq_only_past_the_delay_threshold() {
    let _lock = common::setup_test();

    fn run(antistall_sec: u64) -> (u64, u64) {
        let sched = DynamicScheduler::layered(4);
        sched.layered_layers(&[
            LayerSpec::new("pinned", LayerKind::Confined)
                .with_match(LayerMatch::CommPrefix("pin".into()))
                .with_cpus(vec![CpuId(0), CpuId(1)]),
            LayerSpec::catch_all("rest"),
        ]);
        // 50ms scan period (production hardcodes 15s) purely so the scan runs
        // often enough within a 4s simulated run.
        sched.layered_set_antistall(true, antistall_sec, Some(50_000_000));
        let probes = LayeredProbes::new(&sched);

        let mut b = Scenario::builder()
            .cpus(4)
            .detect_bpf_errors()
            .task(pinned_task(
                "pin_offside",
                Pid(1),
                workloads::cpu_bound(4_000_000_000),
                vec![CpuId(2), CpuId(3)],
            ));
        for i in 0..8 {
            b = b.add_task(&format!("hog{i}"), 0, workloads::cpu_bound(4_000_000_000));
        }
        let sim = Simulator::new(sched);
        let t = sim.run(b.duration_ms(4000).build());
        assert_eq!(t.exit_kind(), &ExitKind::Normal);
        (
            probes.timer_fires(),
            probes.global_stat(GlobalStat::Antistall),
        )
    }

    let (fires_hot, antistall_hot) = run(0);
    assert!(fires_hot > 0, "the antistall scan never ran");
    assert!(
        antistall_hot > 0,
        "antistall never consumed a delayed DSQ even with --antistall-sec 0; \
         the scan ran ({fires_hot} times) but had no effect"
    );

    let (fires_cold, antistall_cold) = run(3600);
    assert!(fires_cold > 0, "the antistall scan never ran (control)");
    assert_eq!(
        antistall_cold, 0,
        "antistall fired despite a one-hour delay threshold — the counter is \
         not actually gated on task delay"
    );
}

/// With antistall disabled the callback still runs (the timer is armed
/// unconditionally) but `antistall_scan()` returns 0 immediately, which stops
/// the re-arm — so it fires exactly once.
#[test]
fn disabled_antistall_stops_rearming_after_one_fire() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(2);
    sched.layered_set_antistall(false, 3, Some(20_000_000));
    let probes = LayeredProbes::new(&sched);

    let scenario = Scenario::builder()
        .cpus(2)
        .detect_bpf_errors()
        .add_task("a", 0, workloads::periodic(2_000_000, 5_000_000))
        .duration_ms(200)
        .build();
    let sim = Simulator::new(sched);
    let t = sim.run(scenario);
    assert_eq!(t.exit_kind(), &ExitKind::Normal);

    assert_eq!(
        probes.timer_fires(),
        1,
        "a disabled antistall scan returns 0 and must not re-arm"
    );
    assert_eq!(probes.global_stat(GlobalStat::Antistall), 0);
}

// ---------------------------------------------------------------------------
// ops.dump
// ---------------------------------------------------------------------------

/// `ops.dump` walks every layer, its per-LLC DSQs and both fallback DSQs, and
/// builds its per-match headers with `bpf_snprintf`. Assert on the text it
/// actually emits, not merely that it does not fault: a no-op dump and a
/// working dump are otherwise indistinguishable.
#[test]
fn ops_dump_emits_every_layer_and_both_fallback_dsqs() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered_with_topology(4, 2, 2, 1);
    sched.layered_layers(&[
        LayerSpec::new("batch", LayerKind::Grouped)
            .with_match(LayerMatch::CommPrefix("batch".into()))
            .with_weight(50),
        LayerSpec::new("iface", LayerKind::Open)
            .with_match(LayerMatch::NiceBelow(0))
            .with_preempt(true),
        LayerSpec::catch_all("rest"),
    ]);

    let scenario = Scenario::builder()
        .cpus(4)
        .detect_bpf_errors()
        .cpus_per_llc(2)
        .add_task("batch_a", 0, workloads::periodic(2_000_000, 5_000_000))
        .add_task("iface_b", -5, workloads::periodic(500_000, 3_000_000))
        .add_task("plain_c", 0, workloads::periodic(1_000_000, 4_000_000))
        .duration_ms(200)
        .build();
    let sim = Simulator::new(sched);
    let t = sim.run(scenario);
    assert_eq!(t.exit_kind(), &ExitKind::Normal);

    let dump = scx_simulator::kfuncs::dump_buffer_take();
    assert!(!dump.is_empty(), "ops.dump produced no output at all");
    for layer in ["batch", "iface", "rest"] {
        assert!(
            dump.contains(layer),
            "ops.dump never mentioned layer {layer:?}; it did not walk every \
             layer.\n--- dump ---\n{dump}"
        );
    }
    // Both per-LLC fallback DSQs are dumped by name.
    assert!(
        dump.contains("HI_") && dump.contains("LO_FALLBACK"),
        "ops.dump did not report the hi/lo fallback DSQs.\n--- dump ---\n{dump}"
    );
    // No conversion specifier may survive into the output. This is the
    // assertion that matters: a formatter that silently passes through the
    // specs it does not understand looks identical to a working one until you
    // check. (It caught a missing `+` flag, which left `%+lldms` in the dump.)
    let leftovers: Vec<&str> = dump
        .split('%')
        .skip(1)
        .filter(|tail| {
            tail.chars()
                .take_while(|c| "-+ #0123456789l".contains(*c))
                .count()
                < tail.len()
                && tail
                    .chars()
                    .find(|c| !"-+ #0123456789l".contains(*c))
                    .is_some_and(|c| "diuxscp".contains(c))
        })
        .collect();
    assert!(
        leftovers.is_empty(),
        "unformatted printf specs leaked into the dump — scx_bpf_dump_bstr \
         formatting is incomplete: {leftovers:?}\n--- dump ---\n{dump}"
    );
}

// ---------------------------------------------------------------------------
// tp_btf/cgroup_attach_task
// ---------------------------------------------------------------------------

/// scx_layered tracks layer membership by the DEFAULT cgroup hierarchy, so it
/// hooks `tp_btf/cgroup_attach_task` rather than `ops.cgroup_move`. Moving a
/// task between cgroups must therefore re-evaluate its layer: a task that
/// starts in a non-matching cgroup and is migrated into a matching one has to
/// end up in the cgroup-matched layer.
///
/// Without the tracepoint the task would stay in whatever layer it was first
/// assigned, so this is the test that proves the delivery path works end to
/// end (engine event → wrapper shim → BPF_PROG ctx marshalling → the real
/// `tp_cgroup_attach_task`).
#[test]
fn cgroup_migration_relayers_the_task_via_tp_btf() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(2);
    sched.layered_layers(&[
        LayerSpec::new("prodcg", LayerKind::Open)
            .with_match(LayerMatch::CgroupPrefix("prod".into())),
        LayerSpec::catch_all("rest"),
    ]);
    let probes = LayeredProbes::new(&sched);

    let all = [CpuId(0), CpuId(1)];
    let scenario = Scenario::builder()
        .cpus(2)
        .detect_bpf_errors()
        .cgroup("prodgrp", &all)
        .cgroup("devgrp", &all)
        // Starts in devgrp (catch-all), migrates into prodgrp mid-run.
        .add_task_in_cgroup(
            "mover",
            0,
            workloads::periodic(1_000_000, 3_000_000),
            "devgrp",
        )
        .cgroup_migrate(Pid(1), "devgrp", "prodgrp", 50_000_000)
        .duration_ms(200)
        .build();
    let sim = Simulator::new(sched);
    let t = sim.run(scenario);
    assert_eq!(t.exit_kind(), &ExitKind::Normal);

    assert_eq!(
        count(&t, |k| matches!(k, TraceKind::CgroupMove { .. })),
        1,
        "the migration should have happened"
    );
    assert_eq!(
        probes.task_layer(Pid(1)),
        0,
        "after migrating into /prodgrp the task must be re-layered into the \
         cgroup-prefix layer — tp_btf/cgroup_attach_task did not reach the \
         scheduler"
    );
}

/// The mirror image: migrating OUT of a matching cgroup must drop the task
/// back to the catch-all. Guards against a one-way re-layering that only ever
/// promotes.
#[test]
fn cgroup_migration_out_of_a_matching_cgroup_relayers_back() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(2);
    sched.layered_layers(&[
        LayerSpec::new("prodcg", LayerKind::Open)
            .with_match(LayerMatch::CgroupPrefix("prod".into())),
        LayerSpec::catch_all("rest"),
    ]);
    let probes = LayeredProbes::new(&sched);

    let all = [CpuId(0), CpuId(1)];
    let scenario = Scenario::builder()
        .cpus(2)
        .detect_bpf_errors()
        .cgroup("prodgrp", &all)
        .cgroup("devgrp", &all)
        .add_task_in_cgroup(
            "mover",
            0,
            workloads::periodic(1_000_000, 3_000_000),
            "prodgrp",
        )
        .cgroup_migrate(Pid(1), "prodgrp", "devgrp", 50_000_000)
        .duration_ms(200)
        .build();
    let sim = Simulator::new(sched);
    let t = sim.run(scenario);
    assert_eq!(t.exit_kind(), &ExitKind::Normal);

    assert_eq!(
        probes.task_layer(Pid(1)),
        1,
        "after migrating out of /prodgrp the task must fall back to the \
         catch-all layer"
    );
}

// ---------------------------------------------------------------------------
// tp_btf/task_rename
// ---------------------------------------------------------------------------

/// scx_layered classifies by `p->comm`, so a rename can change which layer a
/// task belongs to. The kernel signals that through the `task_rename` BTF
/// tracepoint; scx_layered's handler sets `refresh_layer` and the next
/// scheduling event re-matches.
///
/// Renaming a task from a non-matching to a matching name must move it.
#[test]
fn task_rename_relayers_the_task_via_tp_btf() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(2);
    sched.layered_layers(&[
        LayerSpec::new("batch", LayerKind::Open).with_match(LayerMatch::CommPrefix("batch".into())),
        LayerSpec::catch_all("rest"),
    ]);
    let probes = LayeredProbes::new(&sched);

    let scenario = Scenario::builder()
        .cpus(2)
        .detect_bpf_errors()
        .add_task("plain", 0, workloads::periodic(1_000_000, 3_000_000))
        .task_rename(Pid(1), "batch_now", 50_000_000)
        .duration_ms(200)
        .build();
    let sim = Simulator::new(sched);
    let t = sim.run(scenario);
    assert_eq!(t.exit_kind(), &ExitKind::Normal);

    assert_eq!(
        probes.task_layer(Pid(1)),
        0,
        "after renaming to batch_now the task must move into the \
         comm-prefix layer — tp_btf/task_rename did not reach the scheduler"
    );
}

/// The mirror image: renaming out of a matching name must drop the task back
/// to the catch-all.
#[test]
fn task_rename_out_of_a_matching_name_relayers_back() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(2);
    sched.layered_layers(&[
        LayerSpec::new("batch", LayerKind::Open).with_match(LayerMatch::CommPrefix("batch".into())),
        LayerSpec::catch_all("rest"),
    ]);
    let probes = LayeredProbes::new(&sched);

    let scenario = Scenario::builder()
        .cpus(2)
        .detect_bpf_errors()
        .add_task("batch_x", 0, workloads::periodic(1_000_000, 3_000_000))
        .task_rename(Pid(1), "plain_now", 50_000_000)
        .duration_ms(200)
        .build();
    let sim = Simulator::new(sched);
    let t = sim.run(scenario);
    assert_eq!(t.exit_kind(), &ExitKind::Normal);

    assert_eq!(
        probes.task_layer(Pid(1)),
        1,
        "after renaming away from batch* the task must fall back to the \
         catch-all layer"
    );
}

/// External-oracle check on `growth_denied`, with DELIBERATELY ASYMMETRIC
/// inputs.
///
/// `growth_denied_is_real_per_node_allocation_outcome` above contrasts the
/// cadence enabled against disabled. That is a PRESENCE check: with the loop
/// off nothing runs, so `false/0` is trivially true and the contrast proves
/// only that the loop executed. It is also SYMMETRIC — both layers are
/// saturated and both are denied — so it would still pass if the
/// implementation attributed each layer's denial to the other. Neither
/// weakness is visible from a green run.
///
/// This test predicts the answer from the SCENARIO SPEC alone, before running
/// anything, and makes the two layers differ:
///
///   `hot`  — 4 always-runnable tasks on a 4-CPU box, so its measured
///            utilization saturates and `unpinned_cpus_needed = util/0.9`
///            always exceeds the CPUs it holds. It wants to grow, and with
///            every CPU already spoken for it cannot. Predict: DENIED.
///   `cold` — one task asleep ~99% of the time AND a spec-set floor of one
///            CPU, so `unpinned_cpus_needed = 0.01/0.9 = 0.011` never exceeds
///            the >=1 CPU it holds. It never wants to grow.
///            Predict: NOT DENIED.
///
/// The floor is load-bearing and was discovered by this test failing: without
/// it the saturated layer drives `cold` to zero CPUs, and a 1% duty cycle DOES
/// exceed zero, so `cold` is correctly denied as well. The first version of
/// this oracle asserted `cold == 0` without the floor and was simply wrong
/// about the scenario, not about the code.
///
/// Asserting on the cumulative counts rather than the instantaneous flag
/// keeps it independent of which pass happens to land last. The asymmetry is
/// the point: swapping the two layers' denials breaks this test, while a
/// symmetric conservation law over the same run would survive the swap and
/// prove nothing.
#[test]
fn growth_denied_matches_a_prediction_made_from_the_scenario_spec() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered(4);
    sched.layered_layers(&[
        LayerSpec::new("hot", LayerKind::Grouped)
            .with_match(LayerMatch::CommPrefix("hot".into()))
            .with_util_range(0.8, 0.9),
        LayerSpec::new("cold", LayerKind::Grouped)
            .with_or(Vec::new())
            .with_util_range(0.8, 0.9)
            // The floor is what creates the asymmetry, and it is spec-set:
            // without it the saturated layer squeezes `cold` to ZERO CPUs, at
            // which point even a 1% duty cycle exceeds the zero unpinned CPUs
            // it holds, so `cold` is legitimately denied too and the
            // prediction collapses to symmetric. Verified: without this line
            // cold ends at 0 CPUs and records 6 denials.
            .with_cpus_range(1, 4),
    ]);
    sched.layered_enable_control_loop(100_000_000);
    let probes = LayeredProbes::new(&sched);

    let mut b = Scenario::builder().cpus(4).detect_bpf_errors();
    for i in 0..4 {
        b = b.add_task(&format!("hot_{i}"), 0, hog());
    }
    // ~1% duty cycle: measured utilization stays far below one CPU.
    b = b.add_task("cold_a", 0, workloads::periodic(1_000_000, 100_000_000));
    let sim = Simulator::new(sched);
    let trace = sim.run(b.duration_ms(800).build());
    assert_eq!(trace.exit_kind(), &ExitKind::Normal);

    // The oracle's premise, asserted rather than assumed: if the floor did
    // not hold, this test would silently be proving something else.
    assert!(
        probes.layer_nr_cpus(1) >= 1,
        "cold lost its spec-set CPU floor, so the prediction below no longer follows"
    );
    let hot = probes.growth_denied_count(0, 0);
    let cold = probes.growth_denied_count(1, 0);
    assert!(
        hot > 0,
        "the saturated layer must have been denied growth at least once, got {hot}"
    );
    assert_eq!(
        cold, 0,
        "the ~idle layer never wants to grow, so it must never be denied; \
         got {cold}. A non-zero value here means denial is being attributed \
         to the wrong layer, or `wanted` is not actually reading utilization."
    );
}

/// Whole-core allocation, on a path that ACTUALLY MOVES A CORE.
///
/// `smt_allocation_keeps_whole_cores_and_hits_the_shrink_fixed_point` asserts
/// the no-half-core invariant, but it sits at the fixed point: `to_free` is
/// zero, so the core-granular shrink and grow loops never execute and the
/// invariant is never put at risk. Sabotaging those loops to release a single
/// thread instead of a whole core left all 32 tests green — the assertion was
/// unreachable. This test exists because of that.
///
/// Construction, chosen so a transfer is possible at all under SMT:
///   - 8 CPUs = 4 cores, `alloc_unit` 2.
///   - Weights put `busy` on 1 core and `idle` on 3 at start; their
///     `cpus_range` targets pull the other way (busy 3 cores, idle 1).
///   - idle's shrink clears the halving threshold — `6 - ceil((6-2)/2) = 4`
///     CPUs = 2 cores — so it genuinely releases a core, unlike the 2-core to
///     1-core case which is a fixed point (mb sim-klue5).
///
/// The transfer itself is asserted, so the test fails if the loops go idle.
#[test]
fn smt_core_transfer_moves_whole_cores_only() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered_with_topology(8, 8, 1, 2);
    sched.layered_layers(&[
        LayerSpec::new("busy", LayerKind::Grouped)
            .with_match(LayerMatch::CommPrefix("busy".into()))
            .with_util_range(0.8, 0.9)
            .with_cpus_range(6, 6)
            // Weight drives the INITIAL auto-allocation (8*100/400 = 2 CPUs
            // = 1 core). `with_cpus` cannot be used here: the control loop
            // refuses to resize an explicitly pinned layer, which is correct
            // and is itself asserted by
            // `cpuset_growth_is_rejected_instead_of_approximated`.
            .with_weight(100),
        LayerSpec::new("idle", LayerKind::Grouped)
            .with_or(Vec::new())
            .with_util_range(0.8, 0.9)
            .with_cpus_range(2, 2)
            // 8*300/400 = 6 CPUs = 3 cores initially.
            .with_weight(300),
    ]);
    sched.layered_enable_control_loop(100_000_000);
    let probes = LayeredProbes::new(&sched);
    let scenario = Scenario::builder()
        .cpus(8)
        .smt(2)
        .detect_bpf_errors()
        .add_task("busy_a", 0, hog())
        .add_task("busy_b", 0, hog())
        .add_task("idle_a", 0, workloads::periodic(1_000_000, 100_000_000))
        .duration_ms(500)
        .build();
    let sim = Simulator::new(sched);
    let trace = sim.run(scenario);
    assert_eq!(trace.exit_kind(), &ExitKind::Normal);

    // A core really moved: without this the no-half-core assertions below are
    // unreachable and would pass on a loop that did nothing.
    let busy = probes.layer_nr_cpus(0);
    assert!(
        busy > 2,
        "no core was transferred (busy still holds {busy} CPUs), so the \
         whole-core assertions below would prove nothing"
    );

    for layer in 0..2 {
        for first in (0..8).step_by(2) {
            let serialized = (
                probes.layer_has_cpu(layer, CpuId(first)),
                probes.layer_has_cpu(layer, CpuId(first + 1)),
            );
            let bpf = (
                probes.layer_bpf_has_cpu(layer, CpuId(first)),
                probes.layer_bpf_has_cpu(layer, CpuId(first + 1)),
            );
            assert_eq!(
                serialized.0, serialized.1,
                "layer {layer} holds half of core {first} (serialized)"
            );
            assert_eq!(
                bpf.0, bpf.1,
                "layer {layer} holds half of core {first} (BPF kptr)"
            );
            assert_eq!(serialized, bpf, "views disagree on core {first}");
        }
    }
}
