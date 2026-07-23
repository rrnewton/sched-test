//! Tests for LAVD's greedy-task detection and penalty mechanism.
//!
//! LAVD deprioritizes tasks that consume more than their fair share of CPU.
//! The real logic lives in `calc_greedy_penalty()`
//! (`scx/scheds/rust/scx_lavd/src/bpf/lat_cri.bpf.c`) and runs, unmodified,
//! inside scxsim (No-Stub rule). These tests observe its real state via the
//! read-only probes added to `schedulers/lavd/wrapper.c`:
//!
//! * `svc_time_iwgt`  — the task's priority-weighted invariant service time.
//! * `sys_avg_svc_time_iwgt` — the system fairness baseline.
//! * `is_greedy` — the `LAVD_FLAG_IS_GREEDY` flag.
//!
//! Mechanism under test:
//! ```text
//!   lag = sys_avg_svc_time_iwgt - svc_time_iwgt
//!   lag <  0 (over-served)  -> set LAVD_FLAG_IS_GREEDY; penalty in (100%,200%]
//!   lag >= 0 (under-served) -> reset flag;              penalty == 100%
//! ```
//! `penalty` (bounded to [100%,200%]) multiplies the task's virtual deadline
//! in `calc_virtual_deadline_delta`, so a greedy task is scheduled later.
//!
//! # A note on "recovery" (task stops being greedy / penalty decays)
//!
//! `svc_time_iwgt` accumulates monotonically as a task runs and is never
//! decremented, while `sys_avg_svc_time_iwgt` stays comparatively low. So in
//! a saturated CPU-bound workload EVERY established task is over-served
//! relative to the average and the `LAVD_FLAG_IS_GREEDY` flag latches on and
//! does not clear — verified empirically (a heavy task that later goes light,
//! and even a high-priority task among low-priority hogs, both stay flagged).
//! Recovery in LAVD is therefore NOT primarily flag-clearing; it is the
//! *bounded, self-correcting* penalty: `lag` is capped at `-lag_max` "to pay
//! the debt gradually over time" (see `calc_greedy_penalty`), so the penalty
//! never exceeds 200% and whichever task is momentarily ahead is pushed back
//! far enough for the others to catch up. This is exercised by
//! [`test_greedy_penalty_keeps_equal_tasks_fair`]. The non-greedy state IS
//! reachable — a task is born non-greedy and only crosses into greedy once it
//! accumulates service — verified by [`test_task_born_not_greedy`].

use scx_simulator::probes::{LavdMonitor, LavdProbes, LavdSnapshot};
use scx_simulator::*;

mod common;

/// Build a `TaskDef` with the given behavior on the shared default cgroup.
fn task(pid: Pid, name: &str, nice: i8, behavior: TaskBehavior) -> TaskDef {
    TaskDef {
        name: name.into(),
        pid,
        nice,
        behavior,
        start_time_ns: 0,
        mm_id: None,
        allowed_cpus: None,
        parent_pid: None,
        cgroup_name: None,
        task_flags: 0,
        migration_disabled: 0,
    }
}

/// Fraction of a task's snapshots in which it was flagged greedy.
fn greedy_fraction(hist: &[&LavdSnapshot]) -> f64 {
    if hist.is_empty() {
        return 0.0;
    }
    hist.iter().filter(|s| s.is_greedy).count() as f64 / hist.len() as f64
}

/// Over-service = `svc_time_iwgt - sys_avg_svc_time_iwgt` (== `-lag`).
/// Positive means the task has consumed more than the fairness baseline.
fn over_service(s: &LavdSnapshot) -> i128 {
    s.svc_time_iwgt as i128 - s.sys_avg_svc_time_iwgt as i128
}

/// Mean of `over_service` over a snapshot slice (0.0 if empty).
fn mean_over_service(hist: &[&LavdSnapshot]) -> f64 {
    if hist.is_empty() {
        return 0.0;
    }
    hist.iter().map(|s| over_service(s) as f64).sum::<f64>() / hist.len() as f64
}

/// Run a LAVD scenario with a fresh monitor; return `(monitor, trace)`.
fn run_lavd(cpus: u32, tasks: Vec<TaskDef>, duration_ms: u64) -> (LavdMonitor, Trace) {
    let sched = DynamicScheduler::lavd(cpus);
    let mut monitor = LavdMonitor::new(LavdProbes::new(&sched));
    let mut builder = Scenario::builder().cpus(cpus);
    for t in tasks {
        builder = builder.task(t);
    }
    let scenario = builder.duration_ms(duration_ms).build();
    let result = Simulator::new(sched).run_monitored(scenario, &mut monitor);
    (monitor, result.trace)
}

// ---------------------------------------------------------------------------
// 1. Detection: a CPU-greedy task is flagged LAVD_FLAG_IS_GREEDY.
// ---------------------------------------------------------------------------

/// A saturating CPU hog (competing with a light task on one CPU) accumulates
/// far more service time than the fairness baseline and is flagged greedy in
/// the large majority of its scheduling snapshots.
#[test]
fn test_cpu_hog_flagged_greedy() {
    let _lock = common::setup_test();
    let (monitor, _trace) = run_lavd(
        1,
        vec![
            task(Pid(1), "hog", 0, workloads::cpu_bound(10_000_000)),
            task(
                Pid(2),
                "light",
                0,
                workloads::io_bound(1_000_000, 50_000_000),
            ),
        ],
        1000,
    );

    let hog = monitor.task_history(Pid(1));
    assert!(!hog.is_empty(), "hog should have scheduling snapshots");

    let frac = greedy_fraction(&hog);
    assert!(
        frac > 0.75,
        "a saturating CPU hog should be flagged greedy in most snapshots, got frac={frac:.2}"
    );
    // The hog is genuinely over-served (svc_time above the fairness baseline).
    let hog_final = monitor.final_snapshot(Pid(1)).unwrap();
    assert!(
        hog_final.svc_time_iwgt > hog_final.sys_avg_svc_time_iwgt,
        "hog svc_time_iwgt ({}) should exceed sys avg ({})",
        hog_final.svc_time_iwgt,
        hog_final.sys_avg_svc_time_iwgt
    );
}

/// The `LAVD_FLAG_IS_GREEDY` flag discriminates by degree of over-service:
/// snapshots where a task is flagged greedy are, on average, substantially
/// more over-served than snapshots where it is not. This directly validates
/// the `lag < 0` (over-served) detection rule against real scheduler state.
#[test]
fn test_greedy_flag_marks_over_served_tasks() {
    let _lock = common::setup_test();
    // A late joiner starts under-served (non-greedy) and crosses into greedy
    // as it accumulates service — giving us both greedy and non-greedy
    // snapshots in one run.
    let (monitor, _trace) = run_lavd(
        1,
        vec![
            task(Pid(1), "hogA", 0, workloads::cpu_bound(10_000_000)),
            task(Pid(2), "hogB", 0, workloads::cpu_bound(10_000_000)),
            TaskDef {
                start_time_ns: 500_000_000,
                ..task(Pid(3), "joiner", 0, workloads::cpu_bound(10_000_000))
            },
        ],
        1500,
    );

    // Aggregate across all tasks, partitioned by the greedy flag.
    let mut all: Vec<&LavdSnapshot> = monitor.task_history(Pid(1));
    all.extend(monitor.task_history(Pid(2)));
    all.extend(monitor.task_history(Pid(3)));
    let greedy: Vec<&LavdSnapshot> = all.iter().copied().filter(|s| s.is_greedy).collect();
    let non_greedy: Vec<&LavdSnapshot> = all.iter().copied().filter(|s| !s.is_greedy).collect();

    assert!(!greedy.is_empty(), "expected some greedy snapshots");
    assert!(
        !non_greedy.is_empty(),
        "expected some non-greedy snapshots (the late joiner starts under-served)"
    );

    let greedy_os = mean_over_service(&greedy);
    let non_greedy_os = mean_over_service(&non_greedy);
    // Greedy snapshots are over-served on average...
    assert!(
        greedy_os > 0.0,
        "greedy snapshots should be over-served on average, got {greedy_os:.0}"
    );
    // ...and clearly more so than non-greedy snapshots.
    assert!(
        greedy_os > non_greedy_os,
        "greedy snapshots ({greedy_os:.0}) should be more over-served than non-greedy ({non_greedy_os:.0})"
    );
}

// ---------------------------------------------------------------------------
// 2. Penalty: deprioritizes greedy tasks / enforces fairness.
// ---------------------------------------------------------------------------

/// The greedy penalty deprioritizes whichever task is momentarily ahead, so
/// two identical CPU hogs on one CPU converge to near-equal CPU shares — the
/// observable consequence of the penalty on the virtual deadline. (This is
/// also the real "recovery" mechanism: the bounded penalty lets the trailing
/// task catch up rather than being starved. See module docs.)
#[test]
fn test_greedy_penalty_keeps_equal_tasks_fair() {
    let _lock = common::setup_test();
    let (_monitor, trace) = run_lavd(
        1,
        vec![
            task(Pid(1), "hogA", 0, workloads::cpu_bound(10_000_000)),
            task(Pid(2), "hogB", 0, workloads::cpu_bound(10_000_000)),
        ],
        1000,
    );

    let rt_a = trace.total_runtime(Pid(1)) as f64;
    let rt_b = trace.total_runtime(Pid(2)) as f64;
    assert!(rt_a > 0.0 && rt_b > 0.0, "both hogs should run");

    let imbalance = (rt_a - rt_b).abs() / rt_a.max(rt_b);
    assert!(
        imbalance < 0.20,
        "greedy penalty should keep two equal hogs fair; imbalance={imbalance:.3} (rt_a={rt_a}, rt_b={rt_b})"
    );
}

/// The penalty is priority-weighted: `svc_time_iwgt` uses *inverse* task
/// weight, so a low-priority (high-nice) task's weighted service inflates
/// faster and it is penalized harder. Result: a nice=0 hog wins clearly more
/// CPU than a competing nice=+19 hog, and the weak task's `svc_time_iwgt` is
/// far larger despite receiving less actual runtime. Exercises both the
/// per-task weight knob and the greedy/vtime accounting interaction.
#[test]
fn test_greedy_penalty_respects_task_weight() {
    let _lock = common::setup_test();
    let (monitor, trace) = run_lavd(
        1,
        vec![
            task(Pid(1), "prio", 0, workloads::cpu_bound(10_000_000)),
            task(Pid(2), "weak", 19, workloads::cpu_bound(10_000_000)),
        ],
        1000,
    );

    let rt_prio = trace.total_runtime(Pid(1)) as f64;
    let rt_weak = trace.total_runtime(Pid(2)) as f64;
    let share_prio = rt_prio / (rt_prio + rt_weak);
    assert!(
        share_prio > 0.55,
        "nice=0 hog should win most CPU vs a nice=+19 hog, got share={share_prio:.3}"
    );

    // Inverse weighting: the weak (low-priority) task accumulates far more
    // *weighted* service time per unit runtime than the high-priority task.
    let prio = monitor.final_snapshot(Pid(1)).unwrap();
    let weak = monitor.final_snapshot(Pid(2)).unwrap();
    assert!(
        weak.svc_time_iwgt > prio.svc_time_iwgt,
        "low-priority task's weighted svc_time_iwgt ({}) should exceed the high-priority task's ({})",
        weak.svc_time_iwgt,
        prio.svc_time_iwgt
    );
}

// ---------------------------------------------------------------------------
// 3. Recovery / dynamics: the flag reflects accumulated service state.
// ---------------------------------------------------------------------------

/// A task is *born* non-greedy: its first scheduling snapshot has the greedy
/// flag clear (it has consumed ~no service yet, so `lag >= 0`), and it only
/// crosses into greedy after accumulating service. Demonstrates the flag is a
/// dynamic function of service state (the same mechanism that clears the
/// penalty when a task becomes under-served), not a static label.
#[test]
fn test_task_born_not_greedy() {
    let _lock = common::setup_test();
    // A late joiner enters two established hogs so we clearly capture its
    // birth transition from non-greedy to greedy.
    let (monitor, _trace) = run_lavd(
        1,
        vec![
            task(Pid(1), "hogA", 0, workloads::cpu_bound(10_000_000)),
            task(Pid(2), "hogB", 0, workloads::cpu_bound(10_000_000)),
            TaskDef {
                start_time_ns: 500_000_000,
                ..task(Pid(3), "joiner", 0, workloads::cpu_bound(10_000_000))
            },
        ],
        1500,
    );

    let joiner = monitor.task_history(Pid(3));
    assert!(
        joiner.len() >= 3,
        "joiner should have several snapshots, got {}",
        joiner.len()
    );
    assert!(
        !joiner[0].is_greedy,
        "a freshly-started task should be born non-greedy (svc_time ~0), first snapshot was greedy"
    );
    // It becomes greedy once it accumulates service (saturating CPU hog).
    assert!(
        joiner.iter().any(|s| s.is_greedy),
        "the joiner should become greedy after accumulating service"
    );
}

// ---------------------------------------------------------------------------
// 4. Interaction with vtime (service-time) accounting.
// ---------------------------------------------------------------------------

/// `svc_time_iwgt` is the fairness currency that drives greedy detection. For
/// a CPU-bound task it accumulates monotonically as the task runs (it is only
/// ever increased, never decremented), and its final value greatly exceeds
/// its initial value. Greedy detection compares exactly this quantity against
/// the system average.
#[test]
fn test_svc_time_iwgt_accumulates_monotonically() {
    let _lock = common::setup_test();
    let (monitor, _trace) = run_lavd(
        1,
        vec![
            task(Pid(1), "hog", 0, workloads::cpu_bound(10_000_000)),
            task(
                Pid(2),
                "light",
                0,
                workloads::io_bound(1_000_000, 50_000_000),
            ),
        ],
        1000,
    );

    let hog = monitor.task_history(Pid(1));
    assert!(hog.len() >= 5, "hog should have several snapshots");

    // Monotonically non-decreasing across the whole history.
    for w in hog.windows(2) {
        assert!(
            w[1].svc_time_iwgt >= w[0].svc_time_iwgt,
            "svc_time_iwgt must not decrease: {} -> {}",
            w[0].svc_time_iwgt,
            w[1].svc_time_iwgt
        );
    }
    // And it grows substantially as the task consumes CPU.
    let first = hog.first().unwrap().svc_time_iwgt;
    let last = hog.last().unwrap().svc_time_iwgt;
    assert!(
        last > first,
        "svc_time_iwgt should grow as the hog runs: first={first}, last={last}"
    );
}
