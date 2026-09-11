//! Tests for CPU / domain task MIGRATION under load (lavd, cosmos).
//!
//! Distinct from the topology *sweep* tests in `topology.rs` (which assert
//! spread/progress) and the cgroup-migration tests in `cgroup_hierarchy.rs`
//! (task↔cgroup, not CPU↔CPU). Here we actually MEASURE migration: a task is
//! considered migrated when consecutive `TaskScheduled` events for it land on
//! different CPUs, and the scheduler's placement *decision* is read from
//! `SelectTaskRq { prev_cpu, selected_cpu }`. scxsim emits no dedicated
//! migration event, so the test owns the CPU→LLC/node arithmetic
//! (`cpu / cpus_per_llc`, `cpu / cpus_per_node`).
//!
//! Dimensions:
//! 1. Tasks migrating between CPUs under load imbalance (lavd + cosmos).
//! 2. Migration across LLC boundaries (cpus_per_llc).
//! 3. Migration across NUMA nodes (cosmos_with_numa).
//! 4. Migration under high contention (many tasks, few CPUs).
//! 5. Per-scheduler policy: lavd `mig_delta_pct` aggressiveness knob, and a
//!    pinned task that must NEVER migrate (negative control).
//!
//! Deterministic serial engine, fixed seed + `instant_timing`; assertions on
//! observable trace events only.

use scx_simulator::*;

mod common;

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

/// Ordered list of CPUs a task was scheduled on (adjacent diffs == migrations).
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

/// Number of realized migrations for a task (consecutive different CPUs).
fn migration_count(trace: &Trace, pid: Pid) -> usize {
    scheduled_cpus(trace, pid)
        .windows(2)
        .filter(|w| w[0] != w[1])
        .count()
}

/// Realized migrations that cross an LLC boundary, given `cpus_per_llc`.
fn cross_llc_migrations(trace: &Trace, pid: Pid, cpus_per_llc: u32) -> usize {
    scheduled_cpus(trace, pid)
        .windows(2)
        .filter(|w| w[0] != w[1] && w[0].0 / cpus_per_llc != w[1].0 / cpus_per_llc)
        .count()
}

/// Realized migrations that cross a NUMA-node boundary, given `cpus_per_node`.
fn cross_node_migrations(trace: &Trace, pid: Pid, cpus_per_node: u32) -> usize {
    scheduled_cpus(trace, pid)
        .windows(2)
        .filter(|w| w[0] != w[1] && w[0].0 / cpus_per_node != w[1].0 / cpus_per_node)
        .count()
}

/// Total realized migrations across all pids in `1..=nr_tasks`.
fn total_migrations(trace: &Trace, nr_tasks: u32) -> usize {
    (1..=nr_tasks as i32)
        .map(|p| migration_count(trace, Pid(p)))
        .sum()
}

/// Scheduler placement decisions (wakeup path) that chose a different CPU.
fn select_cpu_moves(trace: &Trace, pid: Pid) -> usize {
    trace
        .events()
        .iter()
        .filter(|e| {
            matches!(e.kind,
                TraceKind::SelectTaskRq { pid: p, prev_cpu, selected_cpu }
                if p == pid && prev_cpu != selected_cpu)
        })
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

/// Imbalance generator: `nr_tasks` unpinned run/sleep tasks on `nr_cpus` CPUs.
/// Staggered start times seed an imbalance; the run/sleep cycles force repeated
/// placement decisions so the load balancer moves tasks around.
fn imbalance_scenario(
    nr_cpus: u32,
    nr_tasks: u32,
    cpus_per_llc: u32,
    duration_ms: u64,
) -> Scenario {
    let mut b = Scenario::builder().cpus(nr_cpus).seed(42).instant_timing();
    if cpus_per_llc > 0 {
        b = b.cpus_per_llc(cpus_per_llc);
    }
    for i in 1..=nr_tasks {
        b = b.task(TaskDef {
            name: format!("w{i}"),
            pid: Pid(i as i32),
            nice: 0,
            behavior: run_sleep(3_000_000, 2_000_000),
            // Stagger starts to create a transient imbalance the balancer reacts to.
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
        });
    }
    b.duration_ms(duration_ms).build()
}

// ---------------------------------------------------------------------------
// 1. Migration between CPUs under load imbalance.
// ---------------------------------------------------------------------------

#[test]
fn test_migration_on_load_imbalance_lavd() {
    let _lock = common::setup_test();
    let nr_tasks = 12u32;
    let scenario = imbalance_scenario(4, nr_tasks, 0, 300);
    let trace = Simulator::new(DynamicScheduler::lavd(4)).run(scenario);
    trace.dump();

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    assert!(
        !trace.has_error(),
        "unexpected error: {:?}",
        trace.exit_kind()
    );

    let total = total_migrations(&trace, nr_tasks);
    eprintln!("lavd imbalance: {total} realized migrations across {nr_tasks} tasks / 4 CPUs");
    // LAVD actively balances/steals across CPUs under imbalance.
    assert!(
        total > 10,
        "expected active load balancing under imbalance, got {total}"
    );
}

/// Per-scheduler policy: on the SAME imbalanced workload, LAVD (active
/// balancing / task stealing) migrates substantially more than COSMOS (which
/// favors `prev_cpu` cache affinity and is migration-averse). Both must run
/// healthy. This verifies the schedulers' migration policies genuinely differ.
#[test]
fn test_migration_policy_lavd_more_than_cosmos() {
    let _lock = common::setup_test();
    let nr_tasks = 12u32;

    let lavd =
        Simulator::new(DynamicScheduler::lavd(4)).run(imbalance_scenario(4, nr_tasks, 0, 300));
    let cosmos =
        Simulator::new(DynamicScheduler::cosmos(4)).run(imbalance_scenario(4, nr_tasks, 0, 300));

    assert_eq!(lavd.exit_kind(), &ExitKind::Normal);
    assert_eq!(cosmos.exit_kind(), &ExitKind::Normal);
    assert!(!lavd.has_error() && !cosmos.has_error());

    let lavd_total = total_migrations(&lavd, nr_tasks);
    let cosmos_total = total_migrations(&cosmos, nr_tasks);
    eprintln!(
        "policy: lavd={lavd_total} migrations, cosmos={cosmos_total} migrations (same workload)"
    );
    // Both scheduled every task (no starvation regardless of policy).
    for p in 1..=nr_tasks as i32 {
        assert!(lavd.schedule_count(Pid(p)) > 0 && cosmos.schedule_count(Pid(p)) > 0);
    }
    assert!(
        lavd_total > cosmos_total,
        "expected LAVD (active balancing) to migrate more than COSMOS (cache-affinity): lavd={lavd_total} cosmos={cosmos_total}"
    );
}

// ---------------------------------------------------------------------------
// 2. Migration across LLC boundaries.
// ---------------------------------------------------------------------------

/// 8 CPUs split into 4 LLCs of 2. Under imbalance, at least some migrations
/// must cross an LLC boundary (the scheduler's cross-LLC placement paths run).
#[test]
fn test_cross_llc_migration_lavd() {
    let _lock = common::setup_test();
    let nr_cpus = 8u32;
    let cpus_per_llc = 2u32;
    let nr_tasks = 20u32;
    let scenario = imbalance_scenario(nr_cpus, nr_tasks, cpus_per_llc, 300);
    let trace = Simulator::new(DynamicScheduler::lavd(nr_cpus)).run(scenario);
    trace.dump();

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    assert!(!trace.has_error());

    let cross: usize = (1..=nr_tasks as i32)
        .map(|p| cross_llc_migrations(&trace, Pid(p), cpus_per_llc))
        .sum();
    let total = total_migrations(&trace, nr_tasks);
    eprintln!("lavd cross-LLC: {cross} cross-LLC of {total} total migrations (4 LLCs x 2 CPUs)");
    assert!(total > 0, "expected migrations, got {total}");
    assert!(
        cross > 0,
        "expected at least one cross-LLC migration, got {cross}"
    );
}

// ---------------------------------------------------------------------------
// 3. Migration across NUMA nodes (cosmos).
// ---------------------------------------------------------------------------

/// 8 CPUs, 2 NUMA nodes of 4. Unpinned tasks under imbalance must produce at
/// least one cross-node migration — complements `topology.rs`'s
/// `test_cosmos_numa_per_node_affinity`, which asserts that *pinned* tasks never
/// leave their node.
#[test]
fn test_cross_numa_migration_cosmos() {
    let _lock = common::setup_test();
    let nr_cpus = 8u32;
    let nr_nodes = 2u32;
    let cpus_per_node = nr_cpus / nr_nodes;
    let nr_tasks = 20u32;
    let scenario = imbalance_scenario(nr_cpus, nr_tasks, 0, 300);
    let sched = DynamicScheduler::cosmos_with_numa(nr_cpus, nr_nodes);
    let trace = Simulator::new(sched).run(scenario);
    trace.dump();

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    assert!(!trace.has_error());

    let cross: usize = (1..=nr_tasks as i32)
        .map(|p| cross_node_migrations(&trace, Pid(p), cpus_per_node))
        .sum();
    let total = total_migrations(&trace, nr_tasks);
    eprintln!(
        "cosmos cross-NUMA: {cross} cross-node of {total} total migrations (2 nodes x 4 CPUs)"
    );
    assert!(total > 0, "expected migrations, got {total}");
    assert!(
        cross > 0,
        "expected at least one cross-NUMA migration, got {cross}"
    );
}

// ---------------------------------------------------------------------------
// 4. Migration under high contention.
// ---------------------------------------------------------------------------

/// Many always-runnable hogs on few CPUs. Both schedulers must stay healthy
/// with no task starved. LAVD, which actively balances, is additionally
/// required to migrate tasks; COSMOS favors cache affinity and may keep hogs
/// pinned, so it is only required to stay healthy (its policy, not a bug).
#[test]
fn test_migration_under_high_contention() {
    let _lock = common::setup_test();
    let nr_cpus = 4u32;
    let nr_tasks = 24u32;
    for (name, sched, require_migration) in [
        ("lavd", DynamicScheduler::lavd(nr_cpus), true),
        ("cosmos", DynamicScheduler::cosmos(nr_cpus), false),
    ] {
        let mut b = Scenario::builder().cpus(nr_cpus).seed(42).instant_timing();
        for i in 1..=nr_tasks {
            b = b.add_task(&format!("hog{i}"), 0, forever_run(50_000_000));
        }
        let scenario = b.duration_ms(400).build();
        let trace = Simulator::new(sched).run(scenario);

        assert_eq!(
            trace.exit_kind(),
            &ExitKind::Normal,
            "{name}: should not stall"
        );
        assert!(!trace.has_error(), "{name}: error {:?}", trace.exit_kind());

        let total = total_migrations(&trace, nr_tasks);
        eprintln!("{name} high-contention: {total} migrations, {nr_tasks} tasks / {nr_cpus} CPUs");
        for p in 1..=nr_tasks as i32 {
            assert!(trace.schedule_count(Pid(p)) > 0, "{name}: task {p} starved");
        }
        if require_migration {
            assert!(
                total > 0,
                "{name}: expected active load balancing under contention, got {total}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 5. Per-scheduler policy checks.
// ---------------------------------------------------------------------------

/// LAVD `mig_delta_pct` is the migration-threshold knob (lower % = migrate more
/// readily). An aggressive (low) threshold should produce at least as many
/// migrations as a conservative (high) one on the same imbalanced workload.
#[test]
fn test_mig_delta_pct_affects_migration_lavd() {
    let _lock = common::setup_test();
    let nr_cpus = 8u32;
    let nr_tasks = 16u32;

    let run = |pct: u8| -> usize {
        let scenario = imbalance_scenario(nr_cpus, nr_tasks, 2, 300);
        let sched = DynamicScheduler::lavd(nr_cpus);
        sched.lavd_configure(true, 0, pct);
        let trace = Simulator::new(sched).run(scenario);
        assert_eq!(
            trace.exit_kind(),
            &ExitKind::Normal,
            "mig_delta_pct={pct} should not stall"
        );
        total_migrations(&trace, nr_tasks)
    };

    let aggressive = run(5); // 5% threshold: migrate readily
    let conservative = run(95); // 95% threshold: migrate rarely
    eprintln!("lavd mig_delta_pct: aggressive(5%)={aggressive} conservative(95%)={conservative}");
    assert!(
        aggressive > 0,
        "aggressive threshold should still migrate, got {aggressive}"
    );
    assert!(
        aggressive >= conservative,
        "aggressive (low pct) should migrate >= conservative (high pct): {aggressive} vs {conservative}"
    );
}

/// A task restricted to a single CPU on a busy multi-CPU box must NEVER migrate
/// (migration_count == 0), even while other tasks are being balanced around it.
/// Measurement-based negative control (complements topology's affinity checks).
#[test]
fn test_pinned_task_never_migrates_lavd() {
    let _lock = common::setup_test();
    let nr_cpus = 4u32;
    let mut b = Scenario::builder().cpus(nr_cpus).seed(42).instant_timing();
    // pid 1: pinned to CPU 2.
    b = b.task(TaskDef {
        name: "pinned".into(),
        pid: Pid(1),
        nice: 0,
        behavior: run_sleep(3_000_000, 3_000_000),
        start_time_ns: 0,
        mm_id: None,
        allowed_cpus: Some(vec![CpuId(2)]),
        parent_pid: None,
        cgroup_name: None,
        task_flags: 0,
        migration_disabled: 0,
        thread_group_leader: None,
        uid: Uid(0),
        gid: Gid(0),
    });
    // Background load so the balancer is active around the pinned task.
    for i in 2..=12u32 {
        b = b.add_task(&format!("bg{i}"), 0, forever_run(50_000_000));
    }
    let trace = Simulator::new(DynamicScheduler::lavd(nr_cpus)).run(b.duration_ms(300).build());
    trace.dump();

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    let pinned_migrations = migration_count(&trace, Pid(1));
    let cpus = scheduled_cpus(&trace, Pid(1));
    eprintln!("pinned task ran on CPUs {cpus:?}; migrations={pinned_migrations}");
    assert!(trace.schedule_count(Pid(1)) > 0, "pinned task never ran");
    assert_eq!(pinned_migrations, 0, "pinned task migrated: CPUs {cpus:?}");
    // And every placement was CPU 2.
    assert!(
        cpus.iter().all(|c| *c == CpuId(2)),
        "pinned task left CPU 2: {cpus:?}"
    );
}

/// The scheduler's wakeup placement decisions actually choose a different CPU
/// than `prev_cpu` under imbalance (SelectTaskRq intent, not just realized).
#[test]
fn test_select_cpu_moves_under_imbalance_lavd() {
    let _lock = common::setup_test();
    let nr_tasks = 16u32;
    let scenario = imbalance_scenario(4, nr_tasks, 0, 300);
    let trace = Simulator::new(DynamicScheduler::lavd(4)).run(scenario);

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    let moves: usize = (1..=nr_tasks as i32)
        .map(|p| select_cpu_moves(&trace, Pid(p)))
        .sum();
    eprintln!("lavd select_cpu moves (prev_cpu != selected_cpu): {moves}");
    assert!(
        moves > 0,
        "expected select_cpu to relocate waking tasks, got {moves}"
    );
}

// ---------------------------------------------------------------------------
// Determinism guard (no flakes).
// ---------------------------------------------------------------------------

#[test]
fn test_migration_determinism() {
    let _lock = common::setup_test();
    let t1 = Simulator::new(DynamicScheduler::lavd(8)).run(imbalance_scenario(8, 20, 2, 300));
    let t2 = Simulator::new(DynamicScheduler::lavd(8)).run(imbalance_scenario(8, 20, 2, 300));
    assert_identical(&t1, &t2, "lavd migration");

    let c1 = Simulator::new(DynamicScheduler::cosmos_with_numa(8, 2))
        .run(imbalance_scenario(8, 20, 0, 300));
    let c2 = Simulator::new(DynamicScheduler::cosmos_with_numa(8, 2))
        .run(imbalance_scenario(8, 20, 0, 300));
    assert_identical(&c1, &c2, "cosmos migration");
}
