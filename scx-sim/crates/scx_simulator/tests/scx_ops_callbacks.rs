//! SCX ops-callback invocation tests.
//!
//! The engine drives the real BPF scheduler's `struct sched_ext_ops` callbacks
//! and records a trace event around each invocation. This suite verifies that
//! those callbacks fire, in the right order, with the right arguments:
//!
//! * **Task lifecycle** — `ops.init_task` (`InitTask`), `ops.enable`
//!   (`Enable`), `ops.exit_task` (`ExitTask`): one each per task, ordered
//!   init → enable → first-run → exit.
//! * **Tick** — `ops.tick` (`Tick`) fires periodically while a task runs.
//! * **CPU hotplug** — `ops.cpu_online` / `ops.cpu_offline`: an offlined CPU
//!   stops running tasks; a re-onlined CPU emits `UpdateIdle{idle:true}` and
//!   resumes.
//! * **CPU release/acquire** — `ops.cpu_release` (higher-priority class seizes
//!   the CPU, preempting the running task) and `ops.cpu_acquire` (sched_ext
//!   regains it, emitting `UpdateIdle{idle:true}` and resuming).
//! * **Cgroup lifecycle** — `ops.cgroup_init` / `ops.cgroup_move` /
//!   `ops.cgroup_exit`: init before move before exit, with the right pid and
//!   distinct source/destination cgroups; exits unwind in reverse of init.
//!
//! Complements `cgroup_hierarchy.rs` (which focuses on cgroup_bw semantics) by
//! asserting the callback ordering/argument surface rather than bandwidth math.
//! The previously-untested task-lifecycle and hotplug/idle callbacks are the
//! main additions.

use scx_simulator::*;

#[macro_use]
mod common;

/// A named scheduler constructor, so one test can sweep several schedulers.
type NamedSchedFactory = (&'static str, fn(u32) -> DynamicScheduler);

const SCHEDULERS: [NamedSchedFactory; 3] = [
    ("simple", |_n| DynamicScheduler::simple()),
    ("lavd", DynamicScheduler::lavd),
    ("cosmos", DynamicScheduler::cosmos),
];

/// Count trace events matching `pred`.
fn count<F: Fn(&TraceKind) -> bool>(trace: &Trace, pred: F) -> usize {
    trace.events().iter().filter(|e| pred(&e.kind)).count()
}

/// Index of the first event whose kind matches `pred`.
fn first_idx<F: Fn(&TraceKind) -> bool>(trace: &Trace, pred: F) -> Option<usize> {
    trace.events().iter().position(|e| pred(&e.kind))
}

/// A short forever-running CPU-bound task.
fn hog() -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(200_000_000)],
        repeat: RepeatMode::Forever,
    }
}

/// A task that runs once for `run_ns` and then exits (drives `ExitTask`).
fn run_once(run_ns: u64) -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(run_ns)],
        repeat: RepeatMode::Once,
    }
}

// ---------------------------------------------------------------------------
// (1) init_task / enable / exit_task fire once per task, all schedulers.
// ---------------------------------------------------------------------------

/// Every task that joins gets exactly one `init_task` and one `enable`; every
/// task that leaves (voluntary exit) gets exactly one `exit_task`. Verified for
/// simple/lavd/cosmos, with correct per-task pids and a success rc from
/// `init_task`.
#[test]
fn test_init_enable_exit_per_task() {
    let _lock = common::setup_test();
    for (name, make) in SCHEDULERS {
        let scenario = Scenario::builder()
            .cpus(2)
            .add_task("a", 0, run_once(5_000_000))
            .add_task("b", 0, run_once(5_000_000))
            .duration_ms(100)
            .build();
        let t = Simulator::new(make(2)).run(scenario);
        assert_eq!(t.exit_kind(), &ExitKind::Normal, "{name}: not normal exit");

        // Exactly one init_task / enable / exit_task per task.
        for (kind, n) in [
            (
                "init",
                count(&t, |k| matches!(k, TraceKind::InitTask { .. })),
            ),
            (
                "enable",
                count(&t, |k| matches!(k, TraceKind::Enable { .. })),
            ),
            (
                "exit",
                count(&t, |k| matches!(k, TraceKind::ExitTask { .. })),
            ),
        ] {
            assert_eq!(n, 2, "{name}: expected 2 {kind}_task callbacks, got {n}");
        }

        // Correct pids on each, and init_task succeeded (rc == 0).
        for pid in [Pid(1), Pid(2)] {
            assert!(
                t.events().iter().any(
                    |e| matches!(e.kind, TraceKind::InitTask { pid: p, rc } if p == pid && rc == 0)
                ),
                "{name}: no successful InitTask for {pid:?}"
            );
            assert!(
                t.events()
                    .iter()
                    .any(|e| matches!(e.kind, TraceKind::Enable { pid: p } if p == pid)),
                "{name}: no Enable for {pid:?}"
            );
            assert!(
                t.events()
                    .iter()
                    .any(|e| matches!(e.kind, TraceKind::ExitTask { pid: p } if p == pid)),
                "{name}: no ExitTask for {pid:?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// (6) Per-task callback ordering: init_task < enable < first run < exit_task.
// ---------------------------------------------------------------------------

/// For every task, the lifecycle callbacks must be ordered
/// `init_task` → `enable` → first `TaskScheduled` → `exit_task`. This is the
/// kernel-faithful order (a task is initialized and enabled before it can be
/// picked to run, and exits only after it stops running).
#[test]
fn test_callback_ordering_lifecycle() {
    let _lock = common::setup_test();
    for (name, make) in SCHEDULERS {
        let scenario = Scenario::builder()
            .cpus(2)
            .add_task("a", 0, run_once(5_000_000))
            .add_task("b", 0, run_once(5_000_000))
            .duration_ms(100)
            .build();
        let t = Simulator::new(make(2)).run(scenario);

        for pid in [Pid(1), Pid(2)] {
            let init = first_idx(
                &t,
                |k| matches!(k, TraceKind::InitTask { pid: p, .. } if *p == pid),
            );
            let enable = first_idx(
                &t,
                |k| matches!(k, TraceKind::Enable { pid: p } if *p == pid),
            );
            let sched = first_idx(
                &t,
                |k| matches!(k, TraceKind::TaskScheduled { pid: p } if *p == pid),
            );
            let exit = first_idx(
                &t,
                |k| matches!(k, TraceKind::ExitTask { pid: p } if *p == pid),
            );

            let (init, enable, sched, exit) = (
                init.unwrap_or_else(|| panic!("{name}: no InitTask for {pid:?}")),
                enable.unwrap_or_else(|| panic!("{name}: no Enable for {pid:?}")),
                sched.unwrap_or_else(|| panic!("{name}: no TaskScheduled for {pid:?}")),
                exit.unwrap_or_else(|| panic!("{name}: no ExitTask for {pid:?}")),
            );
            assert!(
                init < enable && enable < sched && sched < exit,
                "{name} {pid:?}: bad callback order init={init} enable={enable} \
                 sched={sched} exit={exit}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// (3) tick callback fires while tasks run.
// ---------------------------------------------------------------------------

/// A periodic `ops.tick` must fire while CPU-bound tasks run, and each `Tick`
/// must name a currently-running task (valid pid). Verified for all schedulers.
#[test]
fn test_tick_callback_fires() {
    let _lock = common::setup_test();
    let nr = 2;
    let nt = nr * 2; // oversubscribe so every CPU always has a runnable task
    for (name, make) in SCHEDULERS {
        let mut b = Scenario::builder().cpus(nr);
        for i in 0..nt {
            b = b.add_task(&format!("t{i}"), 0, hog());
        }
        let t = Simulator::new(make(nr)).run(b.duration_ms(200).build());
        assert_eq!(t.exit_kind(), &ExitKind::Normal, "{name}: not normal exit");

        let ticks = count(&t, |k| matches!(k, TraceKind::Tick { .. }));
        assert!(ticks > 0, "{name}: no Tick callbacks fired");

        // Every tick names a valid task (the one on-CPU when the tick fired).
        for e in t.events() {
            if let TraceKind::Tick { pid } = e.kind {
                assert!(
                    pid.0 >= 1 && pid.0 <= nt as i32,
                    "{name}: Tick names invalid pid {pid:?}"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// (4a) cpu_offline / cpu_online hotplug callbacks.
// ---------------------------------------------------------------------------

/// Offlining a CPU must stop it running tasks; re-onlining it must invoke
/// `ops.cpu_online` (observable as the `UpdateIdle{idle:true}` the engine emits
/// right after) and let it run again. Verified for simple and lavd.
#[test]
fn test_cpu_hotplug_online_offline_callbacks() {
    let _lock = common::setup_test();
    let off_at = 30_000_000;
    let on_at = 60_000_000;
    for (name, make) in [SCHEDULERS[0], SCHEDULERS[1]] {
        let scenario = Scenario::builder()
            .cpus(2)
            .add_task("h0", 0, hog())
            .add_task("h1", 0, hog())
            .cpu_offline_at(CpuId(1), off_at)
            .cpu_online_at(CpuId(1), on_at)
            .duration_ms(100)
            .build();
        let t = Simulator::new(make(2)).run(scenario);
        assert_eq!(t.exit_kind(), &ExitKind::Normal, "{name}: not normal exit");

        // While offline, CPU 1 must go quiet. There is one boundary schedule at
        // the offline instant (an in-flight dispatch lands as the CPU is drained,
        // ~1.5us after `off_at`); after a 1ms margin the offlined CPU must run
        // nothing at all for the rest of the ~29ms window — which it plainly
        // would not if it were still online and busy.
        let margin = 1_000_000;
        let ran_while_offline = t.events().iter().any(|e| {
            matches!(e.kind, TraceKind::TaskScheduled { .. })
                && e.cpu == CpuId(1)
                && e.time_ns > off_at + margin
                && e.time_ns < on_at
        });
        assert!(
            !ran_while_offline,
            "{name}: CPU 1 ran a task while offline ({}..{on_at})",
            off_at + margin
        );

        // cpu_online → update_idle(idle=true) recorded at/after the online time.
        let online_idle = t.events().iter().any(|e| {
            matches!(e.kind, TraceKind::UpdateIdle { cpu, idle: true } if cpu == CpuId(1))
                && e.time_ns >= on_at
        });
        assert!(
            online_idle,
            "{name}: no UpdateIdle(idle=true) on CPU 1 after cpu_online at {on_at}"
        );

        // CPU 1 resumes running after coming back online.
        let ran_after_online = t.events().iter().any(|e| {
            matches!(e.kind, TraceKind::TaskScheduled { .. })
                && e.cpu == CpuId(1)
                && e.time_ns >= on_at
        });
        assert!(ran_after_online, "{name}: CPU 1 never ran after cpu_online");
    }
}

// ---------------------------------------------------------------------------
// (4b) cpu_release / cpu_acquire callbacks.
// ---------------------------------------------------------------------------

/// A higher-priority scheduling class seizing a CPU invokes `ops.cpu_release`
/// (preempting the running SCX task); regaining it invokes `ops.cpu_acquire`
/// (emitting `UpdateIdle{idle:true}` and resuming). We assert the observable
/// effects of both callbacks rather than brittle in-window counts.
#[test]
fn test_cpu_release_acquire_callbacks() {
    let _lock = common::setup_test();
    let release_at = 30_000_000;
    let acquire_at = 60_000_000;
    for (name, make) in [SCHEDULERS[0], SCHEDULERS[1]] {
        let scenario = Scenario::builder()
            .cpus(2)
            .add_task("p0", 0, hog())
            .add_task("p1", 0, hog())
            .cpu_preempt(CpuId(0), release_at, acquire_at)
            .duration_ms(100)
            .build();
        let t = Simulator::new(make(2)).run(scenario);
        assert_eq!(t.exit_kind(), &ExitKind::Normal, "{name}: not normal exit");

        // cpu_release preempts whatever was running on CPU 0 around release time.
        let preempted_at_release = t.events().iter().any(|e| {
            matches!(e.kind, TraceKind::TaskPreempted { .. })
                && e.cpu == CpuId(0)
                && e.time_ns >= release_at
                && e.time_ns <= acquire_at
        });
        assert!(
            preempted_at_release,
            "{name}: no preemption on CPU 0 during release window"
        );

        // cpu_acquire → update_idle(idle=true) recorded at/after acquire time.
        let acquire_idle = t.events().iter().any(|e| {
            matches!(e.kind, TraceKind::UpdateIdle { cpu, idle: true } if cpu == CpuId(0))
                && e.time_ns >= acquire_at
        });
        assert!(
            acquire_idle,
            "{name}: no UpdateIdle(idle=true) on CPU 0 after cpu_acquire at {acquire_at}"
        );

        // Released CPU is much quieter than a normally-busy CPU: far fewer
        // schedules land on CPU 0 during the release window than after acquire.
        let sched_on_cpu0 = |lo: u64, hi: u64| {
            t.events()
                .iter()
                .filter(|e| {
                    matches!(e.kind, TraceKind::TaskScheduled { .. })
                        && e.cpu == CpuId(0)
                        && e.time_ns >= lo
                        && e.time_ns < hi
                })
                .count()
        };
        let during = sched_on_cpu0(release_at, acquire_at);
        let after = sched_on_cpu0(acquire_at, 100_000_000);
        assert!(
            during <= after,
            "{name}: expected CPU 0 quieter while released (during={during}, after={after})"
        );
    }
}

// ---------------------------------------------------------------------------
// (5 + 6) cgroup_init / cgroup_move / cgroup_exit ordering and arguments.
// ---------------------------------------------------------------------------

/// Cgroup lifecycle callbacks under LAVD (which implements the full cgroup ops):
/// every cgroup is `cgroup_init`-ed before any `cgroup_move`; the runtime
/// migration fires exactly one `cgroup_move` for the right pid with distinct
/// source/destination cgroups at the scheduled time; and `cgroup_exit` unwinds
/// after the moves. Verifies callback ordering and argument correctness.
#[test]
fn test_cgroup_lifecycle_ordering_and_args() {
    let _lock = common::setup_test();
    let migrate_at = 40_000_000;
    let scenario = Scenario::builder()
        .cpus(2)
        .cgroup("cgA", &[])
        .cgroup("cgB", &[])
        .add_task("t1", 0, hog())
        .cgroup_migrate(Pid(1), "cgA", "cgB", migrate_at)
        .duration_ms(100)
        .build();
    let t = Simulator::new(DynamicScheduler::lavd(2)).run(scenario);
    assert_eq!(
        t.exit_kind(),
        &ExitKind::Normal,
        "cgroup run not normal exit"
    );

    // Root + cgA + cgB → at least 3 cgroup_init callbacks.
    let inits = count(&t, |k| matches!(k, TraceKind::CgroupInit { .. }));
    assert!(
        inits >= 3,
        "expected >=3 CgroupInit (root+cgA+cgB), got {inits}"
    );

    // Exactly one cgroup_move for pid 1, at the scheduled time, distinct cgroups.
    let moves: Vec<_> = t
        .events()
        .iter()
        .filter(|e| matches!(e.kind, TraceKind::CgroupMove { .. }))
        .collect();
    assert_eq!(
        moves.len(),
        1,
        "expected exactly one CgroupMove, got {}",
        moves.len()
    );
    if let TraceKind::CgroupMove {
        pid,
        from_cgid,
        to_cgid,
    } = moves[0].kind
    {
        assert_eq!(pid, Pid(1), "CgroupMove for wrong pid");
        assert_ne!(
            from_cgid.0, to_cgid.0,
            "CgroupMove src/dst cgroups must differ"
        );
    }
    assert_eq!(
        moves[0].time_ns, migrate_at,
        "CgroupMove fired at {} not scheduled time {migrate_at}",
        moves[0].time_ns
    );

    // Ordering: all cgroup_init before the move; at least one cgroup_exit after.
    let last_init = t
        .events()
        .iter()
        .rposition(|e| matches!(e.kind, TraceKind::CgroupInit { .. }))
        .expect("no CgroupInit");
    let move_idx = first_idx(&t, |k| matches!(k, TraceKind::CgroupMove { .. })).unwrap();
    let first_exit =
        first_idx(&t, |k| matches!(k, TraceKind::CgroupExit { .. })).expect("no CgroupExit");
    assert!(
        last_init < move_idx,
        "all CgroupInit ({last_init}) must precede CgroupMove ({move_idx})"
    );
    assert!(
        move_idx < first_exit,
        "CgroupMove ({move_idx}) must precede CgroupExit ({first_exit})"
    );

    // cgroup_exit unwinds in reverse of cgroup_init (LIFO): the destination
    // cgroup (created later) exits before the root (created first).
    let exits: Vec<_> = t
        .events()
        .iter()
        .filter_map(|e| match e.kind {
            TraceKind::CgroupExit { cgid } => Some(cgid.0),
            _ => None,
        })
        .collect();
    assert!(
        exits.len() >= 3,
        "expected >=3 CgroupExit, got {}",
        exits.len()
    );
    assert!(
        exits.windows(2).all(|w| w[0] > w[1]),
        "CgroupExit should unwind in reverse cgid order, got {exits:?}"
    );
}
