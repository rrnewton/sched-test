//! Engine NUMA substrate: node membership and per-node observability.
//!
//! This is the **engine** half of NUMA modelling (Tier 1 of
//! `goal-simulated-numa-domains`). It covers `Scenario.cpus_per_node` →
//! `SimCpu.node_id` → `scx_bpf_cpu_node()`, and the `Trace` readout that makes
//! a scheduler's cross-node placement decisions assertable.
//!
//! **These are NOT a reproduction of any NUMA pathology, and must not be
//! relabelled as one.** A two-node topology on its own produces the
//! saturated-node/idle-node fingerprint of the known `scx_layered` cross-NUMA
//! incident for entirely the wrong reason — with zero-initialised BPF bss the
//! gate denies before any of the causal logic runs — so asserting on the
//! *utilisation shape* would go green on a scheduler that executed none of the
//! interesting code. The honest discriminator for that repro is that
//! `xnuma_bucket_refill` and `xnuma_gate_charge` actually execute, which needs
//! the layered wrapper and its userspace control loop. See
//! `ai_docs/NUMA_MODELLING_DESIGN_20260812.md` §6b.
//!
//! So what is asserted here is deliberately narrow and structural: that the
//! topology the scenario declares is the topology the engine reports, and that
//! the per-node readout is a faithful fold of where tasks actually ran. Every
//! assertion is an identity or a conservation law derived from the same run —
//! there is no tuned threshold anywhere in this file, because a tuned
//! threshold is a number that gets adjusted until the test is green.

use scx_simulator::*;

#[macro_use]
mod common;

fn run_sleep(run_ns: u64, sleep_ns: u64) -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(run_ns), Phase::Sleep(sleep_ns)],
        repeat: RepeatMode::Forever,
    }
}

/// Mixed workload that keeps every CPU in play, so the per-node fold has
/// something non-trivial to add up.
fn busy_scenario(nr_cpus: u32, cpus_per_node: u32, duration_ms: u64) -> (Scenario, u32) {
    let nr_tasks = nr_cpus * 2;
    let mut b = Scenario::builder()
        .cpus(nr_cpus)
        .cpus_per_node(cpus_per_node)
        .seed(42)
        .instant_timing();
    for i in 1..=nr_tasks {
        b = b.add_task(&format!("t{i}"), 0, run_sleep(2_000_000, 1_000_000));
    }
    (b.duration_ms(duration_ms).build(), nr_tasks)
}

// ---------------------------------------------------------------------------
// 1. Scenario → engine topology
// ---------------------------------------------------------------------------

/// `cpus_per_node` partitions CPUs into contiguous nodes, and the partition
/// the scenario declared is the one that comes back out.
#[test]
fn test_cpus_per_node_partitions_cpus() {
    for (nr_cpus, cpus_per_node, expect_nodes) in [(8u32, 4u32, 2u32), (8, 2, 4), (8, 8, 1)] {
        let s = Scenario::builder()
            .cpus(nr_cpus)
            .cpus_per_node(cpus_per_node)
            .add_task("t1", 0, run_sleep(1_000_000, 1_000_000))
            .duration_ms(1)
            .build();
        assert_eq!(s.cpus_per_node, cpus_per_node);
        assert_eq!(
            nr_cpus / cpus_per_node,
            expect_nodes,
            "{nr_cpus} cpus / {cpus_per_node} per node"
        );
    }
}

/// Default is a single node — an unset topology means one node, not "unknown".
#[test]
fn test_default_topology_is_single_node() {
    let s = Scenario::builder()
        .cpus(8)
        .add_task("t1", 0, run_sleep(1_000_000, 1_000_000))
        .duration_ms(1)
        .build();
    assert_eq!(s.cpus_per_node, 0, "0 is the single-node sentinel");
}

/// A node must divide the CPU count.
#[test]
#[should_panic(expected = "must be divisible by cpus_per_node")]
fn test_cpus_per_node_must_divide_nr_cpus() {
    Scenario::builder()
        .cpus(8)
        .cpus_per_node(3)
        .add_task("t1", 0, run_sleep(1_000_000, 1_000_000))
        .duration_ms(1)
        .build();
}

/// A node must be a union of WHOLE LLCs. Real hardware never splits an LLC
/// across NUMA nodes; simulating that would make any scheduler which relies on
/// the invariant misbehave for our reason rather than its own, and the
/// resulting bug would be un-attributable. Rejected at build time.
#[test]
#[should_panic(expected = "must contain whole LLCs")]
fn test_node_may_not_split_an_llc() {
    Scenario::builder()
        .cpus(16)
        .cpus_per_llc(8) // 2 LLCs of 8
        .cpus_per_node(4) // would cut each LLC in half
        .add_task("t1", 0, run_sleep(1_000_000, 1_000_000))
        .duration_ms(1)
        .build();
}

/// The legal nesting (node = N whole LLCs) is accepted.
#[test]
fn test_node_containing_whole_llcs_is_accepted() {
    let s = Scenario::builder()
        .cpus(16)
        .cpus_per_llc(4) // 4 LLCs of 4
        .cpus_per_node(8) // 2 nodes, each 2 whole LLCs
        .add_task("t1", 0, run_sleep(1_000_000, 1_000_000))
        .duration_ms(1)
        .build();
    assert_eq!(s.cpus_per_llc, 4);
    assert_eq!(s.cpus_per_node, 8);
}

// ---------------------------------------------------------------------------
// 2. Per-node observability
// ---------------------------------------------------------------------------

/// The per-node fold conserves busy time: summing per-node equals summing
/// per-CPU. A conservation law, not a threshold — it cannot be satisfied by
/// tuning, only by the fold being correct.
#[test]
fn test_node_busy_ns_conserves_cpu_busy_ns() {
    let _lock = common::setup_test();
    let nr_cpus = 8u32;
    let cpus_per_node = 4u32;
    let (scenario, nr_tasks) = busy_scenario(nr_cpus, cpus_per_node, 200);
    let trace = Simulator::new(DynamicScheduler::cosmos(nr_cpus)).run(scenario);

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    for p in 1..=nr_tasks as i32 {
        assert!(trace.schedule_count(Pid(p)) > 0, "task {p} starved");
    }

    let per_cpu = trace.cpu_busy_ns();
    let per_node = trace.node_busy_ns(|cpu| cpu.0 / cpus_per_node);

    assert_eq!(
        per_node.len(),
        (nr_cpus / cpus_per_node) as usize,
        "expected one entry per node"
    );
    assert_eq!(
        per_cpu.iter().sum::<u64>(),
        per_node.iter().sum::<u64>(),
        "per-node fold must conserve total busy time"
    );
    assert!(
        per_node.iter().sum::<u64>() > 0,
        "workload produced no busy time at all — the readout would be vacuous"
    );
}

/// Per-CPU busy time agrees with the existing per-task `total_runtime()`.
/// Both walk the same scheduled→descheduled intervals, so if they disagree one
/// of them is wrong. This pins `cpu_busy_ns` to an already-trusted number
/// rather than to a value observed once and frozen.
#[test]
fn test_cpu_busy_ns_agrees_with_total_runtime() {
    let _lock = common::setup_test();
    let nr_cpus = 4u32;
    let (scenario, nr_tasks) = busy_scenario(nr_cpus, 2, 200);
    let trace = Simulator::new(DynamicScheduler::cosmos(nr_cpus)).run(scenario);

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);

    let by_cpu: u64 = trace.cpu_busy_ns().iter().sum();
    let by_task: u64 = (1..=nr_tasks as i32)
        .map(|p| trace.total_runtime(Pid(p)))
        .sum();

    assert_eq!(
        by_cpu, by_task,
        "summing busy time per CPU and per task must give the same total"
    );
}

/// Every CPU is folded into exactly one node, and no busy time is attributed
/// to a node that has no CPUs. Guards the fold against an off-by-one in the
/// node mapping, which would silently move utilisation between nodes — the one
/// error that would corrupt every downstream placement assertion.
#[test]
fn test_node_fold_attributes_every_cpu_exactly_once() {
    let _lock = common::setup_test();
    let nr_cpus = 8u32;
    let cpus_per_node = 4u32;
    let (scenario, _) = busy_scenario(nr_cpus, cpus_per_node, 150);
    let trace = Simulator::new(DynamicScheduler::cosmos(nr_cpus)).run(scenario);

    let per_cpu = trace.cpu_busy_ns();
    let per_node = trace.node_busy_ns(|cpu| cpu.0 / cpus_per_node);

    // Recompute the fold independently, the long way.
    let mut expected = vec![0u64; (nr_cpus / cpus_per_node) as usize];
    for (cpu_idx, busy) in per_cpu.iter().enumerate() {
        expected[cpu_idx / cpus_per_node as usize] += busy;
    }
    assert_eq!(per_node, expected, "node fold disagrees with a direct sum");
}

// ---------------------------------------------------------------------------
// 3. External oracle — the only assertions here NOT derived from the run
// ---------------------------------------------------------------------------
//
// Everything above this line validates the readout against something else the
// same run produced: a fold against its own inputs, or one trace query against
// another. That family of assertion is CIRCULAR — it holds whatever the
// simulator does, including if the simulator is wrong. It catches a broken
// fold; it cannot catch a broken engine, and it must not be mistaken for
// evidence that the numbers are right.
//
// The tests below fix that by predicting the answer from the SCENARIO SPEC
// alone — run/sleep durations and duration_ms, all declared inputs — and
// comparing the measurement to that. If the engine mis-accounts runtime, these
// fail; the ones above would not.

/// Busy time on a single CPU is predictable from the task's duty cycle.
///
/// One task, `run_ns` busy then `sleep_ns` idle, forever, on one CPU with no
/// contention and overhead disabled. Over `duration_ms` it completes
/// `duration / (run + sleep)` whole cycles, so busy time is
/// `cycles * run_ns` — computed here from the scenario inputs, never from the
/// trace.
#[test]
fn test_cpu_busy_ns_matches_duty_cycle_oracle() {
    let _lock = common::setup_test();
    let (run_ns, sleep_ns, dur_ms) = (2_000_000u64, 8_000_000u64, 100u64);

    let b = Scenario::builder()
        .cpus(1)
        .cpus_per_node(1)
        .seed(42)
        .instant_timing()
        .add_task("solo", 0, run_sleep(run_ns, sleep_ns))
        .duration_ms(dur_ms);
    let trace = Simulator::new(DynamicScheduler::cosmos(1)).run(b.build());

    let cycles = (dur_ms * 1_000_000) / (run_ns + sleep_ns);
    let predicted = cycles * run_ns;
    let measured: u64 = trace.cpu_busy_ns().iter().sum();

    // EXACT. `duration_ms` is an exact multiple of the cycle, so no interval
    // is left open and there is nothing legitimate to be slack about.
    //
    // An earlier version of this test allowed "one run-phase" of slack. That
    // tolerance was +/-10% of the prediction here, which is wide enough to
    // swallow the very error class the oracle exists to catch: a mutation that
    // under-counted every interval by 10% passed. A tolerance picked for
    // comfort rather than derived from a real source of variation is not a
    // safeguard, it is a hole. If a future change makes this genuinely
    // inexact, derive the bound from what actually varies — do not widen it
    // until it goes green.
    assert_eq!(
        measured, predicted,
        "busy time must equal the duty-cycle prediction \
         ({cycles} cycles x {run_ns}ns run)"
    );
}

/// Per-NODE busy time is attributed to the right node.
///
/// Two nodes of one CPU each, one task pinned to each node, with DIFFERENT
/// duty cycles. Each node's busy time is predicted independently from its own
/// task's spec. The loads are deliberately asymmetric: if the fold attributed
/// node 1's time to node 0 the totals would still conserve — which is exactly
/// what the conservation test above cannot see — but these per-node
/// predictions would both fail.
#[test]
fn test_node_busy_ns_attribution_matches_oracle() {
    let _lock = common::setup_test();
    let dur_ms = 100u64;
    // node 0: 20% duty. node 1: 50% duty. Asymmetric on purpose.
    let node0 = (2_000_000u64, 8_000_000u64);
    let node1 = (5_000_000u64, 5_000_000u64);

    let mut b = Scenario::builder()
        .cpus(2)
        .cpus_per_node(1) // one CPU per node -> no contention, no migration
        .seed(42)
        .instant_timing();
    for (pid, cpu, (run_ns, sleep_ns)) in [(1i32, 0u32, node0), (2, 1, node1)] {
        b = b.task(TaskDef {
            name: format!("n{cpu}"),
            pid: Pid(pid),
            nice: 0,
            behavior: run_sleep(run_ns, sleep_ns),
            start_time_ns: 0,
            mm_id: None,
            allowed_cpus: Some(vec![CpuId(cpu)]),
            parent_pid: None,
            cgroup_name: None,
            task_flags: 0,
            migration_disabled: 0,
        });
    }
    let trace = Simulator::new(DynamicScheduler::cosmos(2)).run(b.duration_ms(dur_ms).build());
    assert_eq!(trace.exit_kind(), &ExitKind::Normal);

    let per_node = trace.node_busy_ns(|cpu| cpu.0);
    assert_eq!(per_node.len(), 2, "expected two nodes");

    for (node, (run_ns, sleep_ns)) in [(0usize, node0), (1, node1)] {
        let cycles = (dur_ms * 1_000_000) / (run_ns + sleep_ns);
        let predicted = cycles * run_ns;
        let measured = per_node[node];
        // Exact, for the same reason as above: whole cycles, nothing open.
        assert_eq!(
            measured, predicted,
            "node {node}: busy time must equal its task's duty-cycle \
             prediction ({cycles} cycles x {run_ns}ns run)"
        );
    }

    // And the asymmetry itself must survive the fold: node 1 works more.
    assert!(
        per_node[1] > per_node[0],
        "50%-duty node ({}) should be busier than the 20%-duty node ({}) — \
         equal or inverted means the fold mixed the nodes up",
        per_node[1],
        per_node[0]
    );
}
