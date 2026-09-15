use scx_simulator::*;

#[macro_use]
mod common;

// Generic test suite applied to scx_tickless
scheduler_tests!(|nr_cpus| DynamicScheduler::tickless(nr_cpus));

// ---------------------------------------------------------------------------
// scx_tickless-specific tests (depend on SCX_SLICE_INF behavior)
// ---------------------------------------------------------------------------

/// Weighted fairness with SCX_SLICE_INF on a single CPU: under vtime/deadline
/// scheduling a higher-weight (lower-nice) task is picked more often under
/// contention, so it gets more runtime, while the lower-weight task still
/// retains a non-trivial, non-starved share.
#[test]
fn test_weighted_fairness() {
    let _lock = common::setup_test();
    let scenario = Scenario::builder()
        .cpus(1)
        .task(TaskDef {
            name: "heavy".into(),
            pid: Pid(1),
            nice: -3,
            behavior: TaskBehavior {
                phases: vec![Phase::Run(50_000_000)], // 50ms
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
            name: "light".into(),
            pid: Pid(2),
            nice: 0,
            behavior: TaskBehavior {
                phases: vec![Phase::Run(50_000_000)], // 50ms
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

    let trace = Simulator::new(DynamicScheduler::tickless(1)).run(scenario);
    trace.dump();

    let rt_heavy = trace.total_runtime(Pid(1));
    let rt_light = trace.total_runtime(Pid(2));

    // scx_tickless is vtime/deadline-scheduled: enqueue inserts by deadline
    // (scx_bpf_dsq_insert_vtime) and stopping advances the task's per-task
    // tctx->deadline by scale_by_task_weight_inverse(slice) = slice*100/weight.
    // A higher weight (nice=-3, weight 1991 vs nice=0, weight 1024) advances its
    // deadline more slowly, so it is picked more often under single-CPU
    // contention and gets more runtime. The weighting lives entirely in per-task
    // tctx state, so it is only operative when each task has its own task-local
    // storage slot (keyed by task identity).
    assert!(rt_heavy > 0, "heavy task got no runtime");
    assert!(
        rt_light > 0,
        "light task got no runtime (weighting must not starve it)"
    );
    assert!(
        rt_heavy > rt_light,
        "nice=-3 (weight 1991) must get more runtime than nice=0 (weight 1024) \
         under vtime contention; got heavy={rt_heavy}ns light={rt_light}ns"
    );

    // Light must not be starved: weighted-fair scheduling gives it a meaningful
    // share, not strict priority. The floor is well below the ~1.9x weight ratio
    // (robust to finite-run amplification) but high enough to catch a starvation
    // regression.
    let total = rt_heavy + rt_light;
    assert!(
        rt_light * 100 / total >= 15,
        "light task should retain a non-trivial share (>=15% of total); got {}%",
        rt_light * 100 / total
    );
}

/// The read_u64_global accessor resolves real u64 scheduler globals and returns
/// None for unknown symbols (rather than panicking).
#[test]
fn test_read_u64_global_accessor() {
    let _lock = common::setup_test();
    let sim = Simulator::new(DynamicScheduler::tickless(1));
    assert!(
        sim.read_u64_global("nr_primary_dispatches").is_some(),
        "a real u64 scheduler global should resolve"
    );
    assert!(
        sim.read_u64_global("definitely_not_a_real_symbol_xyz")
            .is_none(),
        "an unknown symbol should return None, not panic"
    );
}
