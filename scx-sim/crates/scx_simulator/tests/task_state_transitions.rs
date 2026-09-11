//! Task state-machine transition tests (tg test-task-state-transitions).
//!
//! Verifies that each task moves through the scheduler state machine
//! (CREATED → RUNNABLE → RUNNING → STOPPED → … → DEAD) only via *legal*
//! transitions, under normal, preemption, sleep/wake, and throttled workloads,
//! across all schedulers. Rather than re-checking callback *ordering* (already
//! covered by `scx_ops_callbacks.rs`: init → enable → first-run → exit), this
//! reconstructs each task's transition sequence from the trace and asserts the
//! run-phase state machine is never violated:
//!
//!   * `InitTask` is the first event for a task and appears once (CREATED first).
//!   * `ExitTask`, if present, is the last event for a task (DEAD last).
//!   * `TaskScheduled` (→RUNNING) strictly alternates with a stop event
//!     (`TaskPreempted`/`TaskYielded`/`TaskSlept`/`TaskCompleted`, →STOPPED):
//!     no task runs twice without stopping, none stops without running. This is
//!     exactly RUNNING↔RUNNABLE (preemption) and RUNNING→SLEEPING (sleep).
//!   * A task that slept (`TaskSlept`) is woken (`TaskWoke`) before it runs
//!     again (SLEEPING → RUNNABLE → RUNNING).
//!
//! Item 5 ("invalid transitions are rejected") is verified structurally: the
//! engine's `OpsTaskState` machine only fires callbacks in valid states, so the
//! validator below finding **zero** violations across many workloads/schedulers
//! *is* the assertion that illegal transitions never occur. Item 6 ("state
//! correct at each callback") falls out of the same rules (e.g. a stop callback
//! only ever fires while RUNNING).

use scx_simulator::*;

#[macro_use]
mod common;

type SchedFactory = fn(u32) -> DynamicScheduler;

fn schedulers() -> [(&'static str, SchedFactory); 3] {
    [
        ("simple", |_n| DynamicScheduler::simple()),
        ("lavd", DynamicScheduler::lavd),
        ("cosmos", DynamicScheduler::cosmos),
    ]
}

/// A short label for the lifecycle-relevant kinds of one task's events.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ev {
    Init,
    Runnable,
    Woke,
    Scheduled, // → RUNNING
    Preempted, // stop
    Yielded,   // stop
    Slept,     // stop (→ SLEEPING)
    Completed, // stop
    SimEnd,
    Exit, // → DEAD
    Other,
}

/// Extract the ordered lifecycle-event sequence for one pid.
fn task_events(trace: &Trace, pid: Pid) -> Vec<Ev> {
    trace
        .events()
        .iter()
        .filter_map(|e| {
            let ev = match &e.kind {
                TraceKind::InitTask { pid: p, .. } if *p == pid => Ev::Init,
                TraceKind::Runnable { pid: p, .. } if *p == pid => Ev::Runnable,
                TraceKind::TaskWoke { pid: p } if *p == pid => Ev::Woke,
                TraceKind::TaskScheduled { pid: p } if *p == pid => Ev::Scheduled,
                TraceKind::TaskPreempted { pid: p } if *p == pid => Ev::Preempted,
                TraceKind::TaskYielded { pid: p } if *p == pid => Ev::Yielded,
                TraceKind::TaskSlept { pid: p } if *p == pid => Ev::Slept,
                TraceKind::TaskCompleted { pid: p } if *p == pid => Ev::Completed,
                TraceKind::SimulationEnd { pid: p } if *p == pid => Ev::SimEnd,
                TraceKind::ExitTask { pid: p } if *p == pid => Ev::Exit,
                _ => Ev::Other,
            };
            if ev == Ev::Other {
                None
            } else {
                Some(ev)
            }
        })
        .collect()
}

/// Validate the state machine for one task's event sequence.
fn validate_task(seq: &[Ev], pid: Pid, ctx: &str) {
    if seq.is_empty() {
        return;
    }
    // CREATED first: the first lifecycle event is Init, and Init appears once.
    assert_eq!(
        seq[0],
        Ev::Init,
        "{ctx}: task {pid:?} first lifecycle event is {:?}, expected Init. seq={seq:?}",
        seq[0]
    );
    assert_eq!(
        seq.iter().filter(|&&e| e == Ev::Init).count(),
        1,
        "{ctx}: task {pid:?} has multiple Init events. seq={seq:?}"
    );

    // DEAD last: nothing follows Exit.
    if let Some(pos) = seq.iter().position(|&e| e == Ev::Exit) {
        assert_eq!(
            pos,
            seq.len() - 1,
            "{ctx}: task {pid:?} has events after Exit. seq={seq:?}"
        );
    }

    // Run/stop alternation + sleep→wake ordering.
    let mut on_cpu = false;
    let mut awaiting_wake = false; // set by Slept, cleared by Woke
    for (i, &ev) in seq.iter().enumerate() {
        match ev {
            Ev::Scheduled => {
                assert!(
                    !on_cpu,
                    "{ctx}: task {pid:?} scheduled to RUNNING while already RUNNING (double-run) \
                     at index {i}. seq={seq:?}"
                );
                assert!(
                    !awaiting_wake,
                    "{ctx}: task {pid:?} ran while still SLEEPING (no wake after sleep) \
                     at index {i}. seq={seq:?}"
                );
                on_cpu = true;
            }
            Ev::Preempted | Ev::Yielded | Ev::Slept | Ev::Completed => {
                assert!(
                    on_cpu,
                    "{ctx}: task {pid:?} stop event {ev:?} while not RUNNING at index {i}. seq={seq:?}"
                );
                on_cpu = false;
                if ev == Ev::Slept {
                    awaiting_wake = true;
                }
            }
            Ev::Woke => {
                awaiting_wake = false;
            }
            Ev::SimEnd | Ev::Exit => {
                // Terminators: a task may be RUNNING at sim end; clear state.
                on_cpu = false;
            }
            Ev::Init | Ev::Runnable | Ev::Other => {}
        }
    }
}

/// Run a scenario and validate the state machine of every task.
fn run_and_validate(
    make: fn(u32) -> DynamicScheduler,
    nr_cpus: u32,
    scenario: Scenario,
    pids: &[Pid],
    ctx: &str,
) -> Trace {
    let trace = Simulator::new(make(nr_cpus)).run(scenario);
    assert!(
        !trace.has_error(),
        "{ctx}: run ended with error {:?}",
        trace.exit_kind()
    );
    for &pid in pids {
        let seq = task_events(&trace, pid);
        validate_task(&seq, pid, ctx);
    }
    trace
}

fn task(pid: i32, phases: Vec<Phase>, repeat: RepeatMode) -> TaskDef {
    TaskDef {
        name: format!("t{pid}"),
        pid: Pid(pid),
        nice: 0,
        behavior: TaskBehavior { phases, repeat },
        start_time_ns: 0,
        mm_id: None,
        allowed_cpus: None,
        parent_pid: None,
        cgroup_name: None,
        task_flags: 0,
        migration_disabled: 0,
        thread_group_leader: None,
        uid: Uid(0),
        gid: Gid(0),
    }
}

/// Item 1: normal lifecycle CREATED → RUNNABLE → RUNNING → STOPPED → DEAD.
/// A single finite task runs once to completion; validate its full sequence and
/// that it reached RUNNING (Scheduled) and DEAD (Exit at teardown).
#[test]
fn normal_lifecycle() {
    let _lock = common::setup_test();
    for (name, make) in schedulers() {
        let scenario = Scenario::builder()
            .cpus(2)
            .task(task(1, vec![Phase::Run(10_000_000)], RepeatMode::Once))
            .duration_ms(60)
            .build();
        let trace = run_and_validate(make, 2, scenario, &[Pid(1)], &format!("{name} lifecycle"));

        let seq = task_events(&trace, Pid(1));
        assert!(
            seq.contains(&Ev::Init) && seq.contains(&Ev::Scheduled),
            "{name}: task never reached RUNNING. seq={seq:?}"
        );
        assert!(
            seq.contains(&Ev::Exit),
            "{name}: task never reached DEAD (Exit). seq={seq:?}"
        );
    }
}

/// Item 2: preemption RUNNING → RUNNABLE → RUNNING. Oversubscribe so tasks are
/// repeatedly preempted; validate the alternation holds and preemptions occur.
#[test]
fn preemption_cycles() {
    let _lock = common::setup_test();
    let nr = 2;
    for (name, make) in schedulers() {
        let mut b = Scenario::builder().cpus(nr).seed(11);
        let pids: Vec<Pid> = (1..=6).map(Pid).collect();
        for p in &pids {
            b = b.task(task(p.0, vec![Phase::Run(50_000_000)], RepeatMode::Forever));
        }
        let trace = run_and_validate(
            make,
            nr,
            b.duration_ms(120).build(),
            &pids,
            &format!("{name} preempt"),
        );

        // At least one task was preempted (RUNNING→RUNNABLE) and later ran again.
        let preempts: usize = pids.iter().map(|&p| trace.preempt_count(p)).sum();
        assert!(
            preempts > 0,
            "{name}: expected preemptions under oversubscription, got {preempts}"
        );
    }
}

/// Item 3: sleep RUNNING → SLEEPING → RUNNABLE → RUNNING. A run/sleep cycler
/// must sleep and wake repeatedly; validate the sleep→wake→run ordering.
#[test]
fn sleep_wake_cycles() {
    let _lock = common::setup_test();
    let nr = 2;
    for (name, make) in schedulers() {
        let pids = vec![Pid(1), Pid(2)];
        let mut b = Scenario::builder().cpus(nr).seed(5);
        for p in &pids {
            b = b.task(task(
                p.0,
                vec![Phase::Run(3_000_000), Phase::Sleep(3_000_000)],
                RepeatMode::Forever,
            ));
        }
        let trace = run_and_validate(
            make,
            nr,
            b.duration_ms(120).build(),
            &pids,
            &format!("{name} sleep"),
        );

        // Each task actually slept and woke at least once.
        for &p in &pids {
            let seq = task_events(&trace, p);
            assert!(
                seq.contains(&Ev::Slept),
                "{name}: task {p:?} never slept. seq={seq:?}"
            );
            assert!(
                seq.contains(&Ev::Woke),
                "{name}: task {p:?} never woke. seq={seq:?}"
            );
        }
    }
}

/// Item 4: throttled state. A cgroup with a tight cpu.max bandwidth throttles
/// its tasks; validate the state machine stays legal through throttle/unthrottle
/// (the task blocks and later resumes without any illegal transition). Uses
/// LAVD, which links the cgroup_bw enforcement library.
#[test]
fn throttled_cgroup_state_machine() {
    let _lock = common::setup_test();
    let nr = 2;
    // Tight bandwidth: 10ms quota per 100ms period → heavy throttling.
    let scenario = Scenario::builder()
        .cpus(nr)
        .cgroup_with_bandwidth("cg", &[CpuId(0), CpuId(1)], 100_000, 10_000, 0)
        .add_task_in_cgroup(
            "throttled",
            0,
            TaskBehavior {
                phases: vec![Phase::Run(200_000_000)],
                repeat: RepeatMode::Forever,
            },
            "cg",
        )
        .duration_ms(300)
        .build();

    let trace = run_and_validate(
        DynamicScheduler::lavd,
        nr,
        scenario,
        &[Pid(1)],
        "lavd throttle",
    );
    // The task should have run at least some (throttling limits, not eliminates,
    // runtime) — i.e. it is not permanently stuck in a blocked state.
    assert!(
        trace.total_runtime(Pid(1)) > 0,
        "throttled task got zero runtime (stuck blocked?)"
    );
}

/// Items 5 & 6, property form: across randomized mixed workloads and seeds, no
/// task ever takes an illegal state transition (validator finds zero
/// violations), for every scheduler.
#[test]
fn state_machine_valid_under_random_workloads() {
    let _lock = common::setup_test();

    for (name, make) in schedulers() {
        for k in 0u64..12 {
            let nr = 1 + (k % 4) as u32; // 1..4 CPUs
            let n_tasks = 2 + (k % 6) as u32; // 2..7 tasks
            let mut b = Scenario::builder().cpus(nr).seed(1000 + k as u32);
            let pids: Vec<Pid> = (1..=n_tasks as i32).map(Pid).collect();
            for (i, p) in pids.iter().enumerate() {
                // Alternate CPU-bound and run/sleep cyclers with varied nice.
                let phases = if i % 2 == 0 {
                    vec![Phase::Run(4_000_000 + (i as u64) * 1_000_000)]
                } else {
                    vec![
                        Phase::Run(2_000_000),
                        Phase::Sleep(1_000_000 + (i as u64) * 500_000),
                    ]
                };
                let mut def = task(p.0, phases, RepeatMode::Forever);
                def.nice = (i as i16 % 7 - 3) as i8;
                b = b.task(def);
            }
            let ctx = format!("{name} rand k={k} cpus={nr} tasks={n_tasks}");
            run_and_validate(make, nr, b.duration_ms(80).build(), &pids, &ctx);
        }
    }
}
