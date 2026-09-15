//! Tests for scheduler exit and resource-cleanup paths, exercised across all
//! three natively-supported schedulers (simple, lavd, cosmos).
//!
//! # The shutdown sequence being tested
//!
//! At the end of `Simulator::run` the engine performs the kernel's
//! scheduler-unload handshake (see `safe/engine.rs`), in this order:
//!
//! 1. `SimulationEnd { pid }` for every task still ON-CPU at the deadline.
//! 2. `dump` + `dump_task` for every task (no trace event).
//! 3. `exit_task` for every task → `TraceKind::ExitTask { pid }`.
//! 4. `cgroup_exit` for every cgroup, in reverse pre-order (children before
//!    root) → `TraceKind::CgroupExit { cgid }`.
//! 5. `ops.exit` (`self.scheduler.exit()`).
//! 6. `Trace::set_exit_kind`.
//!
//! Cleanup is therefore observable as the *balance* and *ordering* of the
//! init/exit structops: every task that was `InitTask`'d is `ExitTask`'d, and
//! every cgroup that was `CgroupInit`'d is `CgroupExit`'d — regardless of
//! whether the task finished, was still queued, or was throttled at the
//! deadline. Re-initialisation is tested by loading a fresh scheduler (drop →
//! `dlclose` resets the `.so`'s globals; the next load re-runs `*_setup`).
//!
//! No-Stub / model-the-kernel: assertions are on structops the engine
//! delivers to the real scheduler `.so`. Scenarios are deterministic
//! (`seed` + `instant_timing`).

use scx_simulator::*;
use std::collections::BTreeSet;

mod common;

/// The three natively-supported schedulers. `simple` is single-CPU only, so
/// cross-scheduler scenarios use a single CPU where all three are comparable.
const SCHEDS: &[&str] = &["simple", "lavd", "cosmos"];

fn make_sched(name: &str, ncpu: u32) -> DynamicScheduler {
    match name {
        "simple" => DynamicScheduler::simple(),
        "lavd" => DynamicScheduler::lavd(ncpu),
        "cosmos" => DynamicScheduler::cosmos(ncpu),
        other => panic!("unknown scheduler {other}"),
    }
}

fn run_forever(ns: u64) -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(ns)],
        repeat: RepeatMode::Forever,
    }
}

fn run_once(ns: u64) -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(ns)],
        repeat: RepeatMode::Once,
    }
}

/// LAVD with cgroup-bandwidth enforcement turned on (for the throttled-exit
/// test). Mirrors the recipe in `cgroup_bw_throttle_unthrottle.rs`.
fn lavd_cpu_bw(ncpu: u32) -> DynamicScheduler {
    let sched = DynamicScheduler::lavd(ncpu);
    sched.lavd_set_cgroup_bw_max(64);
    // SAFETY: `enable_cpu_bw` is a `bool` global in LAVD's main.bpf.c; the
    // symbol is present in the loaded `.so`, which outlives this write.
    unsafe {
        let sym: libloading::Symbol<'_, *mut bool> = sched
            .get_symbol(b"enable_cpu_bw\0")
            .expect("enable_cpu_bw symbol");
        std::ptr::write_volatile(*sym, true);
    }
    sched
}

fn count_kind(trace: &Trace, pred: impl Fn(&TraceKind) -> bool) -> usize {
    trace.events().iter().filter(|e| pred(&e.kind)).count()
}

/// Event-vector indices (insertion order) of events matching `pred`.
fn indices(trace: &Trace, pred: impl Fn(&TraceKind) -> bool) -> Vec<usize> {
    trace
        .events()
        .iter()
        .enumerate()
        .filter(|(_, e)| pred(&e.kind))
        .map(|(i, _)| i)
        .collect()
}

fn init_task_pids(trace: &Trace) -> BTreeSet<i32> {
    trace
        .events()
        .iter()
        .filter_map(|e| match &e.kind {
            TraceKind::InitTask { pid, .. } => Some(pid.0),
            _ => None,
        })
        .collect()
}

fn exit_task_pids(trace: &Trace) -> BTreeSet<i32> {
    trace
        .events()
        .iter()
        .filter_map(|e| match &e.kind {
            TraceKind::ExitTask { pid } => Some(pid.0),
            _ => None,
        })
        .collect()
}

fn cgroup_init_cgids(trace: &Trace) -> BTreeSet<u64> {
    trace
        .events()
        .iter()
        .filter_map(|e| match &e.kind {
            TraceKind::CgroupInit { cgid, .. } => Some(cgid.0),
            _ => None,
        })
        .collect()
}

/// `CgroupExit` cgids in emission order (needed to check reverse-order / root-last).
fn cgroup_exit_order(trace: &Trace) -> Vec<u64> {
    trace
        .events()
        .iter()
        .filter_map(|e| match &e.kind {
            TraceKind::CgroupExit { cgid } => Some(cgid.0),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 1. Clean exit when the simulation completes (task finishes before deadline).
// ---------------------------------------------------------------------------

#[test]
fn test_clean_exit_on_completion_all_schedulers() {
    let _lock = common::setup_test();
    for &name in SCHEDS {
        let scenario = Scenario::builder()
            .cpus(1)
            .seed(42)
            .instant_timing()
            .add_task("worker", 0, run_once(5_000_000))
            .duration_ms(50)
            .build();
        let trace = Simulator::new(make_sched(name, 1)).run(scenario);

        assert_eq!(trace.exit_kind(), &ExitKind::Normal, "{name}: clean exit");
        assert!(!trace.has_error(), "{name}: {:?}", trace.exit_kind());

        // The task finished on its own before the deadline ...
        assert_eq!(
            count_kind(&trace, |k| matches!(k, TraceKind::TaskCompleted { .. })),
            1,
            "{name}: the run-once task should complete before the deadline"
        );
        // ... nothing was left on-CPU at the deadline ...
        assert_eq!(
            count_kind(&trace, |k| matches!(k, TraceKind::SimulationEnd { .. })),
            0,
            "{name}: no task should be running at the deadline after completion"
        );
        // ... and it was still cleaned up (exit_task fired for it).
        assert_eq!(
            exit_task_pids(&trace),
            init_task_pids(&trace),
            "{name}: every initialized task must be exit_task'd"
        );
        assert!(
            exit_task_pids(&trace).contains(&1),
            "{name}: the worker task (pid 1) must be exit_task'd"
        );
        // The root cgroup is always torn down.
        assert!(
            cgroup_exit_order(&trace).contains(&CgroupId::ROOT.0),
            "{name}: the root cgroup must be cgroup_exit'd"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. Exit with tasks still in the run queues (never completed).
// ---------------------------------------------------------------------------

#[test]
fn test_exit_with_tasks_still_queued_all_schedulers() {
    let _lock = common::setup_test();
    for &name in SCHEDS {
        // Four never-ending tasks on one CPU: at the deadline several are
        // still queued/runnable and none has completed.
        let scenario = Scenario::builder()
            .cpus(1)
            .seed(42)
            .instant_timing()
            .add_task("a", 0, run_forever(5_000_000))
            .add_task("b", 0, run_forever(5_000_000))
            .add_task("c", 0, run_forever(5_000_000))
            .add_task("d", 0, run_forever(5_000_000))
            .duration_ms(40)
            .build();
        let trace = Simulator::new(make_sched(name, 1)).run(scenario);

        assert_eq!(trace.exit_kind(), &ExitKind::Normal, "{name}: clean exit");
        assert!(!trace.has_error(), "{name}: {:?}", trace.exit_kind());

        // No forever-task ever completes ...
        assert_eq!(
            count_kind(&trace, |k| matches!(k, TraceKind::TaskCompleted { .. })),
            0,
            "{name}: forever tasks should not complete"
        );
        // ... something was still on-CPU at the deadline (active shutdown) ...
        assert!(
            count_kind(&trace, |k| matches!(k, TraceKind::SimulationEnd { .. })) >= 1,
            "{name}: expected a task running at the deadline"
        );
        // ... yet EVERY task (all four, including the queued ones) is cleaned
        // up. This is the core "exit with tasks still in queues" guarantee.
        assert_eq!(
            init_task_pids(&trace),
            BTreeSet::from([1, 2, 3, 4]),
            "{name}: all four tasks should have been initialized"
        );
        assert_eq!(
            exit_task_pids(&trace),
            init_task_pids(&trace),
            "{name}: every queued task must still be exit_task'd at shutdown"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. Exit while a cgroup/task is in the THROTTLED state (LAVD + cgroup bw).
// ---------------------------------------------------------------------------

/// A tight-quota cgroup drives its CPU-bound task into the throttled state;
/// the simulation ends while throttle enforcement is active. Shutdown must
/// still be clean and must tear down the throttled cgroup and its task.
#[test]
fn test_exit_with_throttled_task_clean() {
    let _lock = common::setup_test();
    let scenario = Scenario::builder()
        .cpus(4)
        .seed(42)
        .instant_timing()
        .cgroup_with_bandwidth(
            "tight",
            &[CpuId(0), CpuId(1), CpuId(2), CpuId(3)],
            100_000, // period_us
            10_000,  // quota_us (10%)
            0,
        )
        .add_task_in_cgroup("hog", 0, run_forever(2_000_000_000), "tight")
        .duration_ms(450)
        .build();
    let trace = Simulator::new(lavd_cpu_bw(4)).run(scenario);

    assert_eq!(trace.exit_kind(), &ExitKind::Normal, "throttled-exit clean");
    assert!(
        !trace.has_error(),
        "unexpected error: {:?}",
        trace.exit_kind()
    );

    // We actually exercised the throttled state during the run.
    assert!(
        count_kind(&trace, |k| matches!(
            k,
            TraceKind::CbwThrottleCgroups {
                throttled: true,
                ..
            }
        )) >= 1,
        "expected the tight cgroup to be throttled at some point"
    );

    // The task and BOTH cgroups (tight child + root) are torn down cleanly.
    assert!(
        exit_task_pids(&trace).contains(&1),
        "the throttled task must be exit_task'd"
    );
    let init_cgs = cgroup_init_cgids(&trace);
    let exit_cgs: BTreeSet<u64> = cgroup_exit_order(&trace).into_iter().collect();
    assert_eq!(
        init_cgs, exit_cgs,
        "every cgroup_init must have a matching cgroup_exit even when throttled: \
         init={init_cgs:?} exit={exit_cgs:?}"
    );
    assert!(
        init_cgs.len() >= 2,
        "expected at least the tight cgroup + root, got {init_cgs:?}"
    );
}

// ---------------------------------------------------------------------------
// 4. Resource-cleanup balance & ordering (maps/DSQs/per-CPU via structops).
// ---------------------------------------------------------------------------

/// The cleanup handshake must be balanced and correctly ordered for every
/// scheduler: task resources released (init↔exit balance), cgroup resources
/// released children-before-root, and task cleanup strictly before cgroup
/// cleanup (which precedes `ops.exit`). This is the observable proxy for the
/// scheduler releasing its per-task / per-cgroup / DSQ / map state.
#[test]
fn test_resource_cleanup_balance_and_order_all_schedulers() {
    let _lock = common::setup_test();
    for &name in SCHEDS {
        let scenario = Scenario::builder()
            .cpus(1)
            .seed(42)
            .instant_timing()
            .cgroup("g", &[CpuId(0)])
            .add_task_in_cgroup("t1", 0, run_forever(4_000_000), "g")
            .add_task_in_cgroup("t2", 0, run_forever(4_000_000), "g")
            .duration_ms(40)
            .build();
        let trace = Simulator::new(make_sched(name, 1)).run(scenario);

        assert_eq!(trace.exit_kind(), &ExitKind::Normal, "{name}: clean exit");
        assert!(!trace.has_error(), "{name}: {:?}", trace.exit_kind());

        // Task resources: every init has a matching exit (same pid set).
        assert_eq!(
            init_task_pids(&trace),
            exit_task_pids(&trace),
            "{name}: task init/exit imbalance"
        );
        // Cgroup resources: every init has a matching exit (same cgid set),
        // and the explicit cgroup "g" plus root are both present.
        let init_cgs = cgroup_init_cgids(&trace);
        let exit_seq = cgroup_exit_order(&trace);
        let exit_cgs: BTreeSet<u64> = exit_seq.iter().copied().collect();
        assert_eq!(init_cgs, exit_cgs, "{name}: cgroup init/exit imbalance");
        assert!(
            init_cgs.len() >= 2 && init_cgs.contains(&CgroupId::ROOT.0),
            "{name}: expected an explicit cgroup + root, got {init_cgs:?}"
        );

        // Hierarchical teardown: the root cgroup is destroyed LAST (children
        // before parents).
        assert_eq!(
            *exit_seq.last().expect("at least one cgroup_exit"),
            CgroupId::ROOT.0,
            "{name}: root cgroup must be exited last; order={exit_seq:?}"
        );

        // Ordering: all task cleanup (ExitTask) precedes all cgroup cleanup
        // (CgroupExit); and any SimulationEnd precedes task cleanup. Compared
        // by event-vector index (shutdown records are appended in order).
        let sim_end = indices(&trace, |k| matches!(k, TraceKind::SimulationEnd { .. }));
        let exit_tasks = indices(&trace, |k| matches!(k, TraceKind::ExitTask { .. }));
        let cg_exits = indices(&trace, |k| matches!(k, TraceKind::CgroupExit { .. }));
        assert!(
            !exit_tasks.is_empty() && !cg_exits.is_empty(),
            "{name}: missing cleanup events"
        );
        assert!(
            *exit_tasks.iter().max().unwrap() < *cg_exits.iter().min().unwrap(),
            "{name}: all exit_task must precede all cgroup_exit"
        );
        if let (Some(&max_se), Some(&min_et)) = (sim_end.iter().max(), exit_tasks.iter().min()) {
            assert!(
                max_se < min_et,
                "{name}: SimulationEnd must precede exit_task cleanup"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 5. Re-initialisation after exit.
// ---------------------------------------------------------------------------

/// Loading a fresh scheduler after a previous one exited must reset all
/// global `.so` state: a second run of the same scenario reproduces the first
/// run byte-for-byte (proves exit left no residue and re-init is clean).
#[test]
fn test_reinitialization_same_scheduler_deterministic() {
    let _lock = common::setup_test();
    let mk = || {
        Scenario::builder()
            .cpus(2)
            .seed(42)
            .instant_timing()
            .add_task("a", 0, run_forever(3_000_000))
            .add_task("b", 0, run_forever(3_000_000))
            .duration_ms(30)
            .build()
    };
    for &name in SCHEDS {
        let t1 = Simulator::new(make_sched(name, 2)).run(mk());
        let t2 = Simulator::new(make_sched(name, 2)).run(mk());

        assert_eq!(t1.exit_kind(), &ExitKind::Normal, "{name}: run1");
        assert_eq!(
            t2.exit_kind(),
            &ExitKind::Normal,
            "{name}: run2 (after re-init)"
        );
        assert_eq!(
            t1.events().len(),
            t2.events().len(),
            "{name}: re-init produced a different-length trace"
        );
        for (i, (e1, e2)) in t1.events().iter().zip(t2.events().iter()).enumerate() {
            assert_eq!(
                e1.time_ns, e2.time_ns,
                "{name}: event {i} time differs after re-init"
            );
            assert_eq!(
                e1.cpu, e2.cpu,
                "{name}: event {i} cpu differs after re-init"
            );
            assert_eq!(
                e1.kind, e2.kind,
                "{name}: event {i} kind differs after re-init"
            );
        }
    }
}

/// Unloading one scheduler and loading a DIFFERENT one must not leak global
/// state across the `.so` boundary: a mixed sequence of loads/runs all exit
/// cleanly, and re-loading the first scheduler still reproduces its baseline.
#[test]
fn test_reinitialization_across_different_schedulers() {
    let _lock = common::setup_test();
    let mk = || {
        Scenario::builder()
            .cpus(2)
            .seed(42)
            .instant_timing()
            .add_task("a", 0, run_forever(3_000_000))
            .add_task("b", 0, run_forever(3_000_000))
            .duration_ms(30)
            .build()
    };

    // Baseline for lavd before any other scheduler has been loaded.
    let lavd_baseline = Simulator::new(DynamicScheduler::lavd(2)).run(mk());
    assert_eq!(lavd_baseline.exit_kind(), &ExitKind::Normal);

    // Interleave other schedulers (each unloads on drop).
    for &name in &["cosmos", "simple", "cosmos", "lavd"] {
        let ncpu = if name == "simple" { 1 } else { 2 };
        let trace = Simulator::new(make_sched(name, ncpu)).run(mk());
        assert_eq!(
            trace.exit_kind(),
            &ExitKind::Normal,
            "{name}: should exit cleanly when loaded after another scheduler"
        );
        assert!(!trace.has_error(), "{name}: {:?}", trace.exit_kind());
        assert_eq!(
            exit_task_pids(&trace),
            init_task_pids(&trace),
            "{name}: cleanup imbalance after cross-scheduler reload"
        );
    }

    // Re-loading lavd after the others reproduces its baseline exactly:
    // no cross-scheduler global-state contamination survived the reloads.
    let lavd_again = Simulator::new(DynamicScheduler::lavd(2)).run(mk());
    assert_eq!(
        lavd_baseline.events().len(),
        lavd_again.events().len(),
        "lavd trace length changed after other schedulers were loaded/unloaded"
    );
    for (i, (e1, e2)) in lavd_baseline
        .events()
        .iter()
        .zip(lavd_again.events().iter())
        .enumerate()
    {
        assert_eq!(
            e1.kind, e2.kind,
            "lavd event {i} differs after cross-scheduler reloads"
        );
    }
}
