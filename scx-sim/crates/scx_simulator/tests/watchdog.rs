//! Watchdog / stall-detection integration tests (tg `test-watchdog-timeout-scenarios`).
//!
//! `errors.rs` covers watchdog *configuration* (default timeout, `no_watchdog()`,
//! custom `watchdog_timeout_ns`) and the `ExitKind` type, but it explicitly does
//! NOT trigger the watchdog — its header notes that "Full integration tests for
//! ErrorStall would require special scheduler variants that fail to dispatch."
//! This file closes that gap: it drives the engine watchdog to actually FIRE and
//! verifies detection, reporting, gating, threshold behavior, and recovery.
//!
//! ## How a genuine stall is induced without a buggy scheduler
//!
//! The engine's watchdog ([`check_watchdog`]) flags any task that stays in the
//! `Runnable` state (with `runnable_at_ns` set) longer than the configured
//! timeout, reporting the lowest such PID. Two facts about the simulator shape
//! the setup:
//!   1. `check_watchdog` is only evaluated on `Tick` events, so a *second* task
//!      must be actively running on an online CPU to keep ticks flowing while
//!      the victim is stalled — otherwise a fully-idle system never re-checks.
//!   2. CPU offline force-migrates tasks off the downed CPU (kernel-faithful),
//!      so pinning the victim to a CPU that is *already offline when the victim
//!      arrives* leaves it enqueued but never dispatchable to an allowed online
//!      CPU — a real, deterministic `Runnable`-forever stall.
//!
//! So every stall scenario here uses: a "ticker" task pinned to CPU 0 (keeps
//! ticks flowing) + a "victim" pinned to CPU 1, where CPU 1 is taken offline
//! before the victim's `start_time_ns`.
//!
//! [`check_watchdog`]: (engine-internal)

use scx_simulator::ScenarioBuilder;
use scx_simulator::*;

mod common;

/// Watchdog behavior is engine-level (scheduler-independent), so every scenario
/// is exercised against all three general-purpose schedulers.
const SCHEDULERS: &[&str] = &["simple", "lavd", "cosmos"];

fn make_scheduler(name: &str, nr_cpus: u32) -> DynamicScheduler {
    match name {
        "simple" => DynamicScheduler::simple(),
        "lavd" => DynamicScheduler::lavd(nr_cpus),
        "cosmos" => DynamicScheduler::cosmos(nr_cpus),
        other => panic!("unknown scheduler '{other}'"),
    }
}

/// A CPU-bound task pinned to `cpu` that runs forever (used both as the always-
/// running "ticker" and, when pinned to a downed CPU, as the stall "victim").
fn pinned_cpu_hog(name: &str, pid: i32, cpu: u32, start_time_ns: TimeNs) -> TaskDef {
    TaskDef {
        name: name.into(),
        pid: Pid(pid),
        nice: 0,
        behavior: TaskBehavior {
            phases: vec![Phase::Run(1_000_000_000)],
            repeat: RepeatMode::Forever,
        },
        start_time_ns,
        mm_id: None,
        allowed_cpus: Some(vec![CpuId(cpu)]),
        parent_pid: None,
        cgroup_name: None,
        task_flags: 0,
        migration_disabled: 0,
    }
}

/// Build a scenario with `nr_victims` tasks (pids 1..=nr_victims) pinned to CPU 1
/// that arrive at 10 ms — after CPU 1 has been taken offline at 5 ms — plus a
/// ticker (pid 100) pinned to CPU 1's peer CPU 0 to keep the watchdog checking.
/// The victims can never be dispatched to an allowed online CPU, so they stall.
fn starvation_builder(nr_victims: i32) -> ScenarioBuilder {
    let mut b = Scenario::builder().cpus(2);
    for pid in 1..=nr_victims {
        b = b.task(pinned_cpu_hog(&format!("victim{pid}"), pid, 1, 10_000_000));
    }
    // Ticker keeps CPU 0 busy → Tick events → watchdog re-checks.
    b = b.task(pinned_cpu_hog("ticker", 100, 0, 0));
    b.cpu_offline_at(CpuId(1), 5_000_000)
}

// ---------------------------------------------------------------------------
// 1. Watchdog detection when a task is starved (and 5: correct reporting).
// ---------------------------------------------------------------------------

/// A runnable-but-never-dispatchable task must trip the watchdog, and the
/// resulting `ExitKind::ErrorStall` must name that task and report a
/// `runnable_for_ns` that exceeds the configured timeout (by no more than a
/// tick's worth of slack, since detection happens on the next tick after the
/// threshold is crossed).
#[test]
fn watchdog_fires_on_starvation() {
    const TIMEOUT_NS: TimeNs = 50_000_000; // 50 ms

    for &sched in SCHEDULERS {
        let _lock = common::setup_test();
        let scenario = starvation_builder(1)
            .watchdog_timeout_ns(Some(TIMEOUT_NS))
            .duration_ms(300)
            .build();

        let trace = Simulator::new(make_scheduler(sched, 2)).run(scenario);

        match trace.exit_kind() {
            ExitKind::ErrorStall {
                pid,
                runnable_for_ns,
            } => {
                assert_eq!(*pid, Pid(1), "[{sched}] wrong stalled pid reported");
                assert!(
                    *runnable_for_ns > TIMEOUT_NS,
                    "[{sched}] runnable_for_ns {runnable_for_ns} must exceed timeout {TIMEOUT_NS}"
                );
                assert!(
                    *runnable_for_ns <= TIMEOUT_NS + 15_000_000,
                    "[{sched}] runnable_for_ns {runnable_for_ns} fired far past timeout {TIMEOUT_NS} \
                     (expected detection within ~1 tick)"
                );
            }
            other => panic!("[{sched}] expected ErrorStall, got {other:?}"),
        }

        // The victim genuinely never ran; the trace records that.
        assert_eq!(
            trace.schedule_count(Pid(1)),
            0,
            "[{sched}] victim should never have been scheduled"
        );
        assert!(
            trace.has_error(),
            "[{sched}] has_error() should be true on a stall"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. The configured timeout governs when the watchdog fires.
// ---------------------------------------------------------------------------

/// The same stall, run under a short vs. a long timeout, must fire later (larger
/// `runnable_for_ns`) for the longer timeout — proving the configured value, not
/// a hard-coded constant, drives detection.
#[test]
fn watchdog_timeout_threshold_governs_firing() {
    const SHORT_NS: TimeNs = 30_000_000; // 30 ms
    const LONG_NS: TimeNs = 120_000_000; // 120 ms

    fn stall_runnable_for(sched: &str, timeout_ns: TimeNs) -> TimeNs {
        let scenario = starvation_builder(1)
            .watchdog_timeout_ns(Some(timeout_ns))
            .duration_ms(600)
            .build();
        match Simulator::new(make_scheduler(sched, 2))
            .run(scenario)
            .exit_kind()
        {
            ExitKind::ErrorStall {
                runnable_for_ns, ..
            } => *runnable_for_ns,
            other => panic!("[{sched}] expected ErrorStall at timeout {timeout_ns}, got {other:?}"),
        }
    }

    for &sched in SCHEDULERS {
        let _lock = common::setup_test();
        let short_rf = stall_runnable_for(sched, SHORT_NS);
        let long_rf = stall_runnable_for(sched, LONG_NS);

        assert!(
            short_rf > SHORT_NS && short_rf <= SHORT_NS + 15_000_000,
            "[{sched}] short timeout: runnable_for {short_rf} not just past {SHORT_NS}"
        );
        assert!(
            long_rf > LONG_NS && long_rf <= LONG_NS + 15_000_000,
            "[{sched}] long timeout: runnable_for {long_rf} not just past {LONG_NS}"
        );
        assert!(
            long_rf > short_rf,
            "[{sched}] longer timeout should fire later: long {long_rf} vs short {short_rf}"
        );
    }
}

// ---------------------------------------------------------------------------
// 3a. Recovery: a task serviced before the timeout must NOT trip the watchdog.
// ---------------------------------------------------------------------------

/// If the victim's CPU comes back online before the timeout elapses, the task
/// runs, `runnable_at_ns` is reset, and no stall is reported — the sim completes
/// normally. This exercises the watchdog's reset/recovery path and guards
/// against false positives while the watchdog is active and checking.
#[test]
fn watchdog_recovers_when_task_serviced_before_timeout() {
    const TIMEOUT_NS: TimeNs = 80_000_000; // 80 ms

    for &sched in SCHEDULERS {
        let _lock = common::setup_test();
        // Victim arrives at 10 ms (CPU 1 offline since 5 ms); CPU 1 comes back at
        // 40 ms — only 30 ms of stall, well under the 80 ms timeout — so the
        // victim runs and recovers.
        let scenario = starvation_builder(1)
            .cpu_online_at(CpuId(1), 40_000_000)
            .watchdog_timeout_ns(Some(TIMEOUT_NS))
            .duration_ms(300)
            .build();

        let trace = Simulator::new(make_scheduler(sched, 2)).run(scenario);

        assert_eq!(
            trace.exit_kind(),
            &ExitKind::Normal,
            "[{sched}] recovered task must not stall: {:?}",
            trace.exit_kind()
        );
        assert!(
            trace.schedule_count(Pid(1)) > 0,
            "[{sched}] victim should run once its CPU is back online"
        );
    }
}

// ---------------------------------------------------------------------------
// 3b. Gating: with the watchdog disabled, an unrecoverable stall does not error.
// ---------------------------------------------------------------------------

/// The exact scenario that fires `ErrorStall` in [`watchdog_fires_on_starvation`]
/// must instead complete normally when the watchdog is disabled — proving the
/// error comes from the watchdog, not from the stall itself, and that the sim
/// does not hang.
#[test]
fn watchdog_disabled_tolerates_permanent_stall() {
    for &sched in SCHEDULERS {
        let _lock = common::setup_test();
        let scenario = starvation_builder(1).no_watchdog().duration_ms(300).build();

        let scenario_check = &scenario;
        assert_eq!(
            scenario_check.watchdog_timeout_ns, None,
            "[{sched}] watchdog should be disabled"
        );

        let trace = Simulator::new(make_scheduler(sched, 2)).run(scenario);

        assert_eq!(
            trace.exit_kind(),
            &ExitKind::Normal,
            "[{sched}] disabled watchdog must not raise ErrorStall: {:?}",
            trace.exit_kind()
        );
        // The victim really is permanently stalled — it just isn't flagged.
        assert_eq!(
            trace.schedule_count(Pid(1)),
            0,
            "[{sched}] victim still never runs"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. Deterministic reporting: lowest stalled PID wins.
// ---------------------------------------------------------------------------

/// When several tasks are stalled simultaneously, `check_watchdog` deterministic-
/// ally reports the lowest PID (HashMap iteration order is otherwise unstable).
/// Two victims (pids 1 and 2) stall together; the report must name pid 1.
#[test]
fn watchdog_reports_lowest_stalled_pid() {
    const TIMEOUT_NS: TimeNs = 50_000_000;

    for &sched in SCHEDULERS {
        let _lock = common::setup_test();
        let scenario = starvation_builder(2)
            .watchdog_timeout_ns(Some(TIMEOUT_NS))
            .duration_ms(300)
            .build();

        let trace = Simulator::new(make_scheduler(sched, 2)).run(scenario);

        match trace.exit_kind() {
            ExitKind::ErrorStall { pid, .. } => {
                assert_eq!(
                    *pid,
                    Pid(1),
                    "[{sched}] expected lowest stalled pid (1), got {}",
                    pid.0
                );
            }
            other => panic!("[{sched}] expected ErrorStall, got {other:?}"),
        }

        // Both victims were stalled.
        assert_eq!(trace.schedule_count(Pid(1)), 0, "[{sched}] victim 1 ran?");
        assert_eq!(trace.schedule_count(Pid(2)), 0, "[{sched}] victim 2 ran?");
    }
}

// ---------------------------------------------------------------------------
// 6. Interaction between the watchdog and cgroup bandwidth throttling.
// ---------------------------------------------------------------------------

/// Set a `bool` global in a loaded scheduler `.so`.
///
/// # Safety
/// `name` must be a NUL-terminated literal naming a real `bool` global in the
/// loaded `.so`; the scheduler must outlive the write. Mirrors the helper in
/// `cgroup_bw_replenish_smoking_gun.rs`.
unsafe fn lavd_set_bool(sched: &DynamicScheduler, name: &str, val: bool) {
    let sym: libloading::Symbol<'_, *mut bool> = sched
        .get_symbol(name.as_bytes())
        .unwrap_or_else(|| panic!("symbol {name} not found"));
    std::ptr::write_volatile(*sym, val);
}

/// A cgroup-bandwidth-throttled task must NOT spuriously trip the watchdog:
/// throttling is a *recoverable* state (the task is re-admitted when its quota
/// replenishes each period), so `runnable_at_ns` is reset each time it runs and
/// never crosses a timeout set larger than the replenish period.
///
/// This is a LAVD-only test: `cgroup_bw` is a LAVD library that `simple`/`cosmos`
/// do not link, and it must be armed via the `enable_cpu_bw` global. A tight
/// cgroup (10% of a CPU) running a CPU hog genuinely throttles (asserted via
/// `CgroupBwReplenish { keep_throttled: true }` events), while an unthrottled
/// "ticker" outside the cgroup keeps `Tick` events — and thus watchdog checks —
/// flowing during the throttle windows. With a watchdog timeout (400 ms) well
/// above the 100 ms cgroup period, the run completes normally and the throttled
/// hog still makes progress.
#[test]
fn watchdog_tolerates_cgroup_bw_throttling() {
    let _lock = common::setup_test();

    let sched = DynamicScheduler::lavd(4);
    // SAFETY: `enable_cpu_bw` is a `bool` global in LAVD's main.bpf.c; the
    // scheduler outlives this write. Arms the cgroup_bw library.
    unsafe {
        lavd_set_bool(&sched, "enable_cpu_bw\0", true);
    }

    let scenario = Scenario::builder()
        .cpus(4)
        // Tight cgroup: 10 ms quota per 100 ms period (10% of one CPU).
        .cgroup_with_bandwidth(
            "tight",
            &[CpuId(0), CpuId(1), CpuId(2), CpuId(3)],
            100_000,
            10_000,
            0,
        )
        .add_task_in_cgroup("hog", 0, workloads::cpu_bound(2_000_000_000), "tight")
        // Unthrottled ticker keeps ticks (and watchdog checks) flowing.
        .add_task("ticker", 0, workloads::cpu_bound(2_000_000_000))
        // Timeout > cgroup period so legitimate per-period throttling is tolerated.
        .watchdog_timeout_ns(Some(400_000_000))
        .duration_ms(600)
        .build();

    let trace = Simulator::new(sched).run(scenario);

    // The cgroup genuinely entered the throttle state in at least one period —
    // otherwise this wouldn't be testing the throttling interaction at all.
    let throttled_periods = trace
        .events()
        .iter()
        .filter(|e| matches!(e.kind, TraceKind::CgroupBwReplenish { keep_throttled, .. } if keep_throttled))
        .count();
    assert!(
        throttled_periods > 0,
        "expected the tight cgroup to throttle at least once (got 0 throttled \
         periods) — is the LAVD .so built with SCXSIM_PHASE2_REAL_CGROUP_BW=1?"
    );

    // Despite real throttling, the watchdog must NOT fire: throttling recovers.
    assert_eq!(
        trace.exit_kind(),
        &ExitKind::Normal,
        "cgroup throttling must not spuriously trip the watchdog: {:?}",
        trace.exit_kind()
    );

    // The throttled hog (pid 1) is throttled-then-resumed, not permanently
    // stalled — it makes real progress across the run.
    assert!(
        trace.schedule_count(Pid(1)) > 0 && trace.total_runtime(Pid(1)) > 0,
        "throttled hog should still make progress (sched={}, rt={})",
        trace.schedule_count(Pid(1)),
        trace.total_runtime(Pid(1))
    );
}
