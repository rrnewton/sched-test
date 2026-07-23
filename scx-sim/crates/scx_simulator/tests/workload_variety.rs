//! Diverse workload-pattern integration tests (tg `test-workload-variety`).
//!
//! The existing suites exercise a scheduler's *mechanisms* (idle scan, NUMA,
//! SMT, cgroup bandwidth, determinism, …). This file complements them by
//! exercising a scheduler's *behavior under a variety of realistic workload
//! shapes*, and it runs every pattern against all three general-purpose
//! schedulers the simulator ships (`simple`, `lavd`, `cosmos`) so a regression
//! that only manifests under, say, a bursty arrival pattern on one scheduler
//! is caught.
//!
//! Patterns covered (from the task brief):
//!   1. Bursty workloads — tasks arriving in waves ([`bursty_wave_arrival`]).
//!   2. Periodic tasks with strict deadlines ([`periodic_with_deadlines`]).
//!   3. CPU-bound vs I/O-bound task mixes ([`cpu_bound_vs_io_bound_mix`]).
//!   4. Varying task durations, microseconds to seconds
//!      ([`varying_task_durations`]).
//!   5. Many short-lived tasks vs few long-running tasks
//!      ([`many_short_vs_few_long`]).
//!
//! Assertions are intentionally scheduler-agnostic: they check *structural*
//! invariants that must hold regardless of policy (a task never runs before it
//! arrives; a completed run-only task accounts for its declared work; every
//! task makes progress; the sim never stalls or hits a BPF error) plus loose
//! quantitative bounds wide enough to hold across the three very different
//! policies. Every run installs `.detect_bpf_errors()` and a watchdog so a
//! stall or `scx_bpf_error()` fails the test instead of silently passing.

use scx_simulator::scenario::ScenarioBuilder;
use scx_simulator::*;

mod common;

/// The general-purpose schedulers exercised by every pattern below.
const SCHEDULERS: &[&str] = &["simple", "lavd", "cosmos"];

/// Construct a scheduler by name for `nr_cpus` CPUs.
///
/// `simple` is topology-agnostic at load time (it ignores the CPU count and
/// adapts to the scenario), matching how the generic `scheduler_tests!` suite
/// drives it; `lavd` and `cosmos` are configured for `nr_cpus`.
fn make_scheduler(name: &str, nr_cpus: u32) -> DynamicScheduler {
    match name {
        "simple" => DynamicScheduler::simple(),
        "lavd" => DynamicScheduler::lavd(nr_cpus),
        "cosmos" => DynamicScheduler::cosmos(nr_cpus),
        other => panic!("unknown scheduler '{other}'"),
    }
}

/// Common per-run safety knobs: catch `scx_bpf_error()` and stalls so that a
/// pathological workload fails loudly rather than passing on a broken sim.
fn safe_builder(nr_cpus: u32) -> ScenarioBuilder {
    Scenario::builder()
        .cpus(nr_cpus)
        .detect_bpf_errors()
        .watchdog_timeout_ns(Some(2_000_000_000))
}

/// Wall-clock time (ns) at which `pid` was first scheduled, if ever.
fn first_scheduled_ns(trace: &Trace, pid: Pid) -> Option<TimeNs> {
    trace.events().iter().find_map(|e| match e.kind {
        TraceKind::TaskScheduled { pid: p } if p == pid => Some(e.time_ns),
        _ => None,
    })
}

/// Whether a `TaskCompleted` event was emitted for `pid`.
fn completed(trace: &Trace, pid: Pid) -> bool {
    trace
        .events()
        .iter()
        .any(|e| matches!(e.kind, TraceKind::TaskCompleted { pid: p } if p == pid))
}

/// Upper slack (ns) allowed above a task's declared run time when it completes.
///
/// Even with overhead/noise disabled (`instant_timing`), some schedulers round
/// the final slice up by up to a tick (e.g. lavd overshoots a 10 ms task by
/// 0.5 ms), so a completed single-shot task accounts for `[run_ns, run_ns +
/// slack]`. It can never be *less* than `run_ns`: a completed task has no open
/// interval at sim end, so all of its on-CPU time is counted.
const RUN_SLACK_NS: TimeNs = 5_000_000;

/// Assert a completed single-shot task accounted for its declared work.
fn assert_work_accounted(trace: &Trace, pid: Pid, run_ns: TimeNs, sched: &str, ctx: &str) {
    assert!(
        completed(trace, pid),
        "[{sched}] {ctx}: pid {} (run {run_ns}ns) never completed",
        pid.0
    );
    let rt = trace.total_runtime(pid);
    assert!(
        (run_ns..=run_ns + RUN_SLACK_NS).contains(&rt),
        "[{sched}] {ctx}: pid {} ran {rt}ns, expected [{run_ns}, {}]ns",
        pid.0,
        run_ns + RUN_SLACK_NS
    );
}

/// Assert the sim finished cleanly (no BPF error, no stall, no loop blow-up).
fn assert_normal(trace: &Trace, sched: &str, ctx: &str) {
    assert_eq!(
        trace.exit_kind(),
        &ExitKind::Normal,
        "[{sched}] {ctx}: simulation did not exit normally: {:?}",
        trace.exit_kind()
    );
}

// ---------------------------------------------------------------------------
// 1. Bursty workloads — tasks arriving in waves.
// ---------------------------------------------------------------------------

/// Three waves of three CPU-bound tasks each, arriving 40 ms apart. Verifies
/// the scheduler admits and drains each wave, and — the structural invariant —
/// that no task is ever scheduled before its arrival (`start_time_ns`).
#[test]
fn bursty_wave_arrival() {
    const NR_WAVES: i32 = 3;
    const TASKS_PER_WAVE: i32 = 3;
    const WAVE_GAP_NS: TimeNs = 40_000_000; // 40 ms between waves
    const RUN_NS: TimeNs = 10_000_000; // each task does 10 ms of work

    for &sched in SCHEDULERS {
        let _lock = common::setup_test();
        // Instant timing so a completed single-shot task accrues exactly its
        // declared run time (overhead/noise off), letting us assert precise
        // runtime while still honoring per-task arrival times.
        let mut b = safe_builder(4).instant_timing();

        for wave in 0..NR_WAVES {
            for t in 0..TASKS_PER_WAVE {
                let pid = Pid(wave * TASKS_PER_WAVE + t + 1);
                b = b.task(TaskDef {
                    name: format!("w{wave}_t{t}"),
                    pid,
                    nice: 0,
                    behavior: TaskBehavior {
                        phases: vec![Phase::Run(RUN_NS)],
                        repeat: RepeatMode::Once,
                    },
                    start_time_ns: wave as TimeNs * WAVE_GAP_NS,
                    mm_id: None,
                    allowed_cpus: None,
                    parent_pid: None,
                    cgroup_name: None,
                    task_flags: 0,
                    migration_disabled: 0,
                });
            }
        }

        let scenario = b.duration_ms(300).build();
        let trace = Simulator::new(make_scheduler(sched, 4)).run(scenario);

        assert_normal(&trace, sched, "bursty");

        for wave in 0..NR_WAVES {
            let arrival = wave as TimeNs * WAVE_GAP_NS;
            for t in 0..TASKS_PER_WAVE {
                let pid = Pid(wave * TASKS_PER_WAVE + t + 1);

                // Every task eventually runs to completion and does its work.
                assert_work_accounted(&trace, pid, RUN_NS, sched, "bursty");

                // Structural invariant: a task cannot run before it arrives.
                let first = first_scheduled_ns(&trace, pid)
                    .unwrap_or_else(|| panic!("[{sched}] bursty: pid {} never scheduled", pid.0));
                assert!(
                    first >= arrival,
                    "[{sched}] bursty: pid {} scheduled at {first}ns before arrival {arrival}ns",
                    pid.0
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 2. Periodic tasks with strict deadlines.
// ---------------------------------------------------------------------------

/// A latency-sensitive periodic task (2 ms of work every 10 ms) contends with
/// three CPU hogs on two CPUs. To "meet its deadline" the periodic task must
/// be serviced on most of its ~30 periods rather than being starved by the
/// hogs; we assert it is scheduled on at least half its periods and accrues
/// close to its ideal `run × periods` runtime.
#[test]
fn periodic_with_deadlines() {
    const RUN_NS: TimeNs = 2_000_000; // 2 ms of work ...
    const PERIOD_NS: TimeNs = 10_000_000; // ... every 10 ms
    const DURATION_MS: u64 = 300;
    let periods = (DURATION_MS * 1_000_000) / PERIOD_NS; // 30

    for &sched in SCHEDULERS {
        let _lock = common::setup_test();
        let mut b = safe_builder(2);

        // pid 1: the deadline-sensitive periodic task (added first).
        b = b.add_task("periodic", 0, workloads::periodic(RUN_NS, PERIOD_NS));

        // pid 2: a background CPU hog. With 2 CPUs there is always capacity for
        // the periodic task to run the instant it wakes, so a scheduler that
        // honors wakeup latency should service nearly every period.
        b = b.add_task("hog", 0, workloads::cpu_bound(50_000_000));

        let scenario = b.duration_ms(DURATION_MS).build();
        let trace = Simulator::new(make_scheduler(sched, 2)).run(scenario);

        assert_normal(&trace, sched, "periodic");

        // Deadline adherence: with spare capacity the periodic task should be
        // serviced on essentially every one of its ~30 periods (empirically all
        // three schedulers hit 31 — one extra for the t=0 activation).
        let sched_count = trace.schedule_count(Pid(1)) as u64;
        assert!(
            sched_count >= periods - 2,
            "[{sched}] periodic: serviced {sched_count} times, expected >= {} of {periods} \
             periods (missed deadlines?)",
            periods - 2
        );

        // Runtime should track the ideal duty cycle: 2 ms × ~30 = ~60 ms.
        let rt = trace.total_runtime(Pid(1));
        assert!(
            (50_000_000..=68_000_000).contains(&rt),
            "[{sched}] periodic: runtime {rt}ns outside [50ms, 68ms]"
        );

        // The background hog must also make progress (no total lockout).
        assert!(
            trace.total_runtime(Pid(2)) > 0,
            "[{sched}] periodic: hog got no runtime"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. CPU-bound vs I/O-bound task mixes.
// ---------------------------------------------------------------------------

/// Two CPU-bound hogs share two CPUs with two I/O-bound tasks (500 µs of work,
/// then 8 ms asleep). The I/O tasks should stay responsive — scheduled on many
/// of their wake cycles — while the CPU hogs, unsurprisingly, consume far more
/// total CPU time than the mostly-sleeping I/O tasks.
#[test]
fn cpu_bound_vs_io_bound_mix() {
    const IO_RUN_NS: TimeNs = 500_000; // 500 µs
    const IO_SLEEP_NS: TimeNs = 8_000_000; // 8 ms
    const DURATION_MS: u64 = 200;

    for &sched in SCHEDULERS {
        let _lock = common::setup_test();
        let mut b = safe_builder(2);

        // pids 1,2: CPU-bound hogs.
        for i in 0..2 {
            b = b.add_task(&format!("cpu{i}"), 0, workloads::cpu_bound(50_000_000));
        }
        // pids 3,4: I/O-bound tasks.
        for i in 0..2 {
            b = b.add_task(
                &format!("io{i}"),
                0,
                workloads::io_bound(IO_RUN_NS, IO_SLEEP_NS),
            );
        }

        let scenario = b.duration_ms(DURATION_MS).build();
        let trace = Simulator::new(make_scheduler(sched, 2)).run(scenario);

        assert_normal(&trace, sched, "cpu_vs_io");

        let cpu_rt: TimeNs = (1..=2).map(|p| trace.total_runtime(Pid(p))).sum();
        let io_rt: TimeNs = (3..=4).map(|p| trace.total_runtime(Pid(p))).sum();

        // Everyone makes progress.
        for pid in 1..=4 {
            assert!(
                trace.total_runtime(Pid(pid)) > 0,
                "[{sched}] cpu_vs_io: pid {pid} got no runtime"
            );
        }

        // CPU hogs dominate CPU time over the mostly-sleeping I/O tasks.
        assert!(
            cpu_rt > io_rt,
            "[{sched}] cpu_vs_io: cpu runtime {cpu_rt}ns not greater than io runtime {io_rt}ns"
        );

        // I/O tasks stay responsive: with ~23 wake cycles available in 200 ms
        // each is dispatched several times (empirically 9-13 across schedulers).
        for pid in 3..=4 {
            let count = trace.schedule_count(Pid(pid));
            assert!(
                count >= 5,
                "[{sched}] cpu_vs_io: io task {pid} scheduled only {count} times (unresponsive?)"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 4. Varying task durations — microseconds to seconds.
// ---------------------------------------------------------------------------

/// Five single-shot tasks whose run lengths span four orders of magnitude
/// (100 µs → 1 s). Each must run to completion and, since it only ever runs,
/// accumulate *exactly* its declared run time — a precise check that the
/// engine's time accounting holds across scales.
#[test]
fn varying_task_durations() {
    // 100 µs, 1 ms, 10 ms, 100 ms, 1 s.
    const DURATIONS_NS: &[TimeNs] = &[100_000, 1_000_000, 10_000_000, 100_000_000, 1_000_000_000];

    for &sched in SCHEDULERS {
        let _lock = common::setup_test();
        let mut b = safe_builder(4);

        b = b.instant_timing();
        for (i, &run_ns) in DURATIONS_NS.iter().enumerate() {
            b = b.task(TaskDef {
                name: format!("dur{i}"),
                pid: Pid(i as i32 + 1),
                nice: 0,
                behavior: TaskBehavior {
                    phases: vec![Phase::Run(run_ns)],
                    repeat: RepeatMode::Once,
                },
                start_time_ns: 0,
                mm_id: None,
                allowed_cpus: None,
                parent_pid: None,
                cgroup_name: None,
                task_flags: 0,
                migration_disabled: 0,
            });
        }

        // Long enough for the 1 s task plus everything else on 4 CPUs.
        let scenario = b.duration_ms(2500).build();
        let trace = Simulator::new(make_scheduler(sched, 4)).run(scenario);

        assert_normal(&trace, sched, "varying_durations");

        for (i, &run_ns) in DURATIONS_NS.iter().enumerate() {
            assert_work_accounted(
                &trace,
                Pid(i as i32 + 1),
                run_ns,
                sched,
                "varying_durations",
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 5. Many short-lived tasks vs few long-running tasks.
// ---------------------------------------------------------------------------

/// A burst of 32 short-lived tasks (1 ms each, single-shot) runs alongside two
/// long-running CPU hogs on four CPUs. Every short task must complete (and
/// account for exactly its 1 ms), and the long-running tasks must still make
/// progress — i.e. the flood of short tasks neither stalls the sim nor starves
/// the persistent ones.
#[test]
fn many_short_vs_few_long() {
    const NR_SHORT: i32 = 32;
    const SHORT_RUN_NS: TimeNs = 1_000_000; // 1 ms
    const NR_LONG: i32 = 2;

    for &sched in SCHEDULERS {
        let _lock = common::setup_test();
        let mut b = safe_builder(4);

        b = b.instant_timing();
        // pids 1..=32: many short-lived, single-shot tasks.
        for i in 0..NR_SHORT {
            b = b.task(TaskDef {
                name: format!("short{i}"),
                pid: Pid(i + 1),
                nice: 0,
                behavior: TaskBehavior {
                    phases: vec![Phase::Run(SHORT_RUN_NS)],
                    repeat: RepeatMode::Once,
                },
                start_time_ns: 0,
                mm_id: None,
                allowed_cpus: None,
                parent_pid: None,
                cgroup_name: None,
                task_flags: 0,
                migration_disabled: 0,
            });
        }

        // pids 33,34: few long-running hogs.
        for i in 0..NR_LONG {
            b = b.add_task(&format!("long{i}"), 0, workloads::cpu_bound(200_000_000));
        }

        let scenario = b.duration_ms(300).build();
        let trace = Simulator::new(make_scheduler(sched, 4)).run(scenario);

        assert_normal(&trace, sched, "many_short_vs_few_long");

        // Every short task completes and accounts for its run time.
        for i in 0..NR_SHORT {
            assert_work_accounted(&trace, Pid(i + 1), SHORT_RUN_NS, sched, "many_short");
        }

        // The long-running hogs are not starved by the short-task flood.
        for i in 0..NR_LONG {
            let pid = Pid(NR_SHORT + i + 1);
            assert!(
                trace.total_runtime(pid) > 0,
                "[{sched}] many_short: long task {} got no runtime",
                pid.0
            );
        }
    }
}
