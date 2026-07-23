//! Concurrent task-lifecycle tests (tg test-concurrent-task-lifecycle).
//!
//! These stress the *lifecycle machinery* — task creation (`init_task` /
//! `enable`), completion (`TaskCompleted`), and teardown (`exit_task`) — under
//! concurrency, rather than steady-state scheduling behavior (that is
//! `workload_variety.rs`). Each scenario is run under simple, lavd, AND cosmos
//! and asserts the same scheduler-agnostic invariants.
//!
//! The engine's lifecycle contract (verified empirically and asserted here):
//!   - every task gets exactly one `InitTask` and one `Enable` at creation;
//!   - a `RepeatMode::Once`/`Count` task emits `TaskCompleted` when it finishes
//!     its phases (possibly mid-run, while other tasks are being dispatched);
//!   - every task gets exactly one `exit_task` at simulation teardown, even
//!     tasks that already completed mid-run (mirrors kernel scheduler unload).
//!
//! Coverage of the task's checklist:
//!   1. many tasks forking simultaneously      -> test_simultaneous_fork_storm
//!   2. tasks exiting while others dispatched   -> test_exit_during_dispatch
//!   3. parent/child relationships              -> test_parent_child_relationships
//!   4. task exit during dispatch (race)        -> test_exit_during_dispatch (+ determinism check)
//!   5. high create/exit churn                  -> test_high_task_churn
//!   6. simple / lavd / cosmos                  -> every test via `run_all`
//!
//! Note on "wait semantics": scxsim models the kernel/BPF scheduling substrate,
//! not process `wait(2)`. Parent/child is expressed via `real_parent` (set from
//! `TaskDef::parent_pid`) plus waker->wakee relationships, which is what a BPF
//! scheduler actually observes; there is no `wait()` reaping to test.

use scx_simulator::*;

#[macro_use]
mod common;

/// Fixed seed so lifecycle-event counts are reproducible run-to-run.
const SEED: u32 = 0x11fe_c0de;

/// Full-control task builder (needed for `parent_pid`, `start_time_ns`, and
/// explicit `RepeatMode`).
fn task(
    name: &str,
    pid: i32,
    start_time_ns: TimeNs,
    repeat: RepeatMode,
    phases: Vec<Phase>,
) -> TaskDef {
    task_with_parent(name, pid, start_time_ns, repeat, phases, None)
}

fn task_with_parent(
    name: &str,
    pid: i32,
    start_time_ns: TimeNs,
    repeat: RepeatMode,
    phases: Vec<Phase>,
    parent_pid: Option<Pid>,
) -> TaskDef {
    TaskDef {
        name: name.into(),
        pid: Pid(pid),
        nice: 0,
        behavior: TaskBehavior { phases, repeat },
        start_time_ns,
        mm_id: None,
        allowed_cpus: None,
        parent_pid,
        cgroup_name: None,
        task_flags: 0,
        migration_disabled: 0,
    }
}

fn count(trace: &Trace, pred: impl Fn(&TraceKind) -> bool) -> usize {
    trace.events().iter().filter(|e| pred(&e.kind)).count()
}

/// Run `build()` (called fresh per scheduler) under simple, lavd, and cosmos,
/// invoking `check(name, &trace)` on each result.
fn run_all(nr_cpus: u32, build: impl Fn() -> Scenario, check: impl Fn(&str, &Trace)) {
    // simple() is single-CPU-configured but runs on any scenario.
    type Make = fn(u32) -> DynamicScheduler;
    let makers: &[(&str, Make)] = &[
        ("simple", |_n| DynamicScheduler::simple()),
        ("lavd", DynamicScheduler::lavd),
        ("cosmos", DynamicScheduler::cosmos),
    ];
    for &(name, make) in makers {
        let trace = Simulator::new(make(nr_cpus)).run(build());
        check(name, &trace);
    }
}

/// Number of distinct PIDs that emitted at least one `TaskCompleted`.
fn distinct_completed(trace: &Trace) -> usize {
    trace
        .events()
        .iter()
        .filter_map(|e| match e.kind {
            TraceKind::TaskCompleted { pid } => Some(pid.0),
            _ => None,
        })
        .collect::<std::collections::HashSet<_>>()
        .len()
}

/// Assert the universal lifecycle invariants that hold for EVERY task
/// regardless of `RepeatMode`: clean run, and exactly one `InitTask` / `Enable`
/// / `ExitTask` per task. Per-completion assertions are made by each test,
/// since `Once` emits one `TaskCompleted` per task while `Count(n)` can emit
/// several (once per terminating iteration).
fn assert_lifecycle(name: &str, trace: &Trace, nr_tasks: usize) {
    assert_eq!(
        *trace.exit_kind(),
        ExitKind::Normal,
        "[{name}] did not exit Normal: {:?}",
        trace.exit_kind()
    );
    assert!(
        !trace.has_error(),
        "[{name}] scheduler raised scx_bpf_error"
    );

    let init = count(trace, |k| matches!(k, TraceKind::InitTask { .. }));
    let enable = count(trace, |k| matches!(k, TraceKind::Enable { .. }));
    let exit = count(trace, |k| matches!(k, TraceKind::ExitTask { .. }));

    assert_eq!(init, nr_tasks, "[{name}] InitTask count != task count");
    assert_eq!(enable, nr_tasks, "[{name}] Enable count != task count");
    assert_eq!(
        exit, nr_tasks,
        "[{name}] ExitTask count != task count (teardown must run for every task)"
    );
}

// ============================================================================
// 1. Many tasks forking simultaneously
// ============================================================================

/// A "fork storm": many tasks created at the *same* instant (`start_time_ns=0`)
/// on a small CPU set, so the engine drives a burst of back-to-back
/// `init_task`/`enable` calls and the scheduler must place them all. Every task
/// runs briefly and exits, so this also stresses simultaneous teardown.
#[test]
fn test_simultaneous_fork_storm() {
    let _lock = common::setup_test();
    let nr_cpus = 4u32;
    const N: usize = 48;

    let build = || {
        let mut b = Scenario::builder().cpus(nr_cpus).seed(SEED);
        for i in 0..N {
            b = b.task(task(
                &format!("fork{i}"),
                i as i32 + 1,
                0, // all fork simultaneously
                RepeatMode::Once,
                vec![Phase::Run(3_000_000)],
            ));
        }
        b.duration_ms(200).build()
    };

    run_all(nr_cpus, build, |name, trace| {
        assert_lifecycle(name, trace, N);
        assert_eq!(
            distinct_completed(trace),
            N,
            "[{name}] not all one-shot tasks completed"
        );
        for pid in 1..=N as i32 {
            assert!(
                trace.total_runtime(Pid(pid)) > 0,
                "[{name}] forked task {pid} never ran"
            );
        }
    });
}

// ============================================================================
// 2 + 4. Tasks exiting while others are dispatched (exit-during-dispatch race)
// ============================================================================

/// Long-lived `Forever` tasks are continuously dispatched while a population of
/// `Once` tasks exit at many *distinct, staggered* times (each has a slightly
/// different run length). Task completion is handled inline on the dispatch
/// path (`handle_task_phase_complete`), so this exercises the exit-during-
/// dispatch race across a spread of moments. Invariants: the one-shot tasks all
/// complete, the long tasks keep running (never complete), and every task is
/// initialized and torn down exactly once.
#[test]
fn test_exit_during_dispatch() {
    let _lock = common::setup_test();
    let nr_cpus = 4u32;
    const N_LONG: usize = 3;
    const N_SHORT: usize = 24;

    let build = || {
        let mut b = Scenario::builder().cpus(nr_cpus).seed(SEED);
        let mut pid = 1;
        for i in 0..N_LONG {
            b = b.task(task(
                &format!("long{i}"),
                pid,
                0,
                RepeatMode::Forever,
                vec![Phase::Run(40_000_000)],
            ));
            pid += 1;
        }
        for i in 0..N_SHORT {
            // Staggered start AND staggered run length -> exits scatter across
            // the whole run, overlapping active dispatch of the long tasks.
            let start = (i as u64) * 1_500_000;
            let run = 2_000_000 + (i as u64) * 400_000;
            b = b.task(task(
                &format!("short{i}"),
                pid,
                start,
                RepeatMode::Once,
                vec![Phase::Run(run)],
            ));
            pid += 1;
        }
        b.duration_ms(250).build()
    };

    run_all(nr_cpus, build, |name, trace| {
        assert_lifecycle(name, trace, N_LONG + N_SHORT);
        assert_eq!(
            count(trace, |k| matches!(k, TraceKind::TaskCompleted { .. })),
            N_SHORT,
            "[{name}] one-shot completion count != N_SHORT"
        );
        // Long tasks must still be running (never completed).
        let completed_pids: std::collections::HashSet<i32> = trace
            .events()
            .iter()
            .filter_map(|e| match e.kind {
                TraceKind::TaskCompleted { pid } => Some(pid.0),
                _ => None,
            })
            .collect();
        for pid in 1..=N_LONG as i32 {
            assert!(
                !completed_pids.contains(&pid),
                "[{name}] long task {pid} unexpectedly completed"
            );
            assert!(
                trace.total_runtime(Pid(pid)) > 0,
                "[{name}] long task {pid} got no runtime"
            );
        }
    });
}

/// The exit-during-dispatch scenario must be deterministic (same seed -> same
/// lifecycle-event stream), so the race coverage above is *reliable*, not flaky.
#[test]
fn test_exit_during_dispatch_deterministic() {
    let _lock = common::setup_test();
    let nr_cpus = 4u32;

    let build = || {
        let mut b = Scenario::builder().cpus(nr_cpus).seed(SEED);
        let mut pid = 1;
        b = b.task(task(
            "long",
            pid,
            0,
            RepeatMode::Forever,
            vec![Phase::Run(40_000_000)],
        ));
        pid += 1;
        for i in 0..16 {
            b = b.task(task(
                &format!("s{i}"),
                pid,
                (i as u64) * 1_000_000,
                RepeatMode::Once,
                vec![Phase::Run(2_000_000 + (i as u64) * 300_000)],
            ));
            pid += 1;
        }
        b.duration_ms(200).build()
    };

    // Two runs of cosmos with the same seed must produce identical lifecycle
    // event counts and per-task runtimes.
    let sig = |trace: &Trace| {
        (
            count(trace, |k| matches!(k, TraceKind::InitTask { .. })),
            count(trace, |k| matches!(k, TraceKind::ExitTask { .. })),
            count(trace, |k| matches!(k, TraceKind::TaskCompleted { .. })),
            (1..=17)
                .map(|p| trace.total_runtime(Pid(p)))
                .collect::<Vec<_>>(),
        )
    };
    let a = sig(&Simulator::new(DynamicScheduler::cosmos(nr_cpus)).run(build()));
    let b = sig(&Simulator::new(DynamicScheduler::cosmos(nr_cpus)).run(build()));
    assert_eq!(a, b, "exit-during-dispatch run was not deterministic");
}

// ============================================================================
// 3. Parent / child relationships
// ============================================================================

/// A parent task with several children whose `real_parent` points at it (set
/// from `TaskDef::parent_pid`). The parent periodically wakes each child
/// (waker->wakee), exercising the parent/child + waker relationships a BPF
/// scheduler observes (e.g. LAVD latency-criticality propagation). Children are
/// bursty (run then sleep, woken by the parent). Invariants: everyone is
/// initialized/torn down once and makes progress under every scheduler.
///
/// (scxsim models the scheduling substrate, not `wait(2)`; there is no reaping
/// step to assert — the parent/child link is the `real_parent` pointer.)
#[test]
fn test_parent_child_relationships() {
    let _lock = common::setup_test();
    let nr_cpus = 4u32;
    const N_CHILD: usize = 4;
    let total = 1 + N_CHILD;

    let build = || {
        // Parent (pid 1) runs, wakes each child, then sleeps — repeat forever.
        let mut parent_phases = vec![Phase::Run(2_000_000)];
        for c in 0..N_CHILD {
            parent_phases.push(Phase::Wake(Pid(2 + c as i32)));
        }
        parent_phases.push(Phase::Sleep(5_000_000));

        let mut b = Scenario::builder().cpus(nr_cpus).seed(SEED);
        b = b.task(task("parent", 1, 0, RepeatMode::Forever, parent_phases));
        for c in 0..N_CHILD {
            // Children block on a long sleep until the parent wakes them.
            b = b.task(task_with_parent(
                &format!("child{c}"),
                2 + c as i32,
                0,
                RepeatMode::Forever,
                vec![Phase::Run(3_000_000), Phase::Sleep(100_000_000)],
                Some(Pid(1)),
            ));
        }
        b.duration_ms(200).build()
    };

    run_all(nr_cpus, build, |name, trace| {
        // All Forever -> zero one-shot completions.
        assert_lifecycle(name, trace, total);
        assert_eq!(
            count(trace, |k| matches!(k, TraceKind::TaskCompleted { .. })),
            0,
            "[{name}] Forever parent/child tasks should never complete"
        );
        for pid in 1..=total as i32 {
            assert!(
                trace.total_runtime(Pid(pid)) > 0,
                "[{name}] parent/child task {pid} got no runtime"
            );
        }
    });
}

// ============================================================================
// 5. High task churn — rapid create/exit cycles
// ============================================================================

/// Sustained churn: a large population of short one-shot tasks whose creation
/// times are spread densely across the whole run, so tasks are continuously
/// being created and completing. Stresses the create/teardown path under
/// ongoing load. Invariant: every task is created, completes, and is torn down
/// exactly once — no leaked or dropped lifecycle events.
#[test]
fn test_high_task_churn() {
    let _lock = common::setup_test();
    let nr_cpus = 2u32;
    const N: usize = 80;

    let build = || {
        let mut b = Scenario::builder().cpus(nr_cpus).seed(SEED);
        for i in 0..N {
            // Dense staggered arrivals across a ~160ms window.
            let start = (i as u64) * 2_000_000;
            b = b.task(task(
                &format!("churn{i}"),
                i as i32 + 1,
                start,
                RepeatMode::Once,
                vec![Phase::Run(1_500_000)],
            ));
        }
        b.duration_ms(250).build()
    };

    run_all(nr_cpus, build, |name, trace| {
        assert_lifecycle(name, trace, N);
        assert_eq!(
            distinct_completed(trace),
            N,
            "[{name}] not all one-shot tasks completed"
        );
        for pid in 1..=N as i32 {
            assert!(
                trace.total_runtime(Pid(pid)) > 0,
                "[{name}] churn task {pid} never ran"
            );
        }
    });
}

// ============================================================================
// 6. RepeatMode::Count — bounded repeat then exit
// ============================================================================

/// `RepeatMode::Count(k)` tasks repeat their phases k times and then exit,
/// exercising the bounded-repeat completion path (distinct from `Once`).
/// Confirms each such task is created, completes, and is torn down.
///
/// NOTE: these tasks terminate on a `Sleep` phase, which currently double-emits
/// `TaskCompleted` at the same timestamp (mb sim-ea3feb — a task ending on Run
/// emits once, ending on Sleep emits twice). We therefore assert on the number
/// of DISTINCT completed PIDs, which is correct regardless of that bug.
#[test]
fn test_count_repeat_tasks() {
    let _lock = common::setup_test();
    let nr_cpus = 2u32;
    const N: usize = 12;

    let build = || {
        let mut b = Scenario::builder().cpus(nr_cpus).seed(SEED);
        for i in 0..N {
            b = b.task(task(
                &format!("rep{i}"),
                i as i32 + 1,
                0,
                RepeatMode::Count(3),
                vec![Phase::Run(2_000_000), Phase::Sleep(1_000_000)],
            ));
        }
        b.duration_ms(200).build()
    };

    run_all(nr_cpus, build, |name, trace| {
        assert_lifecycle(name, trace, N);
        assert_eq!(
            distinct_completed(trace),
            N,
            "[{name}] not all one-shot tasks completed"
        );
    });
}
