//! Diverse CPU-topology configuration tests (tg test-topology-configs).
//!
//! This file complements — rather than duplicates — the single-point topology
//! tests already in `cosmos.rs` (SMT / NUMA / heterogeneous capacity) and the
//! `lavd_multi_domain_*` suite in `lavd.rs`. Here we sweep the topology
//! *matrix* the way real hardware varies it:
//!
//!   1. Multi-NUMA node counts (2 / 4 / 8 nodes) — `cosmos_with_numa`.
//!   2. LLC (last-level-cache) sharing patterns — `Scenario::cpus_per_llc`.
//!   3. SMT / hyperthreading depth (1 / 2 / 4 threads per core).
//!   4. Asymmetric big.LITTLE capacity tables — `cosmos_set_cpu_capacity`.
//!   5. Combined topologies (NUMA + SMT + capacity together).
//!
//! Every topology is exercised under an oversubscribed workload so the
//! scheduler's placement / migration / balancing paths run, and the assertions
//! stay behavior-agnostic (simulation completes cleanly and every task makes
//! progress) — the goal is topology coverage, not a specific placement outcome.
//! Where a scheduler-agnostic workload makes sense, the same topology is run
//! under BOTH cosmos and lavd.

use scx_simulator::*;

#[macro_use]
mod common;

/// A run/sleep task that keeps re-entering select_cpu (drives wakeup placement).
fn bursty(run_ns: TimeNs, sleep_ns: TimeNs) -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(run_ns), Phase::Sleep(sleep_ns)],
        repeat: RepeatMode::Forever,
    }
}

/// Run duration for every matrix scenario (ms). Shared by the scenario builder
/// and the utilization floor so the two stay in sync.
const DURATION_MS: u64 = 150;

/// Assert the run completed cleanly, every task (`pid` 1..=`nr_tasks`) made
/// forward progress, AND the scheduler kept the CPUs meaningfully busy under
/// this topology (aggregate on-CPU time >= 50% of `nr_cpus * DURATION_MS`).
///
/// The utilization floor turns these from "didn't crash" checks into "the
/// topology didn't wedge the scheduler into leaving CPUs idle" checks — the
/// real failure mode a bad topology mapping would cause. Measured utilization
/// across the matrix is 83–99%, so a 50% floor is robust with wide headroom.
fn assert_clean_progress(trace: &Trace, nr_cpus: u32, nr_tasks: u32, ctx: &str) {
    assert_eq!(
        trace.exit_kind(),
        &ExitKind::Normal,
        "[{ctx}] simulation did not complete normally: {:?}",
        trace.exit_kind()
    );
    assert!(!trace.has_error(), "[{ctx}] scheduler raised scx_bpf_error");
    for pid in 1..=nr_tasks as i32 {
        assert!(
            trace.total_runtime(Pid(pid)) > 0,
            "[{ctx}] task {pid} of {nr_tasks} got no runtime"
        );
    }
    let total: u64 = (1..=nr_tasks as i32)
        .map(|p| trace.total_runtime(Pid(p)))
        .sum();
    let capacity_ns = nr_cpus as u64 * DURATION_MS * 1_000_000;
    assert!(
        total >= capacity_ns / 2,
        "[{ctx}] under-utilized: {total} ns on-CPU vs {capacity_ns} ns capacity (<50%)"
    );
}

/// Build an oversubscribed bursty workload on the given topology and return the
/// scenario (task count is `tasks_per_cpu * nr_cpus`, capped so a run is fast).
fn oversubscribed_scenario(nr_cpus: u32, smt: u32, cpus_per_llc: u32, nr_tasks: u32) -> Scenario {
    let mut b = Scenario::builder().cpus(nr_cpus).smt(smt);
    if cpus_per_llc > 0 {
        b = b.cpus_per_llc(cpus_per_llc);
    }
    for i in 0..nr_tasks {
        // Nice spread varies task weight so weighted placement paths run.
        let nice = ((i as i16 % 9) - 4) as i8;
        b = b.add_task(&format!("t{i}"), nice, bursty(3_000_000, 1_000_000));
    }
    b.duration_ms(DURATION_MS).build()
}

// ============================================================================
// 1. NUMA node-count sweep (cosmos)
// ============================================================================

/// Sweep COSMOS across 2 / 4 / 8 NUMA nodes (plus the single-node baseline).
/// Each node gets its own shared DSQ (`shared_dsq()` keys on the node), so this
/// exercises the per-node queue routing across a range of node counts, well
/// beyond the single `cosmos_with_numa(4, 2)` case in `cosmos.rs`.
#[test]
fn test_cosmos_numa_node_sweep() {
    let _lock = common::setup_test();
    // (nr_cpus, nr_nodes) — nr_cpus must be divisible by nr_nodes.
    let configs: &[(u32, u32)] = &[(4, 1), (4, 2), (8, 2), (8, 4), (8, 8), (16, 4)];

    for &(nr_cpus, nr_nodes) in configs {
        let ctx = format!("cosmos numa {nr_cpus}cpu/{nr_nodes}node");
        let sched = if nr_nodes <= 1 {
            DynamicScheduler::cosmos(nr_cpus)
        } else {
            DynamicScheduler::cosmos_with_numa(nr_cpus, nr_nodes)
        };
        let nr_tasks = nr_cpus * 2;
        let scenario = oversubscribed_scenario(nr_cpus, 1, 0, nr_tasks);
        let trace = Simulator::new(sched).run(scenario);
        assert_clean_progress(&trace, nr_cpus, nr_tasks, &ctx);
    }
}

// ============================================================================
// 2. SMT depth sweep (cosmos + lavd)
// ============================================================================

/// Sweep SMT depth (1 / 2 / 4 threads per core) under COSMOS with its sibling
/// masks populated (`cosmos_enable_smt_siblings`), so the SMT-contention /
/// full-idle-core placement logic observes real sibling relationships at each
/// depth. `cosmos.rs` only covers `smt(2)`.
#[test]
fn test_cosmos_smt_depth_sweep() {
    let _lock = common::setup_test();
    let nr_cpus = 8u32;
    for &threads in &[1u32, 2, 4] {
        let ctx = format!("cosmos smt {threads}t/core on {nr_cpus}cpu");
        let sched = DynamicScheduler::cosmos(nr_cpus);
        if threads > 1 {
            sched.cosmos_enable_smt_siblings(nr_cpus, threads);
        }
        let nr_tasks = nr_cpus + 2;
        let scenario = oversubscribed_scenario(nr_cpus, threads, 0, nr_tasks);
        let trace = Simulator::new(sched).run(scenario);
        assert_clean_progress(&trace, nr_cpus, nr_tasks, &ctx);
    }
}

/// Sweep SMT depth under LAVD. LAVD's idle-core/SMT selection consumes the
/// engine's SMT topology directly (no per-scheduler sibling setup needed), so
/// this validates LAVD placement across hyperthreading depths.
#[test]
fn test_lavd_smt_depth_sweep() {
    let _lock = common::setup_test();
    let nr_cpus = 8u32;
    for &threads in &[1u32, 2, 4] {
        let ctx = format!("lavd smt {threads}t/core on {nr_cpus}cpu");
        let sched = DynamicScheduler::lavd(nr_cpus);
        let nr_tasks = nr_cpus + 2;
        let scenario = oversubscribed_scenario(nr_cpus, threads, 0, nr_tasks);
        let trace = Simulator::new(sched).run(scenario);
        assert_clean_progress(&trace, nr_cpus, nr_tasks, &ctx);
    }
}

// ============================================================================
// 3. LLC sharing-pattern sweep (lavd + cosmos)
// ============================================================================

/// Sweep LLC (CCX) sharing patterns under LAVD: one shared LLC, two LLCs, and
/// per-pair LLCs. `cpus_per_llc` assigns each CPU an `llc_id`, driving LAVD's
/// LLC-aware DSQ routing / migration decisions. Combined with SMT so the two
/// cache axes interact.
#[test]
fn test_lavd_llc_sharing_sweep() {
    let _lock = common::setup_test();
    let nr_cpus = 8u32;
    // 0 == single LLC; 8 == one big LLC; 4 == two LLCs; 2 == four LLCs.
    for &cpus_per_llc in &[0u32, 8, 4, 2] {
        let ctx = format!("lavd llc {cpus_per_llc}cpu/llc on {nr_cpus}cpu");
        let sched = DynamicScheduler::lavd(nr_cpus);
        let nr_tasks = nr_cpus * 2;
        let scenario = oversubscribed_scenario(nr_cpus, 2, cpus_per_llc, nr_tasks);
        let trace = Simulator::new(sched).run(scenario);
        assert_clean_progress(&trace, nr_cpus, nr_tasks, &ctx);
    }
}

/// LLC sharing patterns under COSMOS — the same cache-topology axis, but driven
/// through cosmos's placement path, so both supported schedulers are covered on
/// the LLC dimension.
#[test]
fn test_cosmos_llc_sharing_sweep() {
    let _lock = common::setup_test();
    let nr_cpus = 8u32;
    for &cpus_per_llc in &[0u32, 4, 2] {
        let ctx = format!("cosmos llc {cpus_per_llc}cpu/llc on {nr_cpus}cpu");
        let sched = DynamicScheduler::cosmos(nr_cpus);
        let nr_tasks = nr_cpus * 2;
        let scenario = oversubscribed_scenario(nr_cpus, 2, cpus_per_llc, nr_tasks);
        let trace = Simulator::new(sched).run(scenario);
        assert_clean_progress(&trace, nr_cpus, nr_tasks, &ctx);
    }
}

// ============================================================================
// 4. LAVD multi-domain (cpdom) node-count sweep
// ============================================================================

/// Sweep LAVD's compute-domain count more broadly than the existing
/// `lavd_multi_domain_*` tests (which fix (4,2)/(6,3)/(8,4)). Larger domain
/// counts spread CPUs thinner per domain, stressing the cross-domain
/// migration / stealing balance paths under oversubscription.
#[test]
fn test_lavd_multi_domain_sweep() {
    let _lock = common::setup_test();
    // (nr_cpus, nr_domains) — nr_cpus >= nr_domains, nr_domains >= 2.
    let configs: &[(u32, u32)] = &[(4, 2), (8, 2), (8, 4), (16, 4), (16, 8)];

    for &(nr_cpus, nr_domains) in configs {
        let ctx = format!("lavd {nr_cpus}cpu/{nr_domains}dom");
        let sched = DynamicScheduler::lavd_multi_domain(nr_cpus, nr_domains);
        let nr_tasks = nr_cpus * 2;
        let scenario = oversubscribed_scenario(nr_cpus, 1, 0, nr_tasks);
        let trace = Simulator::new(sched).run(scenario);
        assert_clean_progress(&trace, nr_cpus, nr_tasks, &ctx);
    }
}

// ============================================================================
// 5. Asymmetric big.LITTLE capacity patterns (cosmos)
// ============================================================================

/// Sweep several asymmetric (big.LITTLE) capacity tables under COSMOS.
/// `cosmos_set_cpu_capacity` sets a heterogeneous `cpu_capacity[]`
/// (`all_cpus_same_capacity=false`), so `scale_by_cpu_capacity()` scales slices
/// by per-CPU capacity. Patterns cover a 2-big/2-little split, a single big
/// core, and a 4-tier gradient — broader than the single split in `cosmos.rs`.
#[test]
fn test_cosmos_big_little_patterns() {
    let _lock = common::setup_test();
    // (label, capacity table). 1024 == SCX_CPUPERF_ONE (fastest).
    let patterns: &[(&str, &[u64])] = &[
        ("2big2little", &[1024, 1024, 512, 512]),
        ("1big3little", &[1024, 340, 340, 340]),
        ("4tier_gradient", &[1024, 768, 512, 256]),
        ("8cpu_hybrid", &[1024, 1024, 1024, 1024, 512, 512, 512, 512]),
    ];

    for &(label, caps) in patterns {
        let nr_cpus = caps.len() as u32;
        let ctx = format!("cosmos big.LITTLE {label} ({nr_cpus}cpu)");
        let sched = DynamicScheduler::cosmos(nr_cpus);
        sched.cosmos_set_cpu_capacity(caps);
        let nr_tasks = nr_cpus * 2;
        let scenario = oversubscribed_scenario(nr_cpus, 1, 0, nr_tasks);
        let trace = Simulator::new(sched).run(scenario);
        assert_clean_progress(&trace, nr_cpus, nr_tasks, &ctx);
    }
}

// ============================================================================
// 6. Combined topologies (NUMA + SMT + capacity)
// ============================================================================

/// A realistic combined topology: multi-NUMA + SMT siblings + asymmetric
/// capacity all active at once under COSMOS. Nothing else exercises these
/// dimensions together, which is where cross-dimension placement bugs hide.
#[test]
fn test_cosmos_combined_numa_smt_capacity() {
    let _lock = common::setup_test();
    let nr_cpus = 8u32;
    let nr_nodes = 2u32;
    let threads = 2u32;

    let sched = DynamicScheduler::cosmos_with_numa(nr_cpus, nr_nodes);
    sched.cosmos_enable_smt_siblings(nr_cpus, threads);
    // Node 0 (cpus 0-3) = big cores, node 1 (cpus 4-7) = little cores.
    sched.cosmos_set_cpu_capacity(&[1024, 1024, 1024, 1024, 512, 512, 512, 512]);

    let nr_tasks = nr_cpus * 2;
    let scenario = oversubscribed_scenario(nr_cpus, threads, 0, nr_tasks);
    let trace = Simulator::new(sched).run(scenario);
    assert_clean_progress(
        &trace,
        nr_cpus,
        nr_tasks,
        "cosmos combined numa+smt+capacity",
    );
}

// ============================================================================
// 7. Cross-scheduler topology matrix (cosmos AND lavd on the same topology)
// ============================================================================

/// Run the same set of topologies (CPU count × SMT depth × LLC pattern) under
/// BOTH cosmos and lavd, asserting each completes cleanly with all tasks making
/// progress. Directly satisfies "test scheduler behavior under each topology
/// with cosmos and lavd".
#[test]
fn test_both_schedulers_topology_matrix() {
    let _lock = common::setup_test();
    // (nr_cpus, smt, cpus_per_llc)
    let topologies: &[(u32, u32, u32)] = &[(4, 1, 0), (4, 2, 2), (8, 2, 4), (8, 4, 2)];

    for &(nr_cpus, smt, cpus_per_llc) in topologies {
        let nr_tasks = nr_cpus + 2;

        // COSMOS
        {
            let ctx = format!("cosmos matrix {nr_cpus}cpu/{smt}smt/{cpus_per_llc}llc");
            let sched = DynamicScheduler::cosmos(nr_cpus);
            if smt > 1 {
                sched.cosmos_enable_smt_siblings(nr_cpus, smt);
            }
            let scenario = oversubscribed_scenario(nr_cpus, smt, cpus_per_llc, nr_tasks);
            let trace = Simulator::new(sched).run(scenario);
            assert_clean_progress(&trace, nr_cpus, nr_tasks, &ctx);
        }

        // LAVD
        {
            let ctx = format!("lavd matrix {nr_cpus}cpu/{smt}smt/{cpus_per_llc}llc");
            let sched = DynamicScheduler::lavd(nr_cpus);
            let scenario = oversubscribed_scenario(nr_cpus, smt, cpus_per_llc, nr_tasks);
            let trace = Simulator::new(sched).run(scenario);
            assert_clean_progress(&trace, nr_cpus, nr_tasks, &ctx);
        }
    }
}
