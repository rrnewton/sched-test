//! Scheduler context-switch correctness tests (tg test-stack-pivot-detection).
//!
//! Despite the task's "stack pivot" name, the property under test is
//! **context-switch correctness** in the simulator's kernel substrate: when a
//! task leaves a CPU and another takes it, all of the state that must survive
//! the switch does, the callback sequence that performs the switch is
//! well-formed, and the scheduler's per-task context (`taskc`, i.e. BPF
//! task-local storage keyed by the `task_struct` pointer) is preserved and
//! stays private to each task.
//!
//! In scxsim a context switch is a sequence of engine-driven ops callbacks —
//! `put_prev_task`/`stopping` on the outgoing task, then
//! `pick_task`/`set_next_task`/`running` on the incoming one — emitting the
//! trace events `PutPrevTask` → (`TaskPreempted`|`TaskSlept`) →
//! `PickTask` → `SetNextTask` → `TaskScheduled`. The per-CPU `current_task`
//! is swapped in that window. These tests exercise:
//!
//!   1. State preserved across switches — a task's remaining work and its
//!      accumulated runtime survive arbitrarily many preemptions; total CPU
//!      time is conserved (never lost, never double-charged — the latter is
//!      the class of the historical V4-A cross-task over-charge bug).
//!   2. Rapid switching among many tasks — heavy oversubscription produces
//!      thousands of switches with every task still making progress.
//!   3. Switch triggered from inside a scheduler callback — a wakeup delivered
//!      in IRQ/callback context, and preemption raised inside `ops.tick`/
//!      `ops.enqueue`, each produce a correct, well-ordered switch.
//!   4. Per-task `taskc` correct after a switch — the storage KEY (raw task
//!      pointer) is stable across every switch, and (under LAVD) the storage
//!      CONTENT (`avg_runtime`, learned across activations) persists and stays
//!      per-task rather than resetting or aliasing.
//!
//! `.instant_timing()` (noise + overhead off) is used where exact runtime
//! accounting is asserted, so figures like "completed Run(N) ⇒ total_runtime
//! == N" hold exactly.

use std::collections::HashMap;
use std::collections::HashSet;

use scx_simulator::{LavdMonitor, LavdProbes};
use scx_simulator::{Monitor, ProbeContext};

use scx_simulator::*;

#[macro_use]
mod common;

/// A named scheduler constructor so one test body can sweep several schedulers.
type NamedSchedFactory = (&'static str, fn(u32) -> DynamicScheduler);

fn schedulers() -> [NamedSchedFactory; 3] {
    [
        ("simple", |_n| DynamicScheduler::simple()),
        ("lavd", DynamicScheduler::lavd),
        ("cosmos", DynamicScheduler::cosmos),
    ]
}

/// A forever-running CPU hog (auto-PID), never voluntarily off-CPU.
fn hog() -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(1_000_000_000)],
        repeat: RepeatMode::Forever,
    }
}

// ---------------------------------------------------------------------------
// (1) Task state is preserved across context switches.
// ---------------------------------------------------------------------------

/// A task with a fixed amount of work (Run(30ms) once) that is forced to share
/// a single CPU with a competitor is preempted many times before it finishes —
/// yet it completes having accumulated its full 30ms of runtime (to within one
/// scheduler slice-granularity; LAVD rounds the final slice by ~500µs). If any
/// run interval were lost or double-counted across a switch, the total would be
/// off by many ms. This is the core statement of "remaining-work state survives
/// every context switch."
#[test]
fn completed_task_accumulates_all_work_despite_preemptions() {
    let _lock = common::setup_test();
    let work_ns = 30_000_000u64;

    for (name, make) in schedulers() {
        let scenario = Scenario::builder()
            .cpus(1)
            .instant_timing()
            // The task under test: a fixed 30ms of work, then it exits.
            .add_task(
                "worker",
                0,
                TaskBehavior {
                    phases: vec![Phase::Run(work_ns)],
                    repeat: RepeatMode::Once,
                },
            )
            // A competitor keeps the CPU contended so the worker is repeatedly
            // preempted mid-work (forcing many context switches).
            .add_task("competitor", 0, hog())
            .duration_ms(400)
            .build();

        let trace = Simulator::new(make(1)).run(scenario);
        assert_eq!(trace.exit_kind(), &ExitKind::Normal, "{name}: exit");

        // The worker actually completed.
        assert!(
            trace
                .events()
                .iter()
                .any(|e| matches!(e.kind, TraceKind::TaskCompleted { pid } if pid == Pid(1))),
            "{name}: worker never completed its 30ms of work"
        );
        // And it was context-switched several times getting there.
        assert!(
            trace.schedule_count(Pid(1)) >= 2,
            "{name}: worker was not preempted/rescheduled (count={})",
            trace.schedule_count(Pid(1))
        );
        // Accounting across every switch: all 30ms of work is present, with no
        // lost or duplicated runtime (tolerance = one slice granularity).
        let rt = trace.total_runtime(Pid(1));
        assert!(
            rt >= work_ns && rt <= work_ns + 1_000_000,
            "{name}: worker runtime {rt} drifted from its {work_ns}ns of work across switches"
        );
    }
}

/// Total on-CPU time is *conserved* across all the context switches on a busy
/// single CPU: the sum of every task's runtime is at most the CPU time the run
/// offered (never over-charged — the failure mode of charging a switched-out
/// task) and accounts for almost all of it (little lost). Guards against the
/// V4-A-class bug where runtime was charged to a task that was not on-CPU.
#[test]
fn cpu_time_conserved_across_switches() {
    let _lock = common::setup_test();
    let dur_ns = 100_000_000u64; // 1 CPU * 100ms of available CPU time

    for (name, make) in schedulers() {
        let mut b = Scenario::builder().cpus(1).instant_timing();
        for _ in 0..5 {
            b = b.add_task("hog", 0, hog());
        }
        let trace = Simulator::new(make(1)).run(b.duration_ns(dur_ns).build());
        assert_eq!(trace.exit_kind(), &ExitKind::Normal, "{name}: exit");

        let sum: u64 = (1..=5).map(|p| trace.total_runtime(Pid(p))).sum();
        // Never over-charged: at most the CPU time offered (+ a tiny epsilon
        // for the in-flight slice at sim end).
        assert!(
            sum <= dur_ns + 1_000_000,
            "{name}: total runtime {sum} exceeds available CPU time {dur_ns} (over-charge across switch)"
        );
        // Little lost: the CPU stayed busy the whole time.
        assert!(
            sum >= dur_ns * 9 / 10,
            "{name}: only {sum} of {dur_ns} CPU-time accounted; runtime lost across switches"
        );
    }
}

// ---------------------------------------------------------------------------
// (2) Rapid context switching between many tasks.
// ---------------------------------------------------------------------------

/// Many tasks that each run in short bursts and yield the CPU (Run(1ms) +
/// Sleep(2ms)) on only 2 CPUs drive a large number of context switches. Every
/// task must still make progress (no task starved out by the churn), the run
/// must stay healthy, and the switch count must be far larger than the task
/// count (proving genuine rapid switching, not just one-shot placement).
#[test]
fn rapid_switching_many_tasks_all_progress() {
    let _lock = common::setup_test();
    let nr = 2;
    let ntasks = 24;

    for (name, make) in schedulers() {
        let mut b = Scenario::builder().cpus(nr).instant_timing();
        for _ in 0..ntasks {
            b = b.add_task(
                "burst",
                0,
                TaskBehavior {
                    phases: vec![Phase::Run(1_000_000), Phase::Sleep(2_000_000)],
                    repeat: RepeatMode::Forever,
                },
            );
        }
        let trace = Simulator::new(make(nr)).run(b.duration_ms(100).build());
        assert_eq!(trace.exit_kind(), &ExitKind::Normal, "{name}: exit");
        assert!(!trace.has_error(), "{name}: error {:?}", trace.exit_kind());

        // Every task ran and was scheduled at least once.
        let mut scheduled_pids: HashSet<i32> = HashSet::new();
        for e in trace.events() {
            if let TraceKind::TaskScheduled { pid } = e.kind {
                scheduled_pids.insert(pid.0);
            }
        }
        for p in 1..=ntasks as i32 {
            assert!(
                trace.total_runtime(Pid(p)) > 0,
                "{name}: task {p} starved under rapid switching"
            );
            assert!(
                scheduled_pids.contains(&p),
                "{name}: task {p} was never scheduled"
            );
        }
        // Rapid switching: many more scheduling events than tasks.
        let sched_events = trace
            .events()
            .iter()
            .filter(|e| matches!(e.kind, TraceKind::TaskScheduled { .. }))
            .count();
        assert!(
            sched_events > ntasks * 3,
            "{name}: only {sched_events} scheduling events for {ntasks} tasks — not rapidly switching"
        );
    }
}

// ---------------------------------------------------------------------------
// (3) Context switch triggered from inside scheduler callbacks.
// ---------------------------------------------------------------------------

/// A wakeup delivered from *inside a callback context* must be able to drive a
/// context switch. An injected hardirq wakes a sleeper synchronously while the
/// IRQ context is active (the engine runs `select_cpu`/`enqueue` inline during
/// `handle_irq_start`). The sleeper's own sleep is far longer than the run, so
/// the *only* thing that can reschedule it is the in-callback wake — any
/// re-scheduling after the IRQ time proves the switch fired from the callback.
#[test]
fn wakeup_during_irq_callback_causes_switch() {
    let _lock = common::setup_test();
    let irq_at = 40_000_000u64;

    for (name, make) in schedulers() {
        let scenario = Scenario::builder()
            .cpus(1)
            .instant_timing()
            // A hog occupies the CPU so a switch is required to run the sleeper.
            .add_task("hog", 0, hog())
            // Sleeper: tiny initial slice, then sleeps ~10x the whole run. It
            // can only run again if the IRQ handler wakes it.
            .add_task(
                "sleeper",
                0,
                TaskBehavior {
                    phases: vec![Phase::Run(500_000), Phase::Sleep(1_000_000_000)],
                    repeat: RepeatMode::Forever,
                },
            )
            .duration_ms(100)
            // Hardirq on CPU0 at 40ms whose handler wakes the sleeper (pid 2).
            .hardirq(CpuId(0), irq_at, 50_000, &[Pid(2)])
            .build();

        let trace = Simulator::new(make(1)).run(scenario);
        assert_eq!(trace.exit_kind(), &ExitKind::Normal, "{name}: exit");

        // The sleeper was scheduled again *after* the IRQ — the wake delivered
        // during the IRQ callback caused a context switch onto it.
        let ran_after_irq = trace.events().iter().any(|e| {
            e.time_ns >= irq_at
                && matches!(e.kind, TraceKind::TaskScheduled { pid } if pid == Pid(2))
        });
        assert!(
            ran_after_irq,
            "{name}: sleeper never scheduled after the IRQ-context wakeup \
             (its sleep is 10x the run, so nothing else could reschedule it)"
        );
    }
}

/// Every preemption — including those raised from inside a scheduler callback
/// (`ops.tick` self-kick / slice-zero, or a higher-criticality wake inside
/// `ops.enqueue`) — is a *well-formed* context switch: the outgoing task is
/// `put_prev`'d before it is marked preempted, and the CPU then installs a next
/// task. Runs under LAVD, which preempts aggressively from its callbacks.
#[test]
fn preemptions_are_well_formed_context_switches() {
    let _lock = common::setup_test();
    let nr = 2;

    // Mixed criticality: a background hog plus latency-sensitive on/off tasks
    // whose wakeups trigger LAVD preemption from inside its callbacks.
    let mut b = Scenario::builder().cpus(nr);
    b = b.add_task("hog0", 5, hog()).add_task("hog1", 5, hog());
    for _ in 0..4 {
        b = b.add_task(
            "latsen",
            -15,
            TaskBehavior {
                phases: vec![Phase::Run(1_000_000), Phase::Sleep(2_000_000)],
                repeat: RepeatMode::Forever,
            },
        );
    }
    let trace = Simulator::new(DynamicScheduler::lavd(nr)).run(b.duration_ms(100).build());
    assert_eq!(trace.exit_kind(), &ExitKind::Normal, "exit");

    let events = trace.events();
    let mut preemptions = 0usize;
    for (i, e) in events.iter().enumerate() {
        let TraceKind::TaskPreempted { pid } = &e.kind else {
            continue;
        };
        preemptions += 1;
        // put_prev_task ran for this pid shortly before the preemption marker.
        let put_prev = events[..i]
            .iter()
            .rev()
            .take(12)
            .any(|ev| matches!(&ev.kind, TraceKind::PutPrevTask { pid: p, .. } if p == pid));
        assert!(
            put_prev,
            "TaskPreempted(pid={}) at {i} not preceded by PutPrevTask (malformed switch)",
            pid.0
        );
        // The CPU installs a next task after the switch (a SetNextTask on the
        // same CPU follows within the switch window).
        let cpu = e.cpu;
        let set_next = events[i + 1..]
            .iter()
            .take(40)
            .any(|ev| ev.cpu == cpu && matches!(ev.kind, TraceKind::SetNextTask { .. }));
        assert!(
            set_next,
            "TaskPreempted(pid={}) on CPU{} at {i} not followed by SetNextTask (switch never completed)",
            pid.0,
            cpu.0
        );
    }
    assert!(
        preemptions > 0,
        "expected callback-driven preemptions to exercise the switch path"
    );
}

// ---------------------------------------------------------------------------
// (4) Per-task data (taskc) is correct after a switch.
// ---------------------------------------------------------------------------

/// Records, per PID, every raw `task_struct` pointer the engine hands to a
/// probe point. That pointer is the KEY under which the scheduler's `taskc`
/// (BPF task-local storage) lives, so it must be identical at every point in a
/// task's life — otherwise a context switch would silently swap the task's
/// per-task context.
#[derive(Default)]
struct TaskPtrMonitor {
    /// pid -> set of distinct raw pointers observed.
    ptrs: HashMap<i32, HashSet<usize>>,
    /// pid -> number of probe samples (proves switches were observed).
    samples: HashMap<i32, usize>,
}

impl Monitor for TaskPtrMonitor {
    fn sample(&mut self, ctx: &ProbeContext) {
        self.ptrs
            .entry(ctx.pid.0)
            .or_default()
            .insert(ctx.task_raw as usize);
        *self.samples.entry(ctx.pid.0).or_default() += 1;
    }
}

/// The `taskc` key (raw task pointer) for each PID is stable across the whole
/// run, through arbitrarily many context switches. Swept across schedulers
/// because task allocation is engine-owned and must be switch-invariant for
/// every scheduler.
#[test]
fn taskc_key_stable_across_switches() {
    let _lock = common::setup_test();
    let nr = 2;
    let ntasks = 6;

    for (name, make) in schedulers() {
        let mut b = Scenario::builder().cpus(nr).instant_timing();
        for _ in 0..ntasks {
            // Run/sleep churn so each task is switched on and off many times.
            b = b.add_task(
                "t",
                0,
                TaskBehavior {
                    phases: vec![Phase::Run(2_000_000), Phase::Sleep(1_000_000)],
                    repeat: RepeatMode::Forever,
                },
            );
        }
        let mut mon = TaskPtrMonitor::default();
        let result = Simulator::new(make(nr)).run_monitored(b.duration_ms(80).build(), &mut mon);
        assert_eq!(result.trace.exit_kind(), &ExitKind::Normal, "{name}: exit");

        for p in 1..=ntasks as i32 {
            let ptrs = mon.ptrs.get(&p);
            let n_samples = mon.samples.get(&p).copied().unwrap_or(0);
            assert!(
                n_samples >= 4,
                "{name}: task {p} sampled only {n_samples} times — not enough switches to test"
            );
            let distinct = ptrs.map(|s| s.len()).unwrap_or(0);
            assert_eq!(
                distinct, 1,
                "{name}: task {p} presented {distinct} distinct taskc keys across switches \
                 (must be exactly 1 — its per-task storage moved)"
            );
        }
        // Sanity: distinct tasks have distinct keys (taskc not aliased).
        let all_keys: HashSet<usize> = mon.ptrs.values().flat_map(|s| s.iter().copied()).collect();
        assert_eq!(
            all_keys.len(),
            ntasks,
            "{name}: expected {ntasks} distinct taskc keys, saw {} (tasks alias storage)",
            all_keys.len()
        );
    }
}

/// Under LAVD, the per-task context CONTENT survives switches and stays
/// private per task. A heavy always-on task and a light bursty task run
/// together; LAVD accumulates each task's `avg_runtime` in its `taskc` across
/// activations. After the run:
///   * the heavy task's learned `avg_runtime` is substantial and strictly
///     greater than the light task's — proving `taskc` is preserved across the
///     task's many context switches (a reset each switch would keep it ~0) and
///     is not shared between the two tasks;
///   * once learned, the heavy task's `avg_runtime` never falls back to 0 in
///     later samples — i.e. a context switch does not clobber it.
#[test]
fn lavd_taskc_content_persists_and_is_per_task() {
    let _lock = common::setup_test();
    let nr = 2;

    let scenario = Scenario::builder()
        .cpus(nr)
        // pid 1: heavy, always runnable -> large learned avg_runtime.
        .add_task("heavy", 0, hog())
        // pid 2: light, short bursts then sleep -> small avg_runtime.
        .add_task(
            "light",
            0,
            TaskBehavior {
                phases: vec![Phase::Run(500_000), Phase::Sleep(5_000_000)],
                repeat: RepeatMode::Forever,
            },
        )
        .duration_ms(200)
        .build();

    let sched = DynamicScheduler::lavd(nr);
    let probes = LavdProbes::new(&sched);
    let mut monitor = LavdMonitor::new(probes);
    let result = Simulator::new(sched).run_monitored(scenario, &mut monitor);
    assert_eq!(result.trace.exit_kind(), &ExitKind::Normal, "exit");

    let heavy = monitor
        .final_snapshot(Pid(1))
        .expect("no taskc snapshot for heavy task");
    let light = monitor
        .final_snapshot(Pid(2))
        .expect("no taskc snapshot for light task");

    // taskc preserved across switches: the heavy task actually learned a
    // non-trivial average on-CPU time.
    assert!(
        heavy.avg_runtime > 500_000,
        "heavy task avg_runtime={} — taskc not accumulating across switches",
        heavy.avg_runtime
    );
    // taskc is per-task: the light task's learned value is clearly smaller.
    assert!(
        heavy.avg_runtime > light.avg_runtime,
        "heavy avg_runtime ({}) !> light avg_runtime ({}) — taskc aliased across tasks?",
        heavy.avg_runtime,
        light.avg_runtime
    );

    // Once learned, a switch never clobbers it back to 0. Look at the tail of
    // the heavy task's history (after it has had time to learn).
    let hist = monitor.task_history(Pid(1));
    assert!(
        hist.len() >= 10,
        "too few heavy-task taskc samples ({}) to judge persistence",
        hist.len()
    );
    let tail = &hist[hist.len() / 2..];
    assert!(
        tail.iter().all(|s| s.avg_runtime > 0),
        "heavy task avg_runtime dropped back to 0 mid-run — taskc clobbered by a switch"
    );
}
