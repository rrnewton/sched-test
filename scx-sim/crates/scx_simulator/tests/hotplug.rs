//! CPU hotplug integration tests (tg `test-cpu-hotplug-simulation`).
//!
//! Exercises CPU online/offline and higher-priority-class release/acquire while
//! a workload is running, across all three general-purpose schedulers
//! (simple, lavd, cosmos).
//!
//! ## Relationship to existing coverage
//!
//! * `scx_ops_callbacks.rs` asserts the *callback observability* of hotplug
//!   (`cpu_online`/`cpu_offline`/`cpu_release`/`cpu_acquire`) but only for
//!   simple + lavd. This file covers **cosmos too** and focuses on
//!   *workload-level* outcomes (tasks keep making progress; the downed CPU
//!   goes quiet; the system never stalls or errors).
//! * `lavd.rs` has lavd-specific hotplug tests.
//! * The **task-migration-on-offline** case (a task on a CPU that goes offline
//!   is absorbed by the surviving CPUs and keeps running) was previously
//!   untested — [`task_migration_on_cpu_offline`] closes that gap.
//!
//! ## Hotplug mechanics used
//!
//! * `cpu_offline_at(cpu, t)` / `cpu_online_at(cpu, t)` — hotplug remove/add. On
//!   offline the engine drains the CPU's local DSQ and re-enqueues its tasks so
//!   the scheduler re-places them (kernel-faithful affinity break); on online it
//!   calls `ops.cpu_online`, emits `UpdateIdle{idle:true}`, and dispatches work.
//! * `cpu_preempt(cpu, release, acquire)` — a higher-priority class seizes the
//!   CPU at `release` (`ops.cpu_release`, preempting the running task) and hands
//!   it back at `acquire` (`ops.cpu_acquire`, emitting `UpdateIdle{idle:true}`).

use scx_simulator::*;

mod common;

/// A named scheduler constructor so each test can sweep all three schedulers.
type NamedSchedFactory = (&'static str, fn(u32) -> DynamicScheduler);

const SCHEDULERS: [NamedSchedFactory; 3] = [
    ("simple", |_n| DynamicScheduler::simple()),
    ("lavd", DynamicScheduler::lavd),
    ("cosmos", DynamicScheduler::cosmos),
];

/// A forever-running CPU-bound task.
fn hog() -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(500_000_000)],
        repeat: RepeatMode::Forever,
    }
}

/// Small margin after an offline instant to skip the single in-flight dispatch
/// that lands as the CPU is drained (~1.5 µs after the event).
const OFFLINE_MARGIN_NS: TimeNs = 2_000_000;

/// Whether any task was scheduled on `cpu` in the half-open window `[lo, hi)`.
fn scheduled_on_cpu_in(trace: &Trace, cpu: CpuId, lo: TimeNs, hi: TimeNs) -> bool {
    trace.events().iter().any(|e| {
        matches!(e.kind, TraceKind::TaskScheduled { .. })
            && e.cpu == cpu
            && e.time_ns >= lo
            && e.time_ns < hi
    })
}

/// Count `TaskScheduled` events on `cpu` at/after `from`.
fn schedules_on_cpu_from(trace: &Trace, cpu: CpuId, from: TimeNs) -> usize {
    trace
        .events()
        .iter()
        .filter(|e| {
            matches!(e.kind, TraceKind::TaskScheduled { .. }) && e.cpu == cpu && e.time_ns >= from
        })
        .count()
}

/// Runtime (ns) `pid` accrued strictly after `after` (intervals are clipped to
/// start no earlier than `after`; open intervals at sim end are not counted).
fn runtime_after(trace: &Trace, pid: Pid, after: TimeNs) -> TimeNs {
    let mut total = 0;
    let mut since: Option<TimeNs> = None;
    for e in trace.events() {
        match &e.kind {
            TraceKind::TaskScheduled { pid: p } if *p == pid => since = Some(e.time_ns),
            TraceKind::TaskPreempted { pid: p }
            | TraceKind::TaskSlept { pid: p }
            | TraceKind::TaskYielded { pid: p }
            | TraceKind::TaskCompleted { pid: p }
                if *p == pid =>
            {
                if let Some(start) = since.take() {
                    let lo = start.max(after);
                    if e.time_ns > lo {
                        total += e.time_ns - lo;
                    }
                }
            }
            _ => {}
        }
    }
    total
}

// ---------------------------------------------------------------------------
// 1. Taking a CPU offline during simulation.
// ---------------------------------------------------------------------------

/// Offlining a CPU mid-run must stop it from running tasks while the rest of the
/// system carries the workload forward: the sim exits normally, the offlined CPU
/// is quiet after the drain margin, scheduling continues on the survivors, and
/// no task is dropped (each accrued some runtime over the run).
#[test]
fn cpu_offline_during_simulation() {
    const OFF_AT: TimeNs = 40_000_000;
    const NR_TASKS: i32 = 4;
    const OFFLINE_CPU: CpuId = CpuId(2);

    for (name, make) in SCHEDULERS {
        let _lock = common::setup_test();
        let mut b = Scenario::builder().cpus(3);
        for i in 0..NR_TASKS {
            b = b.add_task(&format!("h{i}"), 0, hog());
        }
        let scenario = b
            .cpu_offline_at(OFFLINE_CPU, OFF_AT)
            .duration_ms(120)
            .build();

        let trace = Simulator::new(make(3)).run(scenario);

        assert_eq!(
            trace.exit_kind(),
            &ExitKind::Normal,
            "[{name}] offline run should exit normally: {:?}",
            trace.exit_kind()
        );

        // The offlined CPU runs nothing after the drain margin.
        assert!(
            !scheduled_on_cpu_in(&trace, OFFLINE_CPU, OFF_AT + OFFLINE_MARGIN_NS, u64::MAX),
            "[{name}] offlined CPU {OFFLINE_CPU:?} ran a task after going offline"
        );

        // Work continues on a surviving CPU after the offline.
        assert!(
            schedules_on_cpu_from(&trace, CpuId(0), OFF_AT) > 0,
            "[{name}] no scheduling on surviving CPU 0 after offline"
        );

        // No task was dropped by the hotplug event.
        for pid in 1..=NR_TASKS {
            assert!(
                trace.total_runtime(Pid(pid)) > 0,
                "[{name}] task {pid} got no runtime across the run"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 2. Bringing a CPU online during simulation.
// ---------------------------------------------------------------------------

/// A CPU that starts offline (so the system is oversubscribed) and is brought
/// online mid-run must be picked up: `ops.cpu_online` fires (observable as
/// `UpdateIdle{idle:true}`) and the newly-online CPU starts running tasks.
#[test]
fn cpu_online_during_simulation() {
    const OFF_AT: TimeNs = 3_000_000; // offline almost immediately
    const ON_AT: TimeNs = 50_000_000;
    const ONLINE_CPU: CpuId = CpuId(1);

    for (name, make) in SCHEDULERS {
        let _lock = common::setup_test();
        // 3 hogs on 2 CPUs, but CPU 1 is offline until 50 ms → oversubscribed on
        // CPU 0, so there is pending work for CPU 1 to absorb once it joins.
        let mut b = Scenario::builder().cpus(2);
        for i in 0..3 {
            b = b.add_task(&format!("h{i}"), 0, hog());
        }
        let scenario = b
            .cpu_offline_at(ONLINE_CPU, OFF_AT)
            .cpu_online_at(ONLINE_CPU, ON_AT)
            .duration_ms(120)
            .build();

        let trace = Simulator::new(make(2)).run(scenario);

        assert_eq!(
            trace.exit_kind(),
            &ExitKind::Normal,
            "[{name}] online run should exit normally: {:?}",
            trace.exit_kind()
        );

        // cpu_online callback observable as UpdateIdle(idle=true) at/after ON_AT.
        let online_idle = trace.events().iter().any(|e| {
            matches!(e.kind, TraceKind::UpdateIdle { cpu, idle: true } if cpu == ONLINE_CPU)
                && e.time_ns >= ON_AT
        });
        assert!(
            online_idle,
            "[{name}] no UpdateIdle(idle=true) on {ONLINE_CPU:?} after cpu_online at {ON_AT}"
        );

        // The newly-online CPU actually runs work afterward.
        assert!(
            schedules_on_cpu_from(&trace, ONLINE_CPU, ON_AT) > 0,
            "[{name}] {ONLINE_CPU:?} never ran a task after coming online"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. Task migration when a CPU goes offline.
// ---------------------------------------------------------------------------

/// When a CPU running a task goes offline, the task must migrate to a surviving
/// CPU and keep running — the workload is *not* stalled or dropped. With two
/// hogs on two CPUs (one task per CPU), offlining CPU 1 forces its task onto
/// CPU 0; substantial runtime must still accrue after the offline (proving the
/// migrated work continues), while CPU 1 stays idle.
#[test]
fn task_migration_on_cpu_offline() {
    const OFF_AT: TimeNs = 40_000_000;
    const OFFLINE_CPU: CpuId = CpuId(1);

    for (name, make) in SCHEDULERS {
        let _lock = common::setup_test();
        let scenario = Scenario::builder()
            .cpus(2)
            .add_task("a", 0, hog())
            .add_task("b", 0, hog())
            .cpu_offline_at(OFFLINE_CPU, OFF_AT)
            .duration_ms(120)
            .build();

        let trace = Simulator::new(make(2)).run(scenario);

        assert_eq!(
            trace.exit_kind(),
            &ExitKind::Normal,
            "[{name}] migration run should exit normally: {:?}",
            trace.exit_kind()
        );

        // The offlined CPU is quiet after the drain margin.
        assert!(
            !scheduled_on_cpu_in(&trace, OFFLINE_CPU, OFF_AT + OFFLINE_MARGIN_NS, u64::MAX),
            "[{name}] offlined CPU still ran tasks after offline"
        );

        // Both tasks ran at some point (neither was dropped).
        assert!(
            trace.total_runtime(Pid(1)) > 0 && trace.total_runtime(Pid(2)) > 0,
            "[{name}] a task got no runtime (a={}, b={})",
            trace.total_runtime(Pid(1)),
            trace.total_runtime(Pid(2))
        );

        // Work genuinely continues on the surviving CPU after the offline: the
        // task that was on CPU 1 migrated to CPU 0 rather than the system going
        // idle. Over the ~80 ms post-offline window on one CPU, well over 30 ms
        // of runtime must accrue across the two tasks.
        let post = runtime_after(&trace, Pid(1), OFF_AT) + runtime_after(&trace, Pid(2), OFF_AT);
        assert!(
            post > 30_000_000,
            "[{name}] only {post}ns of runtime after offline — work did not migrate"
        );
        // And it all lands on the surviving CPU 0.
        assert!(
            schedules_on_cpu_from(&trace, CpuId(0), OFF_AT + OFFLINE_MARGIN_NS) > 0,
            "[{name}] surviving CPU 0 ran nothing after offline"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. Scheduler response to cpu_release / cpu_acquire.
// ---------------------------------------------------------------------------

/// A higher-priority scheduling class seizing a CPU must invoke `ops.cpu_release`
/// (preempting the running SCX task and quieting the CPU) and, on return,
/// `ops.cpu_acquire` (emitting `UpdateIdle{idle:true}` and resuming). Verified
/// for all three schedulers (existing `scx_ops_callbacks.rs` covers only
/// simple + lavd).
#[test]
fn cpu_release_acquire_all_schedulers() {
    const RELEASE_AT: TimeNs = 30_000_000;
    const ACQUIRE_AT: TimeNs = 60_000_000;
    const PREEMPT_CPU: CpuId = CpuId(0);

    for (name, make) in SCHEDULERS {
        let _lock = common::setup_test();
        let scenario = Scenario::builder()
            .cpus(2)
            .add_task("p0", 0, hog())
            .add_task("p1", 0, hog())
            .cpu_preempt(PREEMPT_CPU, RELEASE_AT, ACQUIRE_AT)
            .duration_ms(100)
            .build();

        let trace = Simulator::new(make(2)).run(scenario);

        assert_eq!(
            trace.exit_kind(),
            &ExitKind::Normal,
            "[{name}] release/acquire run should exit normally: {:?}",
            trace.exit_kind()
        );

        // cpu_release preempts whatever was running on the seized CPU.
        let preempted = trace.events().iter().any(|e| {
            matches!(e.kind, TraceKind::TaskPreempted { .. })
                && e.cpu == PREEMPT_CPU
                && e.time_ns >= RELEASE_AT
                && e.time_ns <= ACQUIRE_AT
        });
        assert!(
            preempted,
            "[{name}] no preemption on {PREEMPT_CPU:?} during the release window"
        );

        // cpu_acquire → UpdateIdle(idle=true) at/after acquire.
        let acquire_idle = trace.events().iter().any(|e| {
            matches!(e.kind, TraceKind::UpdateIdle { cpu, idle: true } if cpu == PREEMPT_CPU)
                && e.time_ns >= ACQUIRE_AT
        });
        assert!(
            acquire_idle,
            "[{name}] no UpdateIdle(idle=true) on {PREEMPT_CPU:?} after cpu_acquire at {ACQUIRE_AT}"
        );

        // The workload keeps making progress despite the seizure.
        assert!(
            trace.total_runtime(Pid(1)) > 0 && trace.total_runtime(Pid(2)) > 0,
            "[{name}] a task got no runtime under release/acquire"
        );
    }
}

// ---------------------------------------------------------------------------
// 5. Hotplug churn: repeated offline/online under a stall/error watchdog.
// ---------------------------------------------------------------------------

/// A burst of overlapping offline/online events across the run must never crash,
/// stall, or raise a BPF error, and every task must keep making progress. CPU 0
/// is never offlined, so there is always at least one CPU to carry the workload.
#[test]
fn hotplug_churn_stress() {
    const NR_TASKS: i32 = 6;

    for (name, make) in SCHEDULERS {
        let _lock = common::setup_test();
        let mut b = Scenario::builder().cpus(4);
        for i in 0..NR_TASKS {
            b = b.add_task(&format!("h{i}"), 0, hog());
        }
        let scenario = b
            // Overlapping churn; CPU 0 always stays online.
            .cpu_offline_at(CpuId(3), 20_000_000)
            .cpu_offline_at(CpuId(2), 40_000_000)
            .cpu_online_at(CpuId(3), 60_000_000)
            .cpu_offline_at(CpuId(1), 80_000_000)
            .cpu_online_at(CpuId(2), 100_000_000)
            .cpu_online_at(CpuId(1), 140_000_000)
            .cpu_offline_at(CpuId(3), 160_000_000)
            .cpu_online_at(CpuId(3), 180_000_000)
            .detect_bpf_errors()
            .watchdog_timeout_ns(Some(1_000_000_000))
            .duration_ms(220)
            .build();

        let trace = Simulator::new(make(4)).run(scenario);

        assert_eq!(
            trace.exit_kind(),
            &ExitKind::Normal,
            "[{name}] hotplug churn must not crash/stall/error: {:?}",
            trace.exit_kind()
        );
        for pid in 1..=NR_TASKS {
            assert!(
                trace.total_runtime(Pid(pid)) > 0,
                "[{name}] task {pid} starved across hotplug churn"
            );
        }
    }
}
