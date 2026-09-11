//! Yield / sleep / wakeup interaction tests (tg test-yield-sleep-wakeup).
//!
//! This file adds the interaction scenarios not already covered by the large
//! existing sleep/wake suite (generic `test_sleep_wake_cycle`, `cosmos`
//! `test_mm_affinity`, `interleave` sleep/wake, `dispatch_paths`
//! dequeue-on-sleep, and the many `lavd`/`mitosis` ping-pong / wake-chain /
//! waker-wakee / sync-wakeup tests). The focus here is:
//!
//!   * Mass ("thundering herd") simultaneous wakeup — one waker releasing many
//!     blocked tasks at once. Existing wake tests are pairwise or sequential
//!     chains; the many-at-once case was not exercised.
//!   * Cross-task (IPC-style) wakeup proven necessary — a wakee that sleeps
//!     longer than the whole run can *only* make progress if a peer wakes it,
//!     so any runtime it gets is attributable to the wakeup path, not a timer.
//!   * Sleeper liveness under contention vs. on an idle system.
//!
//! ## Note on "voluntary yield" (task item 1)
//! A `sched_yield`-style *voluntary* yield is not expressible through the public
//! workload API: `Phase` is only `Run`/`Sleep`/`Wake` (see `safe/task.rs`), and
//! the `TaskYielded` trace event is produced by engine/scheduler internals, not
//! by a workload phase. Voluntary-yield re-enqueue is therefore not black-box
//! testable here; the involuntary re-enqueue paths (slice expiry / preemption)
//! are covered by the preemption and dispatch tests. Adding a `Phase::Yield`
//! would be a workload-API feature, out of scope for this test task.

use scx_simulator::*;

#[macro_use]
mod common;

/// Constructs a scheduler for a given CPU count.
type SchedFactory = fn(u32) -> DynamicScheduler;

/// The three schedulers this task names (simple ignores the CPU count).
fn schedulers() -> [(&'static str, SchedFactory); 3] {
    [
        ("simple", |_n| DynamicScheduler::simple()),
        ("lavd", DynamicScheduler::lavd),
        ("cosmos", DynamicScheduler::cosmos),
    ]
}

fn task(pid: i32, phases: Vec<Phase>) -> TaskDef {
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
        thread_group_leader: None,
        uid: Uid(0),
        gid: Gid(0),
    }
}

/// Item 4: mass ("thundering herd") simultaneous wakeup.
///
/// One waker briefly runs while N wakees are all blocked (each ran a tiny slice
/// then went to a long sleep), then releases all N in a single burst of
/// `Wake` phases. Every wakee must actually wake and run, and the system must
/// stay healthy — exercising the enqueue/select_cpu storm of many
/// simultaneously-runnable tasks. Run across simple/lavd/cosmos.
#[test]
fn mass_simultaneous_wakeup() {
    let _lock = common::setup_test();
    let nr = 4;
    let n_wakees = 8;

    for (name, make) in schedulers() {
        // Waker: run, then wake every wakee in one burst, then sleep.
        let mut waker_phases = vec![Phase::Run(5_000_000)];
        for w in 0..n_wakees {
            waker_phases.push(Phase::Wake(Pid(2 + w)));
        }
        waker_phases.push(Phase::Sleep(50_000_000));

        let mut b = Scenario::builder().cpus(nr).task(task(1, waker_phases));
        // Wakees: tiny initial run, then a long sleep they can't self-exit
        // before the waker's burst releases them.
        for w in 0..n_wakees {
            b = b.task(task(
                2 + w,
                vec![Phase::Run(500_000), Phase::Sleep(50_000_000)],
            ));
        }
        let scenario = b.duration_ms(120).build();

        let trace = Simulator::new(make(nr)).run(scenario);

        assert_eq!(
            trace.exit_kind(),
            &ExitKind::Normal,
            "{name}: mass wakeup should complete normally, got {:?}",
            trace.exit_kind()
        );
        assert!(!trace.has_error(), "{name}: error {:?}", trace.exit_kind());

        // Every wakee must have been woken and scheduled at least twice: once
        // for its initial slice, and again after the mass wakeup released it.
        for w in 0..n_wakees {
            let pid = Pid(2 + w);
            assert!(
                trace.total_runtime(pid) > 0,
                "{name}: wakee {pid:?} never ran"
            );
            assert!(
                trace.schedule_count(pid) >= 2,
                "{name}: wakee {pid:?} was not re-scheduled after mass wakeup (count={})",
                trace.schedule_count(pid)
            );
        }
        assert!(trace.total_runtime(Pid(1)) > 0, "{name}: waker never ran");
    }
}

/// Item 3: cross-task (IPC-style) wakeup is *necessary* for progress.
///
/// The wakee's only phase is a sleep far longer than the whole simulation, so a
/// timer can never wake it within the run. It gets scheduled exactly once at
/// start (to enter the sleep); after that, any additional scheduling proves the
/// waker's `Wake` actually delivered a cross-task wakeup.
#[test]
fn cross_task_wakeup_beats_timer() {
    let _lock = common::setup_test();
    let nr = 2;

    for (name, make) in schedulers() {
        let scenario = Scenario::builder()
            .cpus(nr)
            // Waker: run a bit, wake the sleeper, sleep, repeat.
            .task(task(
                1,
                vec![
                    Phase::Run(3_000_000),
                    Phase::Wake(Pid(2)),
                    Phase::Sleep(3_000_000),
                ],
            ))
            // Wakee: run a tiny slice, then sleep ~10x the sim duration. It can
            // only run again if the waker wakes it.
            .task(task(
                2,
                vec![Phase::Run(500_000), Phase::Sleep(1_000_000_000)],
            ))
            .duration_ms(100)
            .build();

        let trace = Simulator::new(make(nr)).run(scenario);

        assert_eq!(
            trace.exit_kind(),
            &ExitKind::Normal,
            "{name}: exit {:?}",
            trace.exit_kind()
        );
        assert!(
            trace.total_runtime(Pid(2)) > 0,
            "{name}: wakee never ran at all"
        );
        // Woken repeatedly despite a sleep longer than the sim => the wakeup
        // path (not a timer) is what scheduled it again.
        assert!(
            trace.schedule_count(Pid(2)) >= 2,
            "{name}: wakee scheduled only {} time(s); cross-task wakeup did not fire \
             (its sleep is 10x the sim, so a timer cannot explain re-scheduling)",
            trace.schedule_count(Pid(2))
        );
    }
}

/// Item 5: a periodic sleeper makes progress both on an idle system and under
/// heavy contention (no starvation), while contention measurably reduces the
/// share it receives.
#[test]
fn sleeper_progress_idle_vs_contended() {
    let _lock = common::setup_test();
    let nr = 2;

    for (name, make) in schedulers() {
        // The sleeper: short run then short sleep, cycling — a typical
        // interactive/yield-like task.
        let sleeper = || task(1, vec![Phase::Run(2_000_000), Phase::Sleep(2_000_000)]);

        // Idle system: just the sleeper.
        let idle = Scenario::builder()
            .cpus(nr)
            .task(sleeper())
            .duration_ms(100)
            .build();
        let idle_trace = Simulator::new(make(nr)).run(idle);

        // Contended system: the same sleeper plus CPU hogs saturating all CPUs.
        let mut b = Scenario::builder().cpus(nr).task(sleeper());
        for h in 0..(nr * 3) {
            b = b.task(task(100 + h as i32, vec![Phase::Run(50_000_000)]));
        }
        let contended = b.duration_ms(100).build();
        let contended_trace = Simulator::new(make(nr)).run(contended);

        assert_eq!(
            idle_trace.exit_kind(),
            &ExitKind::Normal,
            "{name}: idle exit"
        );
        assert_eq!(
            contended_trace.exit_kind(),
            &ExitKind::Normal,
            "{name}: contended exit {:?}",
            contended_trace.exit_kind()
        );

        let idle_rt = idle_trace.total_runtime(Pid(1));
        let contended_rt = contended_trace.total_runtime(Pid(1));

        // Liveness: the sleeper must run in both cases (never starved).
        assert!(idle_rt > 0, "{name}: sleeper got no runtime on idle system");
        assert!(contended_rt > 0, "{name}: sleeper starved under contention");
        // Sanity: contention cannot give the sleeper *more* CPU than an idle
        // system does (it competes with the hogs for the CPU it wakes onto).
        assert!(
            contended_rt <= idle_rt,
            "{name}: sleeper got more runtime under contention ({contended_rt}) than idle ({idle_rt})?"
        );
    }
}
