//! Mitosis cell-to-cell task-migration and multi-cell topology tests.
//!
//! ## Substrate reality (read before extending)
//!
//! Upstream deleted the BPF cell allocator and the `update_timer`
//! reconfiguration path (scx commits `0f579b78` / `b62f1bae`). At this
//! checkout's scx pin, `scx_mitosis` has **no BPF timer and no `.tick`**;
//! cell membership is applied wholesale by a userspace-driven
//! `apply_cell_config` (`SEC("syscall")`) program. The simulator has **no
//! hook that invokes `apply_cell_config`**, and forces
//! `cpu_controller_disabled = true` (`schedulers/mitosis/wrapper.c`), which
//! makes `mitosis_cgroup_move` / `mitosis_cgroup_init` early-return. The net
//! effect: every cgroup stays bound to cell 0 (root) with a full cpumask, so
//! **cell cpuset confinement, cross-cell rebalancing, cell split/merge, and
//! migration-cost accounting are NOT modeled today** (tracked by the epic
//! `mb sim-010a1` and `mb sim-c923d6`; the stale `sim-b7d70` TODOs in
//! `mitosis.rs` describe the removed BPF-timer design).
//!
//! Consequently this file does NOT assert cpuset confinement or daemon-style
//! rebalancing (that would require stubbing behavior the scheduler no longer
//! performs). Instead it covers the one migration mechanism the simulator
//! genuinely models — cgroup membership migration (`ops.cgroup_move` +
//! engine dequeue/re-enqueue, the `mb sim-03229` path) — and multi-cell
//! topology setup, asserting real observable behavior. A correct-behavior
//! guard for the queued-migrant case is `#[ignore]`d against the bug it
//! currently trips (`mb sim-89948f`).
//!
//! Existing complementary coverage in `mitosis.rs`:
//! `test_mitosis_dynamic_cell_lifecycle` (create/migrate/destroy lifecycle),
//! `test_mitosis_demand_rebalancing` (load-imbalance characterization),
//! `test_mitosis_cpuset_change_detection`, `test_mitosis_basic_cell_isolation`.
//! These tests focus specifically on migration *forward progress* and are
//! written to stay green as the substrate is fixed.

use scx_simulator::*;
use std::collections::HashSet;

mod common;

/// A never-ending CPU-bound task placed (by cgroup) in `cgroup`.
fn forever_hog_in(name: &str, pid: i32, cgroup: &str, run_ns: u64) -> TaskDef {
    TaskDef {
        name: name.into(),
        pid: Pid(pid),
        nice: 0,
        behavior: TaskBehavior {
            phases: vec![Phase::Run(run_ns)],
            repeat: RepeatMode::Forever,
        },
        start_time_ns: 0,
        mm_id: None,
        allowed_cpus: None,
        parent_pid: None,
        cgroup_name: Some(cgroup.into()),
        task_flags: 0,
        migration_disabled: 0,
    }
}

/// Count `TaskScheduled` events for `pid` in the half-open window `[lo, hi)`.
fn sched_count_in(trace: &Trace, pid: Pid, lo: u64, hi: u64) -> usize {
    trace
        .events()
        .iter()
        .filter(|e| {
            e.time_ns >= lo
                && e.time_ns < hi
                && matches!(e.kind, TraceKind::TaskScheduled { pid: p } if p == pid)
        })
        .count()
}

/// Set of CPUs `pid` ran on within `[lo, hi)`.
fn cpus_in_window(trace: &Trace, pid: Pid, lo: u64, hi: u64) -> HashSet<CpuId> {
    trace
        .events()
        .iter()
        .filter(|e| {
            e.time_ns >= lo
                && e.time_ns < hi
                && matches!(e.kind, TraceKind::TaskScheduled { pid: p } if p == pid)
        })
        .map(|e| e.cpu)
        .collect()
}

// ---------------------------------------------------------------------------
// Migration forward-progress (the sim-03229 cgroup_move + re-enqueue path)
// ---------------------------------------------------------------------------

/// An UNCONTENDED task migrated from one cgroup cell to another keeps making
/// forward progress across the migration boundary — it is still scheduled and
/// still accrues runtime well after the move, and the run exits normally.
///
/// This is the working half of the `mb sim-03229` fix (migrant that is
/// running / has an idle CPU, not queued). It restores the post-migration
/// assertion that `test_mitosis_dynamic_cell_lifecycle` had to `TODO` out
/// before the re-enqueue path existed.
#[test]
fn test_uncontended_cross_cell_migration_forward_progress() {
    let _lock = common::setup_test();
    let nr = 4u32;
    // 3 tasks on 4 CPUs → the migrant always has an idle CPU (never queued).
    let scenario = Scenario::builder()
        .cpus(nr)
        .cgroup("cell_a", &[CpuId(0), CpuId(1)])
        .cgroup("cell_b", &[CpuId(2), CpuId(3)])
        .task(forever_hog_in("migrant", 1, "cell_a", 5_000_000))
        .task(forever_hog_in("a_bg", 2, "cell_a", 5_000_000))
        .task(forever_hog_in("b_bg", 3, "cell_b", 5_000_000))
        .cgroup_migrate(Pid(1), "cell_a", "cell_b", 150_000_000)
        .duration_ms(400)
        .build();

    let trace = Simulator::new(DynamicScheduler::mitosis(nr)).run(scenario);

    assert!(
        matches!(trace.exit_kind(), ExitKind::Normal),
        "simulation did not exit normally: {:?}",
        trace.exit_kind()
    );

    // Scheduled both before and long after the migration.
    let pre = sched_count_in(&trace, Pid(1), 0, 150_000_000);
    let post = sched_count_in(&trace, Pid(1), 200_000_000, 400_000_000);
    assert!(pre > 0, "migrant was never scheduled before migration");
    assert!(
        post > 0,
        "migrant made NO forward progress after migration (stranded): \
         pre={pre}, post={post}"
    );

    // Ran for the large majority of the 400ms window (it owned a CPU).
    assert!(
        trace.total_runtime(Pid(1)) > 300_000_000,
        "uncontended migrant should run most of the sim, got {}ns",
        trace.total_runtime(Pid(1))
    );
}

/// Migrating one task between cells must NOT disturb the bystander tasks:
/// every non-migrant task keeps being scheduled after the migration and the
/// run exits normally. (Holds regardless of the migrant's own fate, so this
/// stays green even while `mb sim-89948f` is open.)
#[test]
fn test_cell_migration_does_not_strand_bystanders() {
    let _lock = common::setup_test();
    let nr = 4u32;
    let mut b = Scenario::builder()
        .cpus(nr)
        .cgroup("cell_a", &[CpuId(0), CpuId(1)])
        .cgroup("cell_b", &[CpuId(2), CpuId(3)]);
    for i in 1..=6 {
        let cell = if i <= 4 { "cell_a" } else { "cell_b" };
        b = b.task(forever_hog_in(&format!("t{i}"), i, cell, 4_000_000));
    }
    // Migrate pid=1 from cell_a to cell_b partway through.
    let scenario = b
        .cgroup_migrate(Pid(1), "cell_a", "cell_b", 150_000_000)
        .duration_ms(400)
        .build();

    let trace = Simulator::new(DynamicScheduler::mitosis(nr)).run(scenario);

    assert!(
        matches!(trace.exit_kind(), ExitKind::Normal),
        "simulation did not exit normally: {:?}",
        trace.exit_kind()
    );

    // Every bystander (pid 2..=6) keeps running after the migration event.
    for pid in 2..=6 {
        let post = sched_count_in(&trace, Pid(pid), 200_000_000, 400_000_000);
        assert!(
            post > 0,
            "bystander pid={pid} was disturbed by the migration (no post-migration schedules)"
        );
    }
}

/// Repeatedly migrating an uncontended task back and forth between two cells
/// must not crash, error, or lose the task: it keeps making forward progress
/// through every migration and the run exits normally.
#[test]
fn test_repeated_cross_cell_migration_stable() {
    let _lock = common::setup_test();
    let nr = 4u32;
    // 2 tasks on 4 CPUs → migrant always has an idle CPU.
    let scenario = Scenario::builder()
        .cpus(nr)
        .cgroup("cell_a", &[CpuId(0), CpuId(1)])
        .cgroup("cell_b", &[CpuId(2), CpuId(3)])
        .task(forever_hog_in("migrant", 1, "cell_a", 5_000_000))
        .task(forever_hog_in("bg", 2, "cell_b", 5_000_000))
        .cgroup_migrate(Pid(1), "cell_a", "cell_b", 80_000_000)
        .cgroup_migrate(Pid(1), "cell_b", "cell_a", 160_000_000)
        .cgroup_migrate(Pid(1), "cell_a", "cell_b", 240_000_000)
        .cgroup_migrate(Pid(1), "cell_b", "cell_a", 320_000_000)
        .duration_ms(400)
        .build();

    let trace = Simulator::new(DynamicScheduler::mitosis(nr)).run(scenario);

    assert!(
        matches!(trace.exit_kind(), ExitKind::Normal),
        "simulation did not exit normally after repeated migration: {:?}",
        trace.exit_kind()
    );
    // Forward progress in each inter-migration window.
    for (lo, hi) in [
        (0u64, 80_000_000u64),
        (80_000_000, 160_000_000),
        (160_000_000, 240_000_000),
        (240_000_000, 320_000_000),
        (320_000_000, 400_000_000),
    ] {
        assert!(
            sched_count_in(&trace, Pid(1), lo, hi) > 0,
            "migrant stalled in window [{lo}, {hi})"
        );
    }
}

// ---------------------------------------------------------------------------
// Multi-cell topology
// ---------------------------------------------------------------------------

/// Multi-cell topology set-up (several cgroup cells over an LLC-partitioned
/// machine) runs correctly: every task in every cell gets scheduled
/// repeatedly and the run exits normally.
///
/// This characterizes current behavior — it does NOT assert cpuset
/// confinement, because cells are not partitioned in the simulator today
/// (see the module header and `mb sim-010a1`). It is a smoke/regression guard
/// that a nontrivial multi-cell + topology configuration is handled without
/// error, and a natural place to strengthen into a confinement assertion once
/// the substrate supports it.
#[test]
fn test_multi_cell_topology_all_tasks_run() {
    let _lock = common::setup_test();
    let nr = 8u32;
    // Two LLC domains of 4 CPUs each; two cells aligned to them.
    let scenario = Scenario::builder()
        .cpus(nr)
        .cpus_per_llc(4)
        .cgroup("cell_lo", &[CpuId(0), CpuId(1), CpuId(2), CpuId(3)])
        .cgroup("cell_hi", &[CpuId(4), CpuId(5), CpuId(6), CpuId(7)])
        .task(forever_hog_in("lo1", 1, "cell_lo", 5_000_000))
        .task(forever_hog_in("lo2", 2, "cell_lo", 5_000_000))
        .task(forever_hog_in("lo3", 3, "cell_lo", 5_000_000))
        .task(forever_hog_in("hi1", 4, "cell_hi", 5_000_000))
        .task(forever_hog_in("hi2", 5, "cell_hi", 5_000_000))
        .task(forever_hog_in("hi3", 6, "cell_hi", 5_000_000))
        .duration_ms(300)
        .build();

    let trace = Simulator::new(DynamicScheduler::mitosis(nr)).run(scenario);

    assert!(
        matches!(trace.exit_kind(), ExitKind::Normal),
        "multi-cell topology run did not exit normally: {:?}",
        trace.exit_kind()
    );
    for pid in 1..=6 {
        assert!(
            trace.schedule_count(Pid(pid)) >= 5,
            "task pid={pid} in a multi-cell topology was barely scheduled ({})",
            trace.schedule_count(Pid(pid))
        );
    }
    // Sanity: the workload actually spread across the machine (≥ 4 CPUs used).
    let cpus: HashSet<CpuId> = (1..=6)
        .flat_map(|p| cpus_in_window(&trace, Pid(p), 0, 300_000_000))
        .collect();
    assert!(
        cpus.len() >= 4,
        "multi-cell workload used only {} CPUs",
        cpus.len()
    );
}

// ---------------------------------------------------------------------------
// Correct-behavior guard for a currently-blocked case (mb sim-89948f)
// ---------------------------------------------------------------------------

/// CORRECT-BEHAVIOR GUARD (currently failing → `#[ignore]`d).
///
/// A task that is runnable-but-QUEUED (contended) at the moment of
/// `cgroup_migrate` should keep making forward progress after the move, just
/// like the uncontended case. Today it is STRANDED: post-migration schedule
/// count is 0 while co-resident tasks keep running (`mb sim-89948f`, the
/// `mb sim-03229` scenario recurring for the mitosis `cpu_controller_disabled`
/// path). Un-`#[ignore]` this when sim-89948f is fixed.
#[test]
#[ignore = "mb sim-89948f: queued task strands after cgroup_migrate under mitosis; \
            un-ignore when the re-enqueue path is fixed"]
fn test_queued_task_survives_cell_migration() {
    let _lock = common::setup_test();
    let nr = 4u32;
    // 8 tasks in cell_a on 4 CPUs → pid=1 is frequently queued.
    let mut b = Scenario::builder()
        .cpus(nr)
        .cgroup("cell_a", &[CpuId(0), CpuId(1)])
        .cgroup("cell_b", &[CpuId(2), CpuId(3)]);
    for i in 1..=8 {
        b = b.task(forever_hog_in(&format!("t{i}"), i, "cell_a", 4_000_000));
    }
    let scenario = b
        .cgroup_migrate(Pid(1), "cell_a", "cell_b", 150_000_000)
        .duration_ms(400)
        .build();

    let trace = Simulator::new(DynamicScheduler::mitosis(nr)).run(scenario);

    let post = sched_count_in(&trace, Pid(1), 150_000_000, 400_000_000);
    assert!(
        post > 0,
        "queued migrant made no forward progress after cgroup_migrate (stranded)"
    );
}
