//! CPU hotplug (online/offline) and higher-priority-class release/acquire tests.
//!
//! CPU availability is modeled engine-side (`SimCpu::is_online`): an offline
//! CPU receives no ticks and runs no tasks, and the four transition handlers
//! (`handle_cpu_offline` / `handle_cpu_online` / `handle_cpu_release` /
//! `handle_cpu_acquire`) preempt whatever is on-CPU, drain-and-re-enqueue the
//! CPU's local DSQ so the scheduler re-places those tasks, fire the optional
//! `ops.cpu_{offline,online,release,acquire}` scheduler callbacks, and — on
//! coming back — emit `UpdateIdle{idle:true}` and resume dispatch. Because the
//! availability modeling is engine-side, the *observable* effects hold for
//! every scheduler regardless of whether it implements the (optional)
//! callbacks, so these tests assert on trace-observable effects rather than on
//! the raw callbacks.
//!
//! **Scheduler groups.** The invariants split cleanly by how a scheduler drives
//! dispatch:
//!
//! * [`ALL_SCHEDULERS`] — the engine-enforced invariants that hold for *every*
//!   scheduler: an offlined CPU goes quiet, a returning CPU emits
//!   `UpdateIdle{idle:true}`, and the run always completes normally.
//! * [`RESCHEDULING_SCHEDULERS`] (all but tickless) — schedulers that use finite
//!   time slices and therefore emit fresh `TaskScheduled` events as work
//!   resumes. tickless runs tasks under `SCX_SLICE_INF` (a placed task runs
//!   "forever" with no re-dispatch), so per-window `TaskScheduled` *counts* are
//!   not a meaningful resume signal for it — it is covered only by the
//!   engine-level invariants above.
//! * [`MIGRATING_SCHEDULERS`] (simple, lavd, mitosis) — schedulers that migrate
//!   a task off an offlined CPU onto a survivor. scx_cosmos strands the task
//!   that was running on the offlined CPU instead of migrating it
//!   (`mb sim-52fb8d`), so it is excluded from the per-task migration assertion
//!   until that fidelity gap is resolved.
//!
//! Complements the two smaller hotplug tests in `scx_ops_callbacks.rs` (which
//! only sweep simple+lavd).

use scx_simulator::*;

mod common;

/// A named scheduler constructor, so one test can sweep several schedulers.
type NamedSchedFactory = (&'static str, fn(u32) -> DynamicScheduler);

/// Every scheduler scxsim supports. Used for the engine-enforced invariants
/// (offlined CPU quiet, `UpdateIdle` on return, normal exit) that hold
/// regardless of the scheduler's dispatch model.
const ALL_SCHEDULERS: [NamedSchedFactory; 5] = [
    ("simple", |_n| DynamicScheduler::simple()),
    ("lavd", DynamicScheduler::lavd),
    ("cosmos", DynamicScheduler::cosmos),
    ("mitosis", DynamicScheduler::mitosis),
    ("tickless", DynamicScheduler::tickless),
];

/// Schedulers that use finite time slices and thus emit fresh `TaskScheduled`
/// events as work resumes on a returning CPU. Excludes tickless, whose
/// `SCX_SLICE_INF` model does not re-dispatch a still-running task.
const RESCHEDULING_SCHEDULERS: [NamedSchedFactory; 4] = [
    ("simple", |_n| DynamicScheduler::simple()),
    ("lavd", DynamicScheduler::lavd),
    ("cosmos", DynamicScheduler::cosmos),
    ("mitosis", DynamicScheduler::mitosis),
];

/// Schedulers that migrate a task off an offlined CPU onto a survivor. Excludes
/// tickless (`SCX_SLICE_INF`, see above) and cosmos (strands the task, see
/// `mb sim-52fb8d`).
const MIGRATING_SCHEDULERS: [NamedSchedFactory; 3] = [
    ("simple", |_n| DynamicScheduler::simple()),
    ("lavd", DynamicScheduler::lavd),
    ("mitosis", DynamicScheduler::mitosis),
];

/// A forever-running CPU-bound task.
fn hog() -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(200_000_000)],
        repeat: RepeatMode::Forever,
    }
}

/// Count `TaskScheduled` events on `cpu` in the half-open window `[lo, hi)`.
fn schedules_on(trace: &Trace, cpu: CpuId, lo: u64, hi: u64) -> usize {
    trace
        .events()
        .iter()
        .filter(|e| {
            matches!(e.kind, TraceKind::TaskScheduled { .. })
                && e.cpu == cpu
                && e.time_ns >= lo
                && e.time_ns < hi
        })
        .count()
}

/// Set of distinct pids scheduled on `cpu` in the half-open window `[lo, hi)`.
fn pids_scheduled_on(
    trace: &Trace,
    cpu: CpuId,
    lo: u64,
    hi: u64,
) -> std::collections::HashSet<i32> {
    trace
        .events()
        .iter()
        .filter_map(|e| match e.kind {
            TraceKind::TaskScheduled { pid }
                if e.cpu == cpu && e.time_ns >= lo && e.time_ns < hi =>
            {
                Some(pid.0)
            }
            _ => None,
        })
        .collect()
}

/// True if the engine recorded `UpdateIdle{cpu, idle:true}` at or after `at`.
fn update_idle_true_after(trace: &Trace, cpu: CpuId, at: u64) -> bool {
    trace.events().iter().any(|e| {
        matches!(e.kind, TraceKind::UpdateIdle { cpu: c, idle: true } if c == cpu)
            && e.time_ns >= at
    })
}

/// Build a `cpus`-CPU scenario oversubscribed with `ntasks` forever-hogs.
fn oversubscribed(cpus: u32, ntasks: u32) -> ScenarioBuilder {
    let mut b = Scenario::builder().cpus(cpus);
    for i in 0..ntasks {
        b = b.add_task(&format!("h{i}"), 0, hog());
    }
    b
}

// ---------------------------------------------------------------------------
// (1a) Engine invariant — offline quiesces a CPU, online re-idles it.
//      Holds for EVERY scheduler.
// ---------------------------------------------------------------------------

/// The two engine-enforced hotplug invariants, swept across all five
/// schedulers: (a) an offlined CPU runs no tasks for the whole offline window,
/// and (b) re-onlining it emits `UpdateIdle{idle:true}`. The system stays
/// oversubscribed (4 hogs on 2 CPUs) so a still-online CPU 1 plainly *would*
/// keep scheduling — making the "went quiet" check meaningful.
#[test]
fn test_offline_quiesces_and_online_reidles_all_scheds() {
    let _lock = common::setup_test();
    let off_at = 30_000_000;
    let on_at = 60_000_000;
    // In-flight dispatch can land on the CPU as it is drained; ignore a 1ms
    // settling margin right after the offline instant.
    let margin = 1_000_000;

    for (name, make) in ALL_SCHEDULERS {
        let scenario = oversubscribed(2, 4)
            .cpu_offline_at(CpuId(1), off_at)
            .cpu_online_at(CpuId(1), on_at)
            .duration_ms(100)
            .build();
        let t = Simulator::new(make(2)).run(scenario);
        assert_eq!(t.exit_kind(), &ExitKind::Normal, "{name}: not normal exit");

        // CPU 1 was busy before it went offline (one of only two CPUs carrying
        // four hogs), so this is a real before/after contrast.
        assert!(
            schedules_on(&t, CpuId(1), 0, off_at) > 0,
            "{name}: CPU 1 never ran before offline — setup not exercising it"
        );

        // Quiet for the whole offline window (after the settling margin).
        let during = schedules_on(&t, CpuId(1), off_at + margin, on_at);
        assert_eq!(
            during,
            0,
            "{name}: CPU 1 ran {during} task(s) while offline [{}..{on_at})",
            off_at + margin
        );

        // cpu_online → UpdateIdle(idle=true) recorded at/after the online time.
        assert!(
            update_idle_true_after(&t, CpuId(1), on_at),
            "{name}: no UpdateIdle(idle=true) on CPU 1 after cpu_online at {on_at}"
        );
    }
}

// ---------------------------------------------------------------------------
// (1b) Rescheduling schedulers actually resume dispatch on the returned CPU.
// ---------------------------------------------------------------------------

/// Beyond the engine re-idling the CPU, finite-slice schedulers must actually
/// dispatch tasks onto CPU 1 again after it comes back online. (tickless is
/// excluded: under `SCX_SLICE_INF` an already-running task is not re-dispatched,
/// so no fresh `TaskScheduled` appears — it is covered by the engine invariant
/// test above.)
#[test]
fn test_online_resumes_dispatch_rescheduling_scheds() {
    let _lock = common::setup_test();
    let off_at = 30_000_000;
    let on_at = 60_000_000;
    let end = 100_000_000;

    for (name, make) in RESCHEDULING_SCHEDULERS {
        let scenario = oversubscribed(2, 4)
            .cpu_offline_at(CpuId(1), off_at)
            .cpu_online_at(CpuId(1), on_at)
            .duration_ms(100)
            .build();
        let t = Simulator::new(make(2)).run(scenario);
        assert_eq!(t.exit_kind(), &ExitKind::Normal, "{name}: not normal exit");
        assert!(
            schedules_on(&t, CpuId(1), on_at, end) > 0,
            "{name}: CPU 1 never ran after coming back online"
        );
    }
}

// ---------------------------------------------------------------------------
// (2) A task on the offlined CPU migrates and keeps making progress.
// ---------------------------------------------------------------------------

/// When a CPU is offlined, the task it was running must not be stranded: the
/// engine preempts it and re-enqueues it, so the scheduler re-places it on a
/// surviving CPU. With two hogs on two CPUs, each CPU carries one hog; after CPU
/// 1 goes offline, *both* hogs must continue to be scheduled — necessarily on
/// CPU 0, the only survivor. Verified for the migrating schedulers (cosmos
/// strands the task, `mb sim-52fb8d`; tickless runs under `SCX_SLICE_INF`).
#[test]
fn test_task_migrates_off_offlined_cpu() {
    let _lock = common::setup_test();
    let off_at = 30_000_000;
    let margin = 2_000_000;
    let end = 100_000_000;

    for (name, make) in MIGRATING_SCHEDULERS {
        let scenario = Scenario::builder()
            .cpus(2)
            .add_task("m0", 0, hog())
            .add_task("m1", 0, hog())
            .cpu_offline_at(CpuId(1), off_at)
            .duration_ms(100)
            .build();
        let t = Simulator::new(make(2)).run(scenario);
        assert_eq!(t.exit_kind(), &ExitKind::Normal, "{name}: not normal exit");

        // After offline+margin, all scheduling happens on CPU 0.
        assert_eq!(
            schedules_on(&t, CpuId(1), off_at + margin, end),
            0,
            "{name}: offlined CPU 1 still scheduled tasks"
        );

        // Both tasks keep getting the CPU after the offline — i.e. the hog that
        // had been on CPU 1 successfully migrated to CPU 0 and shares it.
        let survivors = pids_scheduled_on(&t, CpuId(0), off_at + margin, end);
        assert!(
            survivors.contains(&1) && survivors.contains(&2),
            "{name}: expected both pids to run on CPU 0 after offline, saw {survivors:?}"
        );

        // Both tasks accumulate meaningful runtime overall (no permanent stall).
        assert!(
            t.total_runtime(Pid(1)) > 0 && t.total_runtime(Pid(2)) > 0,
            "{name}: a task got no runtime (rt1={}, rt2={})",
            t.total_runtime(Pid(1)),
            t.total_runtime(Pid(2))
        );
    }
}

// ---------------------------------------------------------------------------
// (3) Offlining down to a single CPU still completes and funnels all work.
// ---------------------------------------------------------------------------

/// Offlining every CPU but one must still complete normally, with no scheduling
/// on any offlined CPU once they are all down. Start with 4 CPUs and 4 hogs;
/// offline CPUs 1, 2, 3 (staggered), leaving only CPU 0. The "no work on
/// offlined CPUs" invariant holds for every scheduler; the "survivor keeps
/// running" check (fresh `TaskScheduled` on CPU 0) is asserted for the
/// rescheduling schedulers only, since tickless does not re-dispatch its
/// already-running task.
#[test]
fn test_offline_down_to_single_cpu() {
    let _lock = common::setup_test();
    let margin = 2_000_000;
    let last_off = 40_000_000;
    let end = 100_000_000;

    for (name, make) in ALL_SCHEDULERS {
        let scenario = oversubscribed(4, 4)
            .cpu_offline_at(CpuId(1), 20_000_000)
            .cpu_offline_at(CpuId(2), 30_000_000)
            .cpu_offline_at(CpuId(3), last_off)
            .duration_ms(100)
            .build();
        let t = Simulator::new(make(4)).run(scenario);
        assert_eq!(t.exit_kind(), &ExitKind::Normal, "{name}: not normal exit");

        // Once all three are offline (after the last offline + margin), no
        // offlined CPU may schedule tasks. Holds for every scheduler.
        for cpu in [CpuId(1), CpuId(2), CpuId(3)] {
            assert_eq!(
                schedules_on(&t, cpu, last_off + margin, end),
                0,
                "{name}: {cpu:?} still ran tasks after being offlined"
            );
        }

        // The lone survivor keeps working — but only finite-slice schedulers
        // emit fresh TaskScheduled events to prove it.
        if RESCHEDULING_SCHEDULERS.iter().any(|(n, _)| *n == name) {
            assert!(
                schedules_on(&t, CpuId(0), last_off + margin, end) > 0,
                "{name}: surviving CPU 0 ran nothing after others went offline"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// (4) Idempotency of the transition handlers (early-return paths).
// ---------------------------------------------------------------------------

/// The transition handlers must be idempotent: offlining an already-offline CPU
/// is a no-op, and onlining a CPU that was never offlined is a no-op. Both are
/// early-return paths in the engine; exercising them must not crash, must not
/// spuriously disturb scheduling, and must still complete normally. The
/// "never-offlined CPU keeps running through a redundant online" check needs
/// fresh `TaskScheduled` events, so it is asserted for the rescheduling
/// schedulers; the "no crash / offlined stays quiet / resumes" invariants hold
/// for all.
#[test]
fn test_hotplug_idempotent_transitions() {
    let _lock = common::setup_test();
    let end = 100_000_000;

    for (name, make) in ALL_SCHEDULERS {
        let scenario = oversubscribed(2, 4)
            // Double-offline CPU 1: the second offline hits the "already
            // offline" early-return.
            .cpu_offline_at(CpuId(1), 20_000_000)
            .cpu_offline_at(CpuId(1), 30_000_000)
            // Online a CPU that was never offlined: the "already online"
            // early-return.
            .cpu_online_at(CpuId(0), 40_000_000)
            // Bring CPU 1 back for good.
            .cpu_online_at(CpuId(1), 50_000_000)
            .duration_ms(100)
            .build();
        let t = Simulator::new(make(2)).run(scenario);
        assert_eq!(t.exit_kind(), &ExitKind::Normal, "{name}: not normal exit");

        // CPU 1 stays quiet across the whole double-offline span (holds for all
        // schedulers — engine-enforced).
        assert_eq!(
            schedules_on(&t, CpuId(1), 31_000_000, 50_000_000),
            0,
            "{name}: CPU 1 ran while (doubly) offline"
        );

        if RESCHEDULING_SCHEDULERS.iter().any(|(n, _)| *n == name) {
            // The redundant online of the never-offlined CPU 0 did not disturb
            // it — it kept running.
            assert!(
                schedules_on(&t, CpuId(0), 40_000_000, end) > 0,
                "{name}: never-offlined CPU 0 stopped running after a redundant online"
            );
            // CPU 1 resumes after the real online.
            assert!(
                schedules_on(&t, CpuId(1), 50_000_000, end) > 0,
                "{name}: CPU 1 never resumed after online"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// (5) Repeated offline → online cycles on one CPU.
// ---------------------------------------------------------------------------

/// A CPU can be hotplugged repeatedly. Cycle CPU 1 offline→online three times;
/// in each offline window it must be quiet (engine-enforced, all schedulers),
/// and — for rescheduling schedulers — in each online window it must run again.
#[test]
fn test_repeated_hotplug_cycles() {
    let _lock = common::setup_test();
    // Three (off, on) windows within a 120ms run; gaps large enough to observe.
    let cycles = [
        (15_000_000u64, 25_000_000u64),
        (45_000_000, 55_000_000),
        (75_000_000, 85_000_000),
    ];
    let margin = 1_000_000;
    let end = 120_000_000;

    for (name, make) in ALL_SCHEDULERS {
        let mut b = oversubscribed(2, 4);
        for (off, on) in cycles {
            b = b.cpu_offline_at(CpuId(1), off).cpu_online_at(CpuId(1), on);
        }
        let t = Simulator::new(make(2)).run(b.duration_ms(120).build());
        assert_eq!(t.exit_kind(), &ExitKind::Normal, "{name}: not normal exit");

        let is_rescheduling = RESCHEDULING_SCHEDULERS.iter().any(|(n, _)| *n == name);
        for (idx, (off, on)) in cycles.iter().enumerate() {
            // Quiet during each offline window (all schedulers).
            assert_eq!(
                schedules_on(&t, CpuId(1), off + margin, *on),
                0,
                "{name}: cycle {idx}: CPU 1 ran while offline [{}..{on})",
                off + margin
            );
            // Runs again in the window after coming back online (up to the next
            // offline, or the end of the run for the last cycle).
            if is_rescheduling {
                let next_off = cycles.get(idx + 1).map(|c| c.0).unwrap_or(end);
                assert!(
                    schedules_on(&t, CpuId(1), *on, next_off) > 0,
                    "{name}: cycle {idx}: CPU 1 never ran after online at {on}"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// (6) cpu_release / cpu_acquire (higher-priority class) — every scheduler.
// ---------------------------------------------------------------------------

/// A higher-priority scheduling class seizing a CPU triggers `cpu_release`
/// (preempting the running SCX task and quiescing the CPU); regaining it
/// triggers `cpu_acquire` (emitting `UpdateIdle{idle:true}` and resuming).
/// The preemption, quiescence, and `UpdateIdle` are engine-enforced and checked
/// for every scheduler; the resume-dispatch check applies to rescheduling
/// schedulers only.
#[test]
fn test_release_acquire_all_scheds() {
    let _lock = common::setup_test();
    let release_at = 30_000_000;
    let acquire_at = 60_000_000;
    let end = 100_000_000;
    let margin = 1_000_000;

    for (name, make) in ALL_SCHEDULERS {
        let scenario = oversubscribed(2, 4)
            .cpu_preempt(CpuId(0), release_at, acquire_at)
            .duration_ms(100)
            .build();
        let t = Simulator::new(make(2)).run(scenario);
        assert_eq!(t.exit_kind(), &ExitKind::Normal, "{name}: not normal exit");

        // cpu_release preempts whatever was on CPU 0 around the release instant.
        let preempted = t.events().iter().any(|e| {
            matches!(e.kind, TraceKind::TaskPreempted { .. })
                && e.cpu == CpuId(0)
                && e.time_ns >= release_at
                && e.time_ns <= acquire_at
        });
        assert!(
            preempted,
            "{name}: no preemption on CPU 0 during the release window"
        );

        // Released CPU is quiet while the higher-priority class owns it.
        assert_eq!(
            schedules_on(&t, CpuId(0), release_at + margin, acquire_at),
            0,
            "{name}: CPU 0 ran SCX tasks while released to a higher class"
        );

        // cpu_acquire → UpdateIdle(idle=true) recorded at/after the acquire.
        assert!(
            update_idle_true_after(&t, CpuId(0), acquire_at),
            "{name}: no UpdateIdle(idle=true) on CPU 0 after cpu_acquire at {acquire_at}"
        );

        // CPU 0 resumes running SCX tasks once reacquired (finite-slice only).
        if RESCHEDULING_SCHEDULERS.iter().any(|(n, _)| *n == name) {
            assert!(
                schedules_on(&t, CpuId(0), acquire_at, end) > 0,
                "{name}: CPU 0 never ran again after cpu_acquire"
            );
        }
    }
}
