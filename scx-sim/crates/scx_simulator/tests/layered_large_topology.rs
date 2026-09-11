//! scx_layered on large machines: how far does the simulator scale, and what
//! does it cost in wall clock?
//!
//! Motivated by upstream scx PR 3718 (hodgesds), which fixes a scx_layered
//! userspace panic reached on 384-CPU machines. QEMU/KVM could not bring 384
//! CPUs online for that repro (the PR reports `online=0-255` for a guest
//! asked for 384). This file measures the simulator's answer to the same
//! machine size.
//!
//! Everything here reports WALL CLOCK. The sweep tests are `#[ignore]`d
//! because they are measurements, not assertions.
//!
//! # RUN THE MEASUREMENTS ONE PER PROCESS
//!
//! ```text
//! for t in sweep_cpu_scaling sweep_cpu_scaling_fixed_load sweep_384_shapes \
//!          sweep_duration_at_384 control_loop_cost_at_384 memory_at_384 \
//!          wall_clock_variance_at_384 diag_layer_masks_at_384 \
//!          diag_node_confinement_matrix diag_where_does_node1_work_go; do
//!   cargo test --release --test layered_large_topology "$t" -- --ignored --nocapture
//! done
//! ```
//!
//! `cargo test ... -- --ignored --test-threads=1` runs them all in ONE
//! process and **three of them fail with `rc=-12`** (ENOMEM). That is not a
//! defect in these tests and not cross-test contamination: it is mb
//! **sim-ytru8**. `sim_arena_mark_persistent()` (`csrc/sim_arena.c`) is
//! documented "Idempotent" and assigns unconditionally, so every
//! `DynamicScheduler` construction ratchets the arena's persistent floor
//! upward and `sim_arena_reset()` can never reclaim below it. These sweeps
//! construct dozens of schedulers, several at 384 CPUs, so they are the first
//! thing in the tree to exhaust the 32 MiB arena. Each test passes in
//! isolation; all ten were verified that way.
//!
//! The non-ignored tests are the capability claims: that the topology comes
//! up, and that layered's own decision-making actually runs on it.

use std::time::Instant;

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

/// One machine under test.
#[derive(Clone, Copy, Debug)]
struct Shape {
    label: &'static str,
    nr_cpus: u32,
    cpus_per_llc: u32,
    nr_nodes: u32,
    threads_per_core: u32,
}

impl Shape {
    /// The one description both the engine and the scheduler are built from.
    /// Passing the four numbers to each side separately is what let them
    /// disagree about the node partition; see mb sim-dox34.
    fn topology(&self) -> MachineTopology {
        MachineTopology::uniform(
            self.nr_cpus,
            self.cpus_per_llc,
            self.nr_nodes,
            self.threads_per_core,
        )
    }
}

/// Result of one measured run.
#[derive(Debug)]
struct Measured {
    /// Wall clock for `Simulator::run()` alone.
    run_s: f64,
    /// Wall clock for scheduler load + topology publication + layer config.
    setup_s: f64,
    /// Simulated (virtual) duration.
    sim_ms: u64,
    nr_tasks: usize,
    trace_events: usize,
    /// Layer id histogram over the tasks, read from the scheduler's own
    /// `task_ctx.layer_id` — proof the layers were actually populated.
    layer_hist: Vec<u64>,
    nr_llcs: u32,
    nr_nodes: u32,
    /// Distinct CPUs that actually ran a task, and the highest such CPU id.
    /// The anti-truncation evidence: the cpumask substrate drops CPUs above
    /// its `NR_CPUS` *silently*, so a run that only ever used CPUs 0..127
    /// would look like a success and be a lie.
    cpus_that_ran: usize,
    max_cpu_that_ran: u32,
    /// Distinct CPUs the engine emitted ANY trace event for.
    engine_cpus: usize,
    engine_max_cpu: u32,
    /// Distinct LLCs the scheduler placed work on, derived from the CPUs that
    /// ran and the LLC map the scheduler itself published.
    llcs_that_ran: usize,
}

/// Which CPUs actually ran a task, from the engine's trace.
fn cpu_coverage(trace: &Trace) -> (usize, u32, Vec<u32>) {
    let mut seen: Vec<u32> = trace
        .events()
        .iter()
        .filter(|e| matches!(e.kind, TraceKind::TaskScheduled { .. }))
        .map(|e| e.cpu.0)
        .collect();
    seen.sort_unstable();
    seen.dedup();
    let max = seen.last().copied().unwrap_or(0);
    (seen.len(), max, seen)
}

/// Which CPUs the ENGINE emitted any event for at all. Separates "the engine
/// never modelled these CPUs" from "the engine modelled them and the
/// scheduler never used them" — two different bugs with the same symptom.
fn cpus_the_engine_touched(trace: &Trace) -> (usize, u32) {
    let mut seen: Vec<u32> = trace.events().iter().map(|e| e.cpu.0).collect();
    seen.sort_unstable();
    seen.dedup();
    (seen.len(), seen.last().copied().unwrap_or(0))
}

/// Two layers matched by comm prefix, so layered's real match evaluation runs
/// rather than everything falling into one catch-all.
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

/// Build, run and time one (shape, load) point.
///
/// `tasks_per_cpu` scales the workload with the machine, which is the
/// realistic case: a 384-CPU box is not running four threads.
fn measure(shape: Shape, tasks_per_cpu: u32, duration_ms: u64, control_loop: bool) -> Measured {
    let t_setup = Instant::now();
    let topo = shape.topology();
    let sched = DynamicScheduler::layered_for_topology(&topo);
    sched.layered_layers(&two_layer_specs());
    if control_loop {
        // Production's default scx_layered scheduling interval is 100ms.
        sched.layered_enable_control_loop(100_000_000);
    }
    let probes = LayeredProbes::new(&sched);
    let setup_s = t_setup.elapsed().as_secs_f64();

    let nr_tasks = (shape.nr_cpus * tasks_per_cpu) as usize;
    let mut b = Scenario::builder()
        .topology(topo)
        .detect_bpf_errors()
        .seed(42)
        .duration_ms(duration_ms);
    // A quarter of the tasks carry the "hot" prefix so both layers are live.
    for i in 0..nr_tasks {
        let name = if i % 4 == 0 {
            format!("hot_{i}")
        } else {
            format!("bulk_{i}")
        };
        b = b.add_task(&name, 0, hog());
    }
    let scenario = b.build();

    // `probes` holds raw fn pointers into `sched`'s dlopen'd `.so`, so the
    // Simulator that now owns `sched` must outlive every probe read below.
    let sim = Simulator::new(sched);
    let t_run = Instant::now();
    let trace = sim.run(scenario);
    let run_s = t_run.elapsed().as_secs_f64();

    assert_eq!(trace.exit_kind(), &ExitKind::Normal, "{}", shape.label);

    let nr_layers = probes.nr_layers();
    let mut layer_hist = vec![0u64; nr_layers as usize + 1];
    for i in 0..nr_tasks {
        // Scenario builder assigns pids 1..=n in insertion order.
        let l = probes.task_layer(Pid(i as i32 + 1));
        let slot = (l as usize).min(nr_layers as usize);
        layer_hist[slot] += 1;
    }

    let (cpus_that_ran, max_cpu_that_ran, ran) = cpu_coverage(&trace);
    let (engine_cpus, engine_max_cpu) = cpus_the_engine_touched(&trace);
    let mut llcs: Vec<u32> = ran.iter().map(|&c| probes.cpu_llc(CpuId(c))).collect();
    llcs.sort_unstable();
    llcs.dedup();

    Measured {
        run_s,
        setup_s,
        sim_ms: duration_ms,
        nr_tasks,
        trace_events: trace.events().len(),
        layer_hist,
        nr_llcs: probes.nr_llcs(),
        nr_nodes: probes.nr_nodes(),
        cpus_that_ran,
        max_cpu_that_ran,
        engine_cpus,
        engine_max_cpu,
        llcs_that_ran: llcs.len(),
    }
}

fn report(shape: Shape, tasks_per_cpu: u32, m: &Measured) {
    let total = m.setup_s + m.run_s;
    let virt_s = m.sim_ms as f64 / 1000.0;
    println!(
        "{label:<28} cpus={cpus:<4} llcs={llcs:<3} nodes={nodes:<2} smt={smt} \
         tasks={tasks:<5} | setup={setup:>8.3}s run={run:>8.3}s total={total:>8.3}s \
         | sim={virt:>5.2}s speedup={ratio:>8.1}x | events={ev:<9} \
         | engine_saw={ecpus} (max {emax}) ran_on={ran}/{cpus} (max {maxcpu}), \
         {llcs_ran}/{llcs} llcs | layers={hist:?}",
        label = shape.label,
        cpus = shape.nr_cpus,
        llcs = m.nr_llcs,
        nodes = m.nr_nodes,
        smt = shape.threads_per_core,
        tasks = m.nr_tasks,
        setup = m.setup_s,
        run = m.run_s,
        total = total,
        virt = virt_s,
        ratio = virt_s / total,
        ev = m.trace_events,
        ran = m.cpus_that_ran,
        maxcpu = m.max_cpu_that_ran,
        ecpus = m.engine_cpus,
        emax = m.engine_max_cpu,
        llcs_ran = m.llcs_that_ran,
        hist = m.layer_hist,
    );
    let _ = tasks_per_cpu;
}

// ---------------------------------------------------------------------------
// Capability: does a 384-CPU machine come up at all, and does layered decide
// anything on it?
// ---------------------------------------------------------------------------

/// The dual-socket 384-CPU shape from PR 3718's motivation, with an
/// EPYC-like LLC layout (8-core / 16-thread CCX).
const DUAL_SOCKET_384: Shape = Shape {
    label: "384c dual-socket SMT2",
    nr_cpus: 384,
    cpus_per_llc: 16,
    nr_nodes: 2,
    threads_per_core: 2,
};

/// The same 384 CPUs as one flat NUMA node. This is the shape the simulator
/// actually runs end to end today, and the shape the wall-clock claim rests
/// on. It is also close to the guest topology PR 3718's own repro produced:
/// `-smp cpus=384` with no cores/threads/sockets given.
const FLAT_384: Shape = Shape {
    label: "384c flat (1 node, 1 LLC)",
    nr_cpus: 384,
    cpus_per_llc: 384,
    nr_nodes: 1,
    threads_per_core: 1,
};

/// A 384-CPU machine comes up, runs real scx_layered, and uses ALL of it.
///
/// The `cpus_that_ran` assertion is the load-bearing one and it is not
/// decoration. The cpumask substrate drops `cpu >= NR_CPUS` *silently*
/// (`scxtest/scx_test_cpumask.c::cpumask_set_cpu`, `csrc/sim_bpf_stubs.c`),
/// so a build whose NR_CPUS drifted below the layered wrapper's ceiling would
/// come up, exit `Normal`, populate both layers and place every task on CPUs
/// 0..127. Every other assertion here would still pass. That exact state was
/// observed while writing this file, because NR_CPUS was defined three times
/// and only two of them had been raised.
#[test]
fn a_384_cpu_machine_runs_layered_on_all_384_cpus() {
    let _lock = common::setup_test();
    let m = measure(FLAT_384, 2, 200, true);
    report(FLAT_384, 2, &m);

    // Both layers must actually own tasks — otherwise "layered ran" would be
    // satisfied by every task falling into one catch-all, with no match
    // evaluation having decided anything.
    assert_eq!(
        m.layer_hist[0], 192,
        "layer 0 must hold exactly the 1-in-4 `hot_*` tasks, got {:?}",
        m.layer_hist
    );
    assert_eq!(m.layer_hist[1], 576, "layer 1 must hold the rest");

    assert_eq!(
        m.cpus_that_ran, 384,
        "every CPU must have run something; only {} did (max id {})",
        m.cpus_that_ran, m.max_cpu_that_ran
    );
    assert_eq!(
        m.max_cpu_that_ran, 383,
        "the highest CPU that ran a task must be 383, not {}",
        m.max_cpu_that_ran
    );
}

/// WAS a known gap, now the positive property: with more than one NUMA node,
/// work reaches EVERY node, not just node 0.
///
/// This is the inversion of `known_gap_multi_node_confines_all_work_to_node_zero`,
/// which asserted `cpus_that_ran == nr_cpus / nr_nodes` at these exact shapes.
/// mb **sim-dox34** is that gap; it closed with two changes, and the numbers
/// below are what it measured before them:
///
/// | shape | before | after |
/// |---|---|---|
/// | 64c / 2 nodes | 32 | 64 |
/// | 64c / 4 nodes | 16 | 64 |
/// | 384c / 2 nodes | 192 | 384 |
///
/// 1. The engine grew a NUMA model, and `ForkPlacement` stopped birthing
///    every task on CPU 0. `pick_idle_cpu()` searches the task's LOCAL node
///    first, so a machine where every task is born on node 0 keeps every task
///    on node 0 — correct scheduler behaviour on a machine no fork ever
///    produces.
/// 2. scxsim's layered control loop now ports upstream's `refresh_xnuma()`.
///    The cross-NUMA gate is userspace-written and was left at its BSS zero,
///    which `xnuma_gate()` reads as "deny", permanently, in both directions.
///
/// `crates/scx_simulator/tests/numa_topology.rs` covers the same property
/// across more shapes, both allocation modes, and the gate itself.
#[test]
fn multi_node_topologies_reach_every_node() {
    let _lock = common::setup_test();
    for (nr_cpus, nr_nodes) in [(64u32, 2u32), (64, 4), (384, 2)] {
        let shape = Shape {
            label: "multi-node",
            nr_cpus,
            cpus_per_llc: 8,
            nr_nodes,
            threads_per_core: 1,
        };
        let m = measure(shape, 2, 100, false);
        assert_eq!(
            m.cpus_that_ran,
            nr_cpus as usize,
            "{nr_cpus} CPUs / {nr_nodes} nodes: only {} CPUs ran (max id {}). \
             sim-dox34 regression — the pre-fix value here was exactly {}, \
             i.e. node 0 alone.",
            m.cpus_that_ran,
            m.max_cpu_that_ran,
            nr_cpus / nr_nodes
        );
        assert_eq!(
            m.max_cpu_that_ran,
            nr_cpus - 1,
            "{nr_cpus} CPUs / {nr_nodes} nodes: the highest CPU that ran was {}, \
             not the top of the machine",
            m.max_cpu_that_ran
        );
    }
}

/// The failure mode the anti-truncation assertions above are aimed at, made
/// explicit: the scheduler's own `all_cpus` bitmap must contain every CPU.
///
/// `layered_set_topology()` publishes `all_cpus` through the same
/// `cpu / 8` byte-indexed write that the cpumask helpers guard on `NR_CPUS`.
/// If the two ceilings ever drift apart again, this is the cheap detector.
#[test]
fn every_cpu_is_visible_to_the_scheduler_at_384() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::layered_with_topology(384, 16, 2, 2);
    let probes = LayeredProbes::new(&sched);

    // The published per-CPU LLC/node maps must cover the whole range.
    for cpu in 0..384u32 {
        assert_eq!(
            probes.cpu_llc(CpuId(cpu)),
            cpu / 16,
            "wrapper published the wrong LLC for cpu {cpu}"
        );
        assert_eq!(
            probes.cpu_node(CpuId(cpu)),
            (cpu / 16) / 12,
            "wrapper published the wrong node for cpu {cpu}"
        );
    }
    // SMT siblings must be paired all the way to the top of the range.
    assert_eq!(probes.sibling_cpu(CpuId(382)), 383);
    assert_eq!(probes.sibling_cpu(CpuId(383)), 382);
    assert_eq!(probes.nr_llcs(), 24);
    assert_eq!(probes.nr_nodes(), 2);
}

// ---------------------------------------------------------------------------
// Measurements (run with --ignored --nocapture)
// ---------------------------------------------------------------------------

/// Wall clock as the CPU count grows, at constant tasks-per-CPU.
#[test]
#[ignore = "measurement, not an assertion"]
fn sweep_cpu_scaling() {
    let _lock = common::setup_test();
    println!("\n== constant load per CPU (2 tasks/CPU), 200ms simulated, control loop ON ==");
    for &n in &[8u32, 16, 32, 64, 128, 192, 256, 384] {
        let shape = Shape {
            label: "sweep",
            nr_cpus: n,
            cpus_per_llc: 16.min(n),
            nr_nodes: if n >= 32 { 2 } else { 1 },
            threads_per_core: 2,
        };
        let m = measure(shape, 2, 200, true);
        report(shape, 2, &m);
    }
}

/// Wall clock as the CPU count grows at a FIXED total task count, which
/// isolates the per-CPU cost of the machine itself from the cost of the load.
#[test]
#[ignore = "measurement, not an assertion"]
fn sweep_cpu_scaling_fixed_load() {
    let _lock = common::setup_test();
    println!("\n== fixed 64 tasks, 200ms simulated, control loop ON ==");
    for &n in &[8u32, 16, 32, 64, 128, 192, 256, 384] {
        let shape = Shape {
            label: "sweep-fixed",
            nr_cpus: n,
            cpus_per_llc: 16.min(n),
            nr_nodes: if n >= 32 { 2 } else { 1 },
            threads_per_core: 2,
        };
        // tasks_per_cpu is expressed as a count here via a tiny shim.
        let m = measure_fixed(shape, 64, 200, true);
        report(shape, 0, &m);
    }
}

/// Same as [`measure`] but with an absolute task count.
fn measure_fixed(shape: Shape, nr_tasks: u32, duration_ms: u64, control_loop: bool) -> Measured {
    assert!(nr_tasks > 0);
    let per_cpu_num = nr_tasks;
    // Reuse `measure` by faking tasks_per_cpu when it divides evenly; else
    // build inline. Simplest correct thing: build inline.
    let t_setup = Instant::now();
    let topo = shape.topology();
    let sched = DynamicScheduler::layered_for_topology(&topo);
    sched.layered_layers(&two_layer_specs());
    if control_loop {
        sched.layered_enable_control_loop(100_000_000);
    }
    let probes = LayeredProbes::new(&sched);
    let setup_s = t_setup.elapsed().as_secs_f64();

    let mut b = Scenario::builder()
        .topology(topo)
        .detect_bpf_errors()
        .seed(42)
        .duration_ms(duration_ms);
    for i in 0..per_cpu_num as usize {
        let name = if i % 4 == 0 {
            format!("hot_{i}")
        } else {
            format!("bulk_{i}")
        };
        b = b.add_task(&name, 0, hog());
    }
    let scenario = b.build();

    let sim = Simulator::new(sched);
    let t_run = Instant::now();
    let trace = sim.run(scenario);
    let run_s = t_run.elapsed().as_secs_f64();
    assert_eq!(trace.exit_kind(), &ExitKind::Normal, "{}", shape.label);

    let nr_layers = probes.nr_layers();
    let mut layer_hist = vec![0u64; nr_layers as usize + 1];
    for i in 0..per_cpu_num {
        let l = probes.task_layer(Pid(i as i32 + 1));
        layer_hist[(l as usize).min(nr_layers as usize)] += 1;
    }
    let (cpus_that_ran, max_cpu_that_ran, ran) = cpu_coverage(&trace);
    let (engine_cpus, engine_max_cpu) = cpus_the_engine_touched(&trace);
    let mut llcs: Vec<u32> = ran.iter().map(|&c| probes.cpu_llc(CpuId(c))).collect();
    llcs.sort_unstable();
    llcs.dedup();
    Measured {
        run_s,
        setup_s,
        sim_ms: duration_ms,
        nr_tasks: per_cpu_num as usize,
        trace_events: trace.events().len(),
        layer_hist,
        nr_llcs: probes.nr_llcs(),
        nr_nodes: probes.nr_nodes(),
        cpus_that_ran,
        max_cpu_that_ran,
        engine_cpus,
        engine_max_cpu,
        llcs_that_ran: llcs.len(),
    }
}

/// Wall clock as the simulated duration grows on a fixed 384-CPU machine.
///
/// This is the number that decides whether the simulator is useful. A VM
/// charges a flat ~7s per test plus the declared duration in real time
/// (`ai_docs/PHASE2_DELIVERABLE_20260814.md`), so a scenario declaring D
/// seconds costs the VM roughly 7 + D. The simulator's line has to stay
/// under that.
///
/// Both 384-CPU shapes are swept, and since mb sim-dox34 closed both of them
/// use the whole machine — see `multi_node_topologies_reach_every_node`. The
/// DUAL_SOCKET_384 line is therefore now readable as the cost of the NUMA
/// shape rather than as the cost of half the machine spinning; earlier
/// revisions of this comment warned the opposite, and that warning no longer
/// applies.
#[test]
#[ignore = "measurement, not an assertion"]
fn sweep_duration_at_384() {
    let _lock = common::setup_test();
    for shape in [FLAT_384, DUAL_SOCKET_384] {
        println!(
            "\n== {} , 2 tasks/CPU, growing simulated duration ==",
            shape.label
        );
        println!(
            "{:>7}  {:>10}  {:>10}  {:>12}  {:>12}",
            "sim_ms", "wall_s", "vm_est_s", "vs_vm", "events"
        );
        for &ms in &[50u64, 100, 200, 500, 1000, 2000, 5000] {
            let m = measure(shape, 2, ms, true);
            let wall = m.setup_s + m.run_s;
            // A ktstr-style VM test: ~7s flat + the declared duration, in
            // real seconds. Measured baseline, not a guess.
            let vm_est = 7.0 + ms as f64 / 1000.0;
            println!(
                "{ms:>7}  {wall:>10.3}  {vm_est:>10.2}  {:>11.1}x  {:>12}  (ran_on={}/{})",
                vm_est / wall,
                m.trace_events,
                m.cpus_that_ran,
                shape.nr_cpus,
            );
        }
    }
}

/// The shapes a "384-CPU dual socket" could actually be. Layered's cost
/// depends on the LLC and node partition, not just the CPU count.
#[test]
#[ignore = "measurement, not an assertion"]
fn sweep_384_shapes() {
    let _lock = common::setup_test();
    println!("\n== 384 CPUs, varying LLC/node/SMT layout, 2 tasks/CPU, 200ms ==");
    let shapes = [
        Shape {
            label: "flat (1 LLC, 1 node)",
            nr_cpus: 384,
            cpus_per_llc: 384,
            nr_nodes: 1,
            threads_per_core: 1,
        },
        Shape {
            label: "2 nodes, 16c LLC, SMT2",
            nr_cpus: 384,
            cpus_per_llc: 16,
            nr_nodes: 2,
            threads_per_core: 2,
        },
        Shape {
            label: "2 nodes, 16c LLC, no SMT",
            nr_cpus: 384,
            cpus_per_llc: 16,
            nr_nodes: 2,
            threads_per_core: 1,
        },
        Shape {
            label: "4 nodes (NPS2), 16c LLC",
            nr_cpus: 384,
            cpus_per_llc: 16,
            nr_nodes: 4,
            threads_per_core: 2,
        },
        Shape {
            label: "8 nodes, 8c LLC, SMT2",
            nr_cpus: 384,
            cpus_per_llc: 8,
            nr_nodes: 8,
            threads_per_core: 2,
        },
    ];
    for shape in shapes {
        let m = measure(shape, 2, 200, true);
        report(shape, 2, &m);
    }
}

/// What the userspace control loop costs at 384 CPUs. It runs upstream's real
/// `unified_alloc()` and `layer_core_growth.rs` every period, over
/// nr_layers x nr_cpus data.
#[test]
#[ignore = "measurement, not an assertion"]
fn control_loop_cost_at_384() {
    let _lock = common::setup_test();
    println!("\n== 384c dual-socket SMT2, 2 tasks/CPU, 500ms, control loop on vs off ==");
    for on in [false, true] {
        let m = measure(DUAL_SOCKET_384, 2, 500, on);
        println!("control_loop={on}");
        report(DUAL_SOCKET_384, 2, &m);
    }
}

/// TEMPORARY DIAGNOSTIC: where does the 128-CPU ceiling live?
#[test]
#[ignore = "diagnostic"]
fn diag_layer_masks_at_384() {
    let _lock = common::setup_test();
    let topo = MachineTopology::uniform(384, 16, 2, 2);
    let sched = DynamicScheduler::layered_for_topology(&topo);
    sched.layered_layers(&two_layer_specs());
    let probes = LayeredProbes::new(&sched);
    let mut b = Scenario::builder()
        .topology(topo)
        .detect_bpf_errors()
        .seed(42)
        .duration_ms(100);
    for i in 0..768 {
        let name = if i % 4 == 0 {
            format!("hot_{i}")
        } else {
            format!("bulk_{i}")
        };
        b = b.add_task(&name, 0, hog());
    }
    let sim = Simulator::new(sched);
    let trace = sim.run(b.build());
    println!("exit={:?}", trace.exit_kind());
    for l in 0..probes.nr_layers() {
        let ser = probes.layer_nr_cpus(l);
        let bpf_hi: Vec<u32> = (0..384u32)
            .filter(|&c| probes.layer_bpf_has_cpu(l, CpuId(c)))
            .collect();
        let ser_hi: Vec<u32> = (0..384u32)
            .filter(|&c| probes.layer_has_cpu(l, CpuId(c)))
            .collect();
        println!(
            "layer {l}: layer->nr_cpus={ser} | serialized mask: {} cpus, max {:?} \
             | BPF kptr mask: {} cpus, max {:?}",
            ser_hi.len(),
            ser_hi.last(),
            bpf_hi.len(),
            bpf_hi.last()
        );
    }
    // Where did SelectTaskRq send tasks?
    let mut sel: Vec<u32> = trace
        .events()
        .iter()
        .filter_map(|e| match e.kind {
            TraceKind::SelectTaskRq { selected_cpu, .. } => Some(selected_cpu.0),
            _ => None,
        })
        .collect();
    sel.sort_unstable();
    sel.dedup();
    println!(
        "SelectTaskRq distinct selected_cpu: {} (max {:?})",
        sel.len(),
        sel.last()
    );
    let mut kicked: Vec<u32> = trace
        .events()
        .iter()
        .filter_map(|e| match e.kind {
            TraceKind::KickCpu { target_cpu } => Some(target_cpu.0),
            _ => None,
        })
        .collect();
    kicked.sort_unstable();
    kicked.dedup();
    println!(
        "KickCpu distinct targets: {} (max {:?})",
        kicked.len(),
        kicked.last()
    );

    // Which CPUs ever went idle, per the engine?
    let mut idle: Vec<u32> = trace
        .events()
        .iter()
        .filter_map(|e| match e.kind {
            TraceKind::CpuIdle => Some(e.cpu.0),
            _ => None,
        })
        .collect();
    idle.sort_unstable();
    idle.dedup();
    println!(
        "CpuIdle distinct cpus: {} (max {:?}, min {:?})",
        idle.len(),
        idle.last(),
        idle.first()
    );

    // What kinds of event happen on a CPU above 191 at all?
    let mut kinds: Vec<String> = trace
        .events()
        .iter()
        .filter(|e| e.cpu.0 >= 192)
        .map(|e| {
            format!("{:?}", e.kind)
                .split(' ')
                .next()
                .unwrap()
                .to_string()
        })
        .collect();
    kinds.sort();
    kinds.dedup();
    println!("event kinds seen on cpus >= 192: {kinds:?}");

    // Per-layer task counts and the fallback-DSQ counters.
    for l in 0..probes.nr_layers() {
        println!("layer {l}: nr_tasks={}", probes.layer_nr_tasks(l));
    }
    println!(
        "gstat HiFbEvents={} LoFbEvents={}",
        probes.global_stat(GlobalStat::HiFbEvents),
        probes.global_stat(GlobalStat::LoFbEvents)
    );
}

/// DIAGNOSTIC, kept as the cheapest sim-dox34 regression probe: the node /
/// LLC / layer matrix, printed rather than asserted.
///
/// It was written to isolate the node-0 confinement to `nr_nodes` alone
/// (64c/8llc/1node gave ran_on=64, the same shape at 2 nodes gave 32, and
/// neither the layer config nor the LLC size moved it). Every row should now
/// read `ran_on=64/64`; a row that does not names the shape that regressed.
#[test]
#[ignore = "diagnostic"]
fn diag_node_confinement_matrix() {
    let _lock = common::setup_test();
    println!("\n== 64 CPUs, 8 per LLC, 128 tasks, 100ms: node/LLC/layer matrix ==");
    for (nodes, llc, one_layer) in [
        (1u32, 64u32, false),
        (1, 8, false),
        (2, 8, false),
        (1, 64, true),
        (1, 8, true),
        (2, 8, true),
        (2, 32, true),
    ] {
        let topo = MachineTopology::uniform(64, llc, nodes, 1);
        let sched = DynamicScheduler::layered_for_topology(&topo);
        if one_layer {
            sched.layered_layers(&[LayerSpec::catch_all("all")]);
        } else {
            sched.layered_layers(&two_layer_specs());
        }
        let probes = LayeredProbes::new(&sched);
        let mut b = Scenario::builder()
            .topology(topo)
            .detect_bpf_errors()
            .seed(42)
            .duration_ms(100);
        for i in 0..128 {
            let name = if i % 4 == 0 {
                format!("hot_{i}")
            } else {
                format!("bulk_{i}")
            };
            b = b.add_task(&name, 0, hog());
        }
        let sim = Simulator::new(sched);
        let trace = sim.run(b.build());
        let (ran, maxcpu, _) = cpu_coverage(&trace);
        println!(
            "nodes={nodes} cpus_per_llc={llc:<2} layers={:<9} | ran_on={ran:>3}/64 (max {maxcpu:>3}) \
             | nr_llcs={} nr_nodes={} | events={}",
            if one_layer { "catch-all" } else { "two" },
            probes.nr_llcs(),
            probes.nr_nodes(),
            trace.events().len(),
        );
    }
}

/// DIAGNOSTIC: with 2 nodes, is work never ENQUEUED to node 1's DSQs, or
/// enqueued there and never CONSUMED? Different bug, different owner.
///
/// This is the probe that localised mb sim-dox34 to the placement side: at
/// the time, `ops.enqueue` was only ever invoked from node-0 CPUs and tasks
/// only reached 4 of the 8 per-LLC DSQs, while node-1 CPUs ran `ops.dispatch`
/// and called `scx_bpf_dsq_move_to_local` 31428 times with zero successes.
/// Post-fix it prints 64 enqueue-from CPUs and all 8 DSQs on both arms.
#[test]
#[ignore = "diagnostic"]
fn diag_where_does_node1_work_go() {
    let _lock = common::setup_test();
    for nodes in [1u32, 2] {
        let topo = MachineTopology::uniform(64, 8, nodes, 1);
        let sched = DynamicScheduler::layered_for_topology(&topo);
        sched.layered_layers(&[LayerSpec::catch_all("all")]);
        let probes = LayeredProbes::new(&sched);
        let mut b = Scenario::builder()
            .topology(topo)
            .detect_bpf_errors()
            .seed(42)
            .duration_ms(100);
        for i in 0..128 {
            b = b.add_task(&format!("t_{i}"), 0, hog());
        }
        let sim = Simulator::new(sched);
        let trace = sim.run(b.build());

        // Which CPUs did the engine ask the scheduler to enqueue FROM?
        let mut enq_cpu: Vec<u32> = trace
            .events()
            .iter()
            .filter_map(|e| match e.kind {
                TraceKind::EnqueueTask { .. } => Some(e.cpu.0),
                _ => None,
            })
            .collect();
        enq_cpu.sort_unstable();
        enq_cpu.dedup();

        // Which DSQs did tasks actually land in?
        let mut dsqs: Vec<u64> = trace
            .events()
            .iter()
            .filter_map(|e| match e.kind {
                TraceKind::DsqInsert { dsq_id, .. } => Some(dsq_id.0),
                TraceKind::DsqInsertVtime { dsq_id, .. } => Some(dsq_id.0),
                _ => None,
            })
            .collect();
        dsqs.sort_unstable();
        dsqs.dedup();

        // Which DSQs were successfully consumed, and by whom?
        let mut consumed: Vec<(u64, u32)> = trace
            .events()
            .iter()
            .filter_map(|e| match e.kind {
                TraceKind::DsqMoveToLocal {
                    dsq_id,
                    success: true,
                } => Some((dsq_id.0, e.cpu.0)),
                _ => None,
            })
            .collect();
        consumed.sort_unstable();
        consumed.dedup();
        let consumer_cpus: Vec<u32> = {
            let mut v: Vec<u32> = consumed.iter().map(|&(_, c)| c).collect();
            v.sort_unstable();
            v.dedup();
            v
        };
        let consumed_dsqs: Vec<u64> = {
            let mut v: Vec<u64> = consumed.iter().map(|&(d, _)| d).collect();
            v.sort_unstable();
            v.dedup();
            v
        };
        let (ran, maxcpu, _) = cpu_coverage(&trace);
        println!(
            "\nnodes={nodes} (nr_nodes={}, nr_llcs={}): ran_on={ran}/64 max={maxcpu}",
            probes.nr_nodes(),
            probes.nr_llcs()
        );
        println!(
            "  enqueue-from cpus: {} (max {:?})",
            enq_cpu.len(),
            enq_cpu.last()
        );
        println!(
            "  distinct DSQs inserted into ({}): {:x?}",
            dsqs.len(),
            dsqs
        );
        println!(
            "  distinct DSQs consumed  ({}): {:x?}",
            consumed_dsqs.len(),
            consumed_dsqs
        );
        println!(
            "  consumer cpus: {} (max {:?})",
            consumer_cpus.len(),
            consumer_cpus.last()
        );
    }
}

/// Where the new ceiling is, and that crossing it still REFUSES rather than
/// truncating. 512 is `NR_CPUS` (scxtest/kern_types.h), `LAYERED_MAX_SIM_CPUS`
/// (schedulers/layered/wrapper.c) and scx_layered's own `MAX_CPUS`, all at
/// once. 384 has to sit comfortably inside it.
#[test]
fn the_ceiling_is_512_and_it_refuses_rather_than_truncating() {
    let _lock = common::setup_test();
    // At the ceiling: comes up, and every CPU is real.
    {
        let sched = DynamicScheduler::layered_with_topology(512, 512, 1, 1);
        let probes = LayeredProbes::new(&sched);
        assert_eq!(probes.cpu_llc(CpuId(511)), 0);
        assert_eq!(probes.nr_llcs(), 1);
    }
    // Above it: a panic naming the constant, not a quiet 128-CPU machine.
    let over = std::panic::catch_unwind(|| {
        let _ = DynamicScheduler::layered_with_topology(513, 513, 1, 1);
    });
    let err = over.expect_err("513 CPUs must be refused");
    let msg = err
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| err.downcast_ref::<&str>().copied())
        .unwrap_or("");
    assert!(
        msg.contains("LAYERED_MAX_SIM_CPUS"),
        "the refusal must name the constant that has to move; got {msg:?}"
    );
}

/// Peak RSS of a 384-CPU run, printed so the report can cite a measured
/// number rather than a sum of `sizeof`s.
#[test]
#[ignore = "measurement, not an assertion"]
fn memory_at_384() {
    let _lock = common::setup_test();
    let rss = || -> u64 {
        std::fs::read_to_string("/proc/self/status")
            .unwrap()
            .lines()
            .find(|l| l.starts_with("VmHWM:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    println!("peak RSS before any simulation: {} kB", rss());
    let m = measure(FLAT_384, 2, 1000, true);
    report(FLAT_384, 2, &m);
    println!(
        "peak RSS after 384-CPU / 768-task / 1000ms run: {} kB",
        rss()
    );
}

/// Run-to-run wall-clock spread for ONE fixed point, so every other timing
/// number in this file can be read with an error bar.
///
/// The simulation itself is deterministic — the trace event count must be
/// identical across every repetition, and this asserts that. Only the wall
/// clock moves, and on a shared dev box it moves a lot.
#[test]
#[ignore = "measurement, not an assertion"]
fn wall_clock_variance_at_384() {
    let _lock = common::setup_test();
    const REPS: usize = 9;
    let mut walls = Vec::with_capacity(REPS);
    let mut events = Vec::with_capacity(REPS);
    for _ in 0..REPS {
        let m = measure(FLAT_384, 2, 200, true);
        walls.push(m.setup_s + m.run_s);
        events.push(m.trace_events);
    }
    assert!(
        events.windows(2).all(|w| w[0] == w[1]),
        "the simulation must be deterministic; event counts differed: {events:?}"
    );
    walls.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "\n384c flat, 2 tasks/CPU, 200ms simulated, n={REPS}: \
         min={:.3}s median={:.3}s max={:.3}s spread={:.0}% (events={} every time)",
        walls[0],
        walls[REPS / 2],
        walls[REPS - 1],
        100.0 * (walls[REPS - 1] - walls[0]) / walls[0],
        events[0],
    );
}
