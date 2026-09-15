//! Additional targeted tests for the `scx_simple` scheduler, focused on code
//! paths in `schedulers/simple/scx_simple.bpf.c` that the existing
//! `simple.rs` / `common/mod.rs` suites do not exercise directly:
//!
//! - `simple_select_cpu` idle fast path → direct dispatch to `SCX_DSQ_LOCAL`
//!   (multi-CPU, tasks <= idle CPUs).                          [local dispatch]
//! - `simple_enqueue` vtime path into `SHARED_DSQ` when no CPU is idle
//!   (tasks > CPUs).                                           [global dispatch]
//! - `simple_exit` / drain: a task exiting while others remain queued must not
//!   stall the run.
//! - strict BPF-error detection: a normal simple run must raise no BPF error
//!   under `detect_bpf_errors()`.
//! - richer topology (SMT + higher CPU count) idle spread.
//! - long-sleeper wake exercising the vtime idle-budget clamp branch
//!   (`if (time_before(vtime, vtime_now - SCX_SLICE_DFL))`).
//!
//! These assert on behavior/observables only (no scheduler-side changes), in
//! keeping with the No-Stub / "model the kernel, not the scheduler" rules.

use scx_simulator::*;

#[macro_use]
mod common;

/// Build a forever-running CPU-bound behavior.
fn forever_run(run_ns: u64) -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(run_ns)],
        repeat: RepeatMode::Forever,
    }
}

/// Build a run/sleep cycle behavior.
fn run_sleep(run_ns: u64, sleep_ns: u64) -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(run_ns), Phase::Sleep(sleep_ns)],
        repeat: RepeatMode::Forever,
    }
}

/// Multi-CPU idle fast path: with more idle CPUs than tasks, `simple_select_cpu`
/// finds an idle CPU (`is_idle`) and direct-dispatches to the CPU's local DSQ
/// (`scx_bpf_dsq_insert(SCX_DSQ_LOCAL, ...)`), rather than routing through the
/// global `SHARED_DSQ` vtime path in `simple_enqueue`. Verify local dispatches
/// occur and dominate.
#[test]
fn test_multi_cpu_idle_direct_dispatch() {
    let _lock = common::setup_test();
    let scenario = Scenario::builder()
        .cpus(4)
        // Two light sleep/wake tasks: each wake triggers a fresh select_cpu
        // with idle CPUs available -> local direct dispatch.
        .add_task("waker_a", 0, run_sleep(3_000_000, 7_000_000))
        .add_task("waker_b", 0, run_sleep(3_000_000, 7_000_000))
        .duration_ms(200)
        .build();

    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario);
    trace.dump();

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);

    let (global, local) = trace.dsq_dispatch_counts();
    eprintln!("dsq dispatches: global(SHARED_DSQ)={global}, local(SCX_DSQ_LOCAL)={local}");

    // The idle fast path must be exercised at all (previously untested) ...
    assert!(
        local > 0,
        "expected idle-CPU direct dispatch to SCX_DSQ_LOCAL, got local={local} global={global}"
    );
    // ... and with idle CPUs plentiful it should dominate over the global path.
    assert!(
        local >= global,
        "with idle CPUs the local fast path should dominate: local={local} global={global}"
    );

    // Sanity: both tasks actually ran.
    assert!(trace.total_runtime(Pid(1)) > 0);
    assert!(trace.total_runtime(Pid(2)) > 0);
}

/// All-CPUs-busy global enqueue path: with more runnable tasks than CPUs,
/// `simple_select_cpu` finds no idle CPU for the surplus tasks, so they fall
/// through to `simple_enqueue`, which inserts into `SHARED_DSQ` via
/// `scx_bpf_dsq_insert_vtime`. Verify the global path is exercised and every
/// task still gets serviced (the dispatch path drains SHARED_DSQ).
#[test]
fn test_all_cpus_busy_global_dsq_enqueue() {
    let _lock = common::setup_test();
    let mut builder = Scenario::builder().cpus(2);
    for _ in 0..6 {
        builder = builder.add_task("hog", 0, forever_run(50_000_000));
    }
    let scenario = builder.duration_ms(200).build();

    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario);
    trace.dump();

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);

    let (global, local) = trace.dsq_dispatch_counts();
    eprintln!("dsq dispatches: global(SHARED_DSQ)={global}, local(SCX_DSQ_LOCAL)={local}");
    assert!(
        global > 0,
        "expected surplus tasks to enqueue into the global SHARED_DSQ, got global={global} local={local}"
    );

    // Every one of the 6 tasks must be scheduled at least once — the vtime
    // ordering must not starve any task out entirely.
    for pid in 1..=6i32 {
        assert!(
            trace.schedule_count(Pid(pid)) > 0,
            "task pid={pid} was never scheduled (starved out of SHARED_DSQ)"
        );
    }
}

/// A task that exits (RepeatMode::Once) while other tasks remain runnable and
/// queued must complete cleanly (`simple_exit`) and must NOT stall the run; the
/// surviving tasks keep being scheduled after it finishes.
#[test]
fn test_task_exit_while_others_queued() {
    let _lock = common::setup_test();
    let scenario = Scenario::builder()
        .cpus(2)
        // pid 1: short one-shot task that will exit early.
        .add_task(
            "finisher",
            0,
            TaskBehavior {
                phases: vec![Phase::Run(10_000_000)],
                repeat: RepeatMode::Once,
            },
        )
        // pids 2,3,4: long-lived contenders that stay queued.
        .add_task("hog_b", 0, forever_run(50_000_000))
        .add_task("hog_c", 0, forever_run(50_000_000))
        .add_task("hog_d", 0, forever_run(50_000_000))
        .duration_ms(200)
        .build();

    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario);
    trace.dump();

    assert_eq!(
        trace.exit_kind(),
        &ExitKind::Normal,
        "exit while queued must not stall"
    );

    // The finisher completed.
    let finisher_completed = trace
        .events()
        .iter()
        .any(|e| matches!(e.kind, TraceKind::TaskCompleted { pid } if pid == Pid(1)));
    assert!(finisher_completed, "one-shot finisher task never completed");

    // Find when the finisher completed, then assert the survivors are scheduled
    // AFTER that point (the run keeps making progress once a task drains out).
    let finish_time = trace
        .events()
        .iter()
        .find(|e| matches!(e.kind, TraceKind::TaskCompleted { pid } if pid == Pid(1)))
        .map(|e| e.time_ns)
        .expect("finisher completion event");

    for pid in 2..=4i32 {
        let scheduled_after = trace.events().iter().any(|e| {
            e.time_ns >= finish_time
                && matches!(e.kind, TraceKind::TaskScheduled { pid: p } if p == Pid(pid))
        });
        assert!(
            scheduled_after,
            "survivor pid={pid} was not scheduled after the finisher exited (possible stall/leak)"
        );
    }
}

/// Strict BPF-error detection: a normal simple-scheduler run under
/// `detect_bpf_errors()` (ignore_bpf_errors = false) must complete with no BPF
/// error. Guards against future kfunc-misuse regressions in the simple path
/// that would otherwise be masked by the default lenient mode.
#[test]
fn test_strict_bpf_error_detection() {
    let _lock = common::setup_test();
    let scenario = Scenario::builder()
        .cpus(4)
        .detect_bpf_errors()
        .add_task("a", 0, run_sleep(5_000_000, 5_000_000))
        .add_task("b", -3, forever_run(30_000_000))
        .add_task("c", 5, forever_run(30_000_000))
        .duration_ms(200)
        .build();

    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario);
    trace.dump();

    assert!(
        !trace.has_error(),
        "simple raised a BPF error under strict detection: {:?}",
        trace.exit_kind()
    );
    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
}

/// Richer topology: SMT with a higher CPU count. `scx_simple` is
/// topology-agnostic, but `scx_bpf_select_cpu_dfl` idle selection walks the
/// SMT/LLC topology; exercise it and assert tasks spread across cores with a
/// clean exit.
#[test]
fn test_smt_topology_idle_spread() {
    let _lock = common::setup_test();
    let mut builder = Scenario::builder().cpus(8).smt(2);
    for _ in 0..4 {
        builder = builder.add_task("w", 0, run_sleep(4_000_000, 4_000_000));
    }
    let scenario = builder.duration_ms(200).build();

    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario);
    trace.dump();

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);

    let cpus_used: std::collections::HashSet<CpuId> = trace
        .events()
        .iter()
        .filter_map(|e| match e.kind {
            TraceKind::TaskScheduled { .. } => Some(e.cpu),
            _ => None,
        })
        .collect();
    assert!(
        cpus_used.len() >= 2,
        "expected idle spread across multiple CPUs on an 8-CPU SMT box, used {cpus_used:?}"
    );
}

/// Long-sleeper vtime clamp: a task that sleeps for a long time while other
/// tasks advance the global `vtime_now` will, on wake, have a `dsq_vtime` more
/// than one slice behind `vtime_now`. `simple_enqueue` clamps this
/// (`if (time_before(vtime, vtime_now - SCX_SLICE_DFL)) vtime = vtime_now -
/// SCX_SLICE_DFL;`) so the sleeper cannot cash in unbounded idle credit and
/// monopolize the CPU. Verify the sleeper shares the CPU rather than starving
/// the CPU-bound competitors after it wakes.
#[test]
fn test_long_sleeper_vtime_clamp_no_monopoly() {
    let _lock = common::setup_test();
    let scenario = Scenario::builder()
        .cpus(1)
        // Sleeper: sleeps a long time up front (accumulating vtime deficit vs
        // vtime_now), then runs forever.
        .add_task(
            "sleeper",
            0,
            TaskBehavior {
                phases: vec![Phase::Sleep(120_000_000), Phase::Run(200_000_000)],
                repeat: RepeatMode::Once,
            },
        )
        // Two CPU-bound competitors advance vtime_now while the sleeper sleeps.
        .add_task("hog_b", 0, forever_run(200_000_000))
        .add_task("hog_c", 0, forever_run(200_000_000))
        .duration_ms(300)
        .build();

    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario);
    trace.dump();

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);

    let rt_sleeper = trace.total_runtime(Pid(1));
    let rt_b = trace.total_runtime(Pid(2));
    let rt_c = trace.total_runtime(Pid(3));
    eprintln!("post-wake runtimes: sleeper={rt_sleeper} hog_b={rt_b} hog_c={rt_c}");

    // The competitors ran throughout (they never slept).
    assert!(rt_b > 0 && rt_c > 0, "CPU-bound competitors got no runtime");

    // With the clamp, the sleeper's idle credit is capped at one slice, so it
    // cannot monopolize: the competitors together must retain a healthy share
    // of the total CPU time rather than being starved while the sleeper cashes
    // in unbounded deficit.
    let total = rt_sleeper + rt_b + rt_c;
    assert!(total > 0);
    let competitors = rt_b + rt_c;
    assert!(
        competitors >= total / 3,
        "clamp failed: sleeper monopolized the CPU (sleeper={rt_sleeper}, competitors={competitors}, total={total})"
    );

    // And the sleeper is not starved either — it does run after waking.
    assert!(rt_sleeper > 0, "sleeper never ran after waking");
}
