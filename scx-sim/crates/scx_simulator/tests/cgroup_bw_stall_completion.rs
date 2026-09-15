//! cpu-bw-stall-bug reproduction — FORWARD-PROGRESS / "no task lost" angle.
//!
//! The cpu-bw-stall-bug is, at its core, a task that gets put aside while its
//! cgroup is throttled and is then never re-dispatched — it is LOST in the
//! library's backlog task queue (BTQ), and the watchdog eventually fires a
//! `runnable task stall`. The fix (upstream `period_budget` accounting + the
//! scxsim V4-C engine fix) re-enqueues parked tasks on every replenish.
//!
//! This file exercises that invariant from the FORWARD-PROGRESS side and
//! complements — does NOT duplicate — the existing cgroup-bw tests, which
//! cover the throttle STATE MACHINE with `forever_run` tasks and assert only
//! that the watchdog does not fire:
//!   * `cgroup_bw_throttle_unthrottle.rs` — throttle/unthrottle cycles,
//!     strict alternation (stuck-throttled regression guard), single-task
//!     put-aside→re-enqueue.
//!   * `cgroup_bw_replenish_smoking_gun.rs` — `CgroupBwReplenish` observer.
//!   * `cgroup_hierarchy.rs` — multi-level hierarchy enforcement.
//!   * `bug1_canonical_repro.rs` — subprocess canonical reproducer.
//!
//! What is NEW here:
//!   1. MULTIPLE tasks competing for one throttled cgroup must ALL keep making
//!      forward progress — every one is put aside, re-enqueued, and still runs
//!      in the tail of the simulation (none permanently lost).
//!   2. The full H6 path is asserted as a CAUSAL CHAIN for a task: it bails in
//!      `lavd_enqueue` while the cgroup is throttled
//!      (`cgroup_throttled()`/`scx_cgroup_bw_throttled` →
//!      `LavdBailOnCgroupThrottle`), then is re-enqueued at a later unthrottle
//!      (`CgroupBwReenqueueOnReplenish`) and runs again.
//!   3. Bandwidth is actually ENFORCED (the throttled cgroup runs far below CPU
//!      capacity), not silently bypassed.
//!   4. FINITE-work tasks all eventually COMPLETE despite throttling (action
//!      item 6). NOTE: a finite `RepeatMode::Once` task currently emits
//!      `TaskCompleted` more than once under throttle (put-aside/re-enqueue
//!      restarts its finished phase) — tracked as mb sim-a00345. We therefore
//!      assert the robust invariant (every task completes AT LEAST once, no
//!      stall) rather than an exact completion count.
//!
//! No-Stub / model-the-kernel: `enable_cpu_bw=true` runs LAVD's real
//! `cgroup_throttled()` enqueue gate and the linked-in `cgroup_bw.bpf.c`
//! enforcement library; every assertion is on trace events those emit. All
//! scenarios use the deterministic serial engine (`seed` + `instant_timing`).

use scx_simulator::*;

#[macro_use]
mod common;

/// Replenish period: 100ms, matching the library's `CBW_REPLENISH_PERIOD`.
const PERIOD_US: u64 = 100_000;

/// Set a `bool` global (e.g. `enable_cpu_bw`) in the loaded LAVD `.so`.
///
/// # Safety
/// `name` must be the NUL-terminated literal name of a `bool` global present
/// in the loaded LAVD `.so`.
unsafe fn lavd_set_bool(sched: &DynamicScheduler, name: &str, val: bool) {
    let sym: libloading::Symbol<'_, *mut bool> = sched
        .get_symbol(name.as_bytes())
        .unwrap_or_else(|| panic!("symbol {name} not found"));
    std::ptr::write_volatile(*sym, val);
}

/// LAVD with cgroup-bandwidth enforcement turned ON. `lavd_setup` hard-codes
/// `enable_cpu_bw=false`, which gates the enqueue-time `cgroup_throttled()`
/// check; flip it on to run the real throttle/put-aside/replenish path.
fn lavd_cpu_bw(nr_cpus: u32) -> DynamicScheduler {
    let sched = DynamicScheduler::lavd(nr_cpus);
    sched.lavd_set_cgroup_bw_max(64);
    // SAFETY: `enable_cpu_bw` is a `bool` global in LAVD's main.bpf.c; the
    // symbol is present and the scheduler outlives this write.
    unsafe {
        lavd_set_bool(&sched, "enable_cpu_bw\0", true);
    }
    sched
}

fn forever_run(run_ns: u64) -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(run_ns)],
        repeat: RepeatMode::Forever,
    }
}

fn run_once(run_ns: u64) -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(run_ns)],
        repeat: RepeatMode::Once,
    }
}

fn count_kind(trace: &Trace, pred: impl Fn(&TraceKind) -> bool) -> usize {
    trace.events().iter().filter(|e| pred(&e.kind)).count()
}

/// A tight-quota (10ms per 100ms period = 10% of one CPU) cgroup holding
/// `n` CPU-bound `forever` tasks on a single CPU. One CPU forces sustained
/// demand well above quota, so the cgroup is throttled repeatedly and every
/// task must be put aside and re-enqueued to keep progressing.
fn contended_throttled_scenario(n: usize, duration_ms: u64) -> Scenario {
    let mut b = Scenario::builder().cpus(1).seed(42).instant_timing();
    b = b.cgroup_with_bandwidth("tight", &[CpuId(0)], PERIOD_US, 10_000, 0);
    for _ in 0..n {
        b = b.add_task_in_cgroup("w", 0, forever_run(2_000_000_000), "tight");
    }
    b.duration_ms(duration_ms).build()
}

// ---------------------------------------------------------------------------
// 1. No task is lost: every contending task keeps making forward progress.
// ---------------------------------------------------------------------------

#[test]
fn test_all_tasks_forward_progress_under_throttle() {
    let _lock = common::setup_test();
    const N: usize = 3;
    let duration_ns: u64 = 1_000_000_000;
    let trace = Simulator::new(lavd_cpu_bw(1)).run(contended_throttled_scenario(N, 1000));

    // The whole point of the fix: no `runnable task stall`.
    assert_eq!(
        trace.exit_kind(),
        &ExitKind::Normal,
        "contended throttled cgroup must not stall"
    );
    assert!(
        !trace.has_error(),
        "unexpected error: {:?}",
        trace.exit_kind()
    );

    // The cgroup was really throttled AND recovered (not stuck throttled —
    // the stall-bug shape). Both edges must appear.
    let throttled = count_kind(&trace, |k| {
        matches!(
            k,
            TraceKind::CbwThrottleCgroups {
                throttled: true,
                ..
            }
        )
    });
    let unthrottled = count_kind(&trace, |k| {
        matches!(
            k,
            TraceKind::CbwThrottleCgroups {
                throttled: false,
                ..
            }
        )
    });
    assert!(
        throttled >= 2 && unthrottled >= 2,
        "expected repeated throttle+unthrottle (not stuck): throttled={throttled} unthrottled={unthrottled}"
    );

    // Every one of the N tasks: bailed while throttled, re-enqueued on
    // replenish, AND ran in the final quarter of the run (still alive at the
    // end — not lost in the BTQ).
    let tail = duration_ns * 3 / 4;
    for p in 1..=N as i32 {
        let pid = Pid(p);
        let bailed = trace.events().iter().any(
            |e| matches!(&e.kind, TraceKind::LavdBailOnCgroupThrottle { pid: x, .. } if *x == pid),
        );
        let reenqueued = trace.events().iter().any(|e| {
            matches!(&e.kind, TraceKind::CgroupBwReenqueueOnReplenish { pid: x, .. } if *x == pid)
        });
        let ran_in_tail = trace.events().iter().any(|e| {
            e.time_ns >= tail && matches!(&e.kind, TraceKind::TaskScheduled { pid: x } if *x == pid)
        });
        assert!(
            bailed,
            "task pid={p} was never put aside on throttle (LavdBailOnCgroupThrottle)"
        );
        assert!(
            reenqueued,
            "task pid={p} was never re-enqueued on replenish (CgroupBwReenqueueOnReplenish) — lost in BTQ?"
        );
        assert!(
            ran_in_tail,
            "task pid={p} did not run in the final quarter — starved/lost after put-aside"
        );
        assert!(
            trace.total_runtime(pid) > 0,
            "task pid={p} accrued no runtime"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. H6 path as a causal chain: throttle → lavd_enqueue bail → re-enqueue.
// ---------------------------------------------------------------------------

#[test]
fn test_h6_bail_reenqueue_causal_chain() {
    let _lock = common::setup_test();
    let trace = Simulator::new(lavd_cpu_bw(1)).run(contended_throttled_scenario(3, 1000));
    assert_eq!(trace.exit_kind(), &ExitKind::Normal);

    // Collect throttle transitions for the tight cgroup.
    let mut throttle_windows: Vec<(u64, bool)> = trace
        .events()
        .iter()
        .filter_map(|e| match &e.kind {
            TraceKind::CbwThrottleCgroups { throttled, .. } => Some((e.time_ns, *throttled)),
            _ => None,
        })
        .collect();
    throttle_windows.sort_by_key(|(t, _)| *t);
    assert!(
        !throttle_windows.is_empty(),
        "no throttle transitions observed"
    );

    // Is the cgroup throttled at time `t`? (state = last transition <= t)
    let throttled_at = |t: u64| -> bool {
        throttle_windows
            .iter()
            .take_while(|(tt, _)| *tt <= t)
            .last()
            .map(|(_, s)| *s)
            .unwrap_or(false)
    };

    // For at least one task, verify the full H6 chain in causal order:
    //   (a) lavd_enqueue bails BECAUSE the cgroup is throttled, then
    //   (b) it is re-enqueued at a LATER unthrottle (replenish), then
    //   (c) it actually runs afterwards.
    let mut chain_verified = false;
    for p in 1..=3i32 {
        let pid = Pid(p);
        let bail_at = trace
            .events()
            .iter()
            .filter(|e| {
                matches!(&e.kind, TraceKind::LavdBailOnCgroupThrottle { pid: x, .. } if *x == pid)
            })
            .map(|e| e.time_ns)
            .find(|t| throttled_at(*t)); // bail happened while throttled
        let Some(bail_t) = bail_at else { continue };

        let reenq_t = trace
            .events()
            .iter()
            .filter(|e| {
                matches!(&e.kind, TraceKind::CgroupBwReenqueueOnReplenish { pid: x, .. } if *x == pid)
            })
            .map(|e| e.time_ns)
            .find(|t| *t > bail_t);
        let Some(reenq_t) = reenq_t else { continue };

        let ran_after = trace.events().iter().any(|e| {
            e.time_ns >= reenq_t
                && matches!(&e.kind, TraceKind::TaskScheduled { pid: x } if *x == pid)
        });
        if ran_after {
            chain_verified = true;
            eprintln!("H6 chain pid={p}: bail@{bail_t} (throttled) -> reenqueue@{reenq_t} -> ran");
            break;
        }
    }
    assert!(
        chain_verified,
        "no task exhibited the full H6 chain: throttled-bail -> replenish-reenqueue -> run"
    );
}

// ---------------------------------------------------------------------------
// 3. Bandwidth is actually enforced (throttle not silently bypassed).
// ---------------------------------------------------------------------------

#[test]
fn test_bandwidth_actually_enforced() {
    let _lock = common::setup_test();
    let duration_ms = 1000u64;
    let trace = Simulator::new(lavd_cpu_bw(1)).run(contended_throttled_scenario(3, duration_ms));
    assert_eq!(trace.exit_kind(), &ExitKind::Normal);

    // 3 forever tasks on 1 CPU would consume ~1000ms of CPU unthrottled. Under
    // a 10%/period cap they must run FAR less — heavy limiting proves the
    // throttle path really constrains the cgroup (a loose bound robust to the
    // library's coarse per-period accounting; observed ~200ms).
    let aggregate: u64 = (1..=3).map(|p| trace.total_runtime(Pid(p))).sum();
    let capacity_ns = duration_ms * 1_000_000; // 1 CPU
    assert!(
        aggregate > 0,
        "throttled cgroup made no progress at all (over-throttled/stalled)"
    );
    assert!(
        aggregate < capacity_ns / 2,
        "cgroup ran {aggregate}ns of {capacity_ns}ns capacity — bandwidth cap not enforced"
    );
}

// ---------------------------------------------------------------------------
// 4. Finite workload: all tasks eventually complete despite throttling.
// ---------------------------------------------------------------------------

#[test]
fn test_finite_workload_all_tasks_complete() {
    let _lock = common::setup_test();
    const N: i32 = 3;
    // 3 finite tasks (40ms each) in a tight 10%/period cgroup on 1 CPU. Their
    // combined 120ms of work needs ~12 replenish periods, so they are heavily
    // throttled — yet every one must eventually finish (no task lost).
    let mut b = Scenario::builder().cpus(1).seed(42).instant_timing();
    b = b.cgroup_with_bandwidth("tight", &[CpuId(0)], PERIOD_US, 10_000, 0);
    for _ in 0..N {
        b = b.add_task_in_cgroup("w", 0, run_once(40_000_000), "tight");
    }
    let trace = Simulator::new(lavd_cpu_bw(1)).run(b.duration_ms(5000).build());

    assert_eq!(
        trace.exit_kind(),
        &ExitKind::Normal,
        "finite throttled workload must not stall"
    );

    // Every task completes AT LEAST once. (A finite task currently completes
    // MORE than once under throttle — put-aside/re-enqueue restarts its
    // finished phase — tracked as mb sim-a00345; the robust "no task lost"
    // invariant is that each distinct pid reaches completion.)
    for p in 1..=N {
        let pid = Pid(p);
        let completed = trace
            .events()
            .iter()
            .any(|e| matches!(&e.kind, TraceKind::TaskCompleted { pid: x } if *x == pid));
        assert!(
            completed,
            "finite task pid={p} never completed under throttling (lost in BTQ?)"
        );
    }

    // Sanity: the run really did throttle (otherwise this isn't testing the
    // stall path at all).
    assert!(
        count_kind(&trace, |k| matches!(
            k,
            TraceKind::CbwThrottleCgroups {
                throttled: true,
                ..
            }
        )) >= 1,
        "expected the cgroup to be throttled at least once"
    );
}

// ---------------------------------------------------------------------------
// 5. Determinism guard — the throttled-completion timeline is reproducible.
// ---------------------------------------------------------------------------

#[test]
fn test_throttle_completion_determinism() {
    let _lock = common::setup_test();
    let t1 = Simulator::new(lavd_cpu_bw(1)).run(contended_throttled_scenario(3, 1000));
    let t2 = Simulator::new(lavd_cpu_bw(1)).run(contended_throttled_scenario(3, 1000));

    assert_eq!(t1.exit_kind(), &ExitKind::Normal);
    assert_eq!(
        t1.events().len(),
        t2.events().len(),
        "trace lengths differ across identical runs ({} vs {})",
        t1.events().len(),
        t2.events().len()
    );
    for (i, (e1, e2)) in t1.events().iter().zip(t2.events().iter()).enumerate() {
        assert_eq!(e1.time_ns, e2.time_ns, "event {i} time differs");
        assert_eq!(e1.kind, e2.kind, "event {i} kind differs");
    }
}
