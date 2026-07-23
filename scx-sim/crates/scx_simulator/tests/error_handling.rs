//! Error-handling & edge-case tests for the simulator engine
//! (tg test-error-handling-paths).
//!
//! scxsim follows a "No Silent Failures" policy (see scx-sim/CLAUDE.md): bad
//! input must produce a *loud* failure — a panic at configuration/setup time or
//! a non-`Normal` `ExitKind` at run time — never a silently-wrong result. These
//! tests pin that contract for the engine's edge cases:
//!
//!   1. Invalid workload / topology configuration  -> `build()` panics
//!   2. Zero-/single-CPU topologies                -> panic (0) / valid (1)
//!   3. Tasks with zero runtime / no phases        -> handled, no silent corruption
//!   4. Scheduler returning an invalid CPU         -> engine clamp (see note below)
//!   5. Cgroup resource exhaustion (arena/ENOMEM)  -> `ExitKind::ErrorCgroupExhausted`
//!   6. Malformed cgroup hierarchies               -> panic at setup
//!
//! ## Note on area 4 (scheduler returns an invalid CPU)
//! The engine defends against a scheduler returning an out-of-range CPU from
//! `ops.select_cpu` by clamping to `prev_cpu` with a warning — a deliberate,
//! kernel-matching graceful recovery, not a hard failure (see
//! `safe/engine.rs`, "select_cpu returned out-of-range CPU, falling back to
//! prev_cpu"). This path is not black-box testable through the public scenario
//! API: the supported BPF schedulers never emit an out-of-range CPU, and
//! driving it would require a hand-rolled mock implementing the entire unsafe
//! `Scheduler` ops trait, which would test the mock rather than the engine.
//! The clamp is therefore covered by code inspection and documented here.

use scx_simulator::*;

#[macro_use]
mod common;

/// A trivial always-runnable task.
fn busy_task(pid: i32, phases: Vec<Phase>) -> TaskDef {
    TaskDef {
        name: format!("t{pid}"),
        pid: Pid(pid),
        nice: 0,
        behavior: TaskBehavior {
            phases,
            repeat: RepeatMode::Forever,
        },
        start_time_ns: 0,
        mm_id: None,
        allowed_cpus: None,
        parent_pid: None,
        cgroup_name: None,
        task_flags: 0,
        migration_disabled: 0,
    }
}

// ---------------------------------------------------------------------------
// Area 1 & 2: invalid topology / configuration -> build() panics loudly
// ---------------------------------------------------------------------------

#[test]
#[should_panic(expected = "at least one CPU")]
fn zero_cpus_panics() {
    // Area 2: a zero-CPU topology is nonsensical and must be rejected.
    let _ = Scenario::builder()
        .cpus(0)
        .task(busy_task(1, vec![Phase::Run(1_000_000)]))
        .duration_ms(10)
        .build();
}

#[test]
#[should_panic(expected = "divisible by smt_threads_per_core")]
fn smt_not_divisible_panics() {
    // Area 1: nr_cpus must be a multiple of the SMT thread count.
    let _ = Scenario::builder()
        .cpus(3)
        .smt(2)
        .task(busy_task(1, vec![Phase::Run(1_000_000)]))
        .duration_ms(10)
        .build();
}

#[test]
#[should_panic(expected = "smt_threads_per_core must be at least 1")]
fn zero_smt_threads_panics() {
    // Area 1: SMT thread count of 0 is invalid.
    let _ = Scenario::builder()
        .cpus(4)
        .smt(0)
        .task(busy_task(1, vec![Phase::Run(1_000_000)]))
        .duration_ms(10)
        .build();
}

#[test]
#[should_panic(expected = "divisible by cpus_per_llc")]
fn cpus_per_llc_not_divisible_panics() {
    // Area 1: nr_cpus must be a multiple of cpus_per_llc.
    let _ = Scenario::builder()
        .cpus(4)
        .cpus_per_llc(3)
        .task(busy_task(1, vec![Phase::Run(1_000_000)]))
        .duration_ms(10)
        .build();
}

/// Area 2: a single-CPU topology is a valid edge case and must run cleanly.
#[test]
fn single_cpu_runs_normally() {
    let _lock = common::setup_test();
    let scenario = Scenario::builder()
        .cpus(1)
        .task(busy_task(1, vec![Phase::Run(5_000_000)]))
        .task(busy_task(2, vec![Phase::Run(5_000_000)]))
        .duration_ms(50)
        .build();

    let trace = Simulator::new(DynamicScheduler::lavd(1)).run(scenario);

    assert_eq!(
        trace.exit_kind(),
        &ExitKind::Normal,
        "single-CPU run should complete normally, got {:?}",
        trace.exit_kind()
    );
    assert!(trace.total_runtime(Pid(1)) > 0, "task 1 got no runtime");
    assert!(trace.total_runtime(Pid(2)) > 0, "task 2 got no runtime");
}

// ---------------------------------------------------------------------------
// Area 3: zero-runtime / degenerate task behaviors -> handled, no corruption
// ---------------------------------------------------------------------------

/// A `Phase::Run(0)` (zero-length run) must not crash or hang the engine; the
/// simulation should still complete normally.
#[test]
fn zero_runtime_phase_completes() {
    let _lock = common::setup_test();
    let scenario = Scenario::builder()
        .cpus(2)
        // A zero-length run phase followed by a real one.
        .task(busy_task(1, vec![Phase::Run(0), Phase::Run(5_000_000)]))
        .task(busy_task(2, vec![Phase::Run(5_000_000)]))
        .duration_ms(50)
        .build();

    let trace = Simulator::new(DynamicScheduler::lavd(2)).run(scenario);

    assert_eq!(
        trace.exit_kind(),
        &ExitKind::Normal,
        "zero-runtime phase should not break the sim, got {:?}",
        trace.exit_kind()
    );
}

/// A task with an empty phase list is degenerate (nothing to run); the engine
/// must handle it without a silently-wrong result — the *other* task must still
/// be scheduled and the run must complete.
#[test]
fn empty_phases_task_handled() {
    let _lock = common::setup_test();
    let scenario = Scenario::builder()
        .cpus(2)
        .task(busy_task(1, vec![])) // no phases at all
        .task(busy_task(2, vec![Phase::Run(5_000_000)]))
        .duration_ms(50)
        .build();

    let trace = Simulator::new(DynamicScheduler::lavd(2)).run(scenario);

    assert_eq!(
        trace.exit_kind(),
        &ExitKind::Normal,
        "empty-phase task should not break the sim, got {:?}",
        trace.exit_kind()
    );
    // The real task must still make progress.
    assert!(
        trace.total_runtime(Pid(2)) > 0,
        "the runnable task starved when a peer had empty phases"
    );
}

// ---------------------------------------------------------------------------
// Area 5: cgroup resource exhaustion (arena/hashmap ENOMEM) -> reported, not silent
// ---------------------------------------------------------------------------

/// Creating more cgroups at runtime than `max_cgroups` allows must surface a
/// loud `ExitKind::ErrorCgroupExhausted` (modeling BPF hashmap/arena capacity
/// ENOMEM), never a silent drop.
#[test]
fn max_cgroups_exhaustion_reports_enomem() {
    let _lock = common::setup_test();
    let nr = 2;
    // Root cgroup occupies a slot; cap total at 2 so the 2nd+ runtime create
    // is guaranteed to exceed the limit.
    let mut b = Scenario::builder()
        .cpus(nr)
        .max_cgroups(2)
        .task(busy_task(1, vec![Phase::Run(100_000_000)]));
    for (i, at) in [10_000_000u64, 20_000_000, 30_000_000, 40_000_000]
        .into_iter()
        .enumerate()
    {
        b = b.cgroup_create_at(&format!("cg{i}"), None, None, at);
    }
    let scenario = b.duration_ms(100).build();

    let trace = Simulator::new(DynamicScheduler::lavd(nr)).run(scenario);

    assert!(
        trace.has_error(),
        "exceeding max_cgroups must produce an error exit, got {:?}",
        trace.exit_kind()
    );
    match trace.exit_kind() {
        ExitKind::ErrorCgroupExhausted {
            active_count,
            max_cgroups,
            ..
        } => {
            assert_eq!(*max_cgroups, 2, "reported max_cgroups mismatch");
            assert!(
                *active_count >= *max_cgroups,
                "exhaustion reported below the limit: active={active_count} max={max_cgroups}"
            );
        }
        other => panic!("expected ErrorCgroupExhausted, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Area 6: malformed cgroup hierarchies -> loud panic at setup
// ---------------------------------------------------------------------------

#[test]
#[should_panic(expected = "not found")]
fn nested_cgroup_with_undefined_parent_panics() {
    // A nested cgroup referencing a parent that was never defined is malformed.
    let _lock = common::setup_test();
    let scenario = Scenario::builder()
        .cpus(2)
        .cgroup_nested("child", "ghost_parent", None)
        .task(busy_task(1, vec![Phase::Run(5_000_000)]))
        .duration_ms(50)
        .build();

    // The malformed hierarchy is detected during setup inside run().
    let _ = Simulator::new(DynamicScheduler::lavd(2)).run(scenario);
}

#[test]
#[should_panic(expected = "not found")]
fn task_in_undefined_cgroup_panics() {
    // Assigning a task to a cgroup name that was never defined is malformed.
    let _lock = common::setup_test();
    let scenario = Scenario::builder()
        .cpus(2)
        .add_task_in_cgroup(
            "t1",
            0,
            busy_task(1, vec![Phase::Run(5_000_000)]).behavior,
            "ghost",
        )
        .duration_ms(50)
        .build();

    let _ = Simulator::new(DynamicScheduler::lavd(2)).run(scenario);
}
