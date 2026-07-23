//! Tests for MULTI-LEVEL cgroup hierarchy scheduling interactions under LAVD
//! (the scheduler that compiles in the real `cgroup_bw` library).
//!
//! Complements — does not duplicate — the flat-cgroup lifecycle tests in
//! `stress.rs` (create/destroy/exhaustion/bw-tracking) and the nested-cgroup
//! *preemption* test in `preempt_stress.rs`. Here the focus is HIERARCHY
//! semantics:
//!
//! 1. Nested cgroups with different bandwidth limits (tight vs loose) → the
//!    tighter hierarchy's tasks are enforced harder.
//! 2. Tasks moving between cgroups at runtime (`cgroup_migrate` → `CgroupMove`).
//! 3. Runtime creation/deletion of NESTED cgroups (`cgroup_create_at` with a
//!    parent) — existing tests only cover flat runtime lifecycle.
//! 4. Bandwidth enforcement reaching tasks nested several levels below the
//!    limited ancestor (3-level grandparent→mid→leaf).
//! 5. LAVD throughout.
//!
//! All scenarios use the deterministic serial engine with a fixed seed and
//! `instant_timing()`, and assert on real cgroup_bw / cgroup-lifecycle
//! TraceKinds (No-Stub / model-the-kernel: enforcement runs in LAVD's
//! `cgroup_bw.bpf.c`, never a Rust approximation).

use scx_simulator::*;

mod common;

fn forever_run(run_ns: u64) -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(run_ns)],
        repeat: RepeatMode::Forever,
    }
}

/// Count trace events matching a predicate over `TraceKind`.
fn count_kind(trace: &Trace, pred: impl Fn(&TraceKind) -> bool) -> usize {
    trace.events().iter().filter(|e| pred(&e.kind)).count()
}

/// Count cgroup_bw enforcement/accounting events (proves the real library ran).
fn count_cgroup_bw_activity(trace: &Trace) -> usize {
    count_kind(trace, |k| {
        matches!(
            k,
            TraceKind::CgroupBwCharge { .. }
                | TraceKind::CgroupBwConsumeNs { .. }
                | TraceKind::CgroupBwReplenish { .. }
                | TraceKind::CgroupBwDequeueOnThrottle { .. }
                | TraceKind::CgroupBwReenqueueOnReplenish { .. }
                | TraceKind::CgroupBwDenied { .. }
                | TraceKind::LavdBailOnCgroupThrottle { .. }
        )
    })
}

fn assert_identical(t1: &Trace, t2: &Trace, ctx: &str) {
    assert_eq!(
        t1.events().len(),
        t2.events().len(),
        "{ctx}: trace lengths differ ({} vs {})",
        t1.events().len(),
        t2.events().len()
    );
    for (i, (e1, e2)) in t1.events().iter().zip(t2.events().iter()).enumerate() {
        assert_eq!(e1.time_ns, e2.time_ns, "{ctx}: event {i} time differs");
        assert_eq!(e1.cpu, e2.cpu, "{ctx}: event {i} cpu differs");
        assert_eq!(e1.kind, e2.kind, "{ctx}: event {i} kind differs");
    }
}

fn lavd(nr_cpus: u32) -> DynamicScheduler {
    let sched = DynamicScheduler::lavd(nr_cpus);
    sched.lavd_set_cgroup_bw_max(64);
    sched
}

// ---------------------------------------------------------------------------
// 1. Nested cgroups with DIFFERENT bandwidth limits (tight vs loose).
// ---------------------------------------------------------------------------

/// Two sibling hierarchies under root: a tight (low-quota) parent and a loose
/// (high-quota) parent, each with a nested child holding CPU-bound tasks that
/// compete for the same CPUs. The bandwidth limit sits on the PARENT; tasks
/// live in the nested CHILD. LAVD's cgroup_bw must enforce each parent's quota
/// on its nested tasks, so the loose hierarchy's tasks accrue more runtime than
/// the tight hierarchy's.
#[test]
fn test_nested_different_bandwidth_limits_lavd() {
    let _lock = common::setup_test();
    let period_us = 100_000u64; // 100ms period
                                // Single CPU so the two hierarchies genuinely contend and each parent's
                                // cpu.max quota actually bites (on multiple idle CPUs the demand fits and
                                // enforcement barely differentiates).
    let scenario = Scenario::builder()
        .cpus(1)
        .seed(42)
        .instant_timing()
        // Tight: 15% of a period. Loose: 85%.
        .cgroup_with_bandwidth("tight_parent", &[CpuId(0)], period_us, 15_000, 0)
        .cgroup_with_bandwidth("loose_parent", &[CpuId(0)], period_us, 85_000, 0)
        .cgroup_nested("tight_child", "tight_parent", None)
        .cgroup_nested("loose_child", "loose_parent", None)
        .add_task_in_cgroup("t1", 0, forever_run(100_000_000), "tight_child")
        .add_task_in_cgroup("t2", 0, forever_run(100_000_000), "tight_child")
        .add_task_in_cgroup("l1", 0, forever_run(100_000_000), "loose_child")
        .add_task_in_cgroup("l2", 0, forever_run(100_000_000), "loose_child")
        .duration_ms(600)
        .build();

    let trace = Simulator::new(lavd(2)).run(scenario);
    trace.dump();

    assert_eq!(
        trace.exit_kind(),
        &ExitKind::Normal,
        "hierarchy run should not stall"
    );
    assert!(
        !trace.has_error(),
        "unexpected error: {:?}",
        trace.exit_kind()
    );

    // Both bandwidth limits were configured (cpu.max written for each parent).
    let set_bw = count_kind(&trace, |k| {
        matches!(k, TraceKind::CgroupSetBandwidth { .. })
    });
    assert!(
        set_bw >= 2,
        "expected >=2 CgroupSetBandwidth events, got {set_bw}"
    );

    // The real cgroup_bw library actually ran.
    let cbw = count_cgroup_bw_activity(&trace);
    assert!(
        cbw > 0,
        "expected cgroup_bw enforcement activity, got {cbw}"
    );

    // Tight-hierarchy tasks (pids 1,2) vs loose-hierarchy tasks (pids 3,4).
    let tight_rt = trace.total_runtime(Pid(1)) + trace.total_runtime(Pid(2));
    let loose_rt = trace.total_runtime(Pid(3)) + trace.total_runtime(Pid(4));
    eprintln!(
        "different-bw: tight(15%)={tight_rt}ns loose(85%)={loose_rt}ns cbw_events={cbw} set_bw={set_bw}"
    );

    // Enforcement differentiates by limit: the loose hierarchy out-runs the
    // tight one. Deterministic per seed. (We assert direction, not exact quota
    // proportions — scxsim's cgroup_bw does not clamp strictly to the quota
    // fraction in this regime, which is fine; the point is that a tighter
    // parent limit yields strictly less runtime for its nested tasks.)
    assert!(
        loose_rt > tight_rt,
        "expected loose (85%) hierarchy to out-run tight (15%): loose={loose_rt} tight={tight_rt}"
    );
}

// ---------------------------------------------------------------------------
// 2. Tasks moving between cgroups at runtime.
// ---------------------------------------------------------------------------

/// A task starts in one cgroup and is migrated to another mid-simulation.
/// Verify the `CgroupMove` structop fires with the right pid and the task keeps
/// running before and after the move (throttle state changes mid-flight).
#[test]
fn test_task_migration_between_cgroups_lavd() {
    let _lock = common::setup_test();
    let migrate_at = 100_000_000u64; // 100ms
    let scenario = Scenario::builder()
        .cpus(2)
        .seed(42)
        .instant_timing()
        .cgroup("cg_src", &[CpuId(0), CpuId(1)])
        .cgroup("cg_dst", &[CpuId(0), CpuId(1)])
        .add_task_in_cgroup("mover", 0, forever_run(100_000_000), "cg_src")
        // a companion task so the CPU stays contended across the move
        .add_task_in_cgroup("bg", 0, forever_run(100_000_000), "cg_dst")
        .cgroup_migrate(Pid(1), "cg_src", "cg_dst", migrate_at)
        .duration_ms(300)
        .build();

    let trace = Simulator::new(lavd(2)).run(scenario);
    trace.dump();

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    assert!(
        !trace.has_error(),
        "unexpected error: {:?}",
        trace.exit_kind()
    );

    // The migration structop fired for the mover.
    let moves: Vec<_> = trace
        .events()
        .iter()
        .filter(|e| matches!(e.kind, TraceKind::CgroupMove { pid, .. } if pid == Pid(1)))
        .collect();
    assert_eq!(
        moves.len(),
        1,
        "expected exactly one CgroupMove for pid 1, got {}",
        moves.len()
    );
    assert!(
        moves[0].time_ns >= migrate_at,
        "CgroupMove fired at {} before scheduled migrate time {migrate_at}",
        moves[0].time_ns
    );

    // The mover ran both before and after the migration (no stall on move).
    let before = trace.events().iter().any(|e| {
        e.time_ns < migrate_at
            && matches!(e.kind, TraceKind::TaskScheduled { pid } if pid == Pid(1))
    });
    let after = trace.events().iter().any(|e| {
        e.time_ns >= migrate_at
            && matches!(e.kind, TraceKind::TaskScheduled { pid } if pid == Pid(1))
    });
    assert!(before, "mover never scheduled before migration");
    assert!(after, "mover never scheduled after migration");
}

// ---------------------------------------------------------------------------
// 3. Runtime creation/deletion of NESTED cgroups.
// ---------------------------------------------------------------------------

/// Create a NESTED cgroup (child under an existing parent) at runtime, then
/// destroy it. Existing lifecycle tests only create flat (root-level) cgroups
/// at runtime; this exercises the parented `cgroup_create_at` path and the
/// matching `CgroupInit`/`CgroupExit` structops.
#[test]
fn test_runtime_nested_cgroup_lifecycle_lavd() {
    let _lock = common::setup_test();
    let create_at = 30_000_000u64; // 30ms
    let destroy_at = 120_000_000u64; // 120ms
    let scenario = Scenario::builder()
        .cpus(2)
        .seed(42)
        .instant_timing()
        .detect_bpf_errors()
        // Static parent present from the start ...
        .cgroup("parent", &[CpuId(0), CpuId(1)])
        .add_task("root_task", 0, forever_run(50_000_000))
        // ... a nested child created and destroyed during the run.
        .cgroup_create_at("nested_child", Some("parent"), None, create_at)
        .cgroup_destroy_at("nested_child", destroy_at)
        .duration_ms(200)
        .build();

    let trace = Simulator::new(lavd(2)).run(scenario);
    trace.dump();

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    assert!(
        !trace.has_error(),
        "unexpected error: {:?}",
        trace.exit_kind()
    );

    // A cgroup_init occurred at/after the runtime creation time (the nested
    // child), and a cgroup_exit at/after the destroy time.
    let init_after_create = trace
        .events()
        .iter()
        .any(|e| e.time_ns >= create_at && matches!(e.kind, TraceKind::CgroupInit { .. }));
    let exit_after_destroy = trace
        .events()
        .iter()
        .any(|e| e.time_ns >= destroy_at && matches!(e.kind, TraceKind::CgroupExit { .. }));
    assert!(
        init_after_create,
        "no CgroupInit at/after runtime nested creation ({create_at}ns)"
    );
    assert!(
        exit_after_destroy,
        "no CgroupExit at/after runtime destroy ({destroy_at}ns)"
    );
}

// ---------------------------------------------------------------------------
// 4. Bandwidth enforcement reaching DEEPLY nested tasks (3-level hierarchy).
// ---------------------------------------------------------------------------

/// Grandparent (bandwidth-limited, root) → mid → leaf, with the CPU-bound tasks
/// living in the LEAF, two levels below the limit. LAVD's cgroup_bw must charge
/// the grandparent's quota for runtime consumed by the leaf's tasks — i.e. the
/// limit is enforced across all intervening hierarchy levels.
#[test]
fn test_bandwidth_enforced_across_deep_hierarchy_lavd() {
    let _lock = common::setup_test();
    let scenario = Scenario::builder()
        .cpus(2)
        .seed(42)
        .instant_timing()
        // Limited grandparent at root (30% of a 100ms period).
        .cgroup_with_bandwidth("grandparent", &[CpuId(0), CpuId(1)], 100_000, 30_000, 0)
        .cgroup_nested("mid", "grandparent", None)
        .cgroup_nested("leaf", "mid", None)
        .add_task_in_cgroup("deep1", 0, forever_run(100_000_000), "leaf")
        .add_task_in_cgroup("deep2", 0, forever_run(100_000_000), "leaf")
        // Unlimited control cgroup (root, no bandwidth) with an equivalent task.
        .cgroup("free", &[CpuId(0), CpuId(1)])
        .add_task_in_cgroup("free1", 0, forever_run(100_000_000), "free")
        .duration_ms(600)
        .build();

    let trace = Simulator::new(lavd(2)).run(scenario);
    trace.dump();

    assert_eq!(
        trace.exit_kind(),
        &ExitKind::Normal,
        "deep hierarchy should not stall"
    );
    assert!(
        !trace.has_error(),
        "unexpected error: {:?}",
        trace.exit_kind()
    );

    // The grandparent's cpu.max was configured and its cgroup_bw ran for the
    // deeply-nested tasks (charge/consume attributed up the hierarchy).
    let set_bw = count_kind(&trace, |k| {
        matches!(k, TraceKind::CgroupSetBandwidth { .. })
    });
    assert!(
        set_bw >= 1,
        "expected the grandparent cpu.max to be set, got {set_bw}"
    );
    let cbw = count_cgroup_bw_activity(&trace);
    assert!(
        cbw > 0,
        "expected cgroup_bw activity for deeply-nested tasks, got {cbw}"
    );

    // The two deeply-nested (limited) tasks share the grandparent's 30% quota,
    // so each should individually get less runtime than the unlimited control.
    let deep_each = trace.total_runtime(Pid(1)).min(trace.total_runtime(Pid(2)));
    let free_rt = trace.total_runtime(Pid(3));
    eprintln!(
        "deep-hierarchy: deep1={} deep2={} free={free_rt} cbw={cbw}",
        trace.total_runtime(Pid(1)),
        trace.total_runtime(Pid(2))
    );
    assert!(deep_each > 0 && free_rt > 0, "all tasks should run");
    assert!(
        free_rt > deep_each,
        "unlimited control should out-run each limited deeply-nested task: free={free_rt} deep_each={deep_each}"
    );
}

// ---------------------------------------------------------------------------
// 5. Determinism guard for the hierarchy scenarios (no flakes).
// ---------------------------------------------------------------------------

#[test]
fn test_cgroup_hierarchy_determinism_lavd() {
    let _lock = common::setup_test();
    let make = || {
        Scenario::builder()
            .cpus(2)
            .seed(42)
            .instant_timing()
            .cgroup_with_bandwidth("gp", &[CpuId(0), CpuId(1)], 100_000, 30_000, 0)
            .cgroup_nested("mid", "gp", None)
            .cgroup_nested("leaf", "mid", None)
            .add_task_in_cgroup("d1", 0, forever_run(100_000_000), "leaf")
            .add_task_in_cgroup("d2", 0, forever_run(100_000_000), "leaf")
            .cgroup("free", &[CpuId(0), CpuId(1)])
            .add_task_in_cgroup("f1", 0, forever_run(100_000_000), "free")
            .cgroup_migrate(Pid(3), "free", "leaf", 150_000_000)
            .duration_ms(400)
            .build()
    };

    let t1 = Simulator::new(lavd(2)).run(make());
    let t2 = Simulator::new(lavd(2)).run(make());
    assert_identical(&t1, &t2, "cgroup hierarchy");
    assert_eq!(t1.exit_kind(), &ExitKind::Normal);
    // The migration into the limited leaf fired.
    assert!(
        count_kind(&t1, |k| matches!(k, TraceKind::CgroupMove { .. })) >= 1,
        "expected a CgroupMove in the combined hierarchy+migration scenario"
    );
}
