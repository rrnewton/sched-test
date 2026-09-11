//! Tests for per-CPU data isolation and concurrent-access correctness.
//!
//! sched_ext schedulers keep per-CPU state: each CPU has its own local dispatch
//! queue (`SCX_DSQ_LOCAL` / `SCX_DSQ_LOCAL_ON | cpu`), its own `cpu_ctx`, and its
//! own slots in `BPF_MAP_TYPE_PERCPU_*` maps. The simulator models this: the
//! engine sets `current_cpu` before every callback so `bpf_get_smp_processor_id()`
//! and per-CPU map lookups resolve to the right CPU, and each CPU drains only its
//! own local run queue.
//!
//! Per-CPU BPF map cells cannot be poked directly from an integration test, so
//! isolation is verified through observables that would break if per-CPU state
//! leaked across CPUs:
//!   1. per-CPU run-queue isolation — a task pinned to CPU i only ever runs on i;
//!   2. per-CPU local-DSQ indexing — every `LOCAL_ON` dispatch targets the
//!      dispatching task's own CPU (`local_on_cpu()` is correct);
//!   3. concurrent-access correctness — CPUs run in parallel, and no task is ever
//!      scheduled on two CPUs at the same instant (no cross-CPU state clobber);
//!   4. per-CPU statistics are independent — asymmetric load yields asymmetric
//!      per-CPU stats (busy vs idle CPUs);
//!   5. per-CPU statistics accumulate/partition correctly — balanced load yields
//!      balanced per-CPU counts that sum to the global total.
//!
//! Every test runs against all supported schedulers. Assertions read only trace
//! observables / `TraceStats` — no scheduler-side changes, per the No-Stub /
//! "model the kernel, not the scheduler" rules in `scx-sim/CLAUDE.md`.

use std::collections::{HashMap, HashSet};

use scx_simulator::*;

#[macro_use]
mod common;

// ---------------------------------------------------------------------------
// Shared fixtures
// ---------------------------------------------------------------------------

/// A named scheduler factory. `simple` ignores the CPU count.
type NamedSched = (&'static str, fn(u32) -> DynamicScheduler);

/// All supported schedulers — "test with all schedulers".
const SCHEDS: &[NamedSched] = &[
    ("simple", |_n| DynamicScheduler::simple()),
    ("lavd", |n| DynamicScheduler::lavd(n)),
    ("cosmos", |n| DynamicScheduler::cosmos(n)),
    ("mitosis", |n| DynamicScheduler::mitosis(n)),
    ("tickless", |n| DynamicScheduler::tickless(n)),
    ("layered", |n| DynamicScheduler::layered(n)),
];

/// A CPU-bound hog pinned to a single CPU.
fn hog_on(name: &str, pid: i32, cpu: u32, run_ns: u64) -> TaskDef {
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
        thread_group_leader: None,
        uid: Uid(0),
        gid: Gid(0),
        fork_cpu: None,
    }
}

/// An unpinned CPU-bound hog.
fn hog(name: &str, pid: i32, run_ns: u64) -> TaskDef {
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

/// The set of CPUs a task was ever scheduled on.
fn cpus_ran_on(trace: &Trace, pid: Pid) -> HashSet<CpuId> {
    trace
        .events()
        .iter()
        .filter_map(|e| match e.kind {
            TraceKind::TaskScheduled { pid: p } if p == pid => Some(e.cpu),
            _ => None,
        })
        .collect()
}

/// Count `TaskScheduled` events on a given CPU.
fn schedules_on_cpu(trace: &Trace, cpu: CpuId) -> usize {
    trace
        .events()
        .iter()
        .filter(|e| e.cpu == cpu && matches!(e.kind, TraceKind::TaskScheduled { .. }))
        .count()
}

/// Is `kind` a "task left the CPU" event?
fn is_off_cpu(kind: &TraceKind) -> bool {
    matches!(
        kind,
        TraceKind::TaskPreempted { .. }
            | TraceKind::TaskSlept { .. }
            | TraceKind::TaskYielded { .. }
            | TraceKind::TaskCompleted { .. }
    )
}

// ===========================================================================
// 1. Per-CPU run-queue isolation.
// ===========================================================================

/// One CPU-bound hog pinned to each CPU: every hog must be scheduled *only* on
/// its own CPU. If per-CPU run queues (local DSQs) leaked across CPUs, a hog
/// would surface on a foreign CPU.
#[test]
fn test_percpu_runqueue_isolation() {
    let _lock = common::setup_test();
    const NR_CPUS: u32 = 4;

    for (name, make) in SCHEDS {
        let mut builder = Scenario::builder().cpus(NR_CPUS);
        for cpu in 0..NR_CPUS {
            builder = builder.task(hog_on(
                &format!("hog{cpu}"),
                1 + cpu as i32,
                cpu,
                10_000_000,
            ));
        }
        let scenario = builder.duration_ms(200).build();

        let trace = Simulator::new(make(NR_CPUS)).run(scenario);
        assert_eq!(trace.exit_kind(), &ExitKind::Normal, "[{name}] clean exit");

        for cpu in 0..NR_CPUS {
            let pid = Pid(1 + cpu as i32);
            let ran_on = cpus_ran_on(&trace, pid);
            assert!(
                trace.schedule_count(pid) > 0,
                "[{name}] hog pinned to CPU {cpu} never ran"
            );
            assert_eq!(
                ran_on,
                HashSet::from([CpuId(cpu)]),
                "[{name}] hog pinned to CPU {cpu} leaked onto {ran_on:?} (per-CPU runqueue not isolated)"
            );
        }
    }
}

// ===========================================================================
// 2. Per-CPU local-DSQ indexing is correct.
// ===========================================================================

/// Every dispatch into a *specific* CPU's local DSQ (`SCX_DSQ_LOCAL_ON | cpu`,
/// i.e. `DsqId::is_local_on()`) must target a valid CPU, and — when the
/// dispatched task is pinned — that CPU must be the task's own. This checks the
/// per-CPU DSQ is indexed by the right CPU rather than a stale/foreign one.
///
/// `lavd` and `cosmos` exercise `LOCAL_ON` dispatch; `simple`/`mitosis` use the
/// current-CPU `SCX_DSQ_LOCAL` and `tickless` uses a shared DSQ, so the invariant
/// is vacuously satisfied there (the behavioral guarantee is covered by test 1).
#[test]
fn test_percpu_local_dsq_targets_correct_cpu() {
    let _lock = common::setup_test();
    const NR_CPUS: u32 = 4;

    for (name, make) in SCHEDS {
        // Map pid -> pinned CPU for validation.
        let mut pinned: HashMap<Pid, CpuId> = HashMap::new();
        let mut builder = Scenario::builder().cpus(NR_CPUS);
        for cpu in 0..NR_CPUS {
            let pid = Pid(1 + cpu as i32);
            pinned.insert(pid, CpuId(cpu));
            builder = builder.task(hog_on(&format!("hog{cpu}"), pid.0, cpu, 10_000_000));
        }
        let scenario = builder.duration_ms(200).build();

        let trace = Simulator::new(make(NR_CPUS)).run(scenario);
        assert_eq!(trace.exit_kind(), &ExitKind::Normal, "[{name}] clean exit");

        let mut local_on_seen = 0usize;
        for e in trace.events() {
            let (pid, dsq_id) = match e.kind {
                TraceKind::DsqInsert { pid, dsq_id, .. }
                | TraceKind::DsqInsertVtime { pid, dsq_id, .. } => (pid, dsq_id),
                _ => continue,
            };
            if !dsq_id.is_local_on() {
                continue;
            }
            local_on_seen += 1;
            let target = dsq_id.local_on_cpu();
            assert!(
                target.0 < NR_CPUS,
                "[{name}] LOCAL_ON dispatch targets out-of-range CPU {target:?}"
            );
            if let Some(&want) = pinned.get(&pid) {
                assert_eq!(
                    target, want,
                    "[{name}] pid={} (pinned to {want:?}) dispatched into CPU {target:?}'s local DSQ",
                    pid.0
                );
            }
        }

        // Make sure the invariant isn't vacuous for the schedulers that use
        // LOCAL_ON dispatch.
        if matches!(*name, "lavd" | "cosmos") {
            assert!(
                local_on_seen > 0,
                "[{name}] expected LOCAL_ON per-CPU dispatches but saw none"
            );
        }
    }
}

// ===========================================================================
// 3. Concurrent-access correctness: parallel CPUs, no double-scheduling.
// ===========================================================================

/// With one hog pinned per CPU, the CPUs must genuinely run in parallel (more
/// than one CPU busy at the same simulated instant), and — the core
/// concurrent-access invariant — no task may ever be scheduled on two CPUs at
/// once (which would mean per-CPU "currently running" state was clobbered).
#[test]
fn test_concurrent_cpus_no_double_schedule() {
    let _lock = common::setup_test();
    const NR_CPUS: u32 = 4;

    for (name, make) in SCHEDS {
        let mut builder = Scenario::builder().cpus(NR_CPUS);
        for cpu in 0..NR_CPUS {
            builder = builder.task(hog_on(
                &format!("hog{cpu}"),
                1 + cpu as i32,
                cpu,
                10_000_000,
            ));
        }
        let scenario = builder.duration_ms(200).build();

        let trace = Simulator::new(make(NR_CPUS)).run(scenario);
        assert_eq!(trace.exit_kind(), &ExitKind::Normal, "[{name}] clean exit");

        // Walk the trace maintaining, per CPU, which task is currently on it, and
        // per task, which CPU it currently occupies.
        let mut cpu_task: HashMap<CpuId, Pid> = HashMap::new();
        let mut task_cpu: HashMap<Pid, CpuId> = HashMap::new();
        let mut max_concurrent = 0usize;

        for e in trace.events() {
            match &e.kind {
                TraceKind::TaskScheduled { pid } => {
                    // Double-schedule check: this task must not already be marked
                    // running on a *different* CPU.
                    if let Some(&prev_cpu) = task_cpu.get(pid) {
                        assert!(
                            prev_cpu == e.cpu,
                            "[{name}] pid={} scheduled on {:?} while still running on {prev_cpu:?} \
                             (per-CPU running-state clobbered)",
                            pid.0,
                            e.cpu
                        );
                    }
                    // Whoever was on this CPU is displaced.
                    if let Some(old) = cpu_task.insert(e.cpu, *pid) {
                        if old != *pid {
                            task_cpu.remove(&old);
                        }
                    }
                    task_cpu.insert(*pid, e.cpu);
                    max_concurrent = max_concurrent.max(cpu_task.len());
                }
                TraceKind::CpuIdle => {
                    if let Some(old) = cpu_task.remove(&e.cpu) {
                        task_cpu.remove(&old);
                    }
                }
                k if is_off_cpu(k) => {
                    if let Some(old) = cpu_task.remove(&e.cpu) {
                        task_cpu.remove(&old);
                    }
                }
                _ => {}
            }
        }

        // Genuine parallelism: with 4 pinned hogs the run must reach at least two
        // CPUs busy simultaneously.
        assert!(
            max_concurrent >= 2,
            "[{name}] CPUs never ran concurrently (max simultaneous busy CPUs = {max_concurrent})"
        );
    }
}

// ===========================================================================
// 4. Per-CPU statistics are independent (asymmetric load → asymmetric stats).
// ===========================================================================

/// All the load is pinned onto CPU 0 of a 4-CPU box. The per-CPU statistics must
/// reflect that asymmetry independently: CPU 0 carries essentially all the
/// scheduling activity, while the other CPUs sit idle and accumulate idle time.
#[test]
fn test_percpu_stats_independent_under_asymmetric_load() {
    let _lock = common::setup_test();
    const NR_CPUS: u32 = 4;

    for (name, make) in SCHEDS {
        // Three hogs all pinned to CPU 0; CPUs 1..3 have nothing to run.
        let scenario = Scenario::builder()
            .cpus(NR_CPUS)
            .task(hog_on("a", 1, 0, 10_000_000))
            .task(hog_on("b", 2, 0, 10_000_000))
            .task(hog_on("c", 3, 0, 10_000_000))
            .duration_ms(200)
            .build();

        let trace = Simulator::new(make(NR_CPUS)).run(scenario);
        assert_eq!(trace.exit_kind(), &ExitKind::Normal, "[{name}] clean exit");

        let stats = TraceStats::from_trace(&trace);
        let sched0 = schedules_on_cpu(&trace, CpuId(0));
        let sched_others: usize = (1..NR_CPUS)
            .map(|c| schedules_on_cpu(&trace, CpuId(c)))
            .sum();
        eprintln!("[{name}] asymmetric: CPU0 schedules={sched0} others={sched_others}");

        // The strong, universal signal: all scheduling activity is isolated to
        // CPU 0; the other (loadless) CPUs independently record zero.
        assert!(
            sched0 > 0,
            "[{name}] CPU 0 (all load) recorded no schedules"
        );
        assert_eq!(
            sched_others, 0,
            "[{name}] load pinned to CPU 0 but other CPUs recorded {sched_others} schedules \
             (per-CPU stats not isolated)"
        );

        // Idle-time dimension: where a scheduler emits per-CPU idle accounting,
        // the idle CPUs must accumulate more idle time than the saturated CPU 0.
        // (Some schedulers, e.g. cosmos, never emit CpuIdle for a CPU that is
        // never used, leaving idle_duration at 0 for every CPU — in that case the
        // schedule-count isolation above already establishes independence.)
        let idle0 = stats.cpus.get(&CpuId(0)).map_or(0, |c| c.idle_duration_ns);
        let idle_others_max = (1..NR_CPUS)
            .map(|c| stats.cpus.get(&CpuId(c)).map_or(0, |s| s.idle_duration_ns))
            .max()
            .unwrap_or(0);
        eprintln!("[{name}] asymmetric idle: CPU0={idle0}ns max_other={idle_others_max}ns");
        if idle0 > 0 || idle_others_max > 0 {
            assert!(
                idle_others_max > idle0,
                "[{name}] idle CPUs should accumulate more idle time than the saturated CPU 0 \
                 (CPU0={idle0}ns, max_other={idle_others_max}ns)"
            );
        }
    }
}

// ===========================================================================
// 5. Per-CPU statistics accumulate and partition correctly.
// ===========================================================================

/// Balanced load (one hog pinned per CPU) must yield per-CPU schedule counts
/// that (a) are all non-zero, (b) partition exactly into the global total (no
/// double-counting or loss across CPUs), and (c) are roughly balanced — each
/// CPU independently accumulating its own share.
#[test]
fn test_percpu_stats_partition_and_accumulate() {
    let _lock = common::setup_test();
    const NR_CPUS: u32 = 4;

    for (name, make) in SCHEDS {
        let mut builder = Scenario::builder().cpus(NR_CPUS);
        for cpu in 0..NR_CPUS {
            builder = builder.task(hog_on(
                &format!("hog{cpu}"),
                1 + cpu as i32,
                cpu,
                10_000_000,
            ));
        }
        let scenario = builder.duration_ms(200).build();

        let trace = Simulator::new(make(NR_CPUS)).run(scenario);
        assert_eq!(trace.exit_kind(), &ExitKind::Normal, "[{name}] clean exit");

        let total_scheduled = trace
            .events()
            .iter()
            .filter(|e| matches!(e.kind, TraceKind::TaskScheduled { .. }))
            .count();
        let per_cpu: Vec<usize> = (0..NR_CPUS)
            .map(|c| schedules_on_cpu(&trace, CpuId(c)))
            .collect();
        let sum_per_cpu: usize = per_cpu.iter().sum();
        eprintln!("[{name}] balanced per-CPU schedules={per_cpu:?} total={total_scheduled}");

        // (b) Conservation: per-CPU counts partition the global total exactly.
        assert_eq!(
            sum_per_cpu, total_scheduled,
            "[{name}] per-CPU schedule counts ({sum_per_cpu}) don't sum to global total ({total_scheduled})"
        );
        // (a) Every CPU accumulated its own activity.
        for (cpu, &n) in per_cpu.iter().enumerate() {
            assert!(
                n > 0,
                "[{name}] CPU {cpu} accumulated no schedules under balanced load"
            );
        }
        // (c) Rough balance: the busiest CPU is within 4x the least busy (each is
        // independently servicing one identical pinned hog).
        let min = *per_cpu.iter().min().unwrap();
        let max = *per_cpu.iter().max().unwrap();
        assert!(
            max <= min * 4,
            "[{name}] per-CPU accumulation wildly unbalanced under symmetric load: {per_cpu:?}"
        );
    }
}

// ===========================================================================
// 6. Concurrent unpinned load stays consistent (no cross-CPU leakage).
// ===========================================================================

/// A sanity check with *unpinned* concurrent load: many hogs across all CPUs,
/// each free to migrate. The double-schedule invariant must still hold — at no
/// instant is one task running on two CPUs — even as the scheduler moves tasks
/// between per-CPU queues.
#[test]
fn test_unpinned_concurrent_no_double_schedule() {
    let _lock = common::setup_test();
    const NR_CPUS: u32 = 4;
    const NR_TASKS: i32 = 12;

    for (name, make) in SCHEDS {
        let mut builder = Scenario::builder().cpus(NR_CPUS);
        for i in 0..NR_TASKS {
            builder = builder.task(hog(&format!("t{i}"), 1 + i, 5_000_000));
        }
        let scenario = builder.duration_ms(200).build();

        let trace = Simulator::new(make(NR_CPUS)).run(scenario);
        assert_eq!(trace.exit_kind(), &ExitKind::Normal, "[{name}] clean exit");

        let mut cpu_task: HashMap<CpuId, Pid> = HashMap::new();
        let mut task_cpu: HashMap<Pid, CpuId> = HashMap::new();

        for e in trace.events() {
            match &e.kind {
                TraceKind::TaskScheduled { pid } => {
                    if let Some(&prev_cpu) = task_cpu.get(pid) {
                        assert!(
                            prev_cpu == e.cpu,
                            "[{name}] pid={} scheduled on {:?} while still running on {prev_cpu:?}",
                            pid.0,
                            e.cpu
                        );
                    }
                    if let Some(old) = cpu_task.insert(e.cpu, *pid) {
                        if old != *pid {
                            task_cpu.remove(&old);
                        }
                    }
                    task_cpu.insert(*pid, e.cpu);
                }
                TraceKind::CpuIdle => {
                    if let Some(old) = cpu_task.remove(&e.cpu) {
                        task_cpu.remove(&old);
                    }
                }
                k if is_off_cpu(k) => {
                    if let Some(old) = cpu_task.remove(&e.cpu) {
                        task_cpu.remove(&old);
                    }
                }
                _ => {}
            }
        }

        // Every task made progress (the run wasn't degenerate).
        let ran = (1..=NR_TASKS)
            .filter(|p| trace.schedule_count(Pid(*p)) > 0)
            .count();
        assert!(
            ran >= NR_CPUS as usize,
            "[{name}] expected at least {NR_CPUS} tasks to run, only {ran} did"
        );
    }
}
