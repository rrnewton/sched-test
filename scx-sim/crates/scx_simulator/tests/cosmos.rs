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
            thread_group_leader: None,
            uid: Uid(0),
            gid: Gid(0),
            fork_cpu: None,
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
            thread_group_leader: None,
            uid: Uid(0),
            gid: Gid(0),
            fork_cpu: None,
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

// ---------------------------------------------------------------------------
// Coverage-targeted tests (tg write-cosmos-tests, from coverage-audit
// COVERAGE_AUDIT_20260722.md). These exercise cosmos code paths that the
// generic suite + smt/mm/numa tests never reach: the flat/preferred idle-scan
// fallback, heterogeneous (big.LITTLE) capacity comparison, NUMA node-usability
// gating with restricted affinity, the shared-DSQ deadline path, SMT sibling
// domains, and broad select_cpu/enqueue branch coverage.
// ---------------------------------------------------------------------------

/// A steady CPU-bound task that also periodically sleeps, so it re-enters
/// select_cpu()/enqueue() on each wakeup (exercising idle-scan paths).
fn wakey_task(name: &str, run_ns: TimeNs, sleep_ns: TimeNs) -> TaskBehavior {
    let _ = name;
    TaskBehavior {
        phases: vec![Phase::Run(run_ns), Phase::Sleep(sleep_ns)],
        repeat: RepeatMode::Forever,
    }
}

/// Flat idle-scan path (`--flat-idle-scan`).
///
/// With `flat_idle_scan=true`, `pick_idle_cpu()` routes to
/// `pick_idle_cpu_flat()` → `pick_idle_cpu_pref_smt()` → `get_idle_smtmask()` /
/// `test_cpu_idle()` instead of the `scx_bpf_select_cpu_and()` kfunc. These
/// four functions are otherwise never executed under sim (audit §4.1, ~142
/// uncovered regions). Multiple sleeping/waking tasks on an SMT topology make
/// the flat scan run repeatedly.
#[test]
fn test_flat_idle_scan() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::cosmos(4);
    sched.cosmos_set_idle_scan(4, /*flat=*/ true, /*preferred=*/ false);

    let mut b = Scenario::builder().cpus(4).smt(2);
    for i in 0..4 {
        b = b.add_task(&format!("t{i}"), 0, wakey_task("t", 3_000_000, 2_000_000));
    }
    let scenario = b.duration_ms(200).build();

    let trace = Simulator::new(sched).run(scenario);

    for pid in 1..=4 {
        assert!(
            trace.total_runtime(Pid(pid)) > 0,
            "task {pid} got no runtime under flat idle scan"
        );
    }
}

/// Preferred idle-scan path (`--preferred-idle-scan`).
///
/// With `preferred_idle_scan=true` and a populated `preferred_cpus[]` ranking,
/// `pick_idle_cpu_pref_smt()` iterates CPUs in the preferred order (the
/// `preferred_idle_scan ? preferred_cpus[i] : rotate` branch, main.bpf.c ~735)
/// rather than the round-robin order taken by flat scan.
#[test]
fn test_preferred_idle_scan() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::cosmos(4);
    sched.cosmos_set_idle_scan(4, /*flat=*/ false, /*preferred=*/ true);

    let mut b = Scenario::builder().cpus(4).smt(2);
    for i in 0..4 {
        b = b.add_task(&format!("t{i}"), 0, wakey_task("t", 3_000_000, 2_000_000));
    }
    let scenario = b.duration_ms(200).build();

    let trace = Simulator::new(sched).run(scenario);

    for pid in 1..=4 {
        assert!(
            trace.total_runtime(Pid(pid)) > 0,
            "task {pid} got no runtime under preferred idle scan"
        );
    }
}

/// Heterogeneous (big.LITTLE) capacity: slice scaling.
///
/// Installing an asymmetric `cpu_capacity[]` table (CPUs 0-1 = big/1024,
/// CPUs 2-3 = LITTLE/512) with `all_cpus_same_capacity=false` exercises the
/// non-trivial branch of `scale_by_cpu_capacity()` (main.bpf.c ~1357), which
/// scales a task's slice by its CPU's capacity — otherwise never run under sim
/// (the default has all capacities equal).
///
/// NOTE: the related wakeup-migration block that calls `is_cpu_faster()` /
/// `cpus_share_cache()` (main.bpf.c ~858) is gated on `is_wakeup(wake_flags)`
/// == `wake_flags & SCX_WAKE_TTWU`. The engine now delivers `SCX_WAKE_TTWU` on
/// waker-driven wakes (mb sim-e10316, resolved); those two functions are
/// covered by `test_hybrid_core_wakeup_migration` below. This test focuses on
/// the slice-scaling path.
#[test]
fn test_heterogeneous_capacity() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::cosmos(4);
    // Normalized to [1,1024] exactly as scx_cosmos userspace does.
    sched.cosmos_set_cpu_capacity(&[1024, 1024, 512, 512]);

    let scenario = Scenario::builder()
        .cpus(4)
        .add_task_with_mm(
            "waker",
            0,
            TaskBehavior {
                phases: vec![
                    Phase::Run(4_000_000),
                    Phase::Wake(Pid(2)),
                    Phase::Sleep(4_000_000),
                ],
                repeat: RepeatMode::Forever,
            },
            MmId(1),
        )
        .add_task_with_mm(
            "wakee",
            0,
            TaskBehavior {
                phases: vec![Phase::Run(4_000_000), Phase::Sleep(20_000_000)],
                repeat: RepeatMode::Forever,
            },
            MmId(1),
        )
        .duration_ms(200)
        .build();

    let trace = Simulator::new(sched).run(scenario);

    assert!(trace.total_runtime(Pid(1)) > 0, "waker got no runtime");
    assert!(trace.total_runtime(Pid(2)) > 0, "wakee got no runtime");
    assert!(
        trace.schedule_count(Pid(2)) >= 3,
        "wakee scheduled only {} times",
        trace.schedule_count(Pid(2))
    );
}

/// NUMA scheduling with restricted per-node affinity.
///
/// Runs cosmos with NUMA enabled and each task pinned to a single node's CPUs
/// (a strict subset of all CPUs, so `nr_cpus_allowed < nr_cpu_ids`). This
/// exercises the NUMA-aware select_cpu/enqueue placement paths — including
/// `pick_cpu_on_gpu_node()` and `shared_dsq()` per-node routing — with
/// affinity-restricted tasks, which the existing `test_numa_topology`
/// (unrestricted tasks) does not.
///
/// NOTE: `can_use_node()` itself (main.bpf.c ~474) is reachable only through
/// the GPU-affinity branch of `pick_cpu_on_gpu_node()` (which requires
/// `gpu_node_by_pid(pid)` to return a node), and short-circuits for non-GPU
/// tasks. It is covered by `test_gpu_node_affinity` below, which registers a
/// GPU task via `cosmos_add_gpu_task()` (mb sim-c63e46, resolved); the audit
/// §4.3 hint (plain restricted affinity) was incomplete.
#[test]
fn test_numa_restricted_affinity() {
    let _lock = common::setup_test();
    // 4 CPUs / 2 nodes → node 0 = {0,1}, node 1 = {2,3} (see cosmos_configure_numa).
    let sched = DynamicScheduler::cosmos_with_numa(4, 2);

    let scenario = Scenario::builder()
        .cpus(4)
        .task(TaskDef {
            name: "node0_pinned".into(),
            pid: Pid(1),
            nice: 0,
            behavior: wakey_task("n0", 4_000_000, 2_000_000),
            start_time_ns: 0,
            mm_id: None,
            allowed_cpus: Some(vec![CpuId(0), CpuId(1)]),
            parent_pid: None,
            cgroup_name: None,
            task_flags: 0,
            migration_disabled: 0,
            thread_group_leader: None,
            uid: Uid(0),
            gid: Gid(0),
            fork_cpu: None,
        })
        .task(TaskDef {
            name: "node1_pinned".into(),
            pid: Pid(2),
            nice: 0,
            behavior: wakey_task("n1", 4_000_000, 2_000_000),
            start_time_ns: 0,
            mm_id: None,
            allowed_cpus: Some(vec![CpuId(2), CpuId(3)]),
            parent_pid: None,
            cgroup_name: None,
            task_flags: 0,
            migration_disabled: 0,
            thread_group_leader: None,
            uid: Uid(0),
            gid: Gid(0),
            fork_cpu: None,
        })
        .duration_ms(200)
        .build();

    let trace = Simulator::new(sched).run(scenario);

    assert!(trace.total_runtime(Pid(1)) > 0, "node0 task got no runtime");
    assert!(trace.total_runtime(Pid(2)) > 0, "node1 task got no runtime");
}

/// Shared-DSQ deadline path (`task_dl`).
///
/// COSMOS switches from per-CPU round-robin queues to the global deadline DSQ
/// only when `is_cpu_busy(prev_cpu)` is true, i.e. when userspace has reported
/// `cpu_util_map[cpu] >= busy_threshold`. In that mode cosmos_enqueue() falls
/// through to `scx_bpf_dsq_insert_vtime(..., task_dl(p, tctx), ...)` on the
/// shared DSQ (main.bpf.c ~1233), so `task_dl()` — never covered otherwise
/// (audit §4.4) — computes the virtual deadline.
///
/// The sim doesn't yet derive per-CPU utilization automatically (mb sim-642cb2),
/// so we set it explicitly to 1024 (100%) to match this saturated,
/// oversubscribed workload (6 always-runnable tasks on 2 CPUs), exactly as
/// cosmos userspace would report for a pegged system. `dsq_dispatch_counts().0`
/// (global DSQ inserts, i.e. the vtime path) must then be non-zero.
#[test]
fn test_shared_dsq_contention() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::cosmos(2);
    // Report a saturated system so is_cpu_busy() → deadline mode (task_dl).
    sched.cosmos_set_cpu_util(2, 1024);

    // 8 mostly-CPU-bound tasks that briefly sleep, so demand (~7 CPUs) far
    // exceeds the 2 CPUs (keeps the run queue backed → shared-DSQ deadline
    // path) while the short sleeps let every task cycle through enqueue and
    // make progress.
    let nr_tasks = 8;
    let mut b = Scenario::builder().cpus(2);
    for i in 0..nr_tasks {
        b = b.add_task(
            &format!("busy{i}"),
            0,
            TaskBehavior {
                phases: vec![Phase::Run(8_000_000), Phase::Sleep(1_000_000)],
                repeat: RepeatMode::Forever,
            },
        );
    }
    let scenario = b.duration_ms(300).build();

    let trace = Simulator::new(sched).run(scenario);

    // Primary goal: the global deadline DSQ (task_dl / vtime path) was used.
    let (global, _local) = trace.dsq_dispatch_counts();
    assert!(
        global > 0,
        "expected shared-DSQ (vtime/task_dl) dispatches in deadline mode, got {global}"
    );
    // Every task should still make progress under the deadline scheduler.
    for pid in 1..=nr_tasks {
        assert!(
            trace.total_runtime(Pid(pid)) > 0,
            "task {pid} starved under contention"
        );
    }
}

/// SMT sibling domains (`enable_sibling_cpu`).
///
/// `enable_sibling_cpu` is a `SEC("syscall")` init prog that populates each
/// CPU's `cctx->smt` sibling mask; the sim harness never invoked it (audit
/// §4.5), so it ran 0%. `cosmos_enable_smt_siblings` mirrors cosmos userspace's
/// `init_smt_domains()`. With the masks populated, the scheduler's SMT-aware
/// placement observes real sibling relationships.
#[test]
fn test_smt_sibling_domains() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::cosmos(4);
    sched.cosmos_enable_smt_siblings(4, 2);

    let mut b = Scenario::builder().cpus(4).smt(2);
    for i in 0..4 {
        b = b.add_task(&format!("t{i}"), 0, wakey_task("t", 3_000_000, 2_000_000));
    }
    let scenario = b.duration_ms(200).build();

    let trace = Simulator::new(sched).run(scenario);

    for pid in 1..=4 {
        assert!(
            trace.total_runtime(Pid(pid)) > 0,
            "task {pid} got no runtime with SMT sibling domains"
        );
    }
}

/// Broad select_cpu/enqueue branch coverage.
///
/// cosmos_select_cpu (59 regions) and cosmos_enqueue (126 regions) have many
/// untaken branches (audit §4). This runs a matrix of topologies (CPU count,
/// SMT, nice spread, task count, wake vs pure-CPU behavior) so the placement
/// and enqueue branch conditions (primary/pcpu/event-heavy/busy/idle) are
/// exercised across a range of runtime states. Assertion is limited to "the
/// simulation completes and every task makes progress" — the goal is branch
/// coverage, not a specific placement outcome.
#[test]
fn test_select_cpu_enqueue_matrix() {
    let _lock = common::setup_test();

    // (nr_cpus, smt, nr_tasks, use_wakes)
    let configs: &[(u32, u32, u32, bool)] = &[
        (1, 1, 3, false),
        (2, 1, 2, true),
        (4, 2, 6, false),
        (4, 2, 3, true),
        (8, 2, 5, true),
        (6, 1, 12, false),
    ];

    for &(nr_cpus, smt, nr_tasks, use_wakes) in configs {
        let sched = DynamicScheduler::cosmos(nr_cpus);
        let mut b = Scenario::builder().cpus(nr_cpus).smt(smt);
        for i in 0..nr_tasks {
            // Spread nice values across the range to vary task weights.
            let nice = ((i as i16 % 9) - 4) as i8;
            let behavior = if use_wakes {
                wakey_task("t", 2_000_000 + (i as u64) * 500_000, 3_000_000)
            } else {
                TaskBehavior {
                    phases: vec![Phase::Run(20_000_000)],
                    repeat: RepeatMode::Forever,
                }
            };
            b = b.add_task(&format!("t{i}"), nice, behavior);
        }
        let scenario = b.duration_ms(120).build();

        let trace = Simulator::new(sched).run(scenario);

        for pid in 1..=nr_tasks {
            assert!(
                trace.total_runtime(Pid(pid as i32)) > 0,
                "config (cpus={nr_cpus}, smt={smt}, tasks={nr_tasks}, wakes={use_wakes}): \
                 task {pid} got no runtime"
            );
        }
    }
}

/// Hybrid-core wakeup migration (`is_cpu_faster` / `cpus_share_cache`).
///
/// cosmos `pick_idle_cpu()`'s "move the wakee toward a faster waker CPU" block
/// (main.bpf.c ~858) is gated on `is_wakeup(wake_flags)` ==
/// `(wake_flags & SCX_WAKE_TTWU)`. The engine now delivers `SCX_WAKE_TTWU` on
/// waker-driven wakes (mb sim-e10316), so with an asymmetric big.LITTLE
/// capacity table a waker pinned to a big core, waking wakees that were last on
/// LITTLE cores, makes `is_cpu_faster(waker_cpu, wakee_prev_cpu)` true — which
/// in turn calls `cpus_share_cache()`. Both functions ran 0% under sim before
/// the TTWU fix (audit §4.2). The wakees are allowed on the waker's big core
/// (so `this_cpu` is passed through as allowed) AND on the LITTLE cores (so
/// their `prev_cpu` is genuinely slower than the waker's).
#[test]
fn test_hybrid_core_wakeup_migration() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::cosmos(4);
    // CPUs 0,1 = big (1024); CPUs 2,3 = LITTLE (512).
    sched.cosmos_set_cpu_capacity(&[1024, 1024, 512, 512]);

    let mut b = Scenario::builder().cpus(4);
    // Waker pinned to a big core (cpu 0); it wakes both wakees each cycle.
    b = b.task(TaskDef {
        name: "waker".into(),
        pid: Pid(1),
        nice: 0,
        behavior: TaskBehavior {
            phases: vec![
                Phase::Run(3_000_000),
                Phase::Wake(Pid(2)),
                Phase::Wake(Pid(3)),
                Phase::Sleep(2_000_000),
            ],
            repeat: RepeatMode::Forever,
        },
        start_time_ns: 0,
        mm_id: None,
        allowed_cpus: Some(vec![CpuId(0)]),
        parent_pid: None,
        cgroup_name: None,
        task_flags: 0,
        migration_disabled: 0,
        thread_group_leader: None,
        uid: Uid(0),
        gid: Gid(0),
        fork_cpu: None,
    });
    // Wakees allowed on the big core (cpu 0, the waker's CPU) plus the LITTLE
    // cores (2,3): with cpu 0 occupied by the waker they run on the slower
    // cores, so on wakeup `this_cpu`(=0, big) is faster than their `prev_cpu`.
    for pid in [2, 3] {
        b = b.task(TaskDef {
            name: format!("wakee{pid}"),
            pid: Pid(pid),
            nice: 0,
            behavior: TaskBehavior {
                phases: vec![Phase::Run(2_000_000), Phase::Sleep(6_000_000)],
                repeat: RepeatMode::Forever,
            },
            start_time_ns: 0,
            mm_id: None,
            allowed_cpus: Some(vec![CpuId(0), CpuId(2), CpuId(3)]),
            parent_pid: None,
            cgroup_name: None,
            task_flags: 0,
            migration_disabled: 0,
            thread_group_leader: None,
            uid: Uid(0),
            gid: Gid(0),
            fork_cpu: None,
        });
    }
    let scenario = b.duration_ms(200).build();

    let trace = Simulator::new(sched).run(scenario);

    for pid in 1..=3 {
        assert!(
            trace.total_runtime(Pid(pid)) > 0,
            "task {pid} got no runtime in hybrid-core wakeup test"
        );
    }
}

/// GPU-affinity node gating (`can_use_node`).
///
/// `can_use_node()` (main.bpf.c ~474) is reachable ONLY through
/// `pick_cpu_on_gpu_node()` (~506), whose guard
/// `target_node = gpu_node_by_pid(p->pid)` returns a node only for a task
/// registered in `gpu_pid_map` — populated in production from NVML.
/// `cosmos_add_gpu_task()` registers such a task (mb sim-c63e46). With NUMA
/// enabled and the GPU task pinned to a node OTHER than its GPU node,
/// `cosmos_select_cpu()`'s GPU branch (~1085) evaluates
/// `can_use_node(p, gpu_node)` on every wakeup. It was 0% under sim before this
/// (audit §4.3 — the hint there, "plain restricted affinity", was incomplete;
/// `can_use_node` is GPU-gated).
///
/// NOTE: the task is pinned to CPUs that do NOT intersect the GPU node's
/// cpumask, so `can_use_node()` returns false and `pick_cpu_on_gpu_node()`
/// short-circuits BEFORE `__COMPAT_scx_bpf_pick_idle_cpu_node()`, which the sim
/// does not yet model (it would hit a NULL weak ksym). `can_use_node()` is
/// still fully executed. Covering its `return true` path (and the GPU dispatch
/// itself) needs the idle-CPU-by-node kfunc modeled — tracked in mb sim-c63e46.
#[test]
fn test_gpu_node_affinity() {
    let _lock = common::setup_test();
    // 4 CPUs / 2 nodes: node 0 = {0,1}, node 1 = {2,3} (see cosmos_configure_numa).
    let sched = DynamicScheduler::cosmos_with_numa(4, 2);
    // Register pid 1 as a GPU task whose preferred node is node 1 ({2,3}) ...
    sched.cosmos_add_gpu_task(1, 1);

    let scenario = Scenario::builder()
        .cpus(4)
        .task(TaskDef {
            name: "gpu_task".into(),
            pid: Pid(1),
            nice: 0,
            behavior: wakey_task("gpu", 3_000_000, 2_000_000),
            start_time_ns: 0,
            mm_id: None,
            // ... but pin it to node 0's CPUs {0,1}, which do NOT intersect
            // node 1: it always runs on node 0 (so gpu_node != current node →
            // can_use_node is evaluated) and can_use_node() returns false.
            allowed_cpus: Some(vec![CpuId(0), CpuId(1)]),
            parent_pid: None,
            cgroup_name: None,
            task_flags: 0,
            migration_disabled: 0,
            thread_group_leader: None,
            uid: Uid(0),
            gid: Gid(0),
            fork_cpu: None,
        })
        // A second, non-GPU task on the same node keeps the run realistic.
        .task(TaskDef {
            name: "other".into(),
            pid: Pid(2),
            nice: 0,
            behavior: wakey_task("o", 3_000_000, 2_000_000),
            start_time_ns: 0,
            mm_id: None,
            allowed_cpus: Some(vec![CpuId(0), CpuId(1)]),
            parent_pid: None,
            cgroup_name: None,
            task_flags: 0,
            migration_disabled: 0,
            thread_group_leader: None,
            uid: Uid(0),
            gid: Gid(0),
            fork_cpu: None,
        })
        .duration_ms(200)
        .build();

    let trace = Simulator::new(sched).run(scenario);

    assert!(trace.total_runtime(Pid(1)) > 0, "gpu task got no runtime");
    assert!(trace.total_runtime(Pid(2)) > 0, "other task got no runtime");
}
