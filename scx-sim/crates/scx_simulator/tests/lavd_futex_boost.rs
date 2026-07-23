//! LAVD futex lock-holder boost — end-to-end simulation test.
//!
//! Exercises the futex substrate added per `ai_docs/FUTEX_SIM_DESIGN.md`: a
//! scheduled `FutexOp` event is delivered to LAVD's REAL `lock.bpf.c` hooks
//! (`rtp_sys_enter_futex` + `rtp_sys_exit_futex` via the `lavd_futex_hook`
//! wrapper), which set/clear `LAVD_FLAG_FUTEX_BOOST` on the running task. The
//! engine records the resulting flag as a `TraceKind::FutexBoost` event, so we
//! can assert the real scheduler code ran and the boost flag is observable.
//!
//! No-Stub note: the boost decision runs entirely inside `lock.bpf.c`; the test
//! only injects the event (as the kernel would deliver the futex tracepoint)
//! and observes the outcome.

use scx_simulator::*;

mod common;

/// Collect the `FutexBoost` events for `pid`, in order, as `(op, boosted)`.
fn futex_boosts(trace: &Trace, pid: Pid) -> Vec<(FutexOp, bool)> {
    trace
        .events()
        .iter()
        .filter_map(|e| match &e.kind {
            TraceKind::FutexBoost {
                pid: p,
                op,
                boosted,
            } if *p == pid => Some((*op, *boosted)),
            _ => None,
        })
        .collect()
}

/// A single always-running task on its own CPU is the running task for the
/// whole run, so a scheduled futex op attributes to it deterministically.
/// `WaitAcquired` must set the boost flag (observable as `boosted == true`);
/// `WakeReleased` must leave it cleared (`boosted == false`).
#[test]
fn test_lavd_futex_wait_acquire_sets_boost() {
    let _lock = common::setup_test();

    let sched = DynamicScheduler::lavd(1);
    let scenario = Scenario::builder()
        .cpus(1)
        .seed(1)
        .detect_bpf_errors()
        .add_task("holder", 0, workloads::cpu_bound(50_000_000))
        // pid 1 is running by 10ms; acquire a contended lock, release later.
        .futex_event(Pid(1), 10_000_000, FutexOp::WaitAcquired)
        .futex_event(Pid(1), 30_000_000, FutexOp::WakeReleased)
        .duration_ms(50)
        .build();

    let trace = Simulator::new(sched).run(scenario);
    assert!(
        !trace.has_error(),
        "unexpected error: {:?}",
        trace.exit_kind()
    );

    let boosts = futex_boosts(&trace, Pid(1));
    assert_eq!(
        boosts.len(),
        2,
        "expected two FutexBoost events (acquire, release), got {boosts:?}"
    );
    assert_eq!(
        boosts[0],
        (FutexOp::WaitAcquired, true),
        "wait-acquire must set LAVD_FLAG_FUTEX_BOOST (real lock.bpf.c ran)"
    );
    assert_eq!(
        boosts[1].0,
        FutexOp::WakeReleased,
        "second event should be the release"
    );
    assert!(
        !boosts[1].1,
        "after wake-release the futex boost flag must be clear"
    );
}

/// A futex op targeting a task that is not currently running is skipped
/// (No-Silent-Failures: the engine warns and does not mis-attribute the boost
/// to whatever task happens to be on-CPU). A sleeping task therefore produces
/// no `FutexBoost` event.
#[test]
fn test_lavd_futex_op_on_non_running_task_skipped() {
    let _lock = common::setup_test();

    let sched = DynamicScheduler::lavd(1);
    // pid 1 hogs the only CPU; pid 2 sleeps for the whole window, so it is
    // never the running task when the futex op fires at 10ms.
    let scenario = Scenario::builder()
        .cpus(1)
        .seed(1)
        .detect_bpf_errors()
        .add_task("hog", 0, workloads::cpu_bound(50_000_000))
        .task(TaskDef {
            name: "sleeper".into(),
            pid: Pid(2),
            nice: 0,
            behavior: TaskBehavior {
                phases: vec![Phase::Sleep(100_000_000)],
                repeat: RepeatMode::Once,
            },
            start_time_ns: 0,
            mm_id: None,
            allowed_cpus: None,
            parent_pid: None,
            cgroup_name: None,
            task_flags: 0,
            migration_disabled: 0,
        })
        .futex_event(Pid(2), 10_000_000, FutexOp::WaitAcquired)
        .duration_ms(40)
        .build();

    let trace = Simulator::new(sched).run(scenario);
    assert!(
        !trace.has_error(),
        "unexpected error: {:?}",
        trace.exit_kind()
    );
    assert!(
        futex_boosts(&trace, Pid(2)).is_empty(),
        "futex op on a non-running task must be skipped, not mis-attributed"
    );
}
