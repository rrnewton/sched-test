//! Simulation-termination / quiescent-state-detection tests.
//!
//! Exercises how the event-driven engine ends a run and reaches a quiescent
//! (terminal) state, across `simple`, `lavd`, and `cosmos`:
//!
//!   1. All finite tasks complete → clean `Normal` exit, every task emits
//!      `TaskCompleted`, and nothing is left running (`SimulationEnd` == 0).
//!   2. Infinite (`Forever`) tasks are stopped by the duration cap → `Normal`
//!      exit, no `TaskCompleted`, and each still-running task is flushed with a
//!      `SimulationEnd` stamped exactly at `duration_ns` (the explicit stop).
//!   3. Intermittent task arrivals (staggered `start_time_ns` with idle gaps
//!      between them) → the engine stays alive across the gaps and every late
//!      arrival still runs and completes; the run ends cleanly.
//!   4. Timeout handling: the `duration_ns` cap is the controlling stop for an
//!      unbounded workload — a longer cap yields proportionally more work, and
//!      the flush timestamps track the cap.
//!   5. Clean resource cleanup between runs: running an unrelated scenario in
//!      the same process leaves no residue — re-running an earlier scenario
//!      reproduces a byte-identical trace (engine + C-side scheduler state is
//!      fully reset per `Simulator::run`).
//!
//! Observed behavior the assertions are built on (empirically probed):
//! - Every scheduler runs to the duration cap even when all tasks have
//!   finished (periodic ticks keep the event queue non-empty), so quiescence
//!   is asserted via `TaskCompleted`/`SimulationEnd` counts, NOT via early
//!   exit timing.
//! - `SimulationEnd` events are stamped at exactly `duration_ns`; the shutdown
//!   (dump/exit_task) path may record a few events slightly *after*
//!   `duration_ns`, so "no event past the cap" is deliberately not asserted.

use std::collections::HashMap;

use scx_simulator::*;

#[macro_use]
mod common;

const ALL: [&str; 3] = ["simple", "lavd", "cosmos"];

fn new_sched(label: &str, cpus: u32) -> DynamicScheduler {
    match label {
        "simple" => DynamicScheduler::simple(),
        "lavd" => DynamicScheduler::lavd(cpus),
        "cosmos" => DynamicScheduler::cosmos(cpus),
        _ => unreachable!(),
    }
}

fn task(name: &str, pid: i32, start_ns: u64, behavior: TaskBehavior) -> TaskDef {
    TaskDef {
        name: name.into(),
        pid: Pid(pid),
        nice: 0,
        behavior,
        start_time_ns: start_ns,
        mm_id: None,
        allowed_cpus: None,
        parent_pid: None,
        cgroup_name: None,
        task_flags: 0,
        migration_disabled: 0,
        thread_group_leader: None,
        uid: Uid(0),
        gid: Gid(0),
        fork_cpu: None,
    }
}

fn run_once(run_ns: u64) -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(run_ns)],
        repeat: RepeatMode::Once,
    }
}

fn count(trace: &Trace, pred: impl Fn(&TraceKind) -> bool) -> usize {
    trace.events().iter().filter(|e| pred(&e.kind)).count()
}

fn completed_pids(trace: &Trace) -> Vec<Pid> {
    trace
        .events()
        .iter()
        .filter_map(|e| match e.kind {
            TraceKind::TaskCompleted { pid } => Some(pid),
            _ => None,
        })
        .collect()
}

/// Byte-identical trace signature (same technique as determinism.rs).
fn trace_signature(trace: &Trace) -> String {
    format!("{:?}", trace.events())
}

// ---------------------------------------------------------------------------
// 1. All finite tasks complete → clean, quiescent termination.
// ---------------------------------------------------------------------------

#[test]
fn test_all_finite_tasks_complete_cleanly() {
    let _lock = common::setup_test();
    const N: i32 = 4;
    const RUN_NS: u64 = 5_000_000;
    for label in ALL {
        // Noise off so the per-task runtime is exact (this test is about clean
        // completion, not jitter); with noise the run phase is perturbed.
        let mut b = Scenario::builder()
            .cpus(4)
            .seed(1)
            .noise(false)
            .detect_bpf_errors();
        for i in 0..N {
            b = b.task(task(&format!("f{i}"), 1 + i, 0, run_once(RUN_NS)));
        }
        let trace = Simulator::new(new_sched(label, 4)).run(b.duration_ms(200).build());

        assert_eq!(
            trace.exit_kind(),
            &ExitKind::Normal,
            "[{label}] not a clean exit: {:?}",
            trace.exit_kind()
        );
        // Every task completed exactly once, and each got its full runtime.
        let mut done = completed_pids(&trace);
        done.sort_by_key(|p| p.0);
        assert_eq!(
            done,
            (0..N).map(|i| Pid(1 + i)).collect::<Vec<_>>(),
            "[{label}] not all tasks completed"
        );
        for i in 0..N {
            let rt = trace.total_runtime(Pid(1 + i));
            // Ran its full single phase (plus at most tick-granularity slop),
            // and did not run away — i.e. it completed exactly one Run(RUN_NS).
            assert!(
                (RUN_NS..RUN_NS + 1_000_000).contains(&rt),
                "[{label}] task {i} ran {rt}ns, expected ~{RUN_NS}ns (one phase)"
            );
        }
        // Quiescent: nothing was still on-CPU at the end.
        assert_eq!(
            count(&trace, |k| matches!(k, TraceKind::SimulationEnd { .. })),
            0,
            "[{label}] tasks still running at exit despite all completing"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. Infinite tasks + explicit stop (duration cap) → flushed, clean exit.
// ---------------------------------------------------------------------------

#[test]
fn test_infinite_tasks_stopped_by_duration() {
    let _lock = common::setup_test();
    const N: i32 = 4;
    const DUR_MS: u64 = 150;
    const DUR_NS: u64 = DUR_MS * 1_000_000;
    for label in ALL {
        let mut b = Scenario::builder().cpus(4).seed(2).detect_bpf_errors();
        for i in 0..N {
            b = b.task(task(
                &format!("inf{i}"),
                1 + i,
                0,
                workloads::cpu_bound(50_000_000),
            ));
        }
        let trace = Simulator::new(new_sched(label, 4)).run(b.duration_ms(DUR_MS).build());

        assert_eq!(
            trace.exit_kind(),
            &ExitKind::Normal,
            "[{label}] infinite run did not stop cleanly: {:?}",
            trace.exit_kind()
        );
        // No Forever task ever "completes".
        assert_eq!(
            count(&trace, |k| matches!(k, TraceKind::TaskCompleted { .. })),
            0,
            "[{label}] a Forever task reported completion"
        );
        // The still-running tasks are flushed with SimulationEnd, stamped at
        // exactly the duration cap (the explicit stop point).
        let sim_ends: Vec<u64> = trace
            .events()
            .iter()
            .filter(|e| matches!(e.kind, TraceKind::SimulationEnd { .. }))
            .map(|e| e.time_ns)
            .collect();
        assert!(
            !sim_ends.is_empty(),
            "[{label}] no SimulationEnd flush for still-running tasks"
        );
        for t in &sim_ends {
            assert_eq!(
                *t, DUR_NS,
                "[{label}] SimulationEnd stamped at {t}ns, expected duration cap {DUR_NS}ns"
            );
        }
        // Work actually happened.
        for i in 0..N {
            assert!(
                trace.total_runtime(Pid(1 + i)) > 0,
                "[{label}] infinite task {i} got no runtime"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 3. Quiescent detection with intermittent (staggered) arrivals.
// ---------------------------------------------------------------------------

#[test]
fn test_quiescent_with_intermittent_arrivals() {
    let _lock = common::setup_test();
    const N: i32 = 4;
    const RUN_NS: u64 = 3_000_000; // 3ms
    const GAP_NS: u64 = 30_000_000; // 30ms between arrivals >> run → real idle gaps
    for label in ALL {
        // One CPU so arrivals genuinely interleave with idle gaps.
        let mut b = Scenario::builder().cpus(1).seed(3).detect_bpf_errors();
        for i in 0..N {
            b = b.task(task(
                &format!("s{i}"),
                1 + i,
                (i as u64) * GAP_NS,
                run_once(RUN_NS),
            ));
        }
        // Duration comfortably past the last arrival + its run.
        let trace = Simulator::new(new_sched(label, 1)).run(b.duration_ms(200).build());

        assert_eq!(
            trace.exit_kind(),
            &ExitKind::Normal,
            "[{label}] intermittent run did not end cleanly: {:?}",
            trace.exit_kind()
        );
        // Every staggered task ran and completed despite the idle gaps.
        let mut done = completed_pids(&trace);
        done.sort_by_key(|p| p.0);
        assert_eq!(
            done,
            (0..N).map(|i| Pid(1 + i)).collect::<Vec<_>>(),
            "[{label}] a late-arriving task never completed"
        );
        // Each task's first scheduling respects its arrival time (it did not run
        // before it existed).
        let mut first_sched: HashMap<Pid, u64> = HashMap::new();
        for e in trace.events() {
            if let TraceKind::TaskScheduled { pid } = e.kind {
                first_sched.entry(pid).or_insert(e.time_ns);
            }
        }
        for i in 0..N {
            let arrival = (i as u64) * GAP_NS;
            let first = first_sched
                .get(&Pid(1 + i))
                .copied()
                .unwrap_or_else(|| panic!("[{label}] task {i} never scheduled"));
            assert!(
                first >= arrival,
                "[{label}] task {i} scheduled at {first}ns, before its arrival {arrival}ns"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 4. Timeout handling: the duration cap is the controlling stop condition.
// ---------------------------------------------------------------------------

#[test]
fn test_duration_cap_bounds_run() {
    let _lock = common::setup_test();
    // Same unbounded workload under a short and a long cap; the longer cap must
    // yield proportionally more accumulated work and flush at its own cap.
    let make = |dur_ms: u64| {
        let mut b = Scenario::builder().cpus(2).seed(4).detect_bpf_errors();
        for i in 0..2i32 {
            b = b.task(task(
                &format!("h{i}"),
                1 + i,
                0,
                workloads::cpu_bound(50_000_000),
            ));
        }
        b.duration_ms(dur_ms).build()
    };

    for label in ALL {
        let short = Simulator::new(new_sched(label, 2)).run(make(50));
        let long = Simulator::new(new_sched(label, 2)).run(make(150));

        assert_eq!(
            short.exit_kind(),
            &ExitKind::Normal,
            "[{label}] short run errored"
        );
        assert_eq!(
            long.exit_kind(),
            &ExitKind::Normal,
            "[{label}] long run errored"
        );

        let short_total: u64 = (0..2).map(|i| short.total_runtime(Pid(1 + i))).sum();
        let long_total: u64 = (0..2).map(|i| long.total_runtime(Pid(1 + i))).sum();

        // 3x the wall-clock cap → clearly more work (assert a conservative 2x).
        assert!(
            long_total >= short_total * 2,
            "[{label}] duration cap did not bound the run: short={short_total}ns long={long_total}ns"
        );
        // Each run's flush is stamped at its OWN cap, proving the cap stopped it.
        for (trace, cap_ns) in [(&short, 50_000_000u64), (&long, 150_000_000u64)] {
            for e in trace.events() {
                if matches!(e.kind, TraceKind::SimulationEnd { .. }) {
                    assert_eq!(
                        e.time_ns, cap_ns,
                        "[{label}] SimulationEnd at {}ns, expected cap {cap_ns}ns",
                        e.time_ns
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 5. Clean resource cleanup / isolation between successive runs.
// ---------------------------------------------------------------------------

#[test]
fn test_clean_resource_cleanup_between_runs() {
    let _lock = common::setup_test();
    // Scenario A (the one we check for reproducibility).
    let make_a = || {
        let mut b = Scenario::builder().cpus(2).seed(7).detect_bpf_errors();
        for i in 0..3i32 {
            b = b.task(task(
                &format!("a{i}"),
                1 + i,
                0,
                TaskBehavior {
                    phases: vec![Phase::Run(4_000_000), Phase::Sleep(2_000_000)],
                    repeat: RepeatMode::Count(3),
                },
            ));
        }
        b.duration_ms(120).build()
    };
    // An unrelated, differently-shaped scenario B, run in between.
    let make_b = || {
        let mut b = Scenario::builder().cpus(4).seed(99).detect_bpf_errors();
        for i in 0..6i32 {
            b = b.task(task(
                &format!("b{i}"),
                1 + i,
                0,
                workloads::cpu_bound(10_000_000),
            ));
        }
        b.duration_ms(80).build()
    };

    for label in ALL {
        let a1 = Simulator::new(new_sched(label, 2)).run(make_a());
        let b_mid = Simulator::new(new_sched(label, 4)).run(make_b());
        let a2 = Simulator::new(new_sched(label, 2)).run(make_a());

        assert_eq!(
            a1.exit_kind(),
            &ExitKind::Normal,
            "[{label}] A run 1 errored"
        );
        assert_eq!(
            b_mid.exit_kind(),
            &ExitKind::Normal,
            "[{label}] B run errored"
        );
        assert_eq!(
            a2.exit_kind(),
            &ExitKind::Normal,
            "[{label}] A run 2 errored"
        );

        // If the first A run (and the intervening B run) left no residual engine
        // or C-side scheduler state, the second A run is byte-identical.
        assert_eq!(
            trace_signature(&a1),
            trace_signature(&a2),
            "[{label}] state leaked across runs: A produced a different trace after B"
        );
    }
}
