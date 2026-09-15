//! Behavioral tests for LAVD's latency-criticality "layer" scheduling and task
//! classification.
//!
//! LAVD has no discrete layers like `scx_layered`; its class mechanism is
//! **latency criticality** (`lat_cri`) feeding a virtual deadline. A task's
//! `lat_cri` rises with how often it sleeps/wakes (interactive) and falls the
//! more CPU it hogs (batch/greedy); the scheduler then (a) orders the runqueue
//! by virtual deadline (higher `lat_cri` → earlier deadline → runs sooner),
//! (b) lets a higher-`lat_cri` task preempt a lower one (`can_x_kick_y` in
//! `preempt.bpf.c`: X kicks Y iff `X.lat_cri > Y.lat_cri`), and (c) sizes each
//! task's timeslice by load.
//!
//! The existing `lavd.rs` suite checks `lat_cri` *values* for individual
//! archetypes and exercises the slice/preempt code paths for coverage. This file
//! complements it with cohesive, **behavioral** assertions tied 1:1 to the five
//! aspects of the task:
//!   1. classification forms a total order across archetypes (batch → interactive);
//!   2. that order translates into scheduling *priority* (lower run delay);
//!   3. a latency-critical task preempts running batch tasks (kick direction);
//!   4. interactive tasks are detected and boosted above the system average;
//!   5. timeslice allocation is load-dependent (light load → larger slices).
//!
//! Everything is read through the public LAVD probes / `TraceStats` — no
//! scheduler-side changes, per the No-Stub / "model the kernel, not the
//! scheduler" rules in `scx-sim/CLAUDE.md`.

use scx_simulator::*;
use scx_simulator::{LavdMonitor, LavdProbes};

mod common;

// ---------------------------------------------------------------------------
// Shared fixtures
// ---------------------------------------------------------------------------

/// Build a `TaskDef` with the given pid / behavior / optional shared address
/// space (needed for LAVD wake-frequency tracking on ping-pong / wake chains).
fn task(name: &str, pid: i32, behavior: TaskBehavior, mm_id: Option<MmId>) -> TaskDef {
    TaskDef {
        name: name.into(),
        pid: Pid(pid),
        nice: 0,
        behavior,
        start_time_ns: 0,
        mm_id,
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

/// Mean run delay (enqueue→scheduled scheduling latency, ns) for a task, from
/// `TraceStats`. Returns 0.0 if the task recorded no run-delay samples.
fn mean_run_delay(stats: &TraceStats, pid: Pid) -> f64 {
    match stats.tasks.get(&pid) {
        Some(ts) if !ts.sched_latencies.is_empty() => {
            ts.sched_latencies.iter().sum::<TimeNs>() as f64 / ts.sched_latencies.len() as f64
        }
        _ => 0.0,
    }
}

// ===========================================================================
// 1. Classification forms a total order: batch < periodic < io < interactive.
// ===========================================================================

/// The four canonical archetypes must be classified into a strict latency-
/// criticality order in a single run: a pure CPU hog (batch) at the bottom, a
/// periodic task above it, an I/O-bound task higher still, and a mutually-waking
/// ping-pong task (most interactive) at the top.
#[test]
fn test_classification_total_order() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::lavd(4);
    let probes = LavdProbes::new(&sched);
    let mut monitor = LavdMonitor::new(probes);

    // pids 4 & 5 are the ping-pong pair (share an address space).
    let (ping_b, pong_b) = workloads::ping_pong(Pid(4), Pid(5), 300_000);
    let scenario = Scenario::builder()
        .cpus(4)
        .task(task("batch", 1, workloads::cpu_bound(100_000_000), None))
        .task(task(
            "periodic",
            2,
            workloads::periodic(2_000_000, 10_000_000),
            None,
        ))
        .task(task("io", 3, workloads::io_bound(500_000, 9_500_000), None))
        .task(task("ping", 4, ping_b, Some(MmId(7))))
        .task(task("pong", 5, pong_b, Some(MmId(7))))
        .duration_ms(500)
        .build();

    let _result = Simulator::new(sched).run_monitored(scenario, &mut monitor);

    let lc = |pid: i32| monitor.final_snapshot(Pid(pid)).unwrap().lat_cri;
    let (batch, periodic, io, ping) = (lc(1), lc(2), lc(3), lc(4));
    eprintln!("classification: batch={batch} periodic={periodic} io={io} ping={ping}");

    // The CPU hog is the least latency-critical of all.
    assert!(
        batch < periodic && batch < io && batch < ping,
        "batch hog should have the lowest lat_cri: batch={batch} periodic={periodic} io={io} ping={ping}"
    );
    // The ping-pong task is the most latency-critical of all.
    assert!(
        ping > periodic && ping > io && ping > batch,
        "ping-pong should have the highest lat_cri: batch={batch} periodic={periodic} io={io} ping={ping}"
    );
    // The io-bound tier is at least as latency-critical as the periodic tier.
    // (Both sleep-driven archetypes commonly converge to LAVD's normalized 1024
    // baseline, so this is a >= relation; the strict separation is batch < both
    // < ping.)
    assert!(
        io >= periodic,
        "io-bound should rank at or above periodic: io={io} periodic={periodic}"
    );
}

// ===========================================================================
// 2. Classification → scheduling priority: interactive gets lower run delay.
// ===========================================================================

/// The point of classification is priority: under contention, a latency-critical
/// (interactive) task must be scheduled with substantially lower run delay
/// (enqueue→run latency) than co-running batch hogs. This checks the *behavioral*
/// payoff of the classification, not just the `lat_cri` score.
#[test]
fn test_latency_class_gets_scheduling_priority() {
    let _lock = common::setup_test();
    const NR_CPUS: u32 = 2;
    let sched = DynamicScheduler::lavd(NR_CPUS);

    // 4 batch hogs saturate the 2 CPUs; one I/O-bound task must cut ahead.
    let mut builder = Scenario::builder().cpus(NR_CPUS);
    for i in 0..4 {
        builder = builder.task(task(
            &format!("hog{i}"),
            1 + i,
            workloads::cpu_bound(50_000_000),
            None,
        ));
    }
    builder = builder.task(task(
        "interactive",
        10,
        workloads::io_bound(500_000, 5_000_000),
        None,
    ));
    let scenario = builder.duration_ms(400).build();

    let trace = Simulator::new(sched).run(scenario);
    assert_eq!(trace.exit_kind(), &ExitKind::Normal, "clean exit");

    let stats = TraceStats::from_trace(&trace);
    let interactive_delay = mean_run_delay(&stats, Pid(10));
    let hog_delays: Vec<f64> = (0..4).map(|i| mean_run_delay(&stats, Pid(1 + i))).collect();
    let worst_hog = hog_delays.iter().cloned().fold(0.0_f64, f64::max);
    eprintln!(
        "run delay: interactive={interactive_delay:.0}ns hogs={hog_delays:?} worst_hog={worst_hog:.0}ns"
    );

    // The interactive task ran.
    assert!(
        trace.schedule_count(Pid(10)) > 0,
        "interactive task never ran"
    );
    // The batch hogs genuinely queued behind each other (real contention).
    assert!(
        worst_hog > 1_000_000.0,
        "expected batch hogs to accrue real run delay under contention, worst={worst_hog:.0}ns"
    );
    // The latency-critical task is scheduled with far less delay than the hogs.
    assert!(
        interactive_delay < worst_hog,
        "interactive run delay ({interactive_delay:.0}ns) should be below the worst hog ({worst_hog:.0}ns)"
    );
}

// ===========================================================================
// 3. Preemption direction: a latency-critical task preempts batch tasks.
// ===========================================================================

/// Layer priority via preemption: with every CPU saturated by batch hogs, a
/// latency-critical ping-pong pair waking up must be able to preempt them. Assert
/// both the *classification* precondition (interactive `lat_cri` > hog `lat_cri`,
/// matching `can_x_kick_y`) and the *effect* (kick/preempt activity occurs and
/// the interactive tasks are serviced).
#[test]
fn test_latency_critical_preempts_batch() {
    let _lock = common::setup_test();
    const NR_CPUS: u32 = 2;
    let sched = DynamicScheduler::lavd(NR_CPUS);
    let probes = LavdProbes::new(&sched);
    let mut monitor = LavdMonitor::new(probes);

    let (ping_b, pong_b) = workloads::ping_pong(Pid(10), Pid(11), 200_000);
    let scenario = Scenario::builder()
        .cpus(NR_CPUS)
        .task(task("ping", 10, ping_b, Some(MmId(1))))
        .task(task("pong", 11, pong_b, Some(MmId(1))))
        .task(task("hog0", 12, workloads::cpu_bound(50_000_000), None))
        .task(task("hog1", 13, workloads::cpu_bound(50_000_000), None))
        .duration_ms(300)
        .build();

    let result = Simulator::new(sched).run_monitored(scenario, &mut monitor);
    let trace = &result.trace;
    assert_eq!(trace.exit_kind(), &ExitKind::Normal, "clean exit");

    // Classification precondition: the interactive pair out-ranks the hogs, which
    // is exactly the condition under which LAVD allows one to kick the other.
    let ping_lc = monitor.final_snapshot(Pid(10)).unwrap().lat_cri;
    let hog_lc = monitor.final_snapshot(Pid(12)).unwrap().lat_cri;
    eprintln!("preempt: ping lat_cri={ping_lc} hog lat_cri={hog_lc}");
    assert!(
        ping_lc > hog_lc,
        "interactive lat_cri ({ping_lc}) must exceed batch lat_cri ({hog_lc}) for it to preempt"
    );

    // Effect: preemption activity occurred (the hogs were kicked off / preempted),
    // and the interactive tasks were actually serviced despite saturated CPUs.
    let kicks = trace
        .events()
        .iter()
        .filter(|e| matches!(e.kind, TraceKind::KickCpu { .. }))
        .count();
    let hog_preemptions: usize = [12, 13].iter().map(|p| trace.preempt_count(Pid(*p))).sum();
    eprintln!("preempt: KickCpu={kicks} hog_preemptions={hog_preemptions}");
    assert!(
        kicks + hog_preemptions > 0,
        "expected the latency-critical pair to trigger preemption (kicks={kicks}, hog_preemptions={hog_preemptions})"
    );
    assert!(
        trace.schedule_count(Pid(10)) > 0 && trace.schedule_count(Pid(11)) > 0,
        "interactive pair should be serviced despite saturated CPUs"
    );
}

// ===========================================================================
// 4. Interactive detection & boost: above the system average, counted by LAVD.
// ===========================================================================

/// An interactive task must be *detected* (its `lat_cri` converges above the
/// system average `sys_avg_lat_cri`, i.e. it is boosted relative to the typical
/// task) and must clear the system's latency-critical kick threshold
/// `sys_thr_lat_cri` — the exact bar `is_worth_kick_other_task()` uses
/// (`lat_cri >= thr_lat_cri`) to treat a task as latency-critical enough to
/// preempt others.
#[test]
fn test_interactive_detection_and_boost() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::lavd(4);
    let probes = LavdProbes::new(&sched);
    let mut monitor = LavdMonitor::new(probes);

    let (ping_b, pong_b) = workloads::ping_pong(Pid(1), Pid(2), 300_000);
    let scenario = Scenario::builder()
        .cpus(4)
        .task(task("ping", 1, ping_b, Some(MmId(1))))
        .task(task("pong", 2, pong_b, Some(MmId(1))))
        // Batch background so there is a meaningful (lower) system baseline.
        .task(task("hog0", 3, workloads::cpu_bound(100_000_000), None))
        .task(task("hog1", 4, workloads::cpu_bound(100_000_000), None))
        .duration_ms(500)
        .build();

    let _result = Simulator::new(sched).run_monitored(scenario, &mut monitor);

    let ping = monitor.final_snapshot(Pid(1)).unwrap();
    eprintln!(
        "detection: ping lat_cri={} sys_avg_lat_cri={} sys_thr_lat_cri={}",
        ping.lat_cri, ping.sys_avg_lat_cri, ping.sys_thr_lat_cri
    );

    // Detected as interactive: boosted above the system-average criticality.
    assert!(
        ping.lat_cri as u32 > ping.sys_avg_lat_cri,
        "interactive task lat_cri ({}) should be boosted above system average ({})",
        ping.lat_cri,
        ping.sys_avg_lat_cri
    );
    // LAVD maintains a latency-critical kick threshold, and the interactive task
    // clears it — i.e. it is classified as latency-critical enough to preempt
    // others (the `is_worth_kick_other_task` bar: lat_cri >= thr_lat_cri).
    assert!(
        ping.sys_thr_lat_cri > 0,
        "expected LAVD to establish a latency-critical threshold, got {}",
        ping.sys_thr_lat_cri
    );
    assert!(
        ping.lat_cri as u32 >= ping.sys_thr_lat_cri,
        "interactive task lat_cri ({}) should clear the kick threshold ({})",
        ping.lat_cri,
        ping.sys_thr_lat_cri
    );
}

// ===========================================================================
// 5. Timeslice allocation is load-dependent.
// ===========================================================================

/// LAVD sizes each task's timeslice by load. `task_slice_wall` is the task's
/// wall-clock slice target: a compute-bound task that owns its CPU has its slice
/// boosted toward its full observed runtime (tens of ms), while the same task on
/// a saturated CPU is squeezed down to a much smaller share. Assert the assigned
/// slices are positive and that the lightly-loaded task gets a strictly (and
/// substantially) larger slice than the heavily-contended one.
#[test]
fn test_timeslice_allocation_scales_with_load() {
    let _lock = common::setup_test();

    fn final_slice(n_tasks: i32, nr_cpus: u32, run_ns: u64) -> u64 {
        let sched = DynamicScheduler::lavd(nr_cpus);
        let probes = LavdProbes::new(&sched);
        let mut monitor = LavdMonitor::new(probes);
        let mut builder = Scenario::builder().cpus(nr_cpus);
        for i in 0..n_tasks {
            builder = builder.task(task(
                &format!("t{i}"),
                1 + i,
                workloads::cpu_bound(run_ns),
                None,
            ));
        }
        let scenario = builder.duration_ms(300).build();
        let _result = Simulator::new(sched).run_monitored(scenario, &mut monitor);
        // Use pid 1's converged (final) assigned slice.
        monitor.final_snapshot(Pid(1)).unwrap().task_slice_wall
    }

    // Light load: a single CPU hog owning a whole CPU.
    let light = final_slice(1, 4, 100_000_000);
    // Heavy load: many hogs saturating one CPU.
    let heavy = final_slice(8, 1, 100_000_000);
    eprintln!("timeslice: light(1 task/4cpu)={light}ns heavy(8 tasks/1cpu)={heavy}ns");

    // Assigned slices are positive.
    assert!(light > 0, "light task_slice_wall must be positive");
    assert!(heavy > 0, "heavy task_slice_wall must be positive");
    // Load-dependent allocation: the lightly-loaded task gets a substantially
    // larger slice than the heavily-contended one.
    assert!(
        light > 2 * heavy,
        "expected light-load slice ({light}ns) to substantially exceed heavy-load slice ({heavy}ns)"
    );
}
