//! Arbitrary virtual topologies: sockets, NUMA nodes, LLCs, SMT, asymmetry.
//!
//! # What this file is for
//!
//! Two things, and the second is what makes the first worth anything:
//!
//! 1. **mb sim-dox34 stays fixed.** With `nr_nodes > 1`, scxsim used to run
//!    everything on node 0 — `cpus_that_ran` was exactly `nr_cpus / nr_nodes`
//!    at 2, 4 and 8 nodes, from 32 CPUs up. `layered_large_topology.rs`'s
//!    `known_gap_multi_node_confines_all_work_to_node_zero` has been inverted
//!    into `multi_node_topologies_reach_every_node`; this file covers the
//!    same property across more shapes and both allocation modes.
//! 2. **The claim is checked by observation, not by report.** Every
//!    assertion here reads which CPUs the ENGINE recorded a `TaskScheduled`
//!    on. `probes.nr_nodes()` returning 8 proves only that a number was
//!    written into a variable. A topology that reports correctly and
//!    schedules wrongly is exactly the bug this file exists to keep out —
//!    the reported-vs-observed distinction is checked explicitly in
//!    [`the_engine_and_the_scheduler_agree_about_the_machine`].
//!
//! # The two mechanisms, and why both are load-bearing
//!
//! `pick_idle_cpu()` in scx_layered searches the task's LOCAL node and
//! crosses a NUMA boundary only through the cross-NUMA gate, which upstream
//! writes from `refresh_xnuma()` on every control-loop iteration. So:
//!
//! * with the control loop OFF (scxsim's static Tier-2 allocation, which has
//!   no userspace at all), the gate stays shut — faithfully, because upstream
//!   has nothing to write it — and work only reaches a node if tasks are BORN
//!   there. That is [`ForkPlacement`];
//! * with it ON, [`crate::layered_xnuma`]'s port of `refresh_xnuma` opens the
//!   gate once a node is loaded, imbalanced and unable to grow, and work
//!   migrates.
//!
//! Both arms are exercised below, separately, so a regression in one is not
//! masked by the other.

use scx_simulator::*;

#[macro_use]
mod common;

/// A CPU-bound task that runs for the whole simulation. Hogs never sleep, so
/// after their first wakeup they only move by `ops.dispatch` consuming them —
/// which makes them the sharpest probe of whether work can cross a node.
fn hog() -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(500_000_000)],
        repeat: RepeatMode::Forever,
    }
}

/// Two grouped layers matched by comm prefix, so layered's real match
/// evaluation runs rather than everything landing in one catch-all.
fn two_layer_specs() -> Vec<LayerSpec> {
    vec![
        LayerSpec::new("hot", LayerKind::Grouped)
            .with_match(LayerMatch::CommPrefix("hot".into()))
            .with_util_range(0.8, 0.9),
        LayerSpec::new("rest", LayerKind::Grouped)
            .with_or(Vec::new())
            .with_util_range(0.8, 0.9),
    ]
}

/// Which CPUs actually ran a task, from the engine's trace. The only evidence
/// any assertion in this file rests on.
fn cpus_that_ran(trace: &Trace) -> Vec<u32> {
    let mut seen: Vec<u32> = trace
        .events()
        .iter()
        .filter(|e| matches!(e.kind, TraceKind::TaskScheduled { .. }))
        .map(|e| e.cpu.0)
        .collect();
    seen.sort_unstable();
    seen.dedup();
    seen
}

/// Distinct NUMA nodes that ran a task, per `topo`.
fn nodes_that_ran(trace: &Trace, topo: &MachineTopology) -> Vec<u32> {
    let mut nodes: Vec<u32> = cpus_that_ran(trace)
        .into_iter()
        .map(|c| topo.node_of(CpuId(c)))
        .collect();
    nodes.sort_unstable();
    nodes.dedup();
    nodes
}

struct Run {
    trace: Trace,
    probes: LayeredProbes,
    // `probes` holds raw fn pointers into the dlopen'd `.so`, so the
    // Simulator that owns the scheduler has to outlive every probe read.
    _sim_kept_alive: Simulator<DynamicScheduler>,
}

/// Run `nr_tasks` hogs on `topo` under scx_layered for `duration_ms`.
fn run_layered(
    topo: &MachineTopology,
    specs: &[LayerSpec],
    nr_tasks: usize,
    duration_ms: u64,
    control_loop: bool,
) -> Run {
    let sched = DynamicScheduler::layered_for_topology(topo);
    sched.layered_layers(specs);
    if control_loop {
        // Production's default scx_layered scheduling interval.
        sched.layered_enable_control_loop(100_000_000);
    }
    let probes = LayeredProbes::new(&sched);
    let mut b = Scenario::builder()
        .topology(topo.clone())
        .detect_bpf_errors()
        .seed(42)
        .duration_ms(duration_ms);
    for i in 0..nr_tasks {
        // A quarter carry the "hot" prefix so both layers are live.
        let name = if i % 4 == 0 {
            format!("hot_{i}")
        } else {
            format!("bulk_{i}")
        };
        b = b.add_task(&name, 0, hog());
    }
    let sim = Simulator::new(sched);
    let trace = sim.run(b.build());
    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    Run {
        trace,
        probes,
        _sim_kept_alive: sim,
    }
}

// ---------------------------------------------------------------------------
// sim-dox34: work reaches every node
// ---------------------------------------------------------------------------

/// The property mb sim-dox34 was filed against, at the shapes it was measured
/// at. Static allocation, no userspace control loop.
///
/// Before the fix each of these produced `cpus_that_ran == nr_cpus/nr_nodes`,
/// always the lowest ids. `max_cpu` is asserted alongside the count because
/// the count alone would also be satisfied by the whole machine running on
/// the wrong half.
#[test]
fn multi_node_static_allocation_reaches_every_cpu() {
    let _lock = common::setup_test();
    for &(nr_cpus, cpus_per_llc, nr_nodes) in &[
        (32u32, 8u32, 2u32),
        (64, 8, 2),
        (64, 8, 4),
        (64, 8, 8),
        (128, 16, 2),
    ] {
        let topo = MachineTopology::uniform(nr_cpus, cpus_per_llc, nr_nodes, 1);
        let run = run_layered(
            &topo,
            &two_layer_specs(),
            (nr_cpus * 2) as usize,
            100,
            false,
        );
        let ran = cpus_that_ran(&run.trace);
        assert_eq!(
            ran.len() as u32,
            nr_cpus,
            "{nr_cpus} CPUs / {nr_nodes} nodes: only {} CPUs ran (max id {:?}). \
             sim-dox34 regression: the pre-fix value here was {}.",
            ran.len(),
            ran.last(),
            nr_cpus / nr_nodes
        );
        assert_eq!(
            ran.last().copied(),
            Some(nr_cpus - 1),
            "{nr_cpus} CPUs / {nr_nodes} nodes: the top CPU never ran"
        );
        assert_eq!(
            nodes_that_ran(&run.trace, &topo),
            (0..nr_nodes).collect::<Vec<_>>(),
            "{nr_cpus} CPUs / {nr_nodes} nodes: some node ran nothing"
        );
    }
}

/// Same property with the userspace control loop running, which is the
/// configuration production actually uses. This arm additionally exercises
/// the ported `refresh_xnuma()`: layer CPU sets move between nodes while the
/// run is in progress, so a task can find itself on a node its layer no
/// longer owns and has to be pulled across.
#[test]
fn multi_node_with_control_loop_reaches_every_cpu() {
    let _lock = common::setup_test();
    for &(nr_cpus, cpus_per_llc, nr_nodes, smt) in
        &[(64u32, 8u32, 2u32, 1u32), (64, 8, 4, 1), (128, 16, 2, 2)]
    {
        let topo = MachineTopology::uniform(nr_cpus, cpus_per_llc, nr_nodes, smt);
        let run = run_layered(&topo, &two_layer_specs(), (nr_cpus * 2) as usize, 200, true);
        let ran = cpus_that_ran(&run.trace);
        assert_eq!(
            ran.len() as u32,
            nr_cpus,
            "{nr_cpus} CPUs / {nr_nodes} nodes / smt{smt} with control loop: \
             only {} CPUs ran (max id {:?})",
            ran.len(),
            ran.last()
        );
        assert_eq!(
            nodes_that_ran(&run.trace, &topo),
            (0..nr_nodes).collect::<Vec<_>>()
        );
    }
}

/// The 384-CPU dual-socket shape from upstream scx PR 3718 — the machine this
/// whole line of work is aimed at. sched-test#140 got 384 CPUs running as ONE
/// flat node; this is the same count with the socket topology it actually
/// has.
#[test]
fn the_384_cpu_dual_socket_machine_runs_on_both_sockets() {
    let _lock = common::setup_test();
    // EPYC-like: 8-core / 16-thread CCX, 24 CCXs, 12 per socket.
    let topo = MachineTopology::uniform(384, 16, 2, 2);
    assert_eq!(topo.nr_llcs(), 24);
    let run = run_layered(&topo, &two_layer_specs(), 768, 100, true);
    let ran = cpus_that_ran(&run.trace);
    assert_eq!(
        ran.len(),
        384,
        "only {} of 384 CPUs ran (max id {:?})",
        ran.len(),
        ran.last()
    );
    assert_eq!(ran.last().copied(), Some(383));
    // Both sockets, and neither of them carrying a token share.
    let per_socket = |n: u32| ran.iter().filter(|&&c| topo.node_of(CpuId(c)) == n).count();
    assert_eq!(per_socket(0), 192, "socket 0 did not run all of its CPUs");
    assert_eq!(per_socket(1), 192, "socket 1 did not run all of its CPUs");
}

// ---------------------------------------------------------------------------
// The mechanism, asserted directly rather than inferred from the outcome
// ---------------------------------------------------------------------------

/// The cross-NUMA gate is written by the USERSPACE control loop and by
/// nothing else, so it must be shut without one and open with one.
///
/// Both halves matter. Asserting only the second would pass against a wrapper
/// that hardcoded an infinite budget at init — which is the fake that would
/// have made `multi_node_*_reaches_every_cpu` green without porting anything.
#[test]
fn the_cross_numa_gate_comes_from_the_control_loop_and_nowhere_else() {
    let _lock = common::setup_test();
    let topo = MachineTopology::uniform(64, 8, 2, 1);

    // No control loop: upstream's refresh_xnuma() never runs, so every budget
    // is still the BSS zero, which xnuma_gate() reads as deny.
    let statically = run_layered(&topo, &two_layer_specs(), 128, 100, false);
    for layer in 0..statically.probes.nr_layers() {
        for src in 0..2u32 {
            assert!(
                !statically.probes.xnuma_is_mig_src(layer, src),
                "layer {layer} node {src}: no userspace ran, so nothing may have \
                 written xnuma_is_mig_src"
            );
            for dst in 0..2u32 {
                assert_eq!(
                    statically.probes.xnuma_rate(layer, src, dst),
                    0,
                    "layer {layer} {src}->{dst}: no userspace ran, so the budget \
                     must still be zero"
                );
            }
        }
    }

    // With the control loop, the saturated node becomes a migration source
    // and gets a finite, non-infinite budget out of it.
    let dynamically = run_layered(&topo, &two_layer_specs(), 128, 300, true);
    let opened = (0..dynamically.probes.nr_layers())
        .flat_map(|l| (0..2u32).map(move |n| (l, n)))
        .any(|(l, n)| dynamically.probes.xnuma_is_mig_src(l, n));
    assert!(
        opened,
        "128 hogs on 64 CPUs across 2 nodes saturate the machine; refresh_xnuma \
         should have marked at least one (layer, node) a migration source"
    );
    let budgeted = (0..dynamically.probes.nr_layers())
        .flat_map(|l| (0..2u32).flat_map(move |s| (0..2u32).map(move |d| (l, s, d))))
        .filter(|&(_, s, d)| s != d)
        .map(|(l, s, d)| dynamically.probes.xnuma_rate(l, s, d))
        .collect::<Vec<_>>();
    assert!(
        budgeted.iter().any(|&r| r > 0),
        "refresh_xnuma should have computed a non-zero water-fill rate; got {budgeted:?}"
    );
    assert!(
        budgeted.iter().all(|&r| r != u64::MAX),
        "u64::MAX is 'gating off', which only the (0,0)-threshold config selects; \
         the default (0.6, 0.7) config must produce finite rates. Got {budgeted:?}"
    );
}

/// Upstream's "gating off" configuration — `xnuma_threshold` both `<= 0` —
/// publishes an infinite budget in every direction, on every layer.
///
/// This is the branch a config uses to say "do not restrict cross-node
/// migration at all", and it is the one a shortcut fix would have hardcoded.
/// Asserting it is reachable through the CONFIG keeps it available without
/// letting it become the default.
#[test]
fn threshold_zero_selects_upstreams_gating_off_branch() {
    let _lock = common::setup_test();
    let topo = MachineTopology::uniform(64, 8, 2, 1);
    let specs: Vec<LayerSpec> = two_layer_specs()
        .into_iter()
        .map(|s| s.with_xnuma_threshold((0.0, 0.0), (0.0, 0.0)))
        .collect();
    let run = run_layered(&topo, &specs, 128, 200, true);
    for layer in 0..run.probes.nr_layers() {
        for src in 0..2u32 {
            assert!(run.probes.xnuma_is_mig_src(layer, src));
            for dst in 0..2u32 {
                assert_eq!(
                    run.probes.xnuma_rate(layer, src, dst),
                    u64::MAX,
                    "gating off must write an infinite budget for layer {layer} \
                     {src}->{dst}"
                );
            }
        }
    }
    assert_eq!(cpus_that_ran(&run.trace).len(), 64);
}

/// `layer_duty_sum` — the counter the gate decides from — is accumulated by
/// the real BPF `layered_stopping()`, not by anything scxsim adds.
///
/// It has to be non-zero on a loaded node for the gate to have any input at
/// all, and it counts runnable time rather than CPU time, so on a machine
/// with 2 hogs per CPU it must EXCEED the node's CPU count in CPU-seconds.
/// That inequality is what distinguishes it from `layer_node_usage`, and a
/// probe wired to the wrong field would fail it.
#[test]
fn the_gates_input_counts_runnable_time_not_cpu_time() {
    let _lock = common::setup_test();
    let topo = MachineTopology::uniform(64, 8, 2, 1);
    let run = run_layered(&topo, &two_layer_specs(), 128, 200, true);

    let duty: u64 = (0..run.probes.nr_layers())
        .flat_map(|l| (0..2u32).map(move |n| (l, n)))
        .map(|(l, n)| run.probes.layer_node_duty_raw(l, n))
        .sum();
    let usage: u64 = (0..run.probes.nr_layers())
        .flat_map(|l| (0..2u32).map(move |n| (l, n)))
        .map(|(l, n)| run.probes.layer_node_usage(l, n))
        .sum();
    assert!(duty > 0, "layer_duty_sum never accumulated");
    assert!(
        duty > usage,
        "at 2 hogs per CPU every task spends about half its time queued, so the \
         duty sum ({duty} ns) must exceed CPU time ({usage} ns). If they are equal \
         the probe is reading layer_usages, not layer_duty_sum."
    );
}

// ---------------------------------------------------------------------------
// Expressiveness: what shapes can the simulator actually be given
// ---------------------------------------------------------------------------

/// Reported topology and simulated topology are the same machine.
///
/// The scheduler's own published maps (`probes.cpu_llc` / `cpu_node` /
/// `sibling_cpu` / `nr_llcs` / `nr_nodes`) are compared against the
/// `MachineTopology` the ENGINE was built from, CPU by CPU, across a range of
/// shapes. This is the check that makes every other assertion in this file
/// mean what it says: without it, "work reached node 3" would be a claim
/// about a node id nobody had verified belonged to those CPUs.
#[test]
fn the_engine_and_the_scheduler_agree_about_the_machine() {
    let _lock = common::setup_test();
    let shapes: Vec<MachineTopology> = vec![
        MachineTopology::uniform(8, 8, 1, 1),
        MachineTopology::uniform(64, 8, 2, 1),
        MachineTopology::uniform(64, 8, 8, 1),
        MachineTopology::uniform(128, 16, 2, 2),
        MachineTopology::uniform(384, 16, 2, 2),
        MachineTopology::uniform(96, 8, 4, 4),
        asymmetric_two_socket(),
        smt_on_one_socket_only(),
    ];
    for topo in shapes {
        let sched = DynamicScheduler::layered_for_topology(&topo);
        let probes = LayeredProbes::new(&sched);
        let label = format!(
            "{}c/{}llc/{}node",
            topo.nr_cpus(),
            topo.nr_llcs(),
            topo.nr_nodes()
        );
        assert_eq!(probes.nr_llcs(), topo.nr_llcs(), "{label}: nr_llcs");
        assert_eq!(probes.nr_nodes(), topo.nr_nodes(), "{label}: nr_nodes");
        for cpu in 0..topo.nr_cpus() {
            let c = CpuId(cpu);
            assert_eq!(probes.cpu_llc(c), topo.llc_of(c), "{label}: cpu {cpu} llc");
            assert_eq!(
                probes.cpu_node(c),
                topo.node_of(c),
                "{label}: cpu {cpu} node"
            );
            let want = topo.sibling_cpu(c).map_or(-1, |s| s.0 as i32);
            assert_eq!(
                probes.sibling_cpu(c),
                want,
                "{label}: cpu {cpu} SMT partner"
            );
        }
    }
}

/// An asymmetric dual socket: 48 CPUs on socket 0 in 8-CPU LLCs, 16 CPUs on
/// socket 1 in one big LLC. Nothing about this is expressible with the four
/// uniform numbers.
fn asymmetric_two_socket() -> MachineTopology {
    let mut cpus = Vec::new();
    for cpu in 0..48u32 {
        cpus.push(CpuTopology {
            core_id: cpu,
            llc_id: cpu / 8,
            node_id: 0,
        });
    }
    for cpu in 48..64u32 {
        cpus.push(CpuTopology {
            core_id: cpu,
            llc_id: 6,
            node_id: 1,
        });
    }
    MachineTopology::from_cpus(cpus)
}

/// SMT2 on socket 0, single-thread cores on socket 1. Real hybrid and
/// mixed-generation machines look like this; the uniform constructor cannot.
fn smt_on_one_socket_only() -> MachineTopology {
    let mut cpus = Vec::new();
    // 32 CPUs, 16 SMT2 cores, 2 LLCs.
    for cpu in 0..32u32 {
        cpus.push(CpuTopology {
            core_id: cpu / 2,
            llc_id: cpu / 16,
            node_id: 0,
        });
    }
    // 16 CPUs, 16 single-thread cores, 1 LLC.
    for cpu in 32..48u32 {
        cpus.push(CpuTopology {
            core_id: 16 + (cpu - 32),
            llc_id: 2,
            node_id: 1,
        });
    }
    MachineTopology::from_cpus(cpus)
}

/// An asymmetric machine does not just publish — it schedules, on every CPU
/// of both differently-shaped sockets.
#[test]
fn an_asymmetric_dual_socket_machine_runs_on_all_of_it() {
    let _lock = common::setup_test();
    let topo = asymmetric_two_socket();
    let run = run_layered(&topo, &two_layer_specs(), 128, 100, false);
    let ran = cpus_that_ran(&run.trace);
    assert_eq!(
        ran.len(),
        64,
        "only {} of 64 CPUs ran on the asymmetric machine (max id {:?})",
        ran.len(),
        ran.last()
    );
    let socket1 = ran.iter().filter(|&&c| c >= 48).count();
    assert_eq!(
        socket1, 16,
        "the smaller socket ran {socket1} of its 16 CPUs"
    );
}

/// The same for an asymmetric-SMT machine, and it also pins the consequence:
/// the control loop refuses this shape instead of averaging a
/// `threads_per_core` that does not exist.
#[test]
fn asymmetric_smt_runs_statically_and_is_refused_by_the_control_loop() {
    let _lock = common::setup_test();
    let topo = smt_on_one_socket_only();
    let run = run_layered(&topo, &two_layer_specs(), 96, 100, false);
    assert_eq!(cpus_that_ran(&run.trace).len(), 48);

    let sched = DynamicScheduler::layered_for_topology(&topo);
    sched.layered_layers(&two_layer_specs());
    let refused = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        sched.layered_enable_control_loop(100_000_000);
    }));
    let err = refused.expect_err("an asymmetric machine has no single threads_per_core");
    let msg = err
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| err.downcast_ref::<&str>().copied())
        .unwrap_or("");
    assert!(
        msg.contains("uniform machine"),
        "the refusal must say why; got {msg:?}"
    );
}

/// Sub-NUMA clustering: one socket presented as several NUMA nodes, which is
/// what AMD NPS2/NPS4 and Intel SNC produce. Distinguished from a real
/// multi-socket machine only by the node count relative to the LLCs, which is
/// exactly how the simulator models it — there is no socket concept beneath
/// the node (see `MachineTopology` for that limit).
#[test]
fn sub_numa_clustering_reaches_every_node() {
    let _lock = common::setup_test();
    // 64 CPUs, 8 LLCs, presented as NPS4: 4 nodes of 2 LLCs.
    let topo = MachineTopology::uniform(64, 8, 4, 1);
    let run = run_layered(&topo, &two_layer_specs(), 128, 100, false);
    assert_eq!(nodes_that_ran(&run.trace, &topo), vec![0, 1, 2, 3]);
    assert_eq!(cpus_that_ran(&run.trace).len(), 64);
}

// ---------------------------------------------------------------------------
// Fork placement
// ---------------------------------------------------------------------------

/// `ForkPlacement::Auto` must be a no-op on a single-node machine.
///
/// Every scenario in the tree predates this knob and declares no nodes, so
/// the default has to leave them all exactly as they were. Asserted against
/// the event count, which is a whole-trace fingerprint: a different fork CPU
/// changes the first placement of every task and cannot leave it identical.
#[test]
fn auto_fork_placement_changes_nothing_on_a_single_node_machine() {
    let _lock = common::setup_test();
    let topo = MachineTopology::uniform(32, 8, 1, 1);
    let a = run_layered(&topo, &two_layer_specs(), 64, 50, false);
    let events_auto = a.trace.events().len();

    let sched = DynamicScheduler::layered_for_topology(&topo);
    sched.layered_layers(&two_layer_specs());
    let mut b = Scenario::builder()
        .topology(topo.clone())
        .fork_placement(ForkPlacement::FirstCpu)
        .detect_bpf_errors()
        .seed(42)
        .duration_ms(50);
    for i in 0..64 {
        let name = if i % 4 == 0 {
            format!("hot_{i}")
        } else {
            format!("bulk_{i}")
        };
        b = b.add_task(&name, 0, hog());
    }
    let sim = Simulator::new(sched);
    let trace = sim.run(b.build());
    assert_eq!(
        trace.events().len(),
        events_auto,
        "Auto and FirstCpu must produce identical runs on one node"
    );
}

/// Every fork placement puts tasks where it says it does, before any
/// scheduling happens.
///
/// Checked on the FIRST CPU each task was scheduled on, in pid order, with a
/// scheduler that cannot move anything: one task per CPU on an idle machine
/// stays where it was born.
#[test]
fn fork_placement_puts_tasks_where_it_says() {
    let _lock = common::setup_test();
    let topo = MachineTopology::uniform(8, 2, 4, 1);
    let cases = [
        (ForkPlacement::FirstCpu, vec![0u32; 4]),
        // 4 nodes of 2 CPUs: first CPUs are 0, 2, 4, 6.
        (ForkPlacement::RoundRobinNodes, vec![0, 2, 4, 6]),
        (ForkPlacement::RoundRobinCpus, vec![0, 1, 2, 3]),
    ];
    for (placement, want) in cases {
        let mut b = Scenario::builder()
            .topology(topo.clone())
            .fork_placement(placement)
            .seed(42)
            .duration_ms(10);
        for i in 0..4 {
            b = b.add_task(&format!("t_{i}"), 0, hog());
        }
        let scenario = b.build();
        let got: Vec<u32> = scenario.tasks.iter().map(|t| t.initial_cpu().0).collect();
        assert_eq!(got, want, "{placement:?} placed tasks on {got:?}");
    }
}

/// An explicit cpumask still wins over the fork placement, because in the
/// kernel a new task's CPU is always inside its own affinity.
#[test]
fn an_explicit_cpumask_overrides_the_fork_placement() {
    let _lock = common::setup_test();
    let topo = MachineTopology::uniform(8, 2, 4, 1);
    let scenario = Scenario::builder()
        .topology(topo)
        .fork_placement(ForkPlacement::RoundRobinNodes)
        .seed(42)
        .duration_ms(10)
        .task(TaskDef {
            name: "pinned".into(),
            pid: Pid(1),
            nice: 0,
            behavior: hog(),
            start_time_ns: 0,
            mm_id: None,
            allowed_cpus: Some(vec![CpuId(5), CpuId(6)]),
            parent_pid: None,
            cgroup_name: None,
            task_flags: 0,
            migration_disabled: 0,
            thread_group_leader: None,
            uid: Uid(0),
            gid: Gid(0),
            fork_cpu: None,
        })
        .add_task("free", 0, hog())
        .build();
    assert_eq!(scenario.tasks[0].initial_cpu(), CpuId(5));
    // Second task, so node 1, whose first CPU is 2.
    assert_eq!(scenario.tasks[1].initial_cpu(), CpuId(2));
}

// ---------------------------------------------------------------------------
// Cross-node migration cost
// ---------------------------------------------------------------------------

/// Crossing a socket costs more than crossing a CCX, which costs more than
/// staying put — the only thing `node_id` costs a task in this engine.
///
/// Driven with scx_simple rather than scx_layered, deliberately: simple has
/// one global DSQ and no topology awareness at all, so its tasks bounce
/// across sockets on nearly every wakeup. That isolates the ENGINE's penalty
/// from any scheduler's opinion about whether the migration was a good idea.
/// The workload wakes constantly (200us on, 200us off) so the per-migration
/// cost accumulates into something measurable.
#[test]
fn crossing_a_node_costs_more_than_crossing_an_llc() {
    let _lock = common::setup_test();
    let topo = MachineTopology::uniform(8, 4, 2, 1);
    let churn = || TaskBehavior {
        phases: vec![Phase::Run(200_000), Phase::Sleep(200_000)],
        repeat: RepeatMode::Forever,
    };
    let delivered = |cross_node_ns: u64| -> usize {
        let overhead = OverheadConfig {
            cross_node_migration_penalty_ns: cross_node_ns,
            // Hold the other latency terms fixed and silent so the only
            // moving part is the one under test.
            wakeup_jitter_stddev_ns: 0,
            csw_jitter: false,
            ..OverheadConfig::default()
        };
        let mut b = Scenario::builder()
            .topology(topo.clone())
            .overhead_config(overhead)
            .seed(42)
            .duration_ms(50);
        for i in 0..16 {
            b = b.add_task(&format!("t_{i}"), 0, churn());
        }
        let trace = Simulator::new(DynamicScheduler::simple()).run(b.build());
        trace
            .events()
            .iter()
            .filter(|e| matches!(e.kind, TraceKind::TaskScheduled { .. }))
            .count()
    };
    let free = delivered(0);
    let expensive = delivered(500_000);
    assert!(
        free > 100,
        "the workload must actually churn; got {free} runs"
    );
    assert!(
        expensive < free,
        "a 500us cross-node penalty must reduce throughput; got {expensive} \
         scheduling events with it and {free} without"
    );
}

// ---------------------------------------------------------------------------
// A second scheduler with its own NUMA model
// ---------------------------------------------------------------------------

/// scx_cosmos on the same multi-node machines, asserted at full strength.
///
/// Worth having separately because cosmos reaches NUMA by a completely
/// different route from layered: `scx_bpf_cpu_node()` off a `cpu_node_map`, a
/// shared DSQ per node, and the kernel's node-scoped idle kfuncs — none of
/// which touch `xnuma`.
///
/// **Cosmos was never confined**, and this test is deliberately run under
/// `ForkPlacement::FirstCpu` — every task born on CPU 0, the pre-fix
/// behaviour — to say so precisely. It passes that way, which is the evidence
/// that mb sim-dox34 was specific to scx_layered's node-local `pick_idle_cpu`
/// plus its never-written cross-NUMA gate, rather than a general engine
/// defect. Do not "simplify" this to the default placement: the point is that
/// cosmos migrates across nodes on its own, and the default would hide it.
///
/// `cosmos_configure_numa` divides CPUs directly into `nr_nodes` equal
/// groups, so the matching `MachineTopology` is one LLC per node.
///
/// Note the bound: EVERY CPU, not "at least half". `tests/topology.rs`'s
/// `assert_spread` accepts `>= nr_cpus / 2`, which is exactly the value
/// node-0 confinement produces on a 2-node machine, so that bound cannot
/// distinguish the two.
#[test]
fn cosmos_reaches_every_node_too() {
    let _lock = common::setup_test();
    let nr_cpus = 8u32;
    for nr_nodes in [2u32, 4, 8] {
        let topo = MachineTopology::uniform(nr_cpus, nr_cpus / nr_nodes, nr_nodes, 1);
        let sched = DynamicScheduler::cosmos_with_numa(nr_cpus, nr_nodes);
        let mut b = Scenario::builder()
            .topology(topo.clone())
            .fork_placement(ForkPlacement::FirstCpu)
            .detect_bpf_errors()
            .seed(42)
            .duration_ms(100);
        for i in 0..(nr_cpus * 2) {
            b = b.add_task(&format!("hog_{i}"), 0, hog());
        }
        let trace = Simulator::new(sched).run(b.build());
        assert_eq!(trace.exit_kind(), &ExitKind::Normal);
        let ran = cpus_that_ran(&trace);
        assert_eq!(
            ran.len() as u32,
            nr_cpus,
            "cosmos, {nr_cpus} CPUs / {nr_nodes} nodes: only {} CPUs ran ({ran:?})",
            ran.len()
        );
        assert_eq!(
            nodes_that_ran(&trace, &topo),
            (0..nr_nodes).collect::<Vec<_>>()
        );
    }
}

/// The substrate's node-scoped idle kfuncs answer about the node they are
/// asked about.
///
/// `scx_bpf_pick_idle_cpu_node`, `scx_bpf_get_idle_cpumask_node`,
/// `scx_bpf_get_idle_smtmask_node` and `scx_bpf_pick_any_cpu_node` all took
/// `int node __attribute__((unused))` and answered machine-wide — silently,
/// with no marker, for any scheduler that called them. They could not have
/// done better before the engine had a NUMA model; now it does, and the
/// engine publishes the partition into the substrate at run setup.
///
/// Driven against the substrate directly (the same way `cpumask_operations.rs`
/// drives `bpf_cpumask_*`) rather than through a scheduler, because the point
/// is the kfunc contract and no scheduler we run today calls all four.
/// scx_layered in particular does NOT — it uses `nodec->cpumask` — so this
/// gap was never the mechanism behind mb sim-dox34, and closing it is a
/// separate correctness fix that no other test in the tree would catch.
#[test]
fn the_node_scoped_idle_kfuncs_are_node_scoped() {
    let _lock = common::setup_test();

    // The scxtest cpumask substrate is compiled into this binary and its
    // state is thread-local, so set-up and queries must share a thread —
    // which they do, inside one test body.
    extern "C" {
        fn scx_test_set_all_cpumask(cpu: i32);
        fn scx_test_set_idle_cpumask(cpu: i32);
        fn scx_test_clear_idle_cpumask(cpu: i32);
        fn scx_test_set_cpu_node(cpu: i32, node: u32);
        fn scx_test_clear_cpu_nodes();
        fn scx_bpf_get_online_cpumask() -> *const u64;
        fn scx_bpf_get_idle_cpumask_node(node: i32) -> *const u64;
        fn scx_bpf_pick_idle_cpu_node(cpus_allowed: *const u64, node: i32, flags: u64) -> i32;
        fn scx_bpf_pick_idle_cpu(cpus_allowed: *const u64, flags: u64) -> i32;
        fn scx_bpf_pick_any_cpu_node(cpus_allowed: *const u64, node: i32, flags: u64) -> i32;
    }
    /// `struct cpumask` is `unsigned long bits[128]`, LSB = CPU 0.
    fn mask_cpus(mask: *const u64, nr_cpus: u32) -> Vec<u32> {
        (0..nr_cpus)
            .filter(|c| {
                // SAFETY: `mask` points at a live `struct cpumask` owned by
                // the substrate; `c / 64 < 128` for any c < NR_CPUS.
                let word = unsafe { *mask.add((c / 64) as usize) };
                word & (1u64 << (c % 64)) != 0
            })
            .collect()
    }

    // 8 CPUs, 2 nodes: 0-3 on node 0, 4-7 on node 1.
    let topo = MachineTopology::uniform(8, 4, 2, 1);
    assert_eq!(topo.cpus_of_node(1), (4..8).map(CpuId).collect::<Vec<_>>());

    // SAFETY: every call below is a substrate entry point taking plain
    // integers or a mask the substrate itself owns.
    unsafe {
        scx_test_clear_cpu_nodes();
        for cpu in 0..8u32 {
            scx_test_set_all_cpumask(cpu as i32);
            scx_test_set_cpu_node(cpu as i32, topo.node_of(CpuId(cpu)));
            scx_test_clear_idle_cpumask(cpu as i32);
        }
        // Exactly one idle CPU, and it is on node 1.
        scx_test_set_idle_cpumask(6);

        let online = scx_bpf_get_online_cpumask();
        assert_eq!(
            scx_bpf_pick_idle_cpu_node(online, 1, 0),
            6,
            "node 1 owns CPU 6 and must find it"
        );
        assert_eq!(
            scx_bpf_pick_idle_cpu_node(online, 0, 0),
            -1,
            "node 0 owns CPUs 0-3, none of which is idle. Answering 6 here is \
             the node-blind behaviour this test exists to prevent."
        );
        assert_eq!(
            scx_bpf_pick_idle_cpu(online, 0),
            6,
            "the un-suffixed form stays machine-wide"
        );
        assert_eq!(
            mask_cpus(scx_bpf_get_idle_cpumask_node(0), 8),
            Vec::<u32>::new()
        );
        assert_eq!(mask_cpus(scx_bpf_get_idle_cpumask_node(1), 8), vec![6]);
        assert_eq!(scx_bpf_pick_any_cpu_node(online, 0, 0), 0);
        assert_eq!(
            scx_bpf_pick_any_cpu_node(online, 1, 0),
            4,
            "node 1's lowest CPU is 4, not 0"
        );

        // Leave the substrate as the next test expects to find it.
        scx_test_clear_cpu_nodes();
        scx_test_clear_idle_cpumask(6);
    }
}
