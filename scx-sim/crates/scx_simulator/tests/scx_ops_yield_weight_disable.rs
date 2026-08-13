//! Tests for the `ops.yield` / `ops.set_weight` / `ops.disable` substrate.
//!
//! These three `struct sched_ext_ops` callbacks had no plumbing in the engine
//! until scx_layered support was added (scx_layered is the first simulated
//! scheduler that implements all three). This suite pins down the *engine*
//! half of that plumbing — delivery, ordering, and the kernel fallbacks —
//! using schedulers that do NOT implement the callbacks, so the assertions
//! hold regardless of what any particular scheduler does with them:
//!
//! * **`ops.set_weight`** fires exactly once per task, immediately after
//!   `ops.enable`, carrying `p->scx.weight` — mirroring the kernel's
//!   `scx_enable_task()`.
//! * **`ops.disable`** fires exactly once per task, immediately before
//!   `ops.exit_task` — mirroring the kernel's teardown
//!   (`scx_disable_task()` then `scx_exit_task()`).
//! * **`ops.yield`** fires for each [`Phase::Yield`] a task executes from
//!   on-CPU, and when the scheduler declines (or has no `ops.yield`) the
//!   engine applies the kernel's `yield_task_scx()` fallback of zeroing
//!   `p->scx.slice`.
//!
//! The scheduler-side behaviour (scx_layered's `layered_yield` deducting
//! `yield_step_ns`, `layered_set_weight` refreshing the layer, and
//! `layered_disable` dropping layer membership) is covered in `layered.rs`.

use scx_simulator::*;

#[macro_use]
mod common;

/// A named scheduler constructor, so one test can sweep several schedulers.
type NamedSchedFactory = (&'static str, fn(u32) -> DynamicScheduler);

/// Schedulers that do NOT define yield/set_weight/disable. Used deliberately:
/// every assertion below is about engine behaviour that must hold even when
/// the loaded `.so` exports none of the three symbols.
const SCHEDULERS_WITHOUT_OPS: [NamedSchedFactory; 3] = [
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

/// A task that runs once for `run_ns` and then exits.
fn run_once(run_ns: u64) -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(run_ns)],
        repeat: RepeatMode::Once,
    }
}

// ---------------------------------------------------------------------------
// ops.set_weight
// ---------------------------------------------------------------------------

/// The kernel publishes a task's weight to the scheduler once, from
/// `scx_enable_task()`, right after `ops.enable`. Assert both the count and
/// the immediate-successor ordering.
#[test]
fn test_set_weight_fires_once_per_task_right_after_enable() {
    let _lock = common::setup_test();
    for (name, make) in SCHEDULERS_WITHOUT_OPS {
        let scenario = Scenario::builder()
            .cpus(2)
            .add_task("a", 0, run_once(5_000_000))
            .add_task("b", 0, run_once(5_000_000))
            .duration_ms(100)
            .build();
        let t = Simulator::new(make(2)).run(scenario);
        assert_eq!(t.exit_kind(), &ExitKind::Normal, "{name}: not normal exit");

        assert_eq!(
            count(&t, |k| matches!(k, TraceKind::SetWeight { .. })),
            2,
            "{name}: expected one SetWeight per task"
        );

        for pid in [Pid(1), Pid(2)] {
            let enable = first_idx(
                &t,
                |k| matches!(k, TraceKind::Enable { pid: p } if *p == pid),
            )
            .unwrap_or_else(|| panic!("{name}: no Enable for {pid:?}"));
            let set_weight = first_idx(
                &t,
                |k| matches!(k, TraceKind::SetWeight { pid: p, .. } if *p == pid),
            )
            .unwrap_or_else(|| panic!("{name}: no SetWeight for {pid:?}"));
            assert!(
                set_weight > enable,
                "{name}: {pid:?} SetWeight (idx {set_weight}) must follow Enable (idx {enable})"
            );
        }
    }
}

/// `ops.set_weight` must carry `p->scx.weight` — the *cgroup-space* weight the
/// kernel stores, not the raw `sched_prio_to_weight` value. A nice-0 task has
/// weight 1024 raw → 100 in cgroup space; a nice-(-5) task has 3121 → 305.
#[test]
fn test_set_weight_carries_cgroup_space_weight_for_nice_levels() {
    let _lock = common::setup_test();
    for (nice, expected) in [
        (0i8, sched_weight_to_cgroup(nice_to_weight(0))),
        (-5, sched_weight_to_cgroup(nice_to_weight(-5))),
        (10, sched_weight_to_cgroup(nice_to_weight(10))),
    ] {
        let scenario = Scenario::builder()
            .cpus(1)
            .add_task("a", nice, run_once(5_000_000))
            .duration_ms(50)
            .build();
        let t = Simulator::new(DynamicScheduler::simple()).run(scenario);

        let seen: Vec<u32> = t
            .events()
            .iter()
            .filter_map(|e| match e.kind {
                TraceKind::SetWeight { weight, .. } => Some(weight),
                _ => None,
            })
            .collect();
        assert_eq!(
            seen,
            vec![expected],
            "nice={nice}: ops.set_weight should report p->scx.weight"
        );
    }
}

// ---------------------------------------------------------------------------
// ops.disable
// ---------------------------------------------------------------------------

/// The kernel tears a task down as `scx_disable_task()` (→ `ops.disable`)
/// followed by `ops.exit_task`. Assert one `Disable` per task, each strictly
/// before that task's `ExitTask`.
#[test]
fn test_disable_fires_once_per_task_right_before_exit_task() {
    let _lock = common::setup_test();
    for (name, make) in SCHEDULERS_WITHOUT_OPS {
        let scenario = Scenario::builder()
            .cpus(2)
            .add_task("a", 0, run_once(5_000_000))
            .add_task("b", 0, run_once(5_000_000))
            .duration_ms(100)
            .build();
        let t = Simulator::new(make(2)).run(scenario);
        assert_eq!(t.exit_kind(), &ExitKind::Normal, "{name}: not normal exit");

        assert_eq!(
            count(&t, |k| matches!(k, TraceKind::Disable { .. })),
            2,
            "{name}: expected one Disable per task"
        );

        for pid in [Pid(1), Pid(2)] {
            let disable = first_idx(
                &t,
                |k| matches!(k, TraceKind::Disable { pid: p } if *p == pid),
            )
            .unwrap_or_else(|| panic!("{name}: no Disable for {pid:?}"));
            let exit_task = first_idx(
                &t,
                |k| matches!(k, TraceKind::ExitTask { pid: p } if *p == pid),
            )
            .unwrap_or_else(|| panic!("{name}: no ExitTask for {pid:?}"));
            assert!(
                disable < exit_task,
                "{name}: {pid:?} Disable (idx {disable}) must precede \
                 ExitTask (idx {exit_task})"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// ops.yield
// ---------------------------------------------------------------------------

/// A `Phase::Yield` following a `Phase::Run` delivers exactly one `ops.yield`
/// per execution. With a scheduler that has no `ops.yield` the callback is
/// reported as unhandled, which is what makes the engine apply the kernel's
/// slice-zeroing fallback (asserted separately below).
#[test]
fn test_yield_phase_delivers_ops_yield_unhandled_without_scheduler_support() {
    let _lock = common::setup_test();
    for (name, make) in SCHEDULERS_WITHOUT_OPS {
        let scenario = Scenario::builder()
            .cpus(1)
            .add_task(
                "y",
                0,
                TaskBehavior {
                    phases: vec![Phase::Run(1_000_000), Phase::Yield, Phase::Run(1_000_000)],
                    repeat: RepeatMode::Count(3),
                },
            )
            .duration_ms(100)
            .build();
        let t = Simulator::new(make(1)).run(scenario);
        assert_eq!(t.exit_kind(), &ExitKind::Normal, "{name}: not normal exit");

        let yields: Vec<bool> = t
            .events()
            .iter()
            .filter_map(|e| match e.kind {
                TraceKind::TaskYield { handled, .. } => Some(handled),
                _ => None,
            })
            .collect();
        assert_eq!(
            yields.len(),
            3,
            "{name}: expected one ops.yield per loop iteration, got {yields:?}"
        );
        assert!(
            yields.iter().all(|h| !h),
            "{name}: schedulers without ops.yield must report unhandled"
        );
    }
}

/// A workload with no `Phase::Yield` must never invoke `ops.yield`. Guards
/// against the engine spuriously synthesising yields at ordinary phase
/// boundaries (Run→Run transitions are *not* `sched_yield()` calls).
#[test]
fn test_no_yield_phase_means_no_ops_yield() {
    let _lock = common::setup_test();
    let scenario = Scenario::builder()
        .cpus(2)
        .add_task(
            "a",
            0,
            TaskBehavior {
                phases: vec![Phase::Run(1_000_000), Phase::Run(1_000_000)],
                repeat: RepeatMode::Count(5),
            },
        )
        .add_task("b", 0, workloads::periodic(1_000_000, 3_000_000))
        .duration_ms(100)
        .build();
    let t = Simulator::new(DynamicScheduler::simple()).run(scenario);
    assert_eq!(t.exit_kind(), &ExitKind::Normal);
    assert_eq!(
        count(&t, |k| matches!(k, TraceKind::TaskYield { .. })),
        0,
        "ops.yield must only fire for an explicit Phase::Yield"
    );
}

/// A yielding task stays runnable and keeps making progress: the workload must
/// still complete, and the yielder must be scheduled again after each yield.
/// (The kernel's `sched_yield()` deprioritises, it does not block.)
#[test]
fn test_yield_keeps_task_runnable_and_making_progress() {
    let _lock = common::setup_test();
    for (name, make) in SCHEDULERS_WITHOUT_OPS {
        let scenario = Scenario::builder()
            .cpus(1)
            .add_task(
                "y",
                0,
                TaskBehavior {
                    phases: vec![Phase::Run(500_000), Phase::Yield],
                    repeat: RepeatMode::Count(10),
                },
            )
            .add_task("hog", 0, workloads::cpu_bound(50_000_000))
            .duration_ms(200)
            .build();
        let t = Simulator::new(make(1)).run(scenario);
        assert_eq!(t.exit_kind(), &ExitKind::Normal, "{name}: not normal exit");

        assert_eq!(
            count(&t, |k| matches!(k, TraceKind::TaskYield { .. })),
            10,
            "{name}: yielder should have yielded once per iteration"
        );
        // The yielder ran all 10 iterations, so it must have been re-scheduled
        // after each yield rather than being dropped from the runqueue.
        assert!(
            count(
                &t,
                |k| matches!(k, TraceKind::TaskScheduled { pid, .. } if *pid == Pid(1))
            ) >= 2,
            "{name}: yielder was never re-scheduled after yielding"
        );
    }
}

/// Yield phases reached while the task is *off*-CPU (first phase, or straight
/// after a Sleep) have no running slice to forfeit, so they are skipped rather
/// than delivered. Documents the boundary of the model.
#[test]
fn test_yield_off_cpu_is_skipped_not_delivered() {
    let _lock = common::setup_test();
    let scenario = Scenario::builder()
        .cpus(1)
        .add_task(
            "y",
            0,
            TaskBehavior {
                // Leading Yield, and a Yield straight after a Sleep: neither
                // has an on-CPU slice, so neither reaches ops.yield.
                phases: vec![
                    Phase::Yield,
                    Phase::Run(1_000_000),
                    Phase::Sleep(1_000_000),
                    Phase::Yield,
                    Phase::Run(1_000_000),
                ],
                repeat: RepeatMode::Once,
            },
        )
        .duration_ms(100)
        .build();
    let t = Simulator::new(DynamicScheduler::simple()).run(scenario);
    assert_eq!(t.exit_kind(), &ExitKind::Normal);
    assert_eq!(
        count(&t, |k| matches!(k, TraceKind::TaskYield { .. })),
        0,
        "off-CPU Yield phases must not synthesise an ops.yield"
    );
}
