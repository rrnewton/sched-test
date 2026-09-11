//! Tests for the core scheduling hot path — enqueue, dispatch, and
//! dequeue/stopping — across the three supported schedulers (simple, lavd,
//! cosmos), plus scaling across task counts and a documented-behavior check
//! of weighted-vtime priority dispatch.
//!
//! These complement the existing per-scheduler suites (e.g. `simple.rs`'s
//! weighted-fairness tests) by asserting scheduler-agnostic invariants on the
//! full enqueue→dispatch→dequeue event vocabulary and CPU-selection/fairness
//! behavior across all three schedulers.

use std::collections::HashSet;

use scx_simulator::*;

#[macro_use]
mod common;

/// Run `make()` under each scheduler on `cpus` CPUs, assert no error, and hand
/// the trace to `check`. One C scheduler instance is alive at a time.
fn for_each_scheduler(cpus: u32, make: impl Fn() -> Scenario, check: impl Fn(&Trace, &str)) {
    for label in ["simple", "lavd", "cosmos"] {
        let sched = match label {
            "simple" => DynamicScheduler::simple(),
            "lavd" => DynamicScheduler::lavd(cpus),
            "cosmos" => DynamicScheduler::cosmos(cpus),
            _ => unreachable!(),
        };
        let trace = Simulator::new(sched).run(make());
        assert!(
            !trace.has_error(),
            "[{label}] simulation error: {:?}",
            trace.exit_kind()
        );
        check(&trace, label);
    }
}

fn task(name: &str, pid: i32, nice: i8, behavior: TaskBehavior) -> TaskDef {
    TaskDef {
        name: name.into(),
        pid: Pid(pid),
        nice,
        behavior,
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
        fork_cpu: None,
    }
}

/// Count trace events matching a `TraceKind` predicate.
fn count(trace: &Trace, pred: impl Fn(&TraceKind) -> bool) -> usize {
    trace.events().iter().filter(|e| pred(&e.kind)).count()
}

// ---------------------------------------------------------------------------
// 1. Enqueue → dispatch → dequeue: the full hot-path vocabulary is exercised
//    and traced for every scheduler.
// ---------------------------------------------------------------------------

#[test]
fn test_enqueue_dispatch_dequeue_vocabulary() {
    let _lock = common::setup_test();
    // A run/sleep task (drives enqueue on wake + dequeue on sleep) and a
    // one-shot task (drives completion), on multiple CPUs.
    let make = || {
        Scenario::builder()
            .cpus(4)
            .seed(101)
            .detect_bpf_errors()
            .task(task(
                "cycler",
                1,
                0,
                TaskBehavior {
                    phases: vec![Phase::Run(2_000_000), Phase::Sleep(2_000_000)],
                    repeat: RepeatMode::Forever,
                },
            ))
            .task(task(
                "oneshot",
                2,
                0,
                TaskBehavior {
                    phases: vec![Phase::Run(3_000_000)],
                    repeat: RepeatMode::Once,
                },
            ))
            .task(task("hog", 3, 0, workloads::cpu_bound(20_000_000)))
            .duration_ms(120)
            .build()
    };

    for_each_scheduler(4, make, |trace, label| {
        // ENQUEUE path: tasks are enqueued and inserted into a DSQ.
        let enqueues = count(trace, |k| matches!(k, TraceKind::EnqueueTask { .. }));
        assert!(enqueues > 0, "[{label}] no EnqueueTask events");
        let dsq_inserts: usize = (1..=3).map(|p| trace.dsq_insert_count(Pid(p))).sum();
        assert!(dsq_inserts > 0, "[{label}] no DSQ insertions recorded");

        // DISPATCH path: tasks are scheduled onto CPUs.
        for pid in 1..=3 {
            assert!(
                trace.schedule_count(Pid(pid)) > 0,
                "[{label}] task pid={pid} was never dispatched"
            );
        }

        // DEQUEUE/stopping path: the cycler sleeps (voluntary dequeue) and the
        // one-shot completes.
        let dequeues = count(trace, |k| {
            matches!(
                k,
                TraceKind::Dequeue { .. }
                    | TraceKind::Quiescent { .. }
                    | TraceKind::TaskSlept { .. }
            )
        });
        assert!(dequeues > 0, "[{label}] no dequeue/stopping events");
        let completed = count(
            trace,
            |k| matches!(k, TraceKind::TaskCompleted { pid } if *pid == Pid(2)),
        );
        assert!(completed > 0, "[{label}] one-shot task never completed");
    });
}

// ---------------------------------------------------------------------------
// 2. Dispatch — CPU selection spreads runnable tasks across CPUs.
// ---------------------------------------------------------------------------

#[test]
fn test_dispatch_spreads_across_cpus() {
    let _lock = common::setup_test();
    // 8 always-runnable hogs on 4 CPUs — every CPU should be used.
    let make = || {
        let mut b = Scenario::builder().cpus(4).seed(202).detect_bpf_errors();
        for i in 0..8i32 {
            b = b.task(task(
                &format!("hog{i}"),
                1 + i,
                0,
                workloads::cpu_bound(50_000_000),
            ));
        }
        b.duration_ms(150).build()
    };

    for_each_scheduler(4, make, |trace, label| {
        let cpus_used: HashSet<CpuId> = trace
            .events()
            .iter()
            .filter_map(|e| match e.kind {
                TraceKind::TaskScheduled { .. } => Some(e.cpu),
                _ => None,
            })
            .collect();
        assert!(
            cpus_used.len() >= 3,
            "[{label}] dispatch used only {} of 4 CPUs: {:?}",
            cpus_used.len(),
            cpus_used
        );
    });
}

// ---------------------------------------------------------------------------
// 3. Dispatch — equal-weight fairness: equal-priority hogs get comparable
//    runtime (no single task monopolizes the CPU).
// ---------------------------------------------------------------------------

#[test]
fn test_dispatch_equal_weight_fairness() {
    let _lock = common::setup_test();
    // 4 equal-nice hogs sharing 2 CPUs.
    let make = || {
        let mut b = Scenario::builder().cpus(2).seed(303).detect_bpf_errors();
        for i in 0..4i32 {
            b = b.task(task(
                &format!("hog{i}"),
                1 + i,
                0,
                workloads::cpu_bound(100_000_000),
            ));
        }
        b.duration_ms(200).build()
    };

    for_each_scheduler(2, make, |trace, label| {
        let rts: Vec<u64> = (1..=4).map(|p| trace.total_runtime(Pid(p))).collect();
        let min = *rts.iter().min().unwrap();
        let max = *rts.iter().max().unwrap();
        assert!(min > 0, "[{label}] a hog got zero runtime: {rts:?}");
        // Equal-weight tasks should be within a generous fairness ratio
        // (allows scheduler-specific batching over this finite window — cosmos
        // is the least even — but catches gross starvation/monopolization).
        assert!(
            max <= min * 8,
            "[{label}] unfair dispatch: runtimes {rts:?} (max {max} > 8x min {min})"
        );
    });
}

// ---------------------------------------------------------------------------
// 4. Dequeue/stopping — voluntary sleep and completion both dequeue the task.
// ---------------------------------------------------------------------------

#[test]
fn test_dequeue_on_sleep_and_completion() {
    let _lock = common::setup_test();
    let make = || {
        Scenario::builder()
            .cpus(2)
            .seed(404)
            .detect_bpf_errors()
            // Sleeps repeatedly → repeated voluntary dequeues.
            .task(task(
                "sleeper",
                1,
                0,
                TaskBehavior {
                    phases: vec![Phase::Run(1_000_000), Phase::Sleep(3_000_000)],
                    repeat: RepeatMode::Forever,
                },
            ))
            // Runs a few times then exits → completion dequeue.
            .task(task(
                "finite",
                2,
                0,
                TaskBehavior {
                    phases: vec![Phase::Run(2_000_000), Phase::Sleep(1_000_000)],
                    repeat: RepeatMode::Count(3),
                },
            ))
            .duration_ms(120)
            .build()
    };

    for_each_scheduler(2, make, |trace, label| {
        // The sleeper must be dispatched more than once (woken repeatedly),
        // which requires it to be dequeued and re-enqueued each cycle.
        assert!(
            trace.schedule_count(Pid(1)) >= 3,
            "[{label}] sleeper scheduled {} times (expected repeated wake/dequeue cycles)",
            trace.schedule_count(Pid(1))
        );
        // Sleep events (the voluntary-yield dequeue trigger) must appear.
        let slept = count(trace, |k| matches!(k, TraceKind::TaskSlept { .. }));
        assert!(
            slept > 0,
            "[{label}] no TaskSlept (voluntary dequeue) events"
        );
        // The finite task must complete.
        let completed = count(
            trace,
            |k| matches!(k, TraceKind::TaskCompleted { pid } if *pid == Pid(2)),
        );
        assert!(completed > 0, "[{label}] finite task never completed");
    });
}

// ---------------------------------------------------------------------------
// 5. Documented behavior: weighted-vtime priority dispatch (default mode).
//
// scx_lavd and scx_simple both order the shared runqueue by a weight-scaled
// virtual deadline/vtime, so a higher-priority (lower-nice) CPU hog must accrue
// MORE runtime than a low-priority one sharing the CPU. (scx_simple's own
// weighted-vtime ratio is checked precisely in simple.rs::test_weighted_fairness;
// here we assert the documented direction holds for lavd too.)
//
// Note: scx_simple's alternate FIFO mode (`fifo_sched`) is NOT exercised here
// because that global lives in read-only .rodata in the simple .so (simple is
// built without `-Dconst=`, unlike lavd/cosmos), so it cannot be set from a
// test — see task notes.
// ---------------------------------------------------------------------------

#[test]
fn test_lavd_vtime_favors_priority() {
    let _lock = common::setup_test();
    // Two CPU hogs sharing one CPU: nice -15 vs nice +15.
    let scenario = Scenario::builder()
        .cpus(1)
        .seed(505)
        .detect_bpf_errors()
        .task(task("hi", 1, -15, workloads::cpu_bound(200_000_000)))
        .task(task("lo", 2, 15, workloads::cpu_bound(200_000_000)))
        .duration_ms(200)
        .build();
    let trace = Simulator::new(DynamicScheduler::lavd(1)).run(scenario);
    assert!(!trace.has_error(), "lavd error: {:?}", trace.exit_kind());
    let (hi, lo) = (trace.total_runtime(Pid(1)), trace.total_runtime(Pid(2)));
    assert!(hi > 0 && lo > 0, "a task starved: hi={hi} lo={lo}");
    assert!(
        hi > lo,
        "lavd should favor the higher-priority (lower-nice) task: hi={hi} lo={lo}"
    );
}

// ---------------------------------------------------------------------------
// 6. Scaling — the enqueue/dispatch path handles 1, 10, 100, and 1000 tasks.
// ---------------------------------------------------------------------------

#[test]
fn test_dispatch_scaling_task_counts() {
    let _lock = common::setup_test();
    for &n in &[1usize, 10, 100, 1000] {
        let make = move || {
            let mut b = Scenario::builder().cpus(4).seed(606).detect_bpf_errors();
            for i in 0..n as i32 {
                // Short one-shot tasks: stress the enqueue/dispatch path, then
                // drain, so even 1000 tasks complete quickly.
                b = b.task(task(
                    &format!("t{i}"),
                    1 + i,
                    0,
                    TaskBehavior {
                        phases: vec![Phase::Run(200_000)],
                        repeat: RepeatMode::Once,
                    },
                ));
            }
            b.duration_ms(2000).build()
        };

        for_each_scheduler(4, make, |trace, label| {
            // Aggregate: work was dispatched and completed for this task count.
            let total_sched: usize = (0..n as i32)
                .map(|i| trace.schedule_count(Pid(1 + i)))
                .sum();
            assert!(
                total_sched >= n,
                "[{label}] n={n}: only {total_sched} dispatches for {n} tasks"
            );
            let completed = count(trace, |k| matches!(k, TraceKind::TaskCompleted { .. }));
            assert!(
                completed >= n,
                "[{label}] n={n}: only {completed}/{n} tasks completed"
            );
        });
    }
}
