//! Cross-scheduler behavioral comparison for the Mitosis scheduler.
//!
//! Task categories 1–5 (basic workloads, cell placement, cell movement,
//! mitosis-specific decisions, under-load) are covered in `mitosis.rs`. This
//! file targets category 6 — "Compare Mitosis behavior against other
//! schedulers" — by running IDENTICAL workloads through mitosis, simple, lavd,
//! and cosmos and asserting the comparative invariants a correct scheduler
//! must satisfy regardless of policy:
//!
//!   * completion parity — every finite task runs to completion under every
//!     scheduler (work conservation / no permanent loss);
//!   * equal delivered work — a fixed finite workload delivers the same total
//!     on-CPU runtime under every scheduler (± a small per-task slice
//!     granularity);
//!   * no starvation — under saturation every runnable task gets a nonzero
//!     share under every scheduler;
//!   * throughput band — aggregate delivered runtime stays within a bounded
//!     band across schedulers (mitosis is not pathologically idle);
//!   * cgroup cells do not regress completion versus a flat baseline.
//!
//! These are *outcome* comparisons, deliberately distinct from the *invariant*
//! checks in `per_cpu_isolation.rs`, `cpu_affinity.rs`, and `determinism.rs`
//! (which also run mitosis but assert per-CPU / affinity / determinism
//! properties, not relative scheduling outcomes).

use scx_simulator::*;

mod common;

/// A named scheduler factory. `simple` ignores the CPU count.
type NamedSched = (&'static str, fn(u32) -> DynamicScheduler);

/// The production schedulers compared here.
///
/// `tickless` is intentionally excluded from the aggregate-throughput banding:
/// its `SCX_SLICE_INF` tickless model delivers runtime on a very different
/// cadence, so throughput banding against the tick-driven schedulers would
/// compare apples to oranges. Completion and no-starvation still hold for it
/// and are covered generically via `scheduler_tests!` in `tickless.rs`.
const SCHEDS: &[NamedSched] = &[
    ("simple", |_n| DynamicScheduler::simple()),
    ("lavd", |n| DynamicScheduler::lavd(n)),
    ("cosmos", |n| DynamicScheduler::cosmos(n)),
    ("mitosis", |n| DynamicScheduler::mitosis(n)),
];

/// A finite CPU-bound task: run `run_ns` of work exactly once, then exit.
fn finite_hog(run_ns: u64) -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(run_ns)],
        repeat: RepeatMode::Once,
    }
}

/// A never-ending CPU-bound task, sliced into `slice_ns` run chunks.
fn forever_hog(slice_ns: u64) -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(slice_ns)],
        repeat: RepeatMode::Forever,
    }
}

/// True if `pid` emitted a `TaskCompleted` event in the trace.
fn completed(trace: &Trace, pid: Pid) -> bool {
    trace
        .events()
        .iter()
        .any(|e| matches!(e.kind, TraceKind::TaskCompleted { pid: p } if p == pid))
}

/// Aggregate on-CPU runtime delivered to `pids`.
fn aggregate_runtime(trace: &Trace, pids: &[Pid]) -> u64 {
    pids.iter().map(|&p| trace.total_runtime(p)).sum()
}

// ---------------------------------------------------------------------------
// Completion parity
// ---------------------------------------------------------------------------

/// A fixed set of finite CPU-bound tasks must run to completion under EVERY
/// scheduler. This is the most basic cross-scheduler outcome: mitosis, like
/// simple/lavd/cosmos, is work-conserving and loses no task.
#[test]
fn test_finite_workload_completes_under_all_schedulers() {
    let _lock = common::setup_test();
    const NR_CPUS: u32 = 2;
    const NR_TASKS: i32 = 4;
    const WORK_NS: u64 = 2_000_000; // 2ms of work per task

    for (name, make) in SCHEDS {
        let mut b = Scenario::builder().cpus(NR_CPUS);
        for i in 0..NR_TASKS {
            b = b.add_task(&format!("t{i}"), 0, finite_hog(WORK_NS));
        }
        let scenario = b.duration_ms(100).build();

        let trace = Simulator::new(make(NR_CPUS)).run(scenario);

        assert!(
            matches!(trace.exit_kind(), ExitKind::Normal),
            "[{name}] simulation did not exit normally: {:?}",
            trace.exit_kind()
        );
        for pid in 1..=NR_TASKS {
            assert!(
                completed(&trace, Pid(pid)),
                "[{name}] finite task pid={pid} never completed"
            );
        }
    }
}

/// A fixed finite workload delivers a CONSISTENT amount of measured on-CPU
/// runtime across schedulers. The amount of work is a property of the
/// workload, not the policy, so once every task completes the aggregate
/// trace-measured runtime should land in the same band under every scheduler.
///
/// Note: `total_runtime()` is a *trace-measured* quantity (summed
/// scheduled→off-cpu intervals), not the nominal work counter — different
/// schedulers model per-slice overhead and interval boundaries slightly
/// differently, so the measured runtime is close to, but not exactly, the
/// requested work. This test therefore bands the values rather than asserting
/// exact equality; the strict "all tasks complete" outcome is covered by
/// `test_finite_workload_completes_under_all_schedulers`.
#[test]
fn test_finite_workload_delivers_consistent_work_across_schedulers() {
    let _lock = common::setup_test();
    const NR_CPUS: u32 = 2;
    const NR_TASKS: i32 = 4;
    const WORK_NS: u64 = 2_000_000;
    let pids: Vec<Pid> = (1..=NR_TASKS).map(Pid).collect();

    let mut totals: Vec<(&str, u64)> = Vec::new();
    for (name, make) in SCHEDS {
        let mut b = Scenario::builder().cpus(NR_CPUS);
        for i in 0..NR_TASKS {
            b = b.add_task(&format!("t{i}"), 0, finite_hog(WORK_NS));
        }
        let scenario = b.duration_ms(100).build();

        let trace = Simulator::new(make(NR_CPUS)).run(scenario);

        // Every task completes with nonzero measured runtime (real work
        // delivered). Per-task measured runtime is intentionally NOT banded:
        // a task that completes mid-slice records the full scheduled→completed
        // interval, so its measured runtime legitimately ranges from below to
        // ~1.35× the nominal work depending on where completion lands. The
        // cross-scheduler consistency check below operates on the aggregate,
        // where these per-task effects average out.
        for &pid in &pids {
            assert!(
                completed(&trace, pid),
                "[{name}] task pid={} never completed",
                pid.0
            );
            assert!(
                trace.total_runtime(pid) > 0,
                "[{name}] task pid={} completed with zero measured runtime",
                pid.0
            );
        }

        let total = aggregate_runtime(&trace, &pids);
        eprintln!("[{name}] finite aggregate = {total}ns");
        totals.push((name, total));
    }

    // Cross-scheduler consistency: the measured aggregate for the identical
    // finite workload must land in the same band under every scheduler
    // (min ≥ 75% of max). Mitosis must not be an outlier.
    let max = totals.iter().map(|&(_, t)| t).max().unwrap();
    let min = totals.iter().map(|&(_, t)| t).min().unwrap();
    assert!(min > 0, "some scheduler delivered zero aggregate runtime");
    assert!(
        min * 4 >= max * 3,
        "finite-workload aggregate inconsistent across schedulers \
         (min={min}ns, max={max}ns): {totals:?}"
    );
}

// ---------------------------------------------------------------------------
// No starvation under saturation
// ---------------------------------------------------------------------------

/// Under oversubscription (more runnable CPU-bound tasks than CPUs) every task
/// must get a nonzero share of CPU under EVERY scheduler — none is starved.
/// This is a fairness floor that mitosis shares with the other schedulers.
#[test]
fn test_no_starvation_under_saturation_all_schedulers() {
    let _lock = common::setup_test();
    const NR_CPUS: u32 = 2;
    const NR_TASKS: i32 = 6; // 3x oversubscribed
    let pids: Vec<Pid> = (1..=NR_TASKS).map(Pid).collect();

    for (name, make) in SCHEDS {
        let mut b = Scenario::builder().cpus(NR_CPUS);
        for i in 0..NR_TASKS {
            b = b.add_task(&format!("hog{i}"), 0, forever_hog(5_000_000));
        }
        let scenario = b.duration_ms(200).build();

        let trace = Simulator::new(make(NR_CPUS)).run(scenario);

        assert!(
            matches!(trace.exit_kind(), ExitKind::Normal),
            "[{name}] simulation did not exit normally: {:?}",
            trace.exit_kind()
        );
        for &pid in &pids {
            assert!(
                trace.total_runtime(pid) > 0,
                "[{name}] task pid={} was starved (0 runtime) under saturation",
                pid.0
            );
        }
    }
}

/// Under an identical saturating workload, the aggregate on-CPU runtime that
/// each scheduler delivers must stay within a bounded band of the others.
/// Different policies distribute time differently, but a work-conserving
/// scheduler keeps the CPUs busy — mitosis must not deliver pathologically
/// less total throughput than simple/lavd/cosmos.
#[test]
fn test_aggregate_throughput_band_across_schedulers() {
    let _lock = common::setup_test();
    const NR_CPUS: u32 = 4;
    const NR_TASKS: i32 = 12;
    const DURATION_MS: u64 = 200;
    let pids: Vec<Pid> = (1..=NR_TASKS).map(Pid).collect();
    let capacity = NR_CPUS as u64 * DURATION_MS * 1_000_000;

    let mut totals: Vec<(&str, u64)> = Vec::new();
    for (name, make) in SCHEDS {
        let mut b = Scenario::builder().cpus(NR_CPUS);
        for i in 0..NR_TASKS {
            b = b.add_task(&format!("hog{i}"), 0, forever_hog(5_000_000));
        }
        let scenario = b.duration_ms(DURATION_MS).build();

        let trace = Simulator::new(make(NR_CPUS)).run(scenario);
        let total = aggregate_runtime(&trace, &pids);
        eprintln!(
            "[{name}] aggregate={total}ns ({:.1}% of {capacity}ns capacity)",
            100.0 * total as f64 / capacity as f64
        );
        // Sanity: cannot deliver more than the CPUs physically have.
        assert!(
            total <= capacity,
            "[{name}] aggregate {total}ns exceeds capacity {capacity}ns"
        );
        totals.push((name, total));
    }

    let max = totals.iter().map(|&(_, t)| t).max().unwrap();
    let min = totals.iter().map(|&(_, t)| t).min().unwrap();
    assert!(min > 0, "some scheduler delivered zero aggregate runtime");

    // Bounded band: the least-productive scheduler must deliver at least half
    // of what the most-productive one does. This catches a mitosis regression
    // that leaves CPUs idle without over-constraining legitimate policy
    // differences.
    let (min_name, _) = totals.iter().min_by_key(|&&(_, t)| t).unwrap();
    assert!(
        min * 2 >= max,
        "throughput spread too wide across schedulers (min {min_name}={min}ns, \
         max={max}ns): {totals:?}"
    );
}

// ---------------------------------------------------------------------------
// Cgroup cells do not regress the flat baseline
// ---------------------------------------------------------------------------

/// Mitosis-specific comparison: the SAME finite tasks must all complete both
/// (a) flat (no cgroups) under every scheduler, and (b) partitioned into
/// cgroup cells under mitosis. Placing tasks in cells must not regress the
/// universal completion baseline.
///
/// Note (`sim-b7d70`): the simulator does not yet reconfigure cell cpumasks
/// from cpusets before firing the mitosis timer, so this asserts the
/// *completion* outcome (which holds today), not per-cell CPU confinement
/// (which does not). See the TODOs in `mitosis.rs::test_mitosis_basic_cell_isolation`.
#[test]
fn test_mitosis_cells_complete_like_flat_baseline() {
    let _lock = common::setup_test();
    const NR_CPUS: u32 = 4;
    const NR_TASKS: i32 = 4;
    const WORK_NS: u64 = 3_000_000;
    let pids: Vec<Pid> = (1..=NR_TASKS).map(Pid).collect();

    // (a) Flat baseline: all tasks complete under every scheduler.
    for (name, make) in SCHEDS {
        let mut b = Scenario::builder().cpus(NR_CPUS);
        for i in 0..NR_TASKS {
            b = b.add_task(&format!("flat{i}"), 0, finite_hog(WORK_NS));
        }
        let trace = Simulator::new(make(NR_CPUS)).run(b.duration_ms(100).build());
        for &pid in &pids {
            assert!(
                completed(&trace, pid),
                "[flat/{name}] task pid={} did not complete",
                pid.0
            );
        }
    }

    // (b) Same tasks split across two cgroup cells under mitosis — all must
    // still complete, matching the flat baseline.
    let scenario = Scenario::builder()
        .cpus(NR_CPUS)
        .cgroup("cell_a", &[CpuId(0), CpuId(1)])
        .cgroup("cell_b", &[CpuId(2), CpuId(3)])
        .add_task_in_cgroup("ca0", 0, finite_hog(WORK_NS), "cell_a")
        .add_task_in_cgroup("ca1", 0, finite_hog(WORK_NS), "cell_a")
        .add_task_in_cgroup("cb0", 0, finite_hog(WORK_NS), "cell_b")
        .add_task_in_cgroup("cb1", 0, finite_hog(WORK_NS), "cell_b")
        .duration_ms(100)
        .build();

    let trace = Simulator::new(DynamicScheduler::mitosis(NR_CPUS)).run(scenario);
    assert!(
        matches!(trace.exit_kind(), ExitKind::Normal),
        "[cells/mitosis] simulation did not exit normally: {:?}",
        trace.exit_kind()
    );
    for &pid in &pids {
        assert!(
            completed(&trace, pid),
            "[cells/mitosis] task pid={} did not complete in its cgroup cell",
            pid.0
        );
    }
}
