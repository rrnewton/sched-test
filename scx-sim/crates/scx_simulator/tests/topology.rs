//! Diverse CPU-topology configuration tests.
//!
//! The per-scheduler suites (`cosmos.rs`, `lavd.rs`) each carry a handful of
//! single-config topology tests (one SMT layout, one NUMA layout, one
//! big.LITTLE capacity table). This file instead *sweeps* the topology
//! dimensions across a matrix of configurations and runs the same workload
//! under both COSMOS and LAVD, so scheduler behaviour is exercised over:
//!
//!   * Multi-NUMA node counts (2, 4, 8 nodes) — COSMOS `cosmos_configure_numa`.
//!   * LLC / compute-domain sharing patterns — LAVD multi-domain (cpdoms) and
//!     the engine-level `cpus_per_llc` topology knob observed by the scheduler.
//!   * SMT thread-per-core counts (1, 2, 4).
//!   * Asymmetric (big.LITTLE) per-CPU capacity tables — COSMOS
//!     `cosmos_set_cpu_capacity`.
//!
//! Every case asserts the run exits normally and every task makes progress;
//! spread-oriented cases additionally assert tasks are distributed across the
//! topology. LAVD's asymmetric-capacity sweep lives in `lavd.rs` (it needs the
//! dlsym `cpu_capacity`/`cpu_big` helpers defined there).

use std::collections::HashSet;

use scx_simulator::*;

#[macro_use]
mod common;

/// A named scheduler constructor: `(label, factory)`. Lets a single test body
/// sweep the same topology config across multiple schedulers.
type NamedSchedFactory = (&'static str, fn(u32) -> DynamicScheduler);

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// A saturating mixed workload: one CPU-bound hog plus one I/O-bound waker per
/// CPU. Hogs keep every domain busy (forcing placement / migration decisions);
/// the wakers re-enter `select_cpu()`/`enqueue()` on each wakeup (exercising
/// idle-scan and cross-domain routing). Returns the built `Scenario` and the
/// task count (PIDs are `1..=nr_tasks`).
///
/// `smt` (threads/core, `1` = none) and `cpus_per_llc` (`0` = single LLC) are
/// applied to the engine topology when non-trivial.
fn topo_scenario(nr_cpus: u32, smt: u32, cpus_per_llc: u32) -> (Scenario, u32) {
    let mut b = Scenario::builder().cpus(nr_cpus);
    if smt > 1 {
        b = b.smt(smt);
    }
    if cpus_per_llc > 0 {
        b = b.cpus_per_llc(cpus_per_llc);
    }
    for i in 0..nr_cpus {
        b = b.add_task(&format!("hog{i}"), 0, workloads::cpu_bound(20_000_000));
    }
    for i in 0..nr_cpus {
        b = b.add_task(
            &format!("io{i}"),
            0,
            workloads::io_bound(500_000, 2_000_000),
        );
    }
    let nr_tasks = nr_cpus * 2;
    (b.duration_ms(200).build(), nr_tasks)
}

/// Assert the simulation exited normally and every task in `1..=nr_tasks`
/// accumulated some runtime. `ctx` labels the failing configuration.
fn assert_all_progressed(trace: &Trace, nr_tasks: u32, ctx: &str) {
    assert_eq!(
        trace.exit_kind(),
        &ExitKind::Normal,
        "{ctx}: simulation did not exit normally"
    );
    for pid in 1..=nr_tasks {
        assert!(
            trace.total_runtime(Pid(pid as i32)) > 0,
            "{ctx}: task {pid} got no runtime"
        );
    }
}

/// Distinct CPUs that ran at least one task.
fn distinct_cpus_used(trace: &Trace) -> HashSet<CpuId> {
    trace
        .events()
        .iter()
        .filter_map(|e| match e.kind {
            TraceKind::TaskScheduled { .. } => Some(e.cpu),
            _ => None,
        })
        .collect()
}

/// Assert the workload spread across at least half of `nr_cpus` — a saturating
/// per-CPU hog workload should light up (nearly) every CPU regardless of
/// topology.
fn assert_spread(trace: &Trace, nr_cpus: u32, ctx: &str) {
    let used = distinct_cpus_used(trace);
    assert!(
        used.len() as u32 >= nr_cpus / 2,
        "{ctx}: expected spread across >= {} CPUs, only used {} ({:?})",
        nr_cpus / 2,
        used.len(),
        used
    );
}

// ---------------------------------------------------------------------------
// Multi-NUMA node topologies (COSMOS)
// ---------------------------------------------------------------------------

/// Sweep 2, 4, and 8 NUMA nodes over 8 CPUs (4, 2, and 1 CPU per node). Each
/// node gets its own shared DSQ; `cosmos_configure_numa` wires the per-node
/// cpumasks so `shared_dsq()` / node-usability routing runs for every node
/// count. A saturating workload must keep every node's CPUs busy.
#[test]
fn test_cosmos_numa_node_sweep() {
    let _lock = common::setup_test();
    let nr_cpus = 8;
    for nr_nodes in [2, 4, 8] {
        let ctx = format!("cosmos numa nodes={nr_nodes}");
        let sched = DynamicScheduler::cosmos_with_numa(nr_cpus, nr_nodes);
        let (scenario, nr_tasks) = topo_scenario(nr_cpus, 1, 0);
        let trace = Simulator::new(sched).run(scenario);
        assert_all_progressed(&trace, nr_tasks, &ctx);
        assert_spread(&trace, nr_cpus, &ctx);
    }
}

/// NUMA with per-node affinity restriction: each hog is pinned to a single
/// node's CPUs across 2, 4, and 8-node layouts, so `nr_cpus_allowed <
/// nr_cpu_ids` and the NUMA-aware placement paths run with genuinely
/// node-local tasks (not just unrestricted ones as in `test_numa_node_sweep`).
#[test]
fn test_cosmos_numa_per_node_affinity() {
    let _lock = common::setup_test();
    let nr_cpus = 8;
    for nr_nodes in [2, 4, 8] {
        let ctx = format!("cosmos numa-affinity nodes={nr_nodes}");
        let cpus_per_node = nr_cpus / nr_nodes;
        let sched = DynamicScheduler::cosmos_with_numa(nr_cpus, nr_nodes);

        let mut b = Scenario::builder().cpus(nr_cpus);
        for node in 0..nr_nodes {
            // CPUs are grouped sequentially into nodes (see
            // cosmos_configure_numa): node N owns CPUs [N*cpn, (N+1)*cpn).
            let allowed: Vec<CpuId> = (0..cpus_per_node)
                .map(|k| CpuId(node * cpus_per_node + k))
                .collect();
            b = b.task(TaskDef {
                name: format!("n{node}_pinned"),
                pid: Pid((node + 1) as i32),
                nice: 0,
                behavior: TaskBehavior {
                    phases: vec![Phase::Run(4_000_000), Phase::Sleep(2_000_000)],
                    repeat: RepeatMode::Forever,
                },
                start_time_ns: 0,
                mm_id: None,
                allowed_cpus: Some(allowed),
                parent_pid: None,
                cgroup_name: None,
                task_flags: 0,
                migration_disabled: 0,
                thread_group_leader: None,
                uid: Uid(0),
                gid: Gid(0),
                fork_cpu: None,
            });
        }
        let scenario = b.duration_ms(200).build();
        let trace = Simulator::new(sched).run(scenario);
        assert_all_progressed(&trace, nr_nodes, &ctx);

        // Each pinned task must only run on its node's CPUs.
        for node in 0..nr_nodes {
            let lo = node * cpus_per_node;
            let hi = lo + cpus_per_node;
            for ev in trace.events() {
                if let TraceKind::TaskScheduled { pid } = ev.kind {
                    if pid == Pid((node + 1) as i32) {
                        assert!(
                            ev.cpu.0 >= lo && ev.cpu.0 < hi,
                            "{ctx}: node{node} task ran on {:?} outside [{lo},{hi})",
                            ev.cpu
                        );
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// LLC / compute-domain sharing patterns
// ---------------------------------------------------------------------------

/// LAVD multi-domain (cpdom) sweep: 2 and 4 compute domains over 8 CPUs. Each
/// domain is a neighbor of all others, enabling the cross-domain migration
/// paths in balance.bpf.c (`plan_x_cpdom_migration`, `try_to_steal_task`,
/// `force_to_steal_task`). A saturating workload with idle windows forces
/// stealing across domains.
#[test]
fn test_lavd_domain_sweep() {
    let _lock = common::setup_test();
    let nr_cpus = 8;
    for nr_domains in [2, 4] {
        let ctx = format!("lavd domains={nr_domains}");
        let sched = DynamicScheduler::lavd_multi_domain(nr_cpus, nr_domains);
        let (scenario, nr_tasks) = topo_scenario(nr_cpus, 1, 0);
        let trace = Simulator::new(sched).run(scenario);
        assert_all_progressed(&trace, nr_tasks, &ctx);
        assert_spread(&trace, nr_cpus, &ctx);
    }
}

/// Engine-level LLC topology (`cpus_per_llc`) sweep under LAVD: 8 CPUs split
/// into 4, 2, and 1 LLC domain(s). This sets each CPU's `llc_id`, the topology
/// the scheduler observes for DSQ routing and migration-cost decisions —
/// distinct from `lavd_multi_domain` (which configures the scheduler's own
/// cpdom table). Exercises the scheduler over coarse-to-fine cache sharing.
#[test]
fn test_lavd_cpus_per_llc_sweep() {
    let _lock = common::setup_test();
    let nr_cpus = 8;
    for cpus_per_llc in [2, 4, 8] {
        let ctx = format!("lavd cpus_per_llc={cpus_per_llc}");
        let sched = DynamicScheduler::lavd(nr_cpus);
        let (scenario, nr_tasks) = topo_scenario(nr_cpus, 1, cpus_per_llc);
        let trace = Simulator::new(sched).run(scenario);
        assert_all_progressed(&trace, nr_tasks, &ctx);
        assert_spread(&trace, nr_cpus, &ctx);
    }
}

/// Engine-level LLC topology (`cpus_per_llc`) sweep under COSMOS: same 8-CPU
/// cache-sharing gradient (4 / 2 / 1 LLC domains) observed by scx_cosmos.
#[test]
fn test_cosmos_cpus_per_llc_sweep() {
    let _lock = common::setup_test();
    let nr_cpus = 8;
    for cpus_per_llc in [2, 4, 8] {
        let ctx = format!("cosmos cpus_per_llc={cpus_per_llc}");
        let sched = DynamicScheduler::cosmos(nr_cpus);
        let (scenario, nr_tasks) = topo_scenario(nr_cpus, 1, cpus_per_llc);
        let trace = Simulator::new(sched).run(scenario);
        assert_all_progressed(&trace, nr_tasks, &ctx);
        assert_spread(&trace, nr_cpus, &ctx);
    }
}

// ---------------------------------------------------------------------------
// SMT (hyperthreading) configurations
// ---------------------------------------------------------------------------

/// SMT thread-per-core sweep (1, 2, 4) over 8 logical CPUs — i.e. 8 cores,
/// 4 cores×2 threads, and 2 cores×4 threads. Run under both COSMOS and LAVD so
/// each scheduler's SMT-aware idle-CPU selection (`get_idle_smtmask`, sibling
/// tracking) is exercised across sibling-group granularities.
#[test]
fn test_smt_thread_per_core_sweep() {
    let _lock = common::setup_test();
    let nr_cpus = 8;
    let schedulers: [NamedSchedFactory; 2] = [
        ("cosmos", DynamicScheduler::cosmos),
        ("lavd", DynamicScheduler::lavd),
    ];
    for smt in [1, 2, 4] {
        for (label, make) in schedulers {
            let ctx = format!("{label} smt={smt}");
            let (scenario, nr_tasks) = topo_scenario(nr_cpus, smt, 0);
            let trace = Simulator::new(make(nr_cpus)).run(scenario);
            assert_all_progressed(&trace, nr_tasks, &ctx);
            assert_spread(&trace, nr_cpus, &ctx);
        }
    }
}

/// COSMOS SMT sibling domains populated across SMT granularities. COSMOS
/// userspace's `init_smt_domains()` (mirrored by `cosmos_enable_smt_siblings`)
/// fills each CPU's `cctx->smt` sibling mask; with it populated the scheduler's
/// SMT-aware placement observes real sibling relationships for 2- and 4-way
/// SMT.
#[test]
fn test_cosmos_smt_sibling_sweep() {
    let _lock = common::setup_test();
    let nr_cpus = 8;
    for smt in [2, 4] {
        let ctx = format!("cosmos smt-siblings smt={smt}");
        let sched = DynamicScheduler::cosmos(nr_cpus);
        sched.cosmos_enable_smt_siblings(nr_cpus, smt);
        let (scenario, nr_tasks) = topo_scenario(nr_cpus, smt, 0);
        let trace = Simulator::new(sched).run(scenario);
        assert_all_progressed(&trace, nr_tasks, &ctx);
        assert_spread(&trace, nr_cpus, &ctx);
    }
}

// ---------------------------------------------------------------------------
// Asymmetric (big.LITTLE) capacity topologies (COSMOS)
// ---------------------------------------------------------------------------

/// Asymmetric per-CPU capacity sweep for COSMOS over several big/LITTLE splits.
/// Installing a heterogeneous `cpu_capacity[]` table with
/// `all_cpus_same_capacity=false` exercises `scale_by_cpu_capacity()` and
/// `is_cpu_faster()` — the capacity-comparison branches that never run under
/// the default uniform-capacity topology. Patterns span an even split, a
/// big-heavy and LITTLE-heavy split, and a monotonic capacity gradient.
#[test]
fn test_cosmos_asymmetric_capacity_sweep() {
    let _lock = common::setup_test();
    let nr_cpus = 8;
    let patterns: &[(&str, [u64; 8])] = &[
        (
            "even 4big/4little",
            [1024, 1024, 1024, 1024, 512, 512, 512, 512],
        ),
        (
            "big-heavy 6/2",
            [1024, 1024, 1024, 1024, 1024, 1024, 512, 512],
        ),
        (
            "little-heavy 2/6",
            [1024, 1024, 512, 512, 512, 512, 512, 512],
        ),
        ("gradient", [1024, 896, 768, 640, 512, 384, 256, 128]),
    ];
    for (label, caps) in patterns {
        let ctx = format!("cosmos capacity [{label}]");
        let sched = DynamicScheduler::cosmos(nr_cpus);
        sched.cosmos_set_cpu_capacity(caps);
        let (scenario, nr_tasks) = topo_scenario(nr_cpus, 1, 0);
        let trace = Simulator::new(sched).run(scenario);
        assert_all_progressed(&trace, nr_tasks, &ctx);
        assert_spread(&trace, nr_cpus, &ctx);
    }
}

/// Asymmetric capacity combined with SMT: a big.LITTLE table where each
/// physical core's two SMT threads share capacity (big cores 0-1 → CPUs 0-3
/// at 1024, LITTLE cores 2-3 → CPUs 4-7 at 512). Exercises capacity scaling
/// and SMT sibling placement together, the realistic hybrid-laptop layout.
#[test]
fn test_cosmos_asymmetric_capacity_with_smt() {
    let _lock = common::setup_test();
    let nr_cpus = 8;
    let ctx = "cosmos capacity+smt";
    let sched = DynamicScheduler::cosmos(nr_cpus);
    sched.cosmos_set_cpu_capacity(&[1024, 1024, 1024, 1024, 512, 512, 512, 512]);
    sched.cosmos_enable_smt_siblings(nr_cpus, 2);
    let (scenario, nr_tasks) = topo_scenario(nr_cpus, 2, 0);
    let trace = Simulator::new(sched).run(scenario);
    assert_all_progressed(&trace, nr_tasks, ctx);
    assert_spread(&trace, nr_cpus, ctx);
}
