//! COSMOS idle-CPU tracking and task-stealing behavior.
//!
//! ## What is genuinely observable (READ THIS FIRST)
//!
//! Idle tracking in scxsim is driven by the engine's BPF substrate (the kernel
//! owns the idle cpumask, exactly as in production), and surfaced through two
//! trace events:
//!
//!  * `UpdateIdle { cpu, idle }` — the `ops.update_idle` callback. `idle:true`
//!    fires when a CPU enters idle (including the initial all-idle boot, at
//!    `ts==1`); `idle:false` fires when a CPU that was idle takes a task
//!    (`test_and_clear_cpu_idle` in the engine's dispatch path). Together these
//!    are the observable maintenance of the idle-CPU bitmap.
//!  * `DsqMoveToLocal { dsq_id, success }` — `scx_bpf_dsq_move_to_local`, a CPU
//!    pulling the head of a DSQ onto its local DSQ. This is how scx schedulers
//!    do work-conservation: an idle-going CPU *pulls* queued work rather than
//!    another CPU pushing to it. For COSMOS in busy mode, `dsq_id` is the
//!    per-node shared DSQ (`shared_dsq(cpu) = cpu_node(cpu)`), so a successful
//!    move is a CPU stealing queued work from the shared (busy) domain queue.
//!
//! COSMOS routes enqueues onto the per-node shared DSQ (the vtime/deadline path)
//! only when userspace reports the system busy — the sim doesn't derive per-CPU
//! utilization yet (mb sim-642cb2), so the stealing test sets it explicitly with
//! `cosmos_set_cpu_util`, exactly as `cosmos.rs::test_shared_dsq_contention`
//! does. Under sim COSMOS's real schedulable domain is the NUMA node, not the
//! LLC (its `cpus_share_cache` logic is inert — see `cosmos_llc.rs`'s header and
//! mb sim-439319 / sim-e10316); none of the assertions here depend on LLC
//! placement.
//!
//! ## Coverage of the task's five goals
//!
//!  1. Idle CPU bitmap maintenance → `test_cosmos_idle_bitmap_maintained`
//!     (both idle-enter and idle-exit fire, per-CPU coherently).
//!  2. Task stealing from busy to idle CPUs → `test_cosmos_steals_queued_work`
//!     (idle-going CPUs pull from the shared DSQ; load spreads to all CPUs).
//!  3. Idle notification + wakeup latency → `test_cosmos_wakeup_latency_bounded`
//!     (a wake on an idle system is scheduled promptly).
//!  4. COSMOS idle-scan patterns → `test_cosmos_idle_scan_modes_find_idle_cpus`
//!     (flat vs preferred scan both locate idle CPUs and keep tracking active).
//!  5. All CPUs idle simultaneously → `test_cosmos_all_cpus_idle_simultaneously`
//!     (the whole box goes idle at once, then recovers).
//!
//! Plus a determinism guard. Deterministic serial engine, fixed seed; every
//! assertion is on observable trace events only (No-Stub / model-the-kernel).

use std::collections::HashSet;

use scx_simulator::*;

mod common;

// ---------------------------------------------------------------------------
// Workload + trace helpers
// ---------------------------------------------------------------------------

fn run_sleep(run_ns: u64, sleep_ns: u64) -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(run_ns), Phase::Sleep(sleep_ns)],
        repeat: RepeatMode::Forever,
    }
}

/// A `TaskDef` with an optional affinity mask and a run/sleep behavior.
fn task(name: &str, pid: i32, allowed: Option<Vec<CpuId>>, behavior: TaskBehavior) -> TaskDef {
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
        thread_group_leader: None,
        uid: Uid(0),
        gid: Gid(0),
        fork_cpu: None,
    }
}

/// Count of `UpdateIdle` events matching the given `idle` polarity.
fn update_idle_count(trace: &Trace, want_idle: bool) -> usize {
    trace
        .events()
        .iter()
        .filter(|e| matches!(e.kind, TraceKind::UpdateIdle { idle, .. } if idle == want_idle))
        .count()
}

/// Distinct CPUs that emitted an `UpdateIdle` with the given polarity.
fn cpus_with_idle_event(trace: &Trace, want_idle: bool) -> HashSet<u32> {
    trace
        .events()
        .iter()
        .filter_map(|e| match e.kind {
            TraceKind::UpdateIdle { cpu, idle } if idle == want_idle => Some(cpu.0),
            _ => None,
        })
        .collect()
}

/// Distinct CPUs that ran at least one task.
fn cpus_used(trace: &Trace) -> HashSet<u32> {
    trace
        .events()
        .iter()
        .filter_map(|e| match e.kind {
            TraceKind::TaskScheduled { .. } => Some(e.cpu.0),
            _ => None,
        })
        .collect()
}

/// Successful `scx_bpf_dsq_move_to_local` pulls (a CPU stealing queued work).
fn successful_pulls(trace: &Trace) -> usize {
    trace
        .events()
        .iter()
        .filter(|e| matches!(e.kind, TraceKind::DsqMoveToLocal { success: true, .. }))
        .count()
}

fn assert_identical(t1: &Trace, t2: &Trace, ctx: &str) {
    assert_eq!(
        t1.events().len(),
        t2.events().len(),
        "{ctx}: trace lengths differ"
    );
    for (i, (e1, e2)) in t1.events().iter().zip(t2.events().iter()).enumerate() {
        assert_eq!(e1.time_ns, e2.time_ns, "{ctx}: event {i} time differs");
        assert_eq!(e1.cpu, e2.cpu, "{ctx}: event {i} cpu differs");
        assert_eq!(e1.kind, e2.kind, "{ctx}: event {i} kind differs");
    }
}

// ===========================================================================
// 1. Idle CPU bitmap maintenance.
// ===========================================================================

/// On an under-loaded box (2 wake/sleep tasks on 4 CPUs) COSMOS's idle bitmap
/// must be maintained through transitions: CPUs both *enter* idle
/// (`UpdateIdle{idle:true}`) and *exit* idle (`UpdateIdle{idle:false}`), every
/// CPU goes idle at least once, and — the coherence property — no CPU ever emits
/// an idle-exit while the bitmap already has it marked busy (an exit is always
/// preceded by an enter). The engine boots every CPU idle, so a running per-CPU
/// model of the bitmap must stay consistent with the recorded transitions.
#[test]
fn test_cosmos_idle_bitmap_maintained() {
    let _lock = common::setup_test();
    let nr_cpus = 4u32;

    let sched = DynamicScheduler::cosmos(nr_cpus);
    let mut b = Scenario::builder().cpus(nr_cpus).seed(42).instant_timing();
    for i in 0..2 {
        b = b.task(task(
            &format!("t{i}"),
            1 + i,
            None,
            run_sleep(3_000_000, 5_000_000),
        ));
    }
    let trace = Simulator::new(sched).run(b.duration_ms(150).build());

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    assert!(!trace.has_error(), "error: {:?}", trace.exit_kind());

    // Both directions of the bitmap transition are exercised.
    assert!(
        update_idle_count(&trace, true) > 0,
        "no idle-enter (UpdateIdle idle=true) events — bitmap never marked idle"
    );
    assert!(
        update_idle_count(&trace, false) > 0,
        "no idle-exit (UpdateIdle idle=false) events — bitmap never cleared"
    );

    // Every CPU enters idle at least once on an under-loaded system.
    let entered = cpus_with_idle_event(&trace, true);
    assert_eq!(
        entered.len() as u32,
        nr_cpus,
        "only CPUs {entered:?} ever went idle, expected all {nr_cpus}"
    );

    // Coherence: replay the transitions and confirm the bitmap stays consistent
    // — an idle-exit only ever fires for a CPU currently marked idle.
    let mut idle_set: HashSet<u32> = HashSet::new();
    for e in trace.events() {
        if let TraceKind::UpdateIdle { cpu, idle } = e.kind {
            if idle {
                idle_set.insert(cpu.0);
            } else {
                assert!(
                    idle_set.remove(&cpu.0),
                    "incoherent bitmap: CPU {} exited idle without being marked idle (t={})",
                    cpu.0,
                    e.time_ns
                );
            }
        }
    }
}

// ===========================================================================
// 2. Task stealing from busy to idle CPUs.
// ===========================================================================

/// In busy mode COSMOS queues work on the per-node shared DSQ; a CPU that
/// finishes its task and goes idle *pulls* the next queued task onto its local
/// DSQ (`scx_bpf_dsq_move_to_local`). This is the scx work-conservation /
/// stealing mechanism. With an oversubscribed run/sleep workload on a saturated
/// box, we must see: shared-DSQ dispatches happen, successful pulls occur (idle
/// CPUs steal queued work), the load reaches every CPU (no CPU stranded idle
/// while work is queued), and no task starves.
#[test]
fn test_cosmos_steals_queued_work() {
    let _lock = common::setup_test();
    let nr_cpus = 4u32;
    let nr_tasks = 10u32; // 2.5x oversubscribed → work backs up on the shared DSQ

    let sched = DynamicScheduler::cosmos(nr_cpus);
    // Report a saturated system so enqueues route to the shared DSQ (deadline
    // path) rather than per-CPU queues — the sim doesn't derive util yet
    // (mb sim-642cb2), matching cosmos.rs::test_shared_dsq_contention.
    sched.cosmos_set_cpu_util(nr_cpus, 1024);

    let mut b = Scenario::builder().cpus(nr_cpus).seed(42).instant_timing();
    for i in 1..=nr_tasks {
        b = b.task(task(
            &format!("busy{i}"),
            i as i32,
            None,
            run_sleep(4_000_000, 1_000_000),
        ));
    }
    let trace = Simulator::new(sched).run(b.duration_ms(150).build());

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    assert!(!trace.has_error(), "error: {:?}", trace.exit_kind());

    // The shared-DSQ (deadline) path was actually taken.
    let (global, _local) = trace.dsq_dispatch_counts();
    assert!(
        global > 0,
        "expected shared-DSQ dispatches in busy mode, got {global}"
    );

    // Idle-going CPUs pulled queued work off the shared DSQ (the stealing path).
    let pulls = successful_pulls(&trace);
    assert!(
        pulls > 0,
        "no successful DsqMoveToLocal pulls — work was never stolen to idle CPUs"
    );

    // The pulled work reached every CPU — none was left stranded idle while the
    // shared DSQ held runnable tasks.
    let used = cpus_used(&trace);
    assert_eq!(
        used.len() as u32,
        nr_cpus,
        "queued work did not spread to all CPUs: used {used:?} of {nr_cpus}"
    );

    // No starvation under the stealing scheduler.
    for pid in 1..=nr_tasks as i32 {
        assert!(
            trace.total_runtime(Pid(pid)) > 0,
            "task {pid} starved despite work-stealing"
        );
    }
}

// ===========================================================================
// 3. Idle notification and wakeup latency.
// ===========================================================================

/// A task that repeatedly sleeps and wakes on an otherwise-idle 4-CPU box must
/// be scheduled *promptly* after each wakeup — the idle-CPU tracking lets the
/// scheduler place the wakee on a known-idle CPU without delay. We measure the
/// gap between each `TaskWoke` and the task's next `TaskScheduled` (realistic
/// timing, no `instant_timing`) and require every wakeup to be serviced within a
/// generous bound. This asserts the wakeup path is prompt and never stalls,
/// without over-fitting the exact (few-µs) overhead.
#[test]
fn test_cosmos_wakeup_latency_bounded() {
    let _lock = common::setup_test();
    // A comfortably-generous ceiling: observed latencies are single-digit µs;
    // 1ms catches gross regressions / a stalled wakeup without being flaky.
    const MAX_WAKE_LATENCY_NS: u64 = 1_000_000;

    let sched = DynamicScheduler::cosmos(4);
    let scenario = Scenario::builder()
        .cpus(4)
        .seed(42)
        .task(task("waker", 1, None, run_sleep(2_000_000, 5_000_000)))
        .duration_ms(150)
        .build();
    let trace = Simulator::new(sched).run(scenario);

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    assert!(!trace.has_error(), "error: {:?}", trace.exit_kind());

    // Idle notifications must be firing (the substrate is tracking idle CPUs the
    // wakeup path relies on).
    assert!(
        update_idle_count(&trace, true) > 0,
        "no idle notifications recorded — idle tracking inactive"
    );

    // Pair each wakeup with the next dispatch of the same task.
    let mut woke_at: Option<u64> = None;
    let mut latencies: Vec<u64> = Vec::new();
    for e in trace.events() {
        match e.kind {
            TraceKind::TaskWoke { pid: Pid(1) } => woke_at = Some(e.time_ns),
            TraceKind::TaskScheduled { pid: Pid(1) } => {
                if let Some(w) = woke_at.take() {
                    latencies.push(e.time_ns.saturating_sub(w));
                }
            }
            _ => {}
        }
    }

    assert!(
        latencies.len() >= 5,
        "expected several wake→schedule cycles, got {}",
        latencies.len()
    );
    let worst = *latencies.iter().max().unwrap();
    assert!(
        worst <= MAX_WAKE_LATENCY_NS,
        "worst wakeup latency {worst}ns exceeds bound {MAX_WAKE_LATENCY_NS}ns (all: {latencies:?})"
    );
}

// ===========================================================================
// 4. COSMOS-specific idle-scan patterns (flat vs preferred).
// ===========================================================================

/// COSMOS has two lightweight idle-CPU scan paths selectable at init:
/// `pick_idle_cpu_flat()` (round-robin) and `pick_idle_cpu_pref_smt()`
/// (preferred-order, SMT-aware), chosen by `--flat-idle-scan` /
/// `--preferred-idle-scan` (`cosmos_set_idle_scan`). Under an under-loaded SMT
/// topology, *both* scan modes must (a) keep idle tracking active
/// (`UpdateIdle{idle:true}` fires), (b) actually locate idle CPUs — the wakers
/// spread across multiple CPUs rather than piling on one — and (c) let every
/// task make progress. This complements `cosmos.rs`'s coverage-only scan tests
/// by asserting the scan's *idle-finding outcome*, not just that it ran.
#[test]
fn test_cosmos_idle_scan_modes_find_idle_cpus() {
    let _lock = common::setup_test();
    let nr_cpus = 4u32;

    for (mode, flat, preferred) in [("flat", true, false), ("preferred", false, true)] {
        let sched = DynamicScheduler::cosmos(nr_cpus);
        sched.cosmos_set_idle_scan(nr_cpus, flat, preferred);

        let mut b = Scenario::builder()
            .cpus(nr_cpus)
            .smt(2)
            .seed(42)
            .instant_timing();
        for i in 0..nr_cpus as i32 {
            b = b.task(task(
                &format!("t{i}"),
                1 + i,
                None,
                run_sleep(3_000_000, 2_000_000),
            ));
        }
        let trace = Simulator::new(sched).run(b.duration_ms(150).build());

        assert_eq!(
            trace.exit_kind(),
            &ExitKind::Normal,
            "[{mode}] abnormal exit"
        );
        assert!(
            !trace.has_error(),
            "[{mode}] error: {:?}",
            trace.exit_kind()
        );

        // Idle tracking is active under this scan mode.
        assert!(
            update_idle_count(&trace, true) > 0,
            "[{mode}] no idle notifications — scan had no idle state to consult"
        );

        // The scan found idle CPUs: the four wake/sleep tasks landed on more
        // than one CPU rather than serializing on a single one.
        let used = cpus_used(&trace);
        assert!(
            used.len() >= 2,
            "[{mode}] idle scan did not spread work across idle CPUs; used only {used:?}"
        );

        // Every task made progress.
        for pid in 1..=nr_cpus as i32 {
            assert!(
                trace.total_runtime(Pid(pid)) > 0,
                "[{mode}] task {pid} got no runtime"
            );
        }
    }
}

// ===========================================================================
// 5. All CPUs become idle simultaneously.
// ===========================================================================

/// When every task sleeps at the same time, the whole box must go idle at once —
/// and then recover. With a synchronized short-run / long-sleep workload
/// (one task per CPU), replaying the `UpdateIdle` transitions must show a moment
/// where *all* CPUs are simultaneously marked idle, and the run must still exit
/// normally with every task scheduled (the system wakes back up from a fully
/// idle state rather than getting stuck).
#[test]
fn test_cosmos_all_cpus_idle_simultaneously() {
    let _lock = common::setup_test();
    let nr_cpus = 4u32;

    let sched = DynamicScheduler::cosmos(nr_cpus);
    let mut b = Scenario::builder().cpus(nr_cpus).seed(42);
    for i in 0..nr_cpus as i32 {
        // Short run, long synchronized sleep → all CPUs idle together.
        b = b.task(task(
            &format!("sync{i}"),
            1 + i,
            None,
            run_sleep(2_000_000, 10_000_000),
        ));
    }
    let trace = Simulator::new(sched).run(b.duration_ms(150).build());

    assert_eq!(trace.exit_kind(), &ExitKind::Normal);
    assert!(!trace.has_error(), "error: {:?}", trace.exit_kind());

    // Replay the bitmap and track the peak simultaneous-idle count.
    let mut idle_set: HashSet<u32> = HashSet::new();
    let mut max_simultaneous = 0usize;
    for e in trace.events() {
        if let TraceKind::UpdateIdle { cpu, idle } = e.kind {
            if idle {
                idle_set.insert(cpu.0);
            } else {
                idle_set.remove(&cpu.0);
            }
            max_simultaneous = max_simultaneous.max(idle_set.len());
        }
    }
    assert_eq!(
        max_simultaneous as u32, nr_cpus,
        "the box never reached full idle: peak {max_simultaneous}/{nr_cpus} CPUs idle at once"
    );

    // Every CPU also exited idle at least once (the box recovered, not stuck).
    let exited = cpus_with_idle_event(&trace, false);
    assert_eq!(
        exited.len() as u32,
        nr_cpus,
        "not all CPUs woke back up: only {exited:?} exited idle"
    );

    // Recovery: every task ran (and, being periodic, ran repeatedly).
    for pid in 1..=nr_cpus as i32 {
        assert!(
            trace.schedule_count(Pid(pid)) > 1,
            "task {pid} did not resume after the all-idle window"
        );
    }
}

// ===========================================================================
// Determinism guard.
// ===========================================================================

/// The idle-tracking scenarios must be reproducible run-to-run (no flakes).
#[test]
fn test_cosmos_idle_tracking_determinism() {
    let _lock = common::setup_test();

    let build = || {
        let mut b = Scenario::builder().cpus(4).seed(42).instant_timing();
        for i in 1..=10u32 {
            b = b.task(task(
                &format!("busy{i}"),
                i as i32,
                None,
                run_sleep(4_000_000, 1_000_000),
            ));
        }
        b.duration_ms(150).build()
    };
    let mk = || {
        let s = DynamicScheduler::cosmos(4);
        s.cosmos_set_cpu_util(4, 1024);
        s
    };
    let t1 = Simulator::new(mk()).run(build());
    let t2 = Simulator::new(mk()).run(build());
    assert_identical(&t1, &t2, "cosmos idle-tracking");
}
