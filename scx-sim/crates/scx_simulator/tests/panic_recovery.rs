//! Panic / abort handling and error-recovery tests for the simulator.
//!
//! scxsim's "No Silent Failures" contract (see `scx-sim/CLAUDE.md`) says a
//! fault must be *loud* — a panic at setup or a non-`Normal` `ExitKind` at run
//! time — never a silently-wrong result. This file pins the panic/abort and
//! *recovery* half of that contract, complementing:
//!   * `error_handling.rs` — invalid-config `#[should_panic]` + cgroup ENOMEM,
//!   * `watchdog.rs`        — `ErrorStall` firing / disable,
//!   * `errors.rs`          — `ExitKind` helpers and `Normal` sanity.
//!
//! The behaviors asserted here that those files do not cover:
//!   1. Scheduler BPF error (`scx_bpf_error`) surfaced as `ExitKind::ErrorBpf`
//!      — the simulator's response to a scheduler "panic". Triggered black-box
//!      by putting LAVD in a core-compaction power mode (`Balanced`) without a
//!      valid PCO table, which the real `power.bpf.c` rejects via
//!      `scx_bpf_error` (line ~201, "Incorrect PCO state").
//!   2. The ignore-vs-detect recovery contract: by default
//!      (`ignore_bpf_errors = true`) the engine *tolerates* the scheduler error
//!      and runs to `Normal`; `.detect_bpf_errors()` promotes it to a hard
//!      `ErrorBpf` exit.
//!   3. Graceful finalization: on *any* error exit the engine still returns a
//!      well-formed, queryable `Trace` (events present) — it degrades, it does
//!      not crash or hang the process.
//!   4. Recovery: a caught setup panic does not corrupt global simulator state
//!      (a later run still works), and an error-exiting run does not poison a
//!      subsequent fresh run.
//!   5. Informative messages across the error kinds (`ErrorBpf`, `ErrorStall`,
//!      `ErrorCgroupExhausted`).
//!
//! ## Note on "unimplemented BPF helpers"
//! The No-Stub rule (`scx-sim/CLAUDE.md`) means every kfunc a supported
//! scheduler calls is genuinely implemented — there is no "unimplemented
//! helper" fallback path to exercise. The faithful analog is a scheduler-side
//! `scx_bpf_error`, which is exactly what the `ErrorBpf` tests below cover.
//!
//! All assertions are on observable results (`ExitKind`, `Trace`) via the
//! public scenario API — no scheduler-side changes.

use scx_simulator::*;

#[macro_use]
mod common;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// An always-runnable task with the given phases.
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
    }
}

/// Build a LAVD scenario that provokes a scheduler `scx_bpf_error`: `Balanced`
/// power mode engages core compaction, which reads the PCO (power/core-order)
/// table; with no PCO table installed the real `power.bpf.c` rejects the state
/// via `scx_bpf_error`. `detect` selects whether the engine surfaces it.
///
/// Returns the resulting trace. Kept small and seeded for determinism.
fn run_lavd_pco_error(detect: bool) -> Trace {
    const NR_CPUS: u32 = 4;
    let sched = DynamicScheduler::lavd(NR_CPUS);
    // Deliberately DO NOT install a PCO table (cf. idle_cpu_selection.rs's
    // `lavd_setup_pco`, which sets one up to avoid this very error).
    sched.lavd_set_power_mode(LavdPowerMode::Balanced);

    let mut b = Scenario::builder().cpus(NR_CPUS).seed(1);
    if detect {
        b = b.detect_bpf_errors();
    }
    for i in 0..NR_CPUS as i32 {
        b = b.task(task(
            1 + i,
            vec![Phase::Run(2_000_000), Phase::Sleep(2_000_000)],
        ));
    }
    Simulator::new(sched).run(b.duration_ms(120).build())
}

// ===========================================================================
// 1. Simulator response to a scheduler BPF error (scx_bpf_error -> ErrorBpf).
// ===========================================================================

/// With error detection enabled, a scheduler that calls `scx_bpf_error` (LAVD's
/// PCO-state rejection) must surface a hard `ExitKind::ErrorBpf` — the
/// simulator's faithful response to a scheduler abort. The run must still return
/// (not crash/hang) with `has_error()` set.
#[test]
fn test_scheduler_bpf_error_surfaced_when_detected() {
    let _lock = common::setup_test();
    let trace = run_lavd_pco_error(/*detect=*/ true);

    assert!(
        trace.has_error(),
        "expected a detected scheduler error, got {:?}",
        trace.exit_kind()
    );
    match trace.exit_kind() {
        ExitKind::ErrorBpf(msg) => {
            assert!(
                !msg.is_empty(),
                "ErrorBpf carried an empty message (uninformative)"
            );
        }
        other => panic!("expected ExitKind::ErrorBpf, got {other:?}"),
    }
}

// ===========================================================================
// 2. Recovery contract: BPF errors are tolerated by default (ignore mode).
// ===========================================================================

/// By default (`ignore_bpf_errors = true`) the engine *recovers* from a
/// scheduler `scx_bpf_error`: the error is cleared and the simulation runs to a
/// `Normal` exit with tasks still making progress. This is the tolerant half of
/// the ignore/detect contract — the same scenario that yields `ErrorBpf` under
/// `.detect_bpf_errors()` completes cleanly by default.
#[test]
fn test_scheduler_bpf_error_tolerated_by_default() {
    let _lock = common::setup_test();
    let trace = run_lavd_pco_error(/*detect=*/ false);

    assert_eq!(
        trace.exit_kind(),
        &ExitKind::Normal,
        "default (ignore_bpf_errors) run should tolerate the scheduler error and \
         exit Normal, got {:?}",
        trace.exit_kind()
    );
    assert!(
        !trace.has_error(),
        "tolerated run should not report an error"
    );
    // Work still happened despite the tolerated error.
    let ran: u64 = (1..=4).map(|p| trace.total_runtime(Pid(p))).sum();
    assert!(ran > 0, "no task ran under the tolerated-error path");
}

// ===========================================================================
// 3. Graceful finalization: an error exit still yields a well-formed trace.
// ===========================================================================

/// On an error exit the engine must *finalize gracefully*: the `run()` call
/// returns a populated, queryable `Trace` rather than crashing, hanging, or
/// yielding an empty trace. Checked for two independent error kinds — a
/// scheduler `ErrorBpf` and a watchdog `ErrorStall`.
#[test]
fn test_error_exit_finalizes_trace_gracefully() {
    let _lock = common::setup_test();

    // (a) Scheduler BPF error.
    let bpf = run_lavd_pco_error(/*detect=*/ true);
    assert!(bpf.has_error(), "expected an error exit for the bpf case");
    assert!(
        !bpf.events().is_empty(),
        "ErrorBpf exit produced an empty trace (not finalized gracefully)"
    );

    // (b) Watchdog stall: three always-runnable tasks on one CPU with a short
    // watchdog guarantees a starved runnable task -> ErrorStall.
    let stall = Simulator::new(DynamicScheduler::simple()).run(
        Scenario::builder()
            .cpus(1)
            .watchdog_timeout_ns(Some(5_000_000))
            .task(task(1, vec![Phase::Run(100_000_000)]))
            .task(task(2, vec![Phase::Run(100_000_000)]))
            .task(task(3, vec![Phase::Run(100_000_000)]))
            .duration_ms(2000)
            .build(),
    );
    assert!(
        matches!(stall.exit_kind(), ExitKind::ErrorStall { .. }),
        "expected ErrorStall, got {:?}",
        stall.exit_kind()
    );
    assert!(
        !stall.events().is_empty(),
        "ErrorStall exit produced an empty trace (not finalized gracefully)"
    );
}

// ===========================================================================
// 4a. Recovery: a caught setup panic does not corrupt global simulator state.
// ===========================================================================

/// A malformed configuration must panic *loudly* (the No-Silent-Failures
/// contract) — and, once caught, must not have corrupted global simulator state:
/// a subsequent normal run on the same process completes cleanly with progress.
/// This exercises the recovery path that `#[should_panic]` tests cannot (they
/// end at the panic).
#[test]
fn test_setup_panic_is_loud_and_state_recovers() {
    let _lock = common::setup_test();

    // Silence the default panic hook for the intentional panic so the passing
    // test doesn't print an alarming backtrace; restore it immediately after.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let panicked = std::panic::catch_unwind(|| {
        // Zero-CPU topology is invalid and must panic in build().
        let _ = Scenario::builder()
            .cpus(0)
            .task(task(1, vec![Phase::Run(1_000_000)]))
            .duration_ms(10)
            .build();
    })
    .is_err();
    std::panic::set_hook(prev_hook);

    assert!(panicked, "malformed config (0 CPUs) did not panic loudly");

    // Global state survived the caught panic: a normal run still works.
    let trace = Simulator::new(DynamicScheduler::simple()).run(
        Scenario::builder()
            .cpus(2)
            .task(task(1, vec![Phase::Run(5_000_000)]))
            .duration_ms(30)
            .build(),
    );
    assert_eq!(
        trace.exit_kind(),
        &ExitKind::Normal,
        "a normal run after a caught setup panic should succeed, got {:?}",
        trace.exit_kind()
    );
    assert!(
        trace.total_runtime(Pid(1)) > 0,
        "post-panic run made no progress — global state was corrupted"
    );
}

// ===========================================================================
// 4b. Recovery: an error-exiting run does not poison a subsequent run.
// ===========================================================================

/// After a run that exits with an error, a fresh `Simulator` run must complete
/// normally — per-run state is reset, so one run's failure never leaks into the
/// next.
#[test]
fn test_normal_run_after_error_exit() {
    let _lock = common::setup_test();

    // First: force an ErrorStall.
    let first = Simulator::new(DynamicScheduler::simple()).run(
        Scenario::builder()
            .cpus(1)
            .watchdog_timeout_ns(Some(3_000_000))
            .task(task(1, vec![Phase::Run(100_000_000)]))
            .task(task(2, vec![Phase::Run(100_000_000)]))
            .duration_ms(1000)
            .build(),
    );
    assert!(
        first.has_error(),
        "expected the first run to error, got {:?}",
        first.exit_kind()
    );

    // Second: a clean scenario must run to Normal, unaffected by the first.
    let second = Simulator::new(DynamicScheduler::simple()).run(
        Scenario::builder()
            .cpus(2)
            .task(task(1, vec![Phase::Run(5_000_000)]))
            .duration_ms(30)
            .build(),
    );
    assert_eq!(
        second.exit_kind(),
        &ExitKind::Normal,
        "fresh run after an error exit should be Normal, got {:?}",
        second.exit_kind()
    );
    assert!(
        second.total_runtime(Pid(1)) > 0,
        "fresh run made no progress"
    );
}

// ===========================================================================
// 5. Error messages are informative across the error kinds.
// ===========================================================================

/// Each error `ExitKind` must carry enough detail to diagnose the failure:
///   * `ErrorBpf` — the scheduler's message, here identifying the PCO-state
///     rejection and its source file (asserted via stable substrings only, not
///     the machine-specific absolute path).
///   * `ErrorStall` — the stalled task's pid and how long it was runnable.
///   * `ErrorCgroupExhausted` — the offending cgroup plus the active/limit
///     counts.
#[test]
fn test_error_messages_are_informative() {
    let _lock = common::setup_test();

    // ErrorBpf: message names the specific fault + source file.
    let bpf = run_lavd_pco_error(/*detect=*/ true);
    match bpf.exit_kind() {
        ExitKind::ErrorBpf(msg) => {
            assert!(
                msg.contains("Incorrect PCO state"),
                "ErrorBpf message lacks the diagnostic text; got: {msg:?}"
            );
            assert!(
                msg.contains("power.bpf.c"),
                "ErrorBpf message lacks the source-file hint; got: {msg:?}"
            );
        }
        other => panic!("expected ErrorBpf, got {other:?}"),
    }

    // ErrorStall: names the pid and a positive runnable duration.
    let stall = Simulator::new(DynamicScheduler::simple()).run(
        Scenario::builder()
            .cpus(1)
            .watchdog_timeout_ns(Some(5_000_000))
            .task(task(1, vec![Phase::Run(100_000_000)]))
            .task(task(2, vec![Phase::Run(100_000_000)]))
            .task(task(3, vec![Phase::Run(100_000_000)]))
            .duration_ms(2000)
            .build(),
    );
    match stall.exit_kind() {
        ExitKind::ErrorStall {
            pid,
            runnable_for_ns,
        } => {
            assert!(pid.0 > 0, "ErrorStall named a non-positive pid: {pid:?}");
            assert!(
                *runnable_for_ns > 0,
                "ErrorStall reported a non-positive runnable duration"
            );
        }
        other => panic!("expected ErrorStall, got {other:?}"),
    }

    // ErrorCgroupExhausted: names the cgroup and the active/limit counts.
    let mut b = Scenario::builder()
        .cpus(2)
        .max_cgroups(2)
        .task(task(1, vec![Phase::Run(100_000_000)]));
    for (i, at) in [10_000_000u64, 20_000_000, 30_000_000]
        .into_iter()
        .enumerate()
    {
        b = b.cgroup_create_at(&format!("cg{i}"), None, None, at);
    }
    let cg = Simulator::new(DynamicScheduler::lavd(2)).run(b.duration_ms(100).build());
    match cg.exit_kind() {
        ExitKind::ErrorCgroupExhausted {
            cgroup_name,
            active_count,
            max_cgroups,
        } => {
            assert!(
                !cgroup_name.is_empty(),
                "ErrorCgroupExhausted did not name the offending cgroup"
            );
            assert_eq!(*max_cgroups, 2, "reported max_cgroups mismatch");
            assert!(
                *active_count >= *max_cgroups,
                "exhaustion reported below the limit: active={active_count} max={max_cgroups}"
            );
        }
        other => panic!("expected ErrorCgroupExhausted, got {other:?}"),
    }
}
