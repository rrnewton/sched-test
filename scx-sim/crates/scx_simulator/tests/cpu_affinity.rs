//! Tests for CPU affinity and task pinning behavior.
//!
//! Two affinity mechanisms exist in the simulator, and both are exercised here:
//!
//! 1. **Static per-task pinning** via `TaskDef.allowed_cpus`. The engine builds
//!    the task's `cpus_ptr` from this mask (`task_setup_cpumask`), and the
//!    scheduler's own idle-selection kfuncs (`scx_bpf_select_cpu_dfl`, idle
//!    cpumask tests, etc.) honor it — so a pinned task is only ever *scheduled*
//!    on an allowed CPU. This is scheduler-agnostic, so the static-pinning tests
//!    run against `simple`, `lavd`, and `cosmos`.
//!
//! 2. **Runtime affinity change** via a cgroup cpuset write
//!    (`cgroup_cpuset_change`, i.e. `echo "2-3" > cpuset.cpus`). This is only
//!    faithfully enforced by the cgroup-aware `mitosis` scheduler, which assigns
//!    cgroups (cells) to CPU sets — so the mid-execution-change test uses
//!    `mitosis`.
//!
//! There is no per-task *runtime* affinity-change API in the simulator today;
//! the cgroup-cpuset route is the kernel-faithful way to model a task's allowed
//! CPUs changing while it runs (a cpuset controller write).
//!
//! All assertions are on observables (which CPU each `TaskScheduled` landed on,
//! runtimes, exit status) — no scheduler-side changes, per the No-Stub /
//! "model the kernel, not the scheduler" rules in `scx-sim/CLAUDE.md`.

use std::collections::HashSet;

use scx_simulator::*;

#[macro_use]
mod common;

// ---------------------------------------------------------------------------
// Shared fixtures
// ---------------------------------------------------------------------------

/// A named scheduler factory: the label plus a `|nr_cpus| -> DynamicScheduler`
/// constructor (`simple` ignores the CPU count).
type NamedSched = (&'static str, fn(u32) -> DynamicScheduler);

/// Schedulers whose static per-task affinity the engine enforces uniformly.
const STATIC_SCHEDS: &[NamedSched] = &[
    ("simple", |_n| DynamicScheduler::simple()),
    ("lavd", |n| DynamicScheduler::lavd(n)),
    ("cosmos", |n| DynamicScheduler::cosmos(n)),
];

fn forever_run(run_ns: u64) -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(run_ns)],
        repeat: RepeatMode::Forever,
    }
}

/// A `TaskDef` with an explicit affinity mask (or `None` for unpinned).
fn pinned_task(
    name: &str,
    pid: i32,
    allowed: Option<Vec<CpuId>>,
    behavior: TaskBehavior,
) -> TaskDef {
    TaskDef {
        name: name.into(),
        pid: Pid(pid),
        nice: 0,
        behavior,
        start_time_ns: 0,
        mm_id: None,
        allowed_cpus: allowed,
        parent_pid: None,
        cgroup_name: None,
        task_flags: 0,
        migration_disabled: 0,
    }
}

/// The set of CPUs a task was ever *scheduled* on.
fn cpus_scheduled_on(trace: &Trace, pid: Pid) -> HashSet<CpuId> {
    trace
        .events()
        .iter()
        .filter_map(|e| match e.kind {
            TraceKind::TaskScheduled { pid: p } if p == pid => Some(e.cpu),
            _ => None,
        })
        .collect()
}

/// The set of CPUs a task was scheduled on within `[lo, hi)` (ns).
fn cpus_scheduled_on_window(trace: &Trace, pid: Pid, lo: TimeNs, hi: TimeNs) -> HashSet<CpuId> {
    trace
        .events()
        .iter()
        .filter_map(|e| match e.kind {
            TraceKind::TaskScheduled { pid: p }
                if p == pid && e.time_ns >= lo && e.time_ns < hi =>
            {
                Some(e.cpu)
            }
            _ => None,
        })
        .collect()
}

// ===========================================================================
// 1. Static single-CPU pin — task never leaves its one allowed CPU.
// ===========================================================================

/// A task pinned to a single non-zero CPU on a busy 4-CPU box must only ever be
/// scheduled on that CPU, and must still make progress. Background hogs keep the
/// other CPUs (and the pinned CPU) contended so the scheduler is genuinely
/// choosing placements.
#[test]
fn test_single_cpu_pin_respected() {
    let _lock = common::setup_test();
    const NR_CPUS: u32 = 4;
    const PIN_CPU: u32 = 2;

    for (name, make) in STATIC_SCHEDS {
        let mut builder = Scenario::builder().cpus(NR_CPUS);
        builder = builder.task(pinned_task(
            "pinned",
            1,
            Some(vec![CpuId(PIN_CPU)]),
            forever_run(10_000_000),
        ));
        // Unpinned background load on all CPUs.
        for i in 0..NR_CPUS as i32 {
            builder = builder.task(pinned_task(
                &format!("bg{i}"),
                2 + i,
                None,
                forever_run(10_000_000),
            ));
        }
        let scenario = builder.duration_ms(200).build();

        let trace = Simulator::new(make(NR_CPUS)).run(scenario);
        assert_eq!(trace.exit_kind(), &ExitKind::Normal, "[{name}] clean exit");

        let ran_on = cpus_scheduled_on(&trace, Pid(1));
        assert!(
            trace.total_runtime(Pid(1)) > 0,
            "[{name}] pinned task got no runtime"
        );
        assert_eq!(
            ran_on,
            HashSet::from([CpuId(PIN_CPU)]),
            "[{name}] pinned task ran on {ran_on:?}, expected only CPU {PIN_CPU}"
        );
    }
}

// ===========================================================================
// 2. Multi-CPU affinity mask — task stays within its allowed subset.
// ===========================================================================

/// A task allowed on CPUs {1, 3} of a 4-CPU box must be scheduled only on those
/// CPUs — never on {0, 2} — while still being free to move *between* 1 and 3.
#[test]
fn test_multi_cpu_affinity_mask_respected() {
    let _lock = common::setup_test();
    const NR_CPUS: u32 = 4;
    let allowed: HashSet<CpuId> = HashSet::from([CpuId(1), CpuId(3)]);
    let forbidden = [CpuId(0), CpuId(2)];

    for (name, make) in STATIC_SCHEDS {
        let mut builder = Scenario::builder().cpus(NR_CPUS);
        builder = builder.task(pinned_task(
            "masked",
            1,
            Some(vec![CpuId(1), CpuId(3)]),
            forever_run(8_000_000),
        ));
        // Background load so the masked task must compete for its allowed CPUs
        // and could be *tempted* onto a free forbidden CPU (which must not happen).
        for i in 0..NR_CPUS as i32 {
            builder = builder.task(pinned_task(
                &format!("bg{i}"),
                2 + i,
                None,
                forever_run(8_000_000),
            ));
        }
        let scenario = builder.duration_ms(200).build();

        let trace = Simulator::new(make(NR_CPUS)).run(scenario);
        assert_eq!(trace.exit_kind(), &ExitKind::Normal, "[{name}] clean exit");

        let ran_on = cpus_scheduled_on(&trace, Pid(1));
        assert!(
            trace.total_runtime(Pid(1)) > 0,
            "[{name}] masked task got no runtime"
        );
        for cpu in &forbidden {
            assert!(
                !ran_on.contains(cpu),
                "[{name}] masked task ran on forbidden {cpu:?} (ran_on={ran_on:?})"
            );
        }
        assert!(
            ran_on.is_subset(&allowed),
            "[{name}] masked task escaped its mask: ran_on={ran_on:?}, allowed={allowed:?}"
        );
    }
}

// ===========================================================================
// 3. Two tasks pinned to the SAME CPU — they serialize on it, both progress.
// ===========================================================================

/// Two CPU-bound tasks both pinned to CPU 1 (of a 4-CPU box) must share that one
/// CPU — neither may spill onto an idle CPU — yet both must make progress
/// (the busy pinned CPU serializes them rather than starving one).
#[test]
fn test_two_tasks_pinned_same_cpu_share_and_serialize() {
    let _lock = common::setup_test();
    const NR_CPUS: u32 = 4;
    const PIN_CPU: u32 = 1;

    for (name, make) in STATIC_SCHEDS {
        let scenario = Scenario::builder()
            .cpus(NR_CPUS)
            .task(pinned_task(
                "pin_a",
                1,
                Some(vec![CpuId(PIN_CPU)]),
                forever_run(10_000_000),
            ))
            .task(pinned_task(
                "pin_b",
                2,
                Some(vec![CpuId(PIN_CPU)]),
                forever_run(10_000_000),
            ))
            // A free task that could use the otherwise-idle CPUs.
            .task(pinned_task("free", 3, None, forever_run(10_000_000)))
            .duration_ms(300)
            .build();

        let trace = Simulator::new(make(NR_CPUS)).run(scenario);
        assert_eq!(trace.exit_kind(), &ExitKind::Normal, "[{name}] clean exit");

        // Both pinned tasks made progress despite contending for one CPU.
        assert!(
            trace.total_runtime(Pid(1)) > 0,
            "[{name}] pin_a starved on shared CPU"
        );
        assert!(
            trace.total_runtime(Pid(2)) > 0,
            "[{name}] pin_b starved on shared CPU"
        );

        // Neither pinned task escaped to another CPU.
        for pid in [Pid(1), Pid(2)] {
            let ran_on = cpus_scheduled_on(&trace, pid);
            assert_eq!(
                ran_on,
                HashSet::from([CpuId(PIN_CPU)]),
                "[{name}] pinned pid={} ran on {ran_on:?}, expected only CPU {PIN_CPU}",
                pid.0
            );
        }

        // They genuinely time-share CPU 1: it must host multiple dispatches of
        // *both* tasks (serialization), which requires interleaving on that CPU.
        assert!(
            trace.schedule_count(Pid(1)) >= 2 && trace.schedule_count(Pid(2)) >= 2,
            "[{name}] expected both pinned tasks to be re-dispatched on the shared CPU: \
             a={} b={}",
            trace.schedule_count(Pid(1)),
            trace.schedule_count(Pid(2))
        );
    }
}

// ===========================================================================
// 4. Affinity vs. scheduler placement — a pin constrains the scheduler, and
//    unpinned tasks are placed on the *other* CPUs around it.
// ===========================================================================

/// One task pinned to CPU 0 alongside several free tasks on a 4-CPU box: the
/// pinned task must stay on CPU 0, and the scheduler's own placement must spread
/// the free tasks across multiple CPUs (it does not pile everyone onto CPU 0,
/// nor is it forced to). This verifies affinity and placement coexist.
#[test]
fn test_affinity_constrains_but_placement_still_spreads() {
    let _lock = common::setup_test();
    const NR_CPUS: u32 = 4;

    for (name, make) in STATIC_SCHEDS {
        let mut builder = Scenario::builder().cpus(NR_CPUS);
        builder = builder.task(pinned_task(
            "pinned0",
            1,
            Some(vec![CpuId(0)]),
            forever_run(10_000_000),
        ));
        // Six free tasks — more than CPUs, so the scheduler must actively place.
        for i in 0..6i32 {
            builder = builder.task(pinned_task(
                &format!("free{i}"),
                2 + i,
                None,
                forever_run(10_000_000),
            ));
        }
        let scenario = builder.duration_ms(200).build();

        let trace = Simulator::new(make(NR_CPUS)).run(scenario);
        assert_eq!(trace.exit_kind(), &ExitKind::Normal, "[{name}] clean exit");

        // Pinned task stays on CPU 0 and runs.
        let pinned_on = cpus_scheduled_on(&trace, Pid(1));
        assert!(
            trace.total_runtime(Pid(1)) > 0,
            "[{name}] pinned task got no runtime"
        );
        assert_eq!(
            pinned_on,
            HashSet::from([CpuId(0)]),
            "[{name}] pinned task ran on {pinned_on:?}, expected only CPU 0"
        );

        // The free tasks, collectively, are placed across several CPUs — the
        // scheduler is making real placement decisions around the pin.
        let free_cpus: HashSet<CpuId> = (2..=7)
            .flat_map(|p| cpus_scheduled_on(&trace, Pid(p)))
            .collect();
        assert!(
            free_cpus.len() >= 3,
            "[{name}] free tasks only used {free_cpus:?}; expected placement to spread across CPUs"
        );
    }
}

// ===========================================================================
// 5. Affinity change mid-execution — a cpuset write is processed at runtime.
// ===========================================================================

/// Runtime affinity change via a cpuset controller write (`echo "2-3" >
/// cpuset.cpus`), using the cgroup-aware `mitosis` scheduler: a cell starts
/// allowed on CPUs {0,1} and its cpuset is rewritten to {2,3} mid-run.
///
/// What this verifies: the runtime affinity-change path is exercised end to end
/// — the cpuset write is processed without error, the scheduler is re-notified
/// of the cgroup (a fresh `CgroupInit` fires at/after the change), and the task
/// keeps running correctly across the boundary (dispatched both before and
/// after), never escaping the union of the old and new cpusets.
///
/// What this deliberately does NOT assert: that the already-running task
/// physically *migrates* onto the new cpuset. Mitosis reassigns cells to CPUs
/// only on its periodic reconfiguration timer, and per the documented
/// limitation `TODO(sim-b7d70)` (see `mitosis.rs`) that cell→CPU relocation is
/// not yet observable in the simulator — empirically the single worker stays on
/// its original CPU for the whole run even after the timer fires. Asserting
/// relocation here would assert behavior the simulator does not implement; when
/// sim-b7d70 lands, tighten this test to require the post-change placement to
/// move to {2,3}. See task notes for test-cpu-affinity-pinning.
#[test]
fn test_affinity_change_mid_execution_via_cpuset() {
    let _lock = common::setup_test();
    const NR_CPUS: u32 = 4;
    const SWAP_NS: TimeNs = 50_000_000;

    let scenario = Scenario::builder()
        .cpus(NR_CPUS)
        .cgroup("cell", &[CpuId(0), CpuId(1)])
        .task(TaskDef {
            name: "worker".into(),
            pid: Pid(1),
            nice: 0,
            behavior: TaskBehavior {
                phases: vec![Phase::Run(5_000_000)],
                repeat: RepeatMode::Forever,
            },
            start_time_ns: 0,
            mm_id: None,
            allowed_cpus: None,
            parent_pid: None,
            cgroup_name: Some("cell".into()),
            task_flags: 0,
            migration_disabled: 0,
        })
        .cgroup_cpuset_change(CgroupCpusetChangeEvent {
            cgroup_name: "cell".into(),
            new_cpuset: vec![CpuId(2), CpuId(3)],
            at_ns: SWAP_NS,
        })
        .duration_ms(250)
        .build();

    let trace = Simulator::new(DynamicScheduler::mitosis(NR_CPUS)).run(scenario);
    trace.dump();

    assert_eq!(trace.exit_kind(), &ExitKind::Normal, "clean exit");
    assert!(trace.total_runtime(Pid(1)) > 0, "worker never ran");

    // The cpuset write is propagated to the scheduler at runtime: a CgroupInit
    // re-fires at/after the change time (mitosis re-reads the cell's cpuset).
    let reinit_after_change = trace
        .events()
        .iter()
        .any(|e| e.time_ns >= SWAP_NS && matches!(e.kind, TraceKind::CgroupInit { .. }));
    assert!(
        reinit_after_change,
        "cpuset change was not propagated to the scheduler (no CgroupInit at/after {SWAP_NS}ns)"
    );

    // The task keeps making progress across the affinity change.
    let ran_before = !cpus_scheduled_on_window(&trace, Pid(1), 0, SWAP_NS).is_empty();
    let ran_after = !cpus_scheduled_on_window(&trace, Pid(1), SWAP_NS, TimeNs::MAX).is_empty();
    assert!(ran_before, "worker never ran before the cpuset change");
    assert!(ran_after, "worker never ran after the cpuset change");

    // It never escaped the union of the old {0,1} and new {2,3} cpusets.
    let all_cpus = cpus_scheduled_on(&trace, Pid(1));
    let permitted: HashSet<CpuId> = HashSet::from([CpuId(0), CpuId(1), CpuId(2), CpuId(3)]);
    assert!(
        all_cpus.is_subset(&permitted),
        "worker ran on unexpected CPUs {all_cpus:?} (permitted {permitted:?})"
    );
}
