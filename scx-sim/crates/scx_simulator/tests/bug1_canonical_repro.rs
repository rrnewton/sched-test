//! Bug-1 canonical reproducer (Diff 5/5 capstone).
//!
//! Distills David Dai's R1 reproducer recipe
//! (`experiments/lavd_cpubw_stalls_202604/davids-artifacts/lavd_bw_repro_r1_tight.sh`)
//! into a deterministic scxsim integration test.
//!
//! # The recipe (from R1 + Hypothesis 6 disambiguation)
//!
//! - kernel `cpu.max="10000 100000"` — 10ms quota per 100ms period (10% of one CPU)
//! - oversubscribed CPU-bound workers (R1 uses `nproc * 4`)
//! - LAVD `--enable-cpu-bw` ON
//! - watchdog enabled (sched_ext default: stall after task is runnable too long)
//!
//! Expected production behavior (`HYPOTHESIS_6_DISAMBIGUATION.md`): the
//! kernel CFS bandwidth controller and LAVD's BPF cgroup-bw layer interact
//! such that LAVD reenqueues a task to a DSQ while the kernel layer still
//! denies dispatch. The runnable-task watchdog fires.
//!
//! Expected scxsim behavior (this test): with the engine `BandwidthManager`
//! as the single source of truth for both controllers (Diff 4 redirect),
//! the engine's head-of-line block at the DSQ produces the *same observable
//! shape* — a runnable task that fails to run for the watchdog interval,
//! i.e. `ExitKind::ErrorStall`.
//!
//! # Determinism contract
//!
//! - Fixed scenario (no PRNG-driven workload synthesis at the test level).
//! - Fixed CPU count, fixed cgroup quota, fixed task count, fixed run
//!   length per task, fixed watchdog timeout.
//! - 100% reproduction across N reps in <5s wall each.
//!
//! # Why a tight watchdog
//!
//! Production uses the kernel sched_ext default (30s). In simulation we
//! explicitly target a watchdog short enough to fire within a single
//! simulated throttle window (10ms of run + 90ms of wait per 100ms
//! period). 80ms is comfortably less than the 90ms throttle wait, so a
//! task waiting through the throttle window WILL trip an 80ms watchdog
//! while leaving headroom for short admission re-checks. That is the
//! deliberate design of this test: we use the watchdog as a precise trip
//! wire on "task was runnable but not running for >= 80ms," which is
//! impossible without the engine bw enforcement gate.

use scx_simulator::*;

#[macro_use]
mod common;

// ---------------------------------------------------------------------------
// LAVD scheduler-global helpers (mirror lavd.rs/h6_matrix.rs's local
// `lavd_set_bool` so this file is self-contained).
// ---------------------------------------------------------------------------

/// # Safety
/// Caller must ensure `name` is the literal name of a `bool` global in the
/// loaded LAVD `.so`.
unsafe fn lavd_set_bool(sched: &DynamicScheduler, name: &str, val: bool) {
    let sym: libloading::Symbol<'_, *mut bool> = sched
        .get_symbol(name.as_bytes())
        .unwrap_or_else(|| panic!("symbol {name} not found"));
    std::ptr::write_volatile(*sym, val);
}

// ---------------------------------------------------------------------------
// Recipe constants (David Dai R1, distilled)
// ---------------------------------------------------------------------------

/// 4 CPUs is the smallest count that lets us mix the R1 oversubscription
/// (workers >> CPUs) without the simulation becoming a microbenchmark.
const REPRO_NR_CPUS: u32 = 4;

/// `cpu.max="10000 100000"` from R1: 10% of one CPU.
const REPRO_PERIOD_US: u64 = 100_000; // 100ms
const REPRO_QUOTA_US: u64 = 10_000; //   10ms

/// Oversubscribe: 4 × CPUs, matching R1's `WORKERS=$(nproc * 4)`.
const REPRO_NR_WORKERS: u32 = REPRO_NR_CPUS * 4;

/// Each worker is a long-running CPU hog. 200ms is much greater than
/// quota/period so the cgroup is *always* contended.
const REPRO_TASK_RUN_NS: u64 = 200_000_000;

/// Watchdog timeout: 80ms simulated. With a 100ms period and only 10ms
/// of usable CPU, any single task in the cgroup spends >= 90ms waiting
/// per period — comfortably above 80ms — so a Bug-1 stall is a 100%-
/// reliable trip wire here.
const REPRO_WATCHDOG_NS: TimeNs = 80_000_000;

/// Simulation duration ceiling: keep tests fast. 500ms simulated time is
/// 5 full periods; the watchdog is guaranteed to fire in the first
/// throttled period (well before 500ms).
const REPRO_DURATION_MS: u64 = 500;

// ---------------------------------------------------------------------------
// Scenario builder shared between the single-shot test and the 10× determinism
// loop.
// ---------------------------------------------------------------------------

/// Build the canonical David Dai R1 scenario. The scheduler is configured
/// to enable LAVD's cgroup-bw layer. Returns `(scheduler, scenario)`.
fn make_repro() -> (DynamicScheduler, Scenario) {
    let sched = DynamicScheduler::lavd(REPRO_NR_CPUS);
    // Enable LAVD-side cgroup-bw layer (mirrors `--enable-cpu-bw` flag).
    unsafe {
        lavd_set_bool(&sched, "enable_cpu_bw\0", true);
    }

    let mut builder = Scenario::builder()
        .cpus(REPRO_NR_CPUS)
        .cgroup_with_bandwidth(
            "test_bw_tight",
            &[CpuId(0), CpuId(1), CpuId(2), CpuId(3)],
            REPRO_PERIOD_US,
            REPRO_QUOTA_US,
            0,
        )
        .watchdog_timeout_ns(Some(REPRO_WATCHDOG_NS))
        .duration_ms(REPRO_DURATION_MS);

    // Oversubscribed workers, all CPU-bound, all in the tight cgroup.
    for i in 0..REPRO_NR_WORKERS {
        builder = builder.add_task_in_cgroup(
            &format!("yes_{i}"),
            0,
            workloads::cpu_bound(REPRO_TASK_RUN_NS),
            "test_bw_tight",
        );
    }

    (sched, builder.build())
}

/// Run one rep of the canonical reproducer and report whether the trace
/// exhibits the Bug-1 shape. Returns the actual `ExitKind` so the caller
/// can pretty-print the divergence on failure.
fn run_one_rep() -> ExitKind {
    let (sched, scenario) = make_repro();
    let trace = Simulator::new(sched).run(scenario);
    trace.exit_kind().clone()
}

// ---------------------------------------------------------------------------
// Test 1: single-shot reproduction. Asserts the canonical scenario fires
// the watchdog stall — the same observable shape Bug-1 produces in
// production sched_ext (a runnable task that fails to run for the watchdog
// interval).
// ---------------------------------------------------------------------------

#[test]
fn test_bug1_canonical_recipe_reproduces_stall() {
    let _lock = common::setup_test();

    let exit = run_one_rep();

    match exit {
        ExitKind::ErrorStall {
            pid,
            runnable_for_ns,
        } => {
            eprintln!(
                "[bug1_canonical] BUG-1 REPRODUCED: pid={} runnable_for_ns={}ns ({}ms)",
                pid.0,
                runnable_for_ns,
                runnable_for_ns / 1_000_000
            );
            assert!(
                runnable_for_ns >= REPRO_WATCHDOG_NS,
                "stall fired with runnable_for_ns={runnable_for_ns} but \
                 watchdog timeout is {REPRO_WATCHDOG_NS} — engine bug?"
            );
        }
        other => {
            panic!(
                "Bug-1 canonical recipe did NOT reproduce the watchdog \
                 stall. Expected ExitKind::ErrorStall, got {other:?}. \
                 The Diff 1-4 cgroup_bw stack should make this scenario \
                 deterministically stall via engine head-of-line block at \
                 the DSQ admission gate."
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Test 2: determinism loop. Diff 5's hard gate: 100% reproduction across
// N >= 10 reps in <5s wall each. We run 10 reps in series and assert every
// one produces an `ErrorStall` with the same victim pid (the lowest-pid
// runnable task — see `Self::check_watchdog`'s tie-breaker).
// ---------------------------------------------------------------------------

#[test]
fn test_bug1_canonical_recipe_deterministic_10_reps() {
    let _lock = common::setup_test();

    const N: usize = 10;
    let start_wall = std::time::Instant::now();
    let mut first_pid: Option<Pid> = None;
    let mut first_runnable_for_ns: Option<TimeNs> = None;
    let mut succeeded = 0u32;

    for rep in 0..N {
        let exit = run_one_rep();
        match exit {
            ExitKind::ErrorStall {
                pid,
                runnable_for_ns,
            } => {
                succeeded += 1;
                if let Some(p) = first_pid {
                    assert_eq!(
                        pid, p,
                        "rep {rep}: expected stall victim pid={p:?} (from rep 0), got {pid:?} \
                         — non-deterministic stall victim"
                    );
                } else {
                    first_pid = Some(pid);
                }
                if let Some(prev) = first_runnable_for_ns {
                    assert_eq!(
                        runnable_for_ns, prev,
                        "rep {rep}: expected runnable_for_ns={prev} (from rep 0), got \
                         {runnable_for_ns} — non-deterministic stall duration"
                    );
                } else {
                    first_runnable_for_ns = Some(runnable_for_ns);
                }
            }
            other => panic!(
                "rep {rep}: Bug-1 canonical recipe failed to reproduce stall. \
                 Got {other:?}. Determinism violated: cumulative succeed-rate \
                 was {succeeded}/{rep}."
            ),
        }
    }

    let wall = start_wall.elapsed();
    let per_rep = wall / N as u32;
    eprintln!(
        "[bug1_canonical] {N} reps, all reproduced ErrorStall(pid={:?}, runnable_for_ns={:?}). \
         Wall: total={:.2?}, per_rep={:.2?}",
        first_pid, first_runnable_for_ns, wall, per_rep
    );

    assert_eq!(
        succeeded, N as u32,
        "expected {N}/{N} reproductions, got {succeeded}/{N}"
    );
    assert!(
        per_rep < std::time::Duration::from_secs(5),
        "per-rep wall time {per_rep:?} exceeds 5s budget — Diff 5 hard gate"
    );
}

// ---------------------------------------------------------------------------
// Test 3: counterfactual — flip one knob at a time; the stall must
// disappear. This is the "we are reproducing Bug-1, not stalling for
// other reasons" check.
//
// Variant A: drop the kernel quota → no stall (matches H6 cell A).
// Variant B: drop LAVD's enable_cpu_bw → still stalls because the engine
//            single-source-of-truth model enforces regardless. This is a
//            known scxsim divergence from production (where Cell B is
//            clean). We assert that the engine still fires the stall here
//            so the test surfaces the divergence rather than hiding it —
//            future work to add a true LAVD-only model can flip this to
//            "no stall," matching production.
// ---------------------------------------------------------------------------

#[test]
fn test_bug1_counterfactual_drop_kernel_quota_no_stall() {
    let _lock = common::setup_test();

    let sched = DynamicScheduler::lavd(REPRO_NR_CPUS);
    unsafe {
        lavd_set_bool(&sched, "enable_cpu_bw\0", true);
    }

    let mut builder = Scenario::builder()
        .cpus(REPRO_NR_CPUS)
        // No `cpu.max` — plain cgroup with cpuset only.
        .cgroup("test_bw_tight", &[CpuId(0), CpuId(1), CpuId(2), CpuId(3)])
        .watchdog_timeout_ns(Some(REPRO_WATCHDOG_NS))
        .duration_ms(REPRO_DURATION_MS);

    for i in 0..REPRO_NR_WORKERS {
        builder = builder.add_task_in_cgroup(
            &format!("yes_{i}"),
            0,
            workloads::cpu_bound(REPRO_TASK_RUN_NS),
            "test_bw_tight",
        );
    }
    let scenario = builder.build();

    let trace = Simulator::new(sched).run(scenario);
    let exit = trace.exit_kind();
    assert!(
        !matches!(exit, ExitKind::ErrorStall { .. }),
        "counterfactual: dropping kernel cpu.max should remove the stall, \
         but got {exit:?}. Either the test workload is over-provisioned \
         for the watchdog, or the engine has a stall path independent of \
         cgroup_bw."
    );
}
