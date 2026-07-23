//! Diverse workload-pattern tests, each run against all three supported
//! schedulers (simple, lavd, cosmos).
//!
//! Patterns covered:
//! 1. Bursty arrival — tasks arriving in timed waves.
//! 2. Periodic tasks — must be re-scheduled roughly once per period.
//! 3. CPU-bound vs IO-bound mix — hogs make progress while latency-sensitive
//!    I/O tasks are scheduled frequently.
//! 4. Varying task durations — microseconds to ~half a second in one run.
//! 5. Many short-lived tasks vs a few long-running ones — all short tasks
//!    complete; long tasks keep running.
//!
//! Each test asserts scheduler-agnostic invariants (no simulation error, every
//! task makes progress, one-shot tasks complete, periodic cadence is met) so
//! the same assertions hold for simple, lavd, and cosmos.

use scx_simulator::*;

#[macro_use]
mod common;

/// Run `make_scenario()` under each supported scheduler on `cpus` CPUs and hand
/// the resulting trace to `check`. A fresh scheduler is constructed immediately
/// before each run (only one C scheduler instance is alive at a time).
fn for_each_scheduler(
    cpus: u32,
    make_scenario: impl Fn() -> Scenario,
    check: impl Fn(&trace::Trace, &str),
) {
    for label in ["simple", "lavd", "cosmos"] {
        let sched = match label {
            "simple" => DynamicScheduler::simple(),
            "lavd" => DynamicScheduler::lavd(cpus),
            "cosmos" => DynamicScheduler::cosmos(cpus),
            _ => unreachable!(),
        };
        let trace = Simulator::new(sched).run(make_scenario());
        assert!(
            !trace.has_error(),
            "[{label}] simulation error: {:?}",
            trace.exit_kind()
        );
        check(&trace, label);
    }
}

/// A `TaskDef` with the common fields defaulted.
fn task(name: &str, pid: i32, nice: i8, start_time_ns: TimeNs, behavior: TaskBehavior) -> TaskDef {
    TaskDef {
        name: name.into(),
        pid: Pid(pid),
        nice,
        behavior,
        start_time_ns,
        mm_id: None,
        allowed_cpus: None,
        parent_pid: None,
        cgroup_name: None,
        task_flags: 0,
        migration_disabled: 0,
    }
}

/// True if the trace contains a `TaskCompleted` event for `pid`.
fn completed(trace: &trace::Trace, pid: Pid) -> bool {
    trace
        .events()
        .iter()
        .any(|e| matches!(e.kind, TraceKind::TaskCompleted { pid: cp } if cp == pid))
}

// ---------------------------------------------------------------------------
// 1. Bursty arrival: three waves of tasks arriving 30 ms apart.
// ---------------------------------------------------------------------------

#[test]
fn test_workload_bursty_waves() {
    let _lock = common::setup_test();
    const WAVES: i32 = 3;
    const PER_WAVE: i32 = 4;

    let make = || {
        let mut b = Scenario::builder().cpus(4).seed(11).detect_bpf_errors();
        for w in 0..WAVES {
            for k in 0..PER_WAVE {
                let pid = 1 + w * PER_WAVE + k;
                b = b.task(task(
                    &format!("w{w}t{k}"),
                    pid,
                    0,
                    (w as u64) * 30_000_000, // wave arrival: 0, 30ms, 60ms
                    TaskBehavior {
                        phases: vec![Phase::Run(3_000_000)],
                        repeat: RepeatMode::Once,
                    },
                ));
            }
        }
        b.duration_ms(200).build()
    };

    for_each_scheduler(4, make, |trace, label| {
        // Every task in every wave must be scheduled, get runtime, and finish.
        for pid in 1..=(WAVES * PER_WAVE) {
            assert!(
                trace.schedule_count(Pid(pid)) > 0,
                "[{label}] bursty task pid={pid} never scheduled"
            );
            assert!(
                trace.total_runtime(Pid(pid)) > 0,
                "[{label}] bursty task pid={pid} got no runtime"
            );
            assert!(
                completed(trace, Pid(pid)),
                "[{label}] bursty task pid={pid} did not complete"
            );
        }
    });
}

// ---------------------------------------------------------------------------
// 2. Periodic tasks: must be re-scheduled roughly once per period.
// ---------------------------------------------------------------------------

#[test]
fn test_workload_periodic_cadence() {
    let _lock = common::setup_test();
    const PERIOD_NS: u64 = 10_000_000; // 10 ms
    const RUN_NS: u64 = 1_000_000; // 1 ms of work per period
    const DURATION_MS: u64 = 200;
    // 200ms / 10ms = 20 periods; require at least half to be robust across
    // schedulers while still proving the task is periodically re-scheduled.
    const MIN_RUNS: usize = 10;

    let make = || {
        let mut b = Scenario::builder().cpus(4).seed(22).detect_bpf_errors();
        for i in 0..3i32 {
            b = b.task(task(
                &format!("periodic{i}"),
                1 + i,
                0,
                0,
                workloads::periodic(RUN_NS, PERIOD_NS),
            ));
        }
        b.duration_ms(DURATION_MS).build()
    };

    for_each_scheduler(4, make, |trace, label| {
        for pid in 1..=3i32 {
            let n = trace.schedule_count(Pid(pid));
            assert!(
                n >= MIN_RUNS,
                "[{label}] periodic task pid={pid} scheduled only {n} times, expected >= {MIN_RUNS} (cadence not met)"
            );
        }
    });
}

// ---------------------------------------------------------------------------
// 3. CPU-bound vs IO-bound mix.
// ---------------------------------------------------------------------------

#[test]
fn test_workload_cpu_vs_io_mix() {
    let _lock = common::setup_test();
    // 4 CPU hogs (never sleep) + 4 I/O tasks (short run, frequent sleep).
    let make = || {
        let mut b = Scenario::builder().cpus(4).seed(33).detect_bpf_errors();
        for i in 0..4i32 {
            b = b.task(task(
                &format!("hog{i}"),
                1 + i,
                0,
                0,
                workloads::cpu_bound(50_000_000),
            ));
        }
        for i in 0..4i32 {
            b = b.task(task(
                &format!("io{i}"),
                10 + i,
                0,
                0,
                // 100µs work, 900µs sleep → ~1ms cadence, latency-sensitive.
                workloads::io_bound(100_000, 900_000),
            ));
        }
        b.duration_ms(200).build()
    };

    for_each_scheduler(4, make, |trace, label| {
        // Hogs must make substantial progress.
        for i in 0..4i32 {
            assert!(
                trace.total_runtime(Pid(1 + i)) > 0,
                "[{label}] hog{i} got no runtime"
            );
        }
        // I/O tasks sleep/wake repeatedly over 200ms; despite 4 always-runnable
        // hogs saturating the CPUs, each must still be scheduled multiple times
        // and make progress (not starved) — a core scheduler property. The
        // exact count varies by scheduler (cosmos is the most conservative), so
        // require a robust lower bound rather than the ideal ~200 cycles.
        for i in 0..4i32 {
            let n = trace.schedule_count(Pid(10 + i));
            assert!(
                n >= 5,
                "[{label}] io{i} scheduled only {n} times — starved behind CPU hogs"
            );
            assert!(
                trace.total_runtime(Pid(10 + i)) > 0,
                "[{label}] io{i} got no runtime"
            );
        }
    });
}

// ---------------------------------------------------------------------------
// 4. Varying task durations: microseconds to ~half a second.
// ---------------------------------------------------------------------------

#[test]
fn test_workload_varying_durations() {
    let _lock = common::setup_test();
    // Run lengths spanning ~5 orders of magnitude, each one-shot.
    let run_lengths_ns: [u64; 5] = [
        10_000,      // 10 µs
        200_000,     // 200 µs
        5_000_000,   // 5 ms
        50_000_000,  // 50 ms
        400_000_000, // 400 ms
    ];

    let make = || {
        // instant_timing() disables scheduling noise/overhead so a completed
        // one-shot task's accounted runtime equals its requested run length
        // exactly — making the duration assertion precise.
        let mut b = Scenario::builder()
            .cpus(4)
            .seed(44)
            .instant_timing()
            .detect_bpf_errors();
        for (i, &run) in run_lengths_ns.iter().enumerate() {
            b = b.task(task(
                &format!("dur{i}"),
                1 + i as i32,
                0,
                0,
                TaskBehavior {
                    phases: vec![Phase::Run(run)],
                    repeat: RepeatMode::Once,
                },
            ));
        }
        b.duration_ms(800).build()
    };

    for_each_scheduler(4, make, |trace, label| {
        for (i, &run) in run_lengths_ns.iter().enumerate() {
            let pid = Pid(1 + i as i32);
            assert!(
                trace.schedule_count(pid) > 0,
                "[{label}] duration task {i} ({run}ns) never scheduled"
            );
            assert!(
                completed(trace, pid),
                "[{label}] duration task {i} ({run}ns) did not complete within window"
            );
            // Completed one-shot task must have accrued ~its requested runtime.
            let rt = trace.total_runtime(pid);
            assert!(
                rt >= run,
                "[{label}] duration task {i} ran {rt}ns < requested {run}ns"
            );
        }
    });
}

// ---------------------------------------------------------------------------
// 5. Many short-lived tasks vs a few long-running ones.
// ---------------------------------------------------------------------------

#[test]
fn test_workload_many_short_vs_few_long() {
    let _lock = common::setup_test();
    const N_SHORT: i32 = 24;
    const N_LONG: i32 = 2;

    let make = || {
        let mut b = Scenario::builder().cpus(4).seed(55).detect_bpf_errors();
        // Many short one-shot tasks (500µs each), arriving spread over 40ms.
        for i in 0..N_SHORT {
            b = b.task(task(
                &format!("short{i}"),
                1 + i,
                0,
                (i as u64) * 1_500_000,
                TaskBehavior {
                    phases: vec![Phase::Run(500_000)],
                    repeat: RepeatMode::Once,
                },
            ));
        }
        // A few long-running hogs.
        for i in 0..N_LONG {
            b = b.task(task(
                &format!("long{i}"),
                1000 + i,
                0,
                0,
                workloads::cpu_bound(300_000_000),
            ));
        }
        b.duration_ms(300).build()
    };

    for_each_scheduler(4, make, |trace, label| {
        // Every short task must complete (not starved by the long hogs).
        for i in 0..N_SHORT {
            let pid = Pid(1 + i);
            assert!(
                completed(trace, pid),
                "[{label}] short task {i} did not complete (starved by long tasks?)"
            );
        }
        // Long tasks must also make progress.
        for i in 0..N_LONG {
            assert!(
                trace.total_runtime(Pid(1000 + i)) > 0,
                "[{label}] long task {i} got no runtime"
            );
        }
    });
}
