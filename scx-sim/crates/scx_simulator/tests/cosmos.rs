use scx_simulator::*;

#[macro_use]
mod common;

// Generic test suite applied to scx_cosmos
scheduler_tests!(|nr_cpus| DynamicScheduler::cosmos(nr_cpus));

/// SMT topology: 4 CPUs with 2 threads per core.
/// Tasks should spread across cores and the scheduler should
/// exercise the idle-core preference path.
#[test]
fn test_smt_topology() {
    let _lock = common::setup_test();
    let scenario = Scenario::builder()
        .cpus(4)
        .smt(2) // 2 cores, 2 threads each
        .task(TaskDef {
            name: "t1".into(),
            pid: Pid(1),
            nice: 0,
            behavior: TaskBehavior {
                phases: vec![Phase::Run(50_000_000)],
                repeat: RepeatMode::Forever,
            },
            start_time_ns: 0,
            mm_id: None,
            allowed_cpus: None,
            parent_pid: None,
            cgroup_name: None,
            task_flags: 0,
            migration_disabled: 0,
        })
        .task(TaskDef {
            name: "t2".into(),
            pid: Pid(2),
            nice: 0,
            behavior: TaskBehavior {
                phases: vec![Phase::Run(50_000_000)],
                repeat: RepeatMode::Forever,
            },
            start_time_ns: 0,
            mm_id: None,
            allowed_cpus: None,
            parent_pid: None,
            cgroup_name: None,
            task_flags: 0,
            migration_disabled: 0,
        })
        .duration_ms(200)
        .build();

    let trace = Simulator::new(DynamicScheduler::cosmos(4)).run(scenario);

    assert!(trace.total_runtime(Pid(1)) > 0, "task 1 got no runtime");
    assert!(trace.total_runtime(Pid(2)) > 0, "task 2 got no runtime");

    // Both tasks should get significant runtime (at least 25% each)
    let total = trace.total_runtime(Pid(1)) + trace.total_runtime(Pid(2));
    assert!(
        trace.total_runtime(Pid(1)) >= total / 4,
        "task 1 didn't get fair share"
    );
}

/// Address-space affinity (mm_affinity): threads sharing an address space
/// should be co-located via wake-affine scheduling.
///
/// Task A (waker) runs, sleeps, then wakes task B (wakee). Both share
/// MmId(1). COSMOS's is_wake_affine() should return true and dispatch B
/// directly to A's previous CPU when conditions align.
#[test]
fn test_mm_affinity() {
    let _lock = common::setup_test();
    let scenario = Scenario::builder()
        .cpus(2)
        .add_task_with_mm(
            "waker",
            0,
            TaskBehavior {
                // Run 5ms → wake peer → sleep 5ms → repeat
                phases: vec![
                    Phase::Run(5_000_000),
                    Phase::Wake(Pid(2)),
                    Phase::Sleep(5_000_000),
                ],
                repeat: RepeatMode::Forever,
            },
            MmId(1),
        )
        .add_task_with_mm(
            "wakee",
            0,
            TaskBehavior {
                // Run 5ms → sleep 20ms (will be woken by waker before timer)
                phases: vec![Phase::Run(5_000_000), Phase::Sleep(20_000_000)],
                repeat: RepeatMode::Forever,
            },
            MmId(1),
        )
        .duration_ms(200)
        .build();

    let trace = Simulator::new(DynamicScheduler::cosmos(2)).run(scenario);

    assert!(trace.total_runtime(Pid(1)) > 0, "waker got no runtime");
    assert!(trace.total_runtime(Pid(2)) > 0, "wakee got no runtime");

    // Both tasks should be scheduled multiple times (wake/sleep cycling)
    assert!(
        trace.schedule_count(Pid(1)) >= 3,
        "waker scheduled only {} times",
        trace.schedule_count(Pid(1))
    );
    assert!(
        trace.schedule_count(Pid(2)) >= 3,
        "wakee scheduled only {} times",
        trace.schedule_count(Pid(2))
    );
}

/// With NUMA enabled, each node has its own shared DSQ.
/// Tasks on different nodes should still get fair runtime.
#[test]
fn test_numa_topology() {
    let _lock = common::setup_test();
    let scenario = Scenario::builder()
        .cpus(4)
        .add_task(
            "node0_task",
            0,
            TaskBehavior {
                phases: vec![Phase::Run(50_000_000)],
                repeat: RepeatMode::Forever,
            },
        )
        .add_task(
            "node1_task",
            0,
            TaskBehavior {
                phases: vec![Phase::Run(50_000_000)],
                repeat: RepeatMode::Forever,
            },
        )
        .duration_ms(200)
        .build();

    let trace = Simulator::new(DynamicScheduler::cosmos_with_numa(4, 2)).run(scenario);

    assert!(trace.total_runtime(Pid(1)) > 0, "task 1 got no runtime");
    assert!(trace.total_runtime(Pid(2)) > 0, "task 2 got no runtime");

    // Both tasks should get significant runtime
    let total = trace.total_runtime(Pid(1)) + trace.total_runtime(Pid(2));
    assert!(
        trace.total_runtime(Pid(1)) >= total / 4,
        "task 1 didn't get fair share"
    );
}

// ============================================================================
// Coverage-targeted tests (tg write-cosmos-tests).
//
// `cosmos_setup()` pins COSMOS into its simplest configuration, leaving
// several production code paths dormant under simulation. The tests below use
// the `cosmos_set_*` wrapper knobs (mirroring scx_cosmos userspace config /
// the periodic userspace utilization sampler) to reach them. See
// scx-sim/ai_docs/COVERAGE_AUDIT_20260722.md §4 for the gap analysis.
// ============================================================================

/// Build a CPU-bound task pinned to a single CPU (`allowed_cpus == [cpu]`),
/// which makes COSMOS treat it as a per-CPU task (`is_pcpu_task() == true`).
fn pinned_cpu_bound_task(name: &str, pid: i32, cpu: u32, run_ns: TimeNs) -> TaskDef {
    TaskDef {
        name: name.into(),
        pid: Pid(pid),
        nice: 0,
        behavior: TaskBehavior {
            phases: vec![Phase::Run(run_ns)],
            repeat: RepeatMode::Forever,
        },
        start_time_ns: 0,
        mm_id: None,
        allowed_cpus: Some(vec![CpuId(cpu)]),
        parent_pid: None,
        cgroup_name: None,
        task_flags: 0,
        migration_disabled: 0,
    }
}

/// A CPU-bound run/sleep task that repeatedly wakes and re-selects a CPU,
/// driving many `select_cpu` invocations.
fn bursty_task(behavior_run_ns: TimeNs, sleep_ns: TimeNs) -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(behavior_run_ns), Phase::Sleep(sleep_ns)],
        repeat: RepeatMode::Forever,
    }
}

/// Flat idle-scan path: with `flat_idle_scan = true`, COSMOS uses its BPF-side
/// `pick_idle_cpu_flat()` / `pick_idle_cpu_pref_smt()` scan (plus
/// `get_idle_smtmask()` / `test_cpu_idle()`) instead of the
/// `scx_bpf_select_cpu_and()` kfunc path. These functions are dead in the
/// default config (§4.1 — the single biggest cosmos coverage gap, ~142
/// regions).
#[test]
fn test_flat_idle_scan() {
    let _lock = common::setup_test();
    // Underloaded (4 tasks, 8 CPUs across 4 SMT cores) so CPUs stay idle and
    // the flat scan actually walks the idle mask. Bursty tasks re-select a CPU
    // on every wakeup.
    let mut builder = Scenario::builder().cpus(8).smt(2);
    for i in 0..4u64 {
        builder = builder.add_task(&format!("t{i}"), 0, bursty_task(2_000_000, 3_000_000));
    }
    let scenario = builder.duration_ms(200).build();

    let sched = DynamicScheduler::cosmos(8);
    sched.cosmos_set_flat_idle_scan(true);
    let trace = Simulator::new(sched).run(scenario);

    for pid in 1..=4i32 {
        assert!(
            trace.total_runtime(Pid(pid)) > 0,
            "task {pid} got no runtime under flat idle scan"
        );
    }
}

/// Preferred-order idle scan: `preferred_idle_scan = true` with a populated
/// `preferred_cpus[]` ranking exercises the preferred-order branch inside
/// `pick_idle_cpu_pref_smt()` (§4.1). The ranking is reversed so the scan
/// order differs from natural CPU order.
#[test]
fn test_preferred_idle_scan() {
    let _lock = common::setup_test();
    let nr_cpus = 4u32;
    let mut builder = Scenario::builder().cpus(nr_cpus);
    for i in 0..3u64 {
        builder = builder.add_task(&format!("t{i}"), 0, bursty_task(2_000_000, 4_000_000));
    }
    let scenario = builder.duration_ms(200).build();

    let sched = DynamicScheduler::cosmos(nr_cpus);
    sched.cosmos_set_preferred_idle_scan(true);
    // Reversed preferred ranking: rank 0 -> highest CPU id, etc.
    for rank in 0..nr_cpus {
        sched.cosmos_set_preferred_cpu(rank, CpuId(nr_cpus - 1 - rank));
    }
    let trace = Simulator::new(sched).run(scenario);

    for pid in 1..=3i32 {
        assert!(
            trace.total_runtime(Pid(pid)) > 0,
            "task {pid} got no runtime under preferred idle scan"
        );
    }
}

/// Heterogeneous (big.LITTLE) capacity: with asymmetric `cpu_capacity[]` and
/// `all_cpus_same_capacity = false`, slice computation reaches
/// `scale_by_cpu_capacity()` (§4.2). Uses a wake-affine (shared-mm)
/// waker/wakee pair plus a filler so the scheduler runs a mixed workload.
///
/// NOTE: `is_cpu_faster()` / `cpus_share_cache()` (the wakeup migration branch
/// at main.bpf.c ~L858) remain uncovered because the sim engine never sets
/// `SCX_WAKE_TTWU` in the `wake_flags` passed to `select_cpu`, so cosmos's
/// `is_wakeup()` gate is always false. Tracked in mb sim-e2caba; that branch
/// is unreachable from a test until the engine delivers the TTWU flag.
#[test]
fn test_heterogeneous_capacity() {
    let _lock = common::setup_test();
    // 2 "big" CPUs (0,1) and 2 "little" CPUs (2,3).
    let scenario = Scenario::builder()
        .cpus(4)
        .add_task_with_mm(
            "waker",
            0,
            TaskBehavior {
                phases: vec![
                    Phase::Run(3_000_000),
                    Phase::Wake(Pid(2)),
                    Phase::Sleep(3_000_000),
                ],
                repeat: RepeatMode::Forever,
            },
            MmId(1),
        )
        .add_task_with_mm(
            "wakee",
            0,
            TaskBehavior {
                phases: vec![Phase::Run(3_000_000), Phase::Sleep(15_000_000)],
                repeat: RepeatMode::Forever,
            },
            MmId(1),
        )
        .add_task("filler", 0, bursty_task(2_000_000, 2_000_000))
        .duration_ms(200)
        .build();

    let sched = DynamicScheduler::cosmos(4);
    // 1024 == SCX_CPUPERF_ONE (max). Strongly asymmetric so is_cpu_faster()
    // returns true whenever a task wakes toward a big core from a little core.
    sched.cosmos_set_cpu_capacity(CpuId(0), 1024);
    sched.cosmos_set_cpu_capacity(CpuId(1), 1024);
    sched.cosmos_set_cpu_capacity(CpuId(2), 256);
    sched.cosmos_set_cpu_capacity(CpuId(3), 256);
    // Once mb sim-e2caba lands (engine delivers SCX_WAKE_TTWU), the
    // asymmetric capacities above also exercise is_cpu_faster() /
    // cpus_share_cache() on the wakeup migration path.
    let trace = Simulator::new(sched).run(scenario);

    assert!(trace.total_runtime(Pid(1)) > 0, "waker got no runtime");
    assert!(trace.total_runtime(Pid(2)) > 0, "wakee got no runtime");
}

/// Busy / deadline mode: populating `cpu_util_map` so every CPU reports busy
/// (`is_cpu_busy() == true`) plus an overloaded workload (idle-CPU dispatch
/// fails) forces tasks onto the shared DSQ via `scx_bpf_dsq_insert_vtime()`,
/// which reaches `task_dl()` — the virtual-deadline computation that is never
/// hit in the default config (§4.4).
#[test]
fn test_busy_deadline_mode() {
    let _lock = common::setup_test();
    let nr_cpus = 2u32;
    // 8 bursty tasks on 2 CPUs. Each yields the CPU between bursts, so no task
    // monopolizes a CPU, but the aggregate demand keeps both CPUs saturated —
    // so wakeups that find no idle CPU are routed to the shared DSQ via the
    // deadline path (task_dl / scx_bpf_dsq_insert_vtime).
    let nr_tasks = 8u64;
    let mut builder = Scenario::builder().cpus(nr_cpus);
    for i in 0..nr_tasks {
        builder = builder.add_task(&format!("hog{i}"), 0, bursty_task(4_000_000, 1_000_000));
    }
    let scenario = builder.duration_ms(300).build();

    let sched = DynamicScheduler::cosmos(nr_cpus);
    // busy_threshold small; report near-max utilization on every CPU so
    // is_cpu_busy() is true and the deadline/shared-DSQ path is taken instead
    // of the round-robin (local-DSQ) fast path.
    sched.cosmos_set_busy_threshold(512);
    for cpu in 0..nr_cpus {
        sched.cosmos_set_cpu_util(CpuId(cpu), 1024);
    }
    // Tick preemption so long bursts also rotate the shared DSQ. NOTE: bursty
    // (yielding) tasks are used deliberately — non-yielding CPU-bound tasks in
    // this busy/shared-DSQ config currently monopolize their CPUs and starve
    // the shared-DSQ tasks even with time_preemption on (mb sim-2345f8).
    sched.cosmos_set_time_preemption(true);
    let trace = Simulator::new(sched).run(scenario);

    let runtimes: Vec<u64> = (1..=nr_tasks as i32)
        .map(|p| trace.total_runtime(Pid(p)))
        .collect();
    let total: u64 = runtimes.iter().sum();
    assert!(total > 0, "no task ran in busy/deadline mode");
    // Every bursty task should make progress under deadline ordering.
    for (i, &rt) in runtimes.iter().enumerate() {
        assert!(rt > 0, "task {} starved in deadline mode", i + 1);
    }
}

/// Per-CPU (pinned) tasks: tasks pinned to a single CPU take COSMOS's
/// `is_pcpu_task()` branches in both `cosmos_enqueue` and the idle-dispatch
/// path (direct-to-`prev_cpu` handling, migration skipped). Combined with a
/// busy system to also route pinned tasks that can't get their CPU through the
/// shared-DSQ fallback. Pure Rust — no wrapper knobs beyond utilization.
#[test]
fn test_pinned_pcpu_tasks() {
    let _lock = common::setup_test();
    let nr_cpus = 4u32;
    // Two tasks pinned to CPU 0 (contending pcpu tasks) + one pinned per other
    // CPU + a free-floating filler.
    let scenario = Scenario::builder()
        .cpus(nr_cpus)
        .task(pinned_cpu_bound_task("pin0a", 1, 0, 10_000_000))
        .task(pinned_cpu_bound_task("pin0b", 2, 0, 10_000_000))
        .task(pinned_cpu_bound_task("pin1", 3, 1, 10_000_000))
        .task(pinned_cpu_bound_task("pin2", 4, 2, 10_000_000))
        .add_task("filler", 0, bursty_task(2_000_000, 2_000_000))
        .duration_ms(200)
        .build();

    let sched = DynamicScheduler::cosmos(nr_cpus);
    // Mark CPU 0 busy so the two contending pinned tasks exercise the
    // busy-pcpu fallback (shared DSQ) rather than always keeping prev_cpu.
    sched.cosmos_set_busy_threshold(512);
    sched.cosmos_set_cpu_util(CpuId(0), 1024);
    let trace = Simulator::new(sched).run(scenario);

    // Both CPU-0 pinned tasks must share the CPU and make progress.
    assert!(trace.total_runtime(Pid(1)) > 0, "pin0a starved");
    assert!(trace.total_runtime(Pid(2)) > 0, "pin0b starved");
    assert!(trace.total_runtime(Pid(3)) > 0, "pin1 starved");
}

/// Flat idle scan combined with heterogeneous capacity and pinned tasks — a
/// broad branch-coverage sweep over `pick_idle_cpu_flat` /
/// `pick_idle_cpu_pref_smt` with a mix of migratable and per-CPU tasks and
/// asymmetric cores.
#[test]
fn test_flat_scan_heterogeneous_mixed() {
    let _lock = common::setup_test();
    let nr_cpus = 6u32;
    let scenario = Scenario::builder()
        .cpus(nr_cpus)
        .smt(2)
        .add_task("m0", 0, bursty_task(2_000_000, 3_000_000))
        .add_task("m1", -5, bursty_task(1_000_000, 2_000_000))
        .task(pinned_cpu_bound_task("p0", 3, 0, 4_000_000))
        .task(pinned_cpu_bound_task("p1", 4, 3, 4_000_000))
        .duration_ms(200)
        .build();

    let sched = DynamicScheduler::cosmos(nr_cpus);
    sched.cosmos_set_flat_idle_scan(true);
    // Asymmetric capacities across the 3 SMT cores.
    for cpu in 0..nr_cpus {
        let cap = if cpu < 2 { 1024 } else { 512 };
        sched.cosmos_set_cpu_capacity(CpuId(cpu), cap);
    }
    let trace = Simulator::new(sched).run(scenario);

    for pid in 1..=4i32 {
        assert!(
            trace.total_runtime(Pid(pid)) > 0,
            "task {pid} got no runtime in mixed flat-scan scenario"
        );
    }
}
