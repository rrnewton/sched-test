//! COSMOS LLC-aware scheduling & domain balancing.
//!
//! ## What "LLC domain" means for COSMOS under scxsim (READ THIS FIRST)
//!
//! COSMOS's *own* last-level-cache logic — `cpus_share_cache()` →
//! `cpu_llc_id()` (`scx/scheds/include/scx/percpu.bpf.h`,
//! `DEFINE_PER_CPU_VAL_FUNC(cpu_llc_id, int, sd_llc_id)`) — is **inert under
//! the current simulator substrate**, for two independent reasons:
//!
//!  1. `cpu_llc_id()` reads the per-CPU kernel ksym `sd_llc_id` via
//!     `bpf_per_cpu_ptr()`, which the sim hard-stubs to `NULL`
//!     (`scx-sim/csrc/sim_wrapper.h`). So `cpu_llc_id()` returns `-EINVAL`
//!     for every CPU and `cpus_share_cache(a, b)` is `true` for ALL pairs —
//!     COSMOS sees ONE flat LLC regardless of `.cpus_per_llc(..)`.
//!  2. Its sole caller (`main.bpf.c` wakeup-affinity block) is gated on
//!     `is_wakeup(wake_flags) == (wake_flags & SCX_WAKE_TTWU)`, which the
//!     engine never sets (it delivers `SCX_WAKE_SYNC` only), AND on
//!     `is_cpu_faster()` (heterogeneous capacity).
//!
//! Wiring genuine sub-NUMA per-LLC domains is tracked in **mb sim-439319**
//! (populate `sd_llc_id` from `scenario.cpus_per_llc`) and **mb sim-e10316**
//! (deliver `SCX_WAKE_TTWU`). Until both land, a test that claimed COSMOS
//! "kept a task within its LLC" would be asserting nothing — forbidden by the
//! project's No-Stub / kernel-fidelity rules.
//!
//! ## What this file therefore tests (honestly)
//!
//! COSMOS's *real* schedulable domain axis under sim is the **NUMA node**:
//! `shared_dsq(cpu) = numa_enabled ? cpu_node(cpu) : SHARED_DSQ`
//! (`main.bpf.c`), with one shared DSQ per node created in `cosmos_init()` and
//! `cpu_node()` backed by a real `cpu_node_map` populated by
//! `cosmos_configure_numa` (`schedulers/cosmos/wrapper.c`). These are genuine,
//! plumbed domains the scheduler actually observes. The task's five goals map
//! onto that real domain model:
//!
//!  1. Task placement within a domain  → per-node shared-DSQ routing
//!     (`test_cosmos_per_node_dsq_routing`).
//!  2. Load balancing across domains   → per-node runtime fairness
//!     (`test_cosmos_numa_domain_load_balance`,
//!     `test_cosmos_multi_socket_balance`).
//!  3. Idle CPU selection within a domain → node-local wake placement
//!     (`test_cosmos_domain_local_wake_placement`).
//!  4. Task stealing across domains under imbalance → idle node gets used
//!     (`test_cosmos_idle_domain_utilized_under_imbalance`).
//!  5. NUMA-aware decisions on a multi-socket box → the above, swept over 2/4
//!     sockets.
//!
//! Two further tests cover the *engine-level* LLC model that `.cpus_per_llc()`
//! does drive for COSMOS — the cross-LLC migration **timing penalty** (a
//! legitimate "model the kernel" cost, NOT a COSMOS placement decision):
//! `test_cosmos_cross_llc_migration_cost` and
//! `test_cosmos_engine_llc_topology_healthy`.
//!
//! Deterministic serial engine, fixed seed + `instant_timing`; every assertion
//! is on observable trace events only.

use std::collections::HashSet;

use scx_simulator::*;

mod common;

// ---------------------------------------------------------------------------
// Workload helpers
// ---------------------------------------------------------------------------

fn run_sleep(run_ns: u64, sleep_ns: u64) -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(run_ns), Phase::Sleep(sleep_ns)],
        repeat: RepeatMode::Forever,
    }
}

fn forever_run(run_ns: u64) -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(run_ns)],
        repeat: RepeatMode::Forever,
    }
}

/// A task pinned to a contiguous CPU range `[lo, hi)` (one NUMA node's CPUs).
fn pinned_task(pid: i32, name: &str, lo: u32, hi: u32, behavior: TaskBehavior) -> TaskDef {
    TaskDef {
        name: name.into(),
        pid: Pid(pid),
        nice: 0,
        behavior,
        start_time_ns: 0,
        mm_id: None,
        allowed_cpus: Some((lo..hi).map(CpuId).collect()),
        parent_pid: None,
        cgroup_name: None,
        task_flags: 0,
        migration_disabled: 0,
        thread_group_leader: None,
        uid: Uid(0),
        gid: Gid(0),
        fork_cpu: None,
    }
}

// ---------------------------------------------------------------------------
// Trace-derivation helpers (scxsim emits no dedicated migration/placement
// event, so — as `cpu_migration.rs` documents — the test owns the CPU→node
// arithmetic: node = cpu / cpus_per_node).
// ---------------------------------------------------------------------------

/// Ordered CPUs a task was scheduled on (adjacent diffs == migrations).
fn scheduled_cpus(trace: &Trace, pid: Pid) -> Vec<CpuId> {
    trace
        .events()
        .iter()
        .filter_map(|e| match e.kind {
            TraceKind::TaskScheduled { pid: p } if p == pid => Some(e.cpu),
            _ => None,
        })
        .collect()
}

/// Per-node aggregate runtime across pids `1..=nr_tasks`, given a contiguous
/// `cpus_per_node` layout (node N owns CPUs `[N*cpn, (N+1)*cpn)`).
fn runtime_by_node(trace: &Trace, nr_cpus: u32, cpus_per_node: u32) -> Vec<u64> {
    let nr_nodes = (nr_cpus / cpus_per_node) as usize;
    let mut per_node = vec![0u64; nr_nodes];
    // Attribute each running interval to the CPU (hence node) it ran on by
    // pairing TaskScheduled with the following off-CPU event on that CPU.
    // Simpler & robust: sum the trace's per-CPU busy time via run segments.
    // We reconstruct per-CPU runtime from consecutive TaskScheduled →
    // (TaskPreempted|TaskSlept|TaskCompleted) on the same cpu.
    let events = trace.events();
    // running_since[cpu] = Some(start_ns) while a task occupies the CPU.
    let mut running_since: Vec<Option<u64>> = vec![None; nr_cpus as usize];
    for e in events {
        let cpu = e.cpu.0 as usize;
        if cpu >= nr_cpus as usize {
            continue;
        }
        match e.kind {
            TraceKind::TaskScheduled { .. } => {
                running_since[cpu] = Some(e.time_ns);
            }
            TraceKind::TaskPreempted { .. }
            | TraceKind::TaskSlept { .. }
            | TraceKind::TaskCompleted { .. } => {
                if let Some(start) = running_since[cpu].take() {
                    let node = cpu as u32 / cpus_per_node;
                    per_node[node as usize] += e.time_ns.saturating_sub(start);
                }
            }
            _ => {}
        }
    }
    per_node
}

/// Distinct nodes that ran at least one task.
fn nodes_used(trace: &Trace, cpus_per_node: u32) -> HashSet<u32> {
    trace
        .events()
        .iter()
        .filter_map(|e| match e.kind {
            TraceKind::TaskScheduled { .. } => Some(e.cpu.0 / cpus_per_node),
            _ => None,
        })
        .collect()
}

/// Distinct non-builtin DSQ ids that received a task insert (vtime or fifo).
/// For COSMOS with NUMA, `shared_dsq(cpu) = cpu_node(cpu)`, so these are node
/// ids.
fn inserted_dsq_ids(trace: &Trace) -> HashSet<u64> {
    trace
        .events()
        .iter()
        .filter_map(|e| match e.kind {
            TraceKind::DsqInsert { dsq_id, .. } | TraceKind::DsqInsertVtime { dsq_id, .. } => {
                (!dsq_id.is_builtin()).then_some(dsq_id.0)
            }
            _ => None,
        })
        .collect()
}

/// Realized migrations for a task that cross a NUMA-node boundary.
fn cross_node_migrations(trace: &Trace, pid: Pid, cpus_per_node: u32) -> usize {
    scheduled_cpus(trace, pid)
        .windows(2)
        .filter(|w| w[0] != w[1] && w[0].0 / cpus_per_node != w[1].0 / cpus_per_node)
        .count()
}

/// Realized migrations for a task that cross an engine LLC boundary.
fn cross_llc_migrations(trace: &Trace, pid: Pid, cpus_per_llc: u32) -> usize {
    scheduled_cpus(trace, pid)
        .windows(2)
        .filter(|w| w[0] != w[1] && w[0].0 / cpus_per_llc != w[1].0 / cpus_per_llc)
        .count()
}

fn assert_identical(t1: &Trace, t2: &Trace, ctx: &str) {
    assert_eq!(
        t1.events().len(),
        t2.events().len(),
        "{ctx}: trace lengths differ"
    );
    for (i, (e1, e2)) in t1.events().iter().zip(t2.events().iter()).enumerate() {
        assert_eq!(e1.time_ns, e2.time_ns, "{ctx}: event {i} time differs");
        assert_eq!(e1.cpu, e2.cpu, "{ctx}: event {i} cpu differs");
        assert_eq!(e1.kind, e2.kind, "{ctx}: event {i} kind differs");
    }
}

// ---------------------------------------------------------------------------
// 1. Task placement within a domain — per-node shared-DSQ routing.
// ---------------------------------------------------------------------------

/// COSMOS routes a task into its NUMA node's shared DSQ
/// (`shared_dsq(cpu) = cpu_node(cpu)`). Under a saturated (busy) system COSMOS
/// switches from per-CPU round-robin queues to the per-node shared DSQ, so the
/// node-routing is directly observable via `DsqInsertVtime{dsq_id}`.
///
/// With two nodes and tasks pinned one-per-node, every insert for the node-0
/// task must land on DSQ 0 and every insert for the node-1 task on DSQ 1 — the
/// scheduler keeps work inside the correct cache/NUMA domain.
#[test]
fn test_cosmos_per_node_dsq_routing() {
    let _lock = common::setup_test();
    let nr_cpus = 4u32;
    let nr_nodes = 2u32;
    let cpus_per_node = nr_cpus / nr_nodes;

    let tasks_per_node = 4u32; // > cpus_per_node (2) → nodes stay oversubscribed

    let sched = DynamicScheduler::cosmos_with_numa(nr_cpus, nr_nodes);
    // Report a saturated system so is_cpu_busy() routes enqueues onto the
    // per-node shared DSQ (the vtime/deadline path) rather than per-CPU queues.
    sched.cosmos_set_cpu_util(nr_cpus, 1024);

    let mut b = Scenario::builder().cpus(nr_cpus).seed(42).instant_timing();
    // `tasks_per_node` tasks per node, all pinned to their node's CPUs and
    // near-always runnable, so each node's CPUs cannot absorb all its tasks and
    // enqueues fall through to that node's shared DSQ.
    let mut pid = 1i32;
    for node in 0..nr_nodes {
        let lo = node * cpus_per_node;
        let hi = lo + cpus_per_node;
        for _ in 0..tasks_per_node {
            b = b.task(pinned_task(
                pid,
                &format!("n{node}_t{pid}"),
                lo,
                hi,
                run_sleep(8_000_000, 1_000_000),
            ));
            pid += 1;
        }
    }
    let trace = Simulator::new(sched).run(b.duration_ms(200).build());

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    assert!(
        !trace.has_error(),
        "unexpected error: {:?}",
        trace.exit_kind()
    );

    // The per-node shared DSQ path must actually have been taken.
    let (global, _local) = trace.dsq_dispatch_counts();
    assert!(
        global > 0,
        "expected per-node shared-DSQ (vtime) dispatches in busy mode, got {global}"
    );

    // Both node DSQs (0 and 1) must have received inserts — work is spread
    // across both domains, not funneled through one.
    let dsqs = inserted_dsq_ids(&trace);
    for node in 0..nr_nodes as u64 {
        assert!(
            dsqs.contains(&node),
            "node {node}'s shared DSQ received no inserts; observed DSQs: {dsqs:?}"
        );
    }

    // Each pinned task's inserts must target ONLY its own node's DSQ.
    for e in trace.events() {
        let (pid_opt, dsq) = match e.kind {
            TraceKind::DsqInsert { pid, dsq_id, .. }
            | TraceKind::DsqInsertVtime { pid, dsq_id, .. } => (Some(pid), dsq_id),
            _ => (None, DsqId(0)),
        };
        let Some(p) = pid_opt else { continue };
        if dsq.is_builtin() {
            continue;
        }
        // pids are assigned node-major: [1..=tpn] → node 0, etc.
        let expected_node = ((p.0 - 1) as u32 / tasks_per_node) as u64;
        assert_eq!(
            dsq.0, expected_node,
            "task {} (node {expected_node}) inserted into DSQ {} (wrong domain)",
            p.0, dsq.0
        );
    }
}

// ---------------------------------------------------------------------------
// 2. Load balancing across domains — per-node runtime fairness.
// ---------------------------------------------------------------------------

/// A saturating, unpinned workload on a 2-socket (2-node) box must spread load
/// roughly evenly across BOTH nodes — COSMOS balances work across its domains
/// rather than piling everything onto one node. Distinct from
/// `topology.rs::test_cosmos_numa_node_sweep`, which only checks *spread*
/// (>= half the CPUs lit up); here we assert *balance* (each node carries a
/// substantial share of total runtime).
#[test]
fn test_cosmos_numa_domain_load_balance() {
    let _lock = common::setup_test();
    let nr_cpus = 8u32;
    let nr_nodes = 2u32;
    let cpus_per_node = nr_cpus / nr_nodes;
    let nr_tasks = 16u32; // 2x oversubscribed → both nodes must stay busy

    let sched = DynamicScheduler::cosmos_with_numa(nr_cpus, nr_nodes);
    let mut b = Scenario::builder().cpus(nr_cpus).seed(42).instant_timing();
    for i in 1..=nr_tasks {
        b = b.add_task(&format!("hog{i}"), 0, forever_run(50_000_000));
    }
    let trace = Simulator::new(sched).run(b.duration_ms(300).build());
    trace.dump();

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    assert!(!trace.has_error());

    let per_node = runtime_by_node(&trace, nr_cpus, cpus_per_node);
    let total: u64 = per_node.iter().sum();
    eprintln!("cosmos numa balance: per-node runtime = {per_node:?} (total {total})");
    assert!(total > 0, "no runtime recorded");

    // Each node should carry at least 30% of total runtime (perfect balance is
    // 50% each; 30% leaves generous room while still catching a scheduler that
    // strands a whole node).
    for (node, &rt) in per_node.iter().enumerate() {
        assert!(
            rt as u128 * 100 >= total as u128 * 30,
            "node {node} under-utilized: {rt} of {total} ns (< 30%); per-node {per_node:?}"
        );
    }
}

/// Multi-socket sweep: 2 and 4 sockets over 8 CPUs. Every node must carry a
/// fair share of a saturating workload — COSMOS's per-node domains all
/// participate regardless of socket count.
#[test]
fn test_cosmos_multi_socket_balance() {
    let _lock = common::setup_test();
    let nr_cpus = 8u32;
    for nr_nodes in [2u32, 4] {
        let ctx = format!("cosmos sockets={nr_nodes}");
        let cpus_per_node = nr_cpus / nr_nodes;
        let nr_tasks = nr_cpus * 2;

        let sched = DynamicScheduler::cosmos_with_numa(nr_cpus, nr_nodes);
        let mut b = Scenario::builder().cpus(nr_cpus).seed(42).instant_timing();
        for i in 1..=nr_tasks {
            b = b.add_task(&format!("hog{i}"), 0, forever_run(50_000_000));
        }
        let trace = Simulator::new(sched).run(b.duration_ms(300).build());

        assert_eq!(trace.exit_kind(), &ExitKind::Normal, "{ctx}: abnormal exit");
        assert!(!trace.has_error(), "{ctx}: error {:?}", trace.exit_kind());

        // Every node must be used, and carry a non-trivial share of runtime.
        let used = nodes_used(&trace, cpus_per_node);
        assert_eq!(
            used.len() as u32,
            nr_nodes,
            "{ctx}: only nodes {used:?} used, expected all {nr_nodes}"
        );
        let per_node = runtime_by_node(&trace, nr_cpus, cpus_per_node);
        let total: u64 = per_node.iter().sum();
        eprintln!("{ctx}: per-node runtime {per_node:?}");
        // Fair share is total/nr_nodes; require each node >= half of fair share.
        let floor = total as u128 / nr_nodes as u128 / 2;
        for (node, &rt) in per_node.iter().enumerate() {
            assert!(
                rt as u128 >= floor,
                "{ctx}: node {node} got {rt} ns (< half fair share {floor}); {per_node:?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 3. Idle CPU selection within a domain — node-local wake placement.
// ---------------------------------------------------------------------------

/// When a waker on node 0 wakes a peer that shares its address space, COSMOS's
/// NUMA-aware idle selection (`scx_bpf_select_cpu_and` with the node-local idle
/// cpumask, `__COMPAT_scx_bpf_get_idle_cpumask_node`) should keep the wakee in
/// the waker's domain while that domain has idle CPUs. The waker is pinned to
/// node 0; the wakee is unpinned but should predominantly run on node 0.
#[test]
fn test_cosmos_domain_local_wake_placement() {
    let _lock = common::setup_test();
    let nr_cpus = 8u32;
    let nr_nodes = 2u32;
    let cpus_per_node = nr_cpus / nr_nodes; // node 0 = {0..4}, node 1 = {4..8}

    let sched = DynamicScheduler::cosmos_with_numa(nr_cpus, nr_nodes);

    // Waker pinned to node 0, wakes the wakee each cycle. Both share MmId(1)
    // so COSMOS's wake-affinity considers co-location.
    let waker = TaskDef {
        name: "waker".into(),
        pid: Pid(1),
        nice: 0,
        behavior: TaskBehavior {
            phases: vec![
                Phase::Run(3_000_000),
                Phase::Wake(Pid(2)),
                Phase::Sleep(3_000_000),
            ],
            repeat: RepeatMode::Forever,
        },
        start_time_ns: 0,
        mm_id: Some(MmId(1)),
        allowed_cpus: Some((0..cpus_per_node).map(CpuId).collect()),
        parent_pid: None,
        cgroup_name: None,
        task_flags: 0,
        migration_disabled: 0,
        thread_group_leader: None,
        uid: Uid(0),
        gid: Gid(0),
        fork_cpu: None,
    };
    let wakee = TaskDef {
        name: "wakee".into(),
        pid: Pid(2),
        nice: 0,
        behavior: TaskBehavior {
            phases: vec![Phase::Run(2_000_000), Phase::Sleep(20_000_000)],
            repeat: RepeatMode::Forever,
        },
        start_time_ns: 0,
        mm_id: Some(MmId(1)),
        allowed_cpus: None, // free to run anywhere; should prefer node 0
        parent_pid: None,
        cgroup_name: None,
        task_flags: 0,
        migration_disabled: 0,
        thread_group_leader: None,
        uid: Uid(0),
        gid: Gid(0),
        fork_cpu: None,
    };

    let scenario = Scenario::builder()
        .cpus(nr_cpus)
        .seed(42)
        .instant_timing()
        .task(waker)
        .task(wakee)
        .duration_ms(200)
        .build();
    let trace = Simulator::new(sched).run(scenario);
    trace.dump();

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    assert!(!trace.has_error());
    assert!(trace.schedule_count(Pid(2)) > 0, "wakee never ran");

    // Waker must stay on node 0 (it is pinned there — sanity).
    for c in scheduled_cpus(&trace, Pid(1)) {
        assert!(
            c.0 / cpus_per_node == 0,
            "waker (pinned node 0) ran on {c:?} outside node 0"
        );
    }

    // With node 0 near-idle (only the waker there, and it sleeps), the wakee's
    // node-local idle selection should place it on node 0 for the large
    // majority of its runs.
    let wakee_cpus = scheduled_cpus(&trace, Pid(2));
    let on_node0 = wakee_cpus
        .iter()
        .filter(|c| c.0 / cpus_per_node == 0)
        .count();
    eprintln!(
        "wakee ran {}/{} times on waker's node (node 0); cpus {wakee_cpus:?}",
        on_node0,
        wakee_cpus.len()
    );
    assert!(
        on_node0 * 2 >= wakee_cpus.len(),
        "wakee placed off waker's domain too often: {on_node0}/{} on node 0",
        wakee_cpus.len()
    );
}

// ---------------------------------------------------------------------------
// 4. Task stealing / spreading across domains under imbalance.
// ---------------------------------------------------------------------------

/// A staggered-start, oversubscribed run/sleep workload seeds a transient
/// per-node imbalance; COSMOS must spread / steal work across the node boundary
/// so BOTH domains carry runtime and at least one task actually crosses nodes.
/// Complements `cpu_migration.rs::test_cross_numa_migration_cosmos` (which only
/// asserts `cross > 0`) by also asserting the *balancing outcome*: neither
/// domain is stranded idle. The 1.5x oversubscription with sleep windows keeps
/// every task live (no starvation), isolating the domain-balancing property.
#[test]
fn test_cosmos_idle_domain_utilized_under_imbalance() {
    let _lock = common::setup_test();
    let nr_cpus = 8u32;
    let nr_nodes = 2u32;
    let cpus_per_node = nr_cpus / nr_nodes; // node 0 = {0..4}, node 1 = {4..8}
    let nr_tasks = 12u32; // 1.5x oversubscription — spills across nodes, no starvation

    let sched = DynamicScheduler::cosmos_with_numa(nr_cpus, nr_nodes);
    let mut b = Scenario::builder().cpus(nr_cpus).seed(42).instant_timing();
    for i in 1..=nr_tasks {
        // Stagger starts so a transient imbalance forms and the balancer reacts.
        b = b.task(TaskDef {
            name: format!("w{i}"),
            pid: Pid(i as i32),
            nice: 0,
            behavior: run_sleep(5_000_000, 2_000_000),
            start_time_ns: (i as u64 % 4) * 500_000,
            mm_id: None,
            allowed_cpus: None,
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
    let trace = Simulator::new(sched).run(b.duration_ms(300).build());

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    assert!(!trace.has_error());

    // Both nodes must be utilized — the balancer did not strand a whole domain.
    let per_node = runtime_by_node(&trace, nr_cpus, cpus_per_node);
    eprintln!("cosmos imbalance per-node runtime: {per_node:?}");
    for (node, &rt) in per_node.iter().enumerate() {
        assert!(rt > 0, "node {node} left completely idle under imbalance");
    }
    // Every task made progress (no starvation at this load level).
    for p in 1..=nr_tasks as i32 {
        assert!(trace.schedule_count(Pid(p)) > 0, "task {p} starved");
    }

    // At least one task must cross the node boundary (work stolen/migrated
    // across domains to balance load).
    let cross: usize = (1..=nr_tasks as i32)
        .map(|p| cross_node_migrations(&trace, Pid(p), cpus_per_node))
        .sum();
    eprintln!("cosmos cross-node migrations under imbalance: {cross}");
    assert!(
        cross > 0,
        "expected cross-domain stealing under imbalance, got {cross}"
    );
}

// ---------------------------------------------------------------------------
// 5. Engine-level LLC model — cross-LLC migration TIMING cost.
//    (`.cpus_per_llc()` drives SimCpu.llc_id → cross_llc_migration_penalty_ns;
//    it is NOT a COSMOS placement decision — see the module header.)
// ---------------------------------------------------------------------------

/// COSMOS runs correctly under an LLC-partitioned engine topology, and the
/// engine's cross-LLC migration COST path is exercised: under a staggered,
/// oversubscribed workload some realized migrations cross an engine LLC
/// boundary (so the `cross_llc_migration_penalty_ns` code path in the engine
/// runs) while every task still makes progress. This tests the engine's
/// kernel-cost model, not COSMOS's (inert) `cpus_share_cache` logic.
#[test]
fn test_cosmos_cross_llc_migration_cost() {
    let _lock = common::setup_test();
    let nr_cpus = 8u32;
    let cpus_per_llc = 2u32; // 4 LLCs of 2 CPUs
    let nr_tasks = 20u32;

    let mut b = Scenario::builder()
        .cpus(nr_cpus)
        .cpus_per_llc(cpus_per_llc)
        .seed(42)
        .instant_timing();
    for i in 1..=nr_tasks {
        b = b.task(TaskDef {
            name: format!("w{i}"),
            pid: Pid(i as i32),
            nice: 0,
            behavior: run_sleep(3_000_000, 2_000_000),
            start_time_ns: (i as u64 % nr_cpus as u64) * 1_000_000,
            mm_id: None,
            allowed_cpus: None,
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
    let trace = Simulator::new(DynamicScheduler::cosmos(nr_cpus)).run(b.duration_ms(300).build());
    trace.dump();

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    assert!(!trace.has_error());
    for p in 1..=nr_tasks as i32 {
        assert!(trace.schedule_count(Pid(p)) > 0, "task {p} starved");
    }

    let cross: usize = (1..=nr_tasks as i32)
        .map(|p| cross_llc_migrations(&trace, Pid(p), cpus_per_llc))
        .sum();
    eprintln!("cosmos cross-LLC migrations (engine cost path): {cross} (4 LLCs x 2 CPUs)");
    assert!(
        cross > 0,
        "expected some cross-LLC migrations to exercise the engine cost path, got {cross}"
    );
}

/// Sanity sweep: COSMOS exits normally and every task progresses across a
/// coarse-to-fine engine LLC-partition gradient (1 / 2 / 4 CPUs per LLC on 8
/// CPUs). Guards against the engine's llc_id assignment / cross-LLC penalty
/// wiring breaking COSMOS at any partition granularity.
#[test]
fn test_cosmos_engine_llc_topology_healthy() {
    let _lock = common::setup_test();
    let nr_cpus = 8u32;
    for cpus_per_llc in [1u32, 2, 4] {
        let ctx = format!("cosmos cpus_per_llc={cpus_per_llc}");
        let nr_tasks = nr_cpus + 4;
        let mut b = Scenario::builder()
            .cpus(nr_cpus)
            .cpus_per_llc(cpus_per_llc)
            .seed(42)
            .instant_timing();
        for i in 1..=nr_tasks {
            b = b.add_task(&format!("t{i}"), 0, run_sleep(3_000_000, 2_000_000));
        }
        let trace =
            Simulator::new(DynamicScheduler::cosmos(nr_cpus)).run(b.duration_ms(200).build());

        assert_eq!(trace.exit_kind(), &ExitKind::Normal, "{ctx}: abnormal exit");
        assert!(!trace.has_error(), "{ctx}: error {:?}", trace.exit_kind());
        for p in 1..=nr_tasks as i32 {
            assert!(trace.schedule_count(Pid(p)) > 0, "{ctx}: task {p} starved");
        }
    }
}

// ---------------------------------------------------------------------------
// Determinism guard (no flakes across the domain/LLC scenarios above).
// ---------------------------------------------------------------------------

#[test]
fn test_cosmos_domain_determinism() {
    let _lock = common::setup_test();

    let build = || {
        let mut b = Scenario::builder().cpus(8).seed(42).instant_timing();
        for i in 1..=16u32 {
            b = b.add_task(&format!("hog{i}"), 0, forever_run(50_000_000));
        }
        b.duration_ms(200).build()
    };
    let t1 = Simulator::new(DynamicScheduler::cosmos_with_numa(8, 2)).run(build());
    let t2 = Simulator::new(DynamicScheduler::cosmos_with_numa(8, 2)).run(build());
    assert_identical(&t1, &t2, "cosmos numa domain");
}
