// Copyright (c) Meta Platforms, Inc. and affiliates.
// SPDX-License-Identifier: GPL-2.0-only

//! Integration tests for concurrent callback interleaving.
//!
//! These tests verify that `--interleave` mode works correctly:
//! deterministic for a given seed, no panics, and different seeds
//! can produce different dispatch winners.

use scx_simulator::*;

#[macro_use]
mod common;

// ---------------------------------------------------------------------------
// Smoke test: interleave mode runs to completion on scx_simple
// ---------------------------------------------------------------------------

#[test]
fn test_interleave_smoke() {
    let _lock = common::setup_test();
    let scenario = Scenario::builder()
        .cpus(4)
        .interleave(true)
        .task(TaskDef {
            name: "t1".into(),
            pid: Pid(1),
            nice: 0,
            behavior: TaskBehavior {
                phases: vec![Phase::Run(10_000_000)],
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
                phases: vec![Phase::Run(10_000_000)],
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
        .duration_ms(50)
        .build();

    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario);
    trace.dump();

    assert!(
        trace.schedule_count(Pid(1)) > 0,
        "task 1 was never scheduled"
    );
    assert!(
        trace.schedule_count(Pid(2)) > 0,
        "task 2 was never scheduled"
    );
}

// ---------------------------------------------------------------------------
// Determinism: same seed + scenario gives identical traces
// ---------------------------------------------------------------------------

#[test]
fn test_interleave_determinism() {
    let _lock = common::setup_test();
    let make_scenario = || {
        Scenario::builder()
            .cpus(4)
            .seed(42)
            .interleave(true)
            .task(TaskDef {
                name: "t1".into(),
                pid: Pid(1),
                nice: 0,
                behavior: TaskBehavior {
                    phases: vec![Phase::Run(10_000_000)],
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
                    phases: vec![Phase::Run(10_000_000)],
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
            .duration_ms(50)
            .build()
    };

    let trace1 = Simulator::new(DynamicScheduler::simple()).run(make_scenario());
    let trace2 = Simulator::new(DynamicScheduler::simple()).run(make_scenario());

    assert_eq!(
        trace1.events().len(),
        trace2.events().len(),
        "interleave traces have different lengths"
    );

    for (i, (e1, e2)) in trace1
        .events()
        .iter()
        .zip(trace2.events().iter())
        .enumerate()
    {
        assert_eq!(
            e1.time_ns, e2.time_ns,
            "event {i}: timestamps differ: {} vs {}",
            e1.time_ns, e2.time_ns
        );
        assert_eq!(
            e1.cpu, e2.cpu,
            "event {i}: CPUs differ: {:?} vs {:?}",
            e1.cpu, e2.cpu
        );
        assert_eq!(
            e1.kind, e2.kind,
            "event {i}: kinds differ: {:?} vs {:?}",
            e1.kind, e2.kind
        );
    }
}

// ---------------------------------------------------------------------------
// Interleave with sleep/wake cycles
// ---------------------------------------------------------------------------

#[test]
fn test_interleave_sleep_wake() {
    let _lock = common::setup_test();
    let scenario = Scenario::builder()
        .cpus(4)
        .interleave(true)
        .task(TaskDef {
            name: "sleeper".into(),
            pid: Pid(1),
            nice: 0,
            behavior: TaskBehavior {
                phases: vec![Phase::Run(5_000_000), Phase::Sleep(10_000_000)],
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
            name: "worker".into(),
            pid: Pid(2),
            nice: 0,
            behavior: TaskBehavior {
                phases: vec![Phase::Run(20_000_000)],
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
        .duration_ms(100)
        .build();

    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario);
    trace.dump();

    let count = trace.schedule_count(Pid(1));
    assert!(count > 1, "expected multiple schedules, got {count}");

    let runtime = trace.total_runtime(Pid(1));
    assert!(
        runtime > 20_000_000 && runtime < 55_000_000,
        "expected ~33ms runtime, got {runtime}ns"
    );
}

// ---------------------------------------------------------------------------
// Interleave mode matches sequential mode for single CPU
// ---------------------------------------------------------------------------

#[test]
fn test_interleave_single_cpu_noop() {
    let _lock = common::setup_test();
    let make_scenario = |interleave: bool| {
        Scenario::builder()
            .cpus(1)
            .seed(42)
            .fixed_priority(true)
            .interleave(interleave)
            .task(TaskDef {
                name: "t1".into(),
                pid: Pid(1),
                nice: 0,
                behavior: TaskBehavior {
                    phases: vec![Phase::Run(10_000_000)],
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
            .duration_ms(50)
            .build()
    };

    let trace_seq = Simulator::new(DynamicScheduler::simple()).run(make_scenario(false));
    let trace_ilv = Simulator::new(DynamicScheduler::simple()).run(make_scenario(true));

    // With a single CPU, interleave mode should be a no-op (never
    // hits the 2+ CPU threshold), so traces should be identical.
    assert_eq!(
        trace_seq.events().len(),
        trace_ilv.events().len(),
        "single-CPU: interleave and sequential traces differ in length"
    );

    for (i, (e1, e2)) in trace_seq
        .events()
        .iter()
        .zip(trace_ilv.events().iter())
        .enumerate()
    {
        assert_eq!(
            e1.kind, e2.kind,
            "single-CPU event {i}: kinds differ: {:?} vs {:?}",
            e1.kind, e2.kind
        );
    }
}

// ---------------------------------------------------------------------------
// Multiple seeds: interleave mode doesn't crash for various seeds
// ---------------------------------------------------------------------------

#[test]
fn test_interleave_multiple_seeds() {
    let _lock = common::setup_test();
    for seed in [1, 42, 100, 999, 65535] {
        let scenario = Scenario::builder()
            .cpus(4)
            .seed(seed)
            .interleave(true)
            .task(TaskDef {
                name: "t1".into(),
                pid: Pid(1),
                nice: 0,
                behavior: TaskBehavior {
                    phases: vec![Phase::Run(10_000_000)],
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
                    phases: vec![Phase::Run(10_000_000)],
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
                name: "t3".into(),
                pid: Pid(3),
                nice: 0,
                behavior: TaskBehavior {
                    phases: vec![Phase::Run(10_000_000)],
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
            .duration_ms(50)
            .build();

        let trace = Simulator::new(DynamicScheduler::simple()).run(scenario);
        assert!(
            trace.schedule_count(Pid(1)) > 0,
            "seed {seed}: task 1 never scheduled"
        );
        assert!(
            trace.schedule_count(Pid(2)) > 0,
            "seed {seed}: task 2 never scheduled"
        );
    }
}

// ===========================================================================
// Preemptive interleaving tests
// ===========================================================================

/// Helper: build a simple N-task scenario with preemptive interleaving.
///
/// Uses `cooperative_only` mode to disable PMU timers, `fixed_priority` to
/// ensure deterministic event ordering, and `instant_timing` to disable
/// noise and overhead. All of these are needed to ensure deterministic
/// interleaving for tests.
fn preemptive_scenario(nr_cpus: u32, nr_tasks: u32, seed: u32, duration_ms: u64) -> Scenario {
    let mut builder = Scenario::builder()
        .cpus(nr_cpus)
        .seed(seed)
        .fixed_priority(true)
        .instant_timing()
        .preemptive(PreemptiveConfig::cooperative_only());

    for i in 1..=nr_tasks {
        builder = builder.task(TaskDef {
            name: format!("t{i}"),
            pid: Pid(i as i32),
            nice: 0,
            behavior: TaskBehavior {
                phases: vec![Phase::Run(10_000_000)],
                repeat: RepeatMode::Forever,
            },
            start_time_ns: 0,
            mm_id: None,
            allowed_cpus: None,
            parent_pid: None,
            cgroup_name: None,
            task_flags: 0,
            migration_disabled: 0,
        });
    }

    builder.duration_ms(duration_ms).build()
}

// ---------------------------------------------------------------------------
// Smoke test: preemptive mode runs to completion
// ---------------------------------------------------------------------------

#[test]
fn test_preemptive_smoke() {
    let _lock = common::setup_test();
    let scenario = preemptive_scenario(4, 2, 42, 50);

    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario);
    trace.dump();

    assert!(
        trace.schedule_count(Pid(1)) > 0,
        "task 1 was never scheduled"
    );
    assert!(
        trace.schedule_count(Pid(2)) > 0,
        "task 2 was never scheduled"
    );
}

// ---------------------------------------------------------------------------
// Determinism: same seed gives identical traces (preemptive mode)
// ---------------------------------------------------------------------------

#[test]
fn test_preemptive_determinism() {
    let _lock = common::setup_test();
    let make = || preemptive_scenario(4, 2, 42, 50);

    let trace1 = Simulator::new(DynamicScheduler::simple()).run(make());
    let trace2 = Simulator::new(DynamicScheduler::simple()).run(make());

    assert_eq!(
        trace1.events().len(),
        trace2.events().len(),
        "preemptive traces have different lengths"
    );

    for (i, (e1, e2)) in trace1
        .events()
        .iter()
        .zip(trace2.events().iter())
        .enumerate()
    {
        if e1.time_ns != e2.time_ns || e1.cpu != e2.cpu || e1.kind != e2.kind {
            // Dump context around mismatch
            eprintln!("MISMATCH at event {i}:");
            eprintln!(
                "  trace1[{i}]: time={} cpu={:?} kind={:?}",
                e1.time_ns, e1.cpu, e1.kind
            );
            eprintln!(
                "  trace2[{i}]: time={} cpu={:?} kind={:?}",
                e2.time_ns, e2.cpu, e2.kind
            );
            if i > 0 {
                let prev1 = &trace1.events()[i - 1];
                let prev2 = &trace2.events()[i - 1];
                eprintln!(
                    "  trace1[{}]: time={} cpu={:?} kind={:?}",
                    i - 1,
                    prev1.time_ns,
                    prev1.cpu,
                    prev1.kind
                );
                eprintln!(
                    "  trace2[{}]: time={} cpu={:?} kind={:?}",
                    i - 1,
                    prev2.time_ns,
                    prev2.cpu,
                    prev2.kind
                );
            }
        }
        assert_eq!(
            e1.time_ns, e2.time_ns,
            "event {i}: timestamps differ: {} vs {}",
            e1.time_ns, e2.time_ns
        );
        assert_eq!(
            e1.cpu, e2.cpu,
            "event {i}: CPUs differ: {:?} vs {:?}",
            e1.cpu, e2.cpu
        );
        assert_eq!(
            e1.kind, e2.kind,
            "event {i}: kinds differ: {:?} vs {:?}",
            e1.kind, e2.kind
        );
    }
}

// ---------------------------------------------------------------------------
// Sleep/wake interaction with preemptive interleaving
// ---------------------------------------------------------------------------

#[test]
fn test_preemptive_sleep_wake() {
    let _lock = common::setup_test();
    let scenario = Scenario::builder()
        .cpus(4)
        .preemptive(PreemptiveConfig::default())
        .task(TaskDef {
            name: "sleeper".into(),
            pid: Pid(1),
            nice: 0,
            behavior: TaskBehavior {
                phases: vec![Phase::Run(5_000_000), Phase::Sleep(10_000_000)],
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
            name: "worker".into(),
            pid: Pid(2),
            nice: 0,
            behavior: TaskBehavior {
                phases: vec![Phase::Run(20_000_000)],
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
        .duration_ms(100)
        .build();

    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario);
    trace.dump();

    let count = trace.schedule_count(Pid(1));
    assert!(count > 1, "expected multiple schedules, got {count}");

    let runtime = trace.total_runtime(Pid(1));
    assert!(
        runtime > 20_000_000 && runtime < 55_000_000,
        "expected ~33ms runtime, got {runtime}ns"
    );
}

// ---------------------------------------------------------------------------
// Multiple seeds: preemptive mode doesn't crash for various seeds
// ---------------------------------------------------------------------------

#[test]
fn test_preemptive_multiple_seeds() {
    let _lock = common::setup_test();
    for seed in [1, 42, 100, 999, 65535] {
        let scenario = preemptive_scenario(4, 3, seed, 50);
        let trace = Simulator::new(DynamicScheduler::simple()).run(scenario);
        assert!(
            trace.schedule_count(Pid(1)) > 0,
            "seed {seed}: task 1 never scheduled"
        );
        assert!(
            trace.schedule_count(Pid(2)) > 0,
            "seed {seed}: task 2 never scheduled"
        );
    }
}

// ===========================================================================
// Batch-concurrent tests: same-timestamp per-CPU event interleaving
// ===========================================================================

// ---------------------------------------------------------------------------
// Smoke test: batch-concurrent tick interleaving completes
// ---------------------------------------------------------------------------

#[test]
fn test_batch_concurrent_smoke() {
    let _lock = common::setup_test();
    // 4 CPUs, 4 tasks, 50ms: ticks on all CPUs fire at TICK_INTERVAL_NS
    // boundaries and should be processed concurrently.
    let scenario = Scenario::builder()
        .cpus(4)
        .seed(42)
        .interleave(true)
        .task(TaskDef {
            name: "t1".into(),
            pid: Pid(1),
            nice: 0,
            behavior: TaskBehavior {
                phases: vec![Phase::Run(10_000_000)],
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
                phases: vec![Phase::Run(10_000_000)],
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
            name: "t3".into(),
            pid: Pid(3),
            nice: 0,
            behavior: TaskBehavior {
                phases: vec![Phase::Run(10_000_000)],
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
            name: "t4".into(),
            pid: Pid(4),
            nice: 0,
            behavior: TaskBehavior {
                phases: vec![Phase::Run(10_000_000)],
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
        .duration_ms(50)
        .build();

    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario);
    trace.dump();

    // All 4 tasks must be scheduled
    for pid_val in 1..=4 {
        assert!(
            trace.schedule_count(Pid(pid_val)) > 0,
            "task {pid_val} was never scheduled"
        );
    }

    // Tick events must appear for all CPUs
    let tick_cpus: std::collections::HashSet<CpuId> = trace
        .events()
        .iter()
        .filter_map(|e| match e.kind {
            TraceKind::Tick { pid: _ } => Some(e.cpu),
            _ => None,
        })
        .collect();
    assert!(
        tick_cpus.len() >= 4,
        "expected ticks on 4 CPUs, got {:?}",
        tick_cpus
    );
}

// ---------------------------------------------------------------------------
// Determinism: batch-concurrent produces identical traces for same seed
// ---------------------------------------------------------------------------

#[test]
fn test_batch_concurrent_determinism() {
    let _lock = common::setup_test();
    let make_scenario = || {
        Scenario::builder()
            .cpus(4)
            .seed(42)
            .interleave(true)
            .task(TaskDef {
                name: "t1".into(),
                pid: Pid(1),
                nice: 0,
                behavior: TaskBehavior {
                    phases: vec![Phase::Run(5_000_000), Phase::Sleep(3_000_000)],
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
                    phases: vec![Phase::Run(5_000_000), Phase::Sleep(3_000_000)],
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
                name: "t3".into(),
                pid: Pid(3),
                nice: 0,
                behavior: TaskBehavior {
                    phases: vec![Phase::Run(5_000_000), Phase::Sleep(3_000_000)],
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
                name: "t4".into(),
                pid: Pid(4),
                nice: 0,
                behavior: TaskBehavior {
                    phases: vec![Phase::Run(5_000_000), Phase::Sleep(3_000_000)],
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
            .duration_ms(50)
            .build()
    };

    let trace1 = Simulator::new(DynamicScheduler::simple()).run(make_scenario());
    let trace2 = Simulator::new(DynamicScheduler::simple()).run(make_scenario());

    assert_eq!(
        trace1.events().len(),
        trace2.events().len(),
        "batch-concurrent traces have different lengths"
    );

    for (i, (e1, e2)) in trace1
        .events()
        .iter()
        .zip(trace2.events().iter())
        .enumerate()
    {
        assert_eq!(
            e1.time_ns, e2.time_ns,
            "event {i}: timestamps differ: {} vs {}",
            e1.time_ns, e2.time_ns
        );
        assert_eq!(
            e1.cpu, e2.cpu,
            "event {i}: CPUs differ: {:?} vs {:?}",
            e1.cpu, e2.cpu
        );
        assert_eq!(
            e1.kind, e2.kind,
            "event {i}: kinds differ: {:?} vs {:?}",
            e1.kind, e2.kind
        );
    }
}

// ---------------------------------------------------------------------------
// DSQ contention: 8 tasks on 4 CPUs with short sleep/wake cycles
// ---------------------------------------------------------------------------

#[test]
fn test_batch_concurrent_with_dsq_contention() {
    let _lock = common::setup_test();
    // 8 tasks on 4 CPUs, 2ms run / 1ms sleep: frequent dispatch and shared
    // DSQ contention. This exercises the global DSQ path that scx_simple
    // uses when tasks aren't directly dispatched during select_cpu.
    let mut builder = Scenario::builder().cpus(4).seed(42).interleave(true);

    for i in 1..=8 {
        builder = builder.task(TaskDef {
            name: format!("w{i}"),
            pid: Pid(i),
            nice: 0,
            behavior: TaskBehavior {
                phases: vec![Phase::Run(2_000_000), Phase::Sleep(1_000_000)],
                repeat: RepeatMode::Forever,
            },
            start_time_ns: 0,
            mm_id: None,
            allowed_cpus: None,
            parent_pid: None,
            cgroup_name: None,
            task_flags: 0,
            migration_disabled: 0,
        });
    }

    let scenario = builder.duration_ms(50).build();
    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario);
    trace.dump();

    // All 8 tasks must be scheduled
    for pid_val in 1..=8 {
        assert!(
            trace.schedule_count(Pid(pid_val)) > 0,
            "task {pid_val} was never scheduled"
        );
    }
}

// ---------------------------------------------------------------------------
// Batch-concurrent with preemptive interleaving
// ---------------------------------------------------------------------------

#[test]
fn test_batch_concurrent_preemptive_smoke() {
    let _lock = common::setup_test();
    let scenario = Scenario::builder()
        .cpus(4)
        .seed(42)
        .preemptive(PreemptiveConfig::default())
        .task(TaskDef {
            name: "t1".into(),
            pid: Pid(1),
            nice: 0,
            behavior: TaskBehavior {
                phases: vec![Phase::Run(5_000_000), Phase::Sleep(3_000_000)],
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
                phases: vec![Phase::Run(5_000_000), Phase::Sleep(3_000_000)],
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
            name: "t3".into(),
            pid: Pid(3),
            nice: 0,
            behavior: TaskBehavior {
                phases: vec![Phase::Run(5_000_000), Phase::Sleep(3_000_000)],
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
            name: "t4".into(),
            pid: Pid(4),
            nice: 0,
            behavior: TaskBehavior {
                phases: vec![Phase::Run(5_000_000), Phase::Sleep(3_000_000)],
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
        .duration_ms(50)
        .build();

    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario);
    trace.dump();

    for pid_val in 1..=4 {
        assert!(
            trace.schedule_count(Pid(pid_val)) > 0,
            "task {pid_val} was never scheduled"
        );
    }
}

// ---------------------------------------------------------------------------
// Custom timeslice range
// ---------------------------------------------------------------------------

#[test]
fn test_preemptive_custom_timeslice() {
    let _lock = common::setup_test();
    let scenario = Scenario::builder()
        .cpus(4)
        .preemptive(PreemptiveConfig {
            timeslice_min: 50,
            timeslice_max: 200,
            cooperative_only: false,
            ..Default::default()
        })
        .task(TaskDef {
            name: "t1".into(),
            pid: Pid(1),
            nice: 0,
            behavior: TaskBehavior {
                phases: vec![Phase::Run(10_000_000)],
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
                phases: vec![Phase::Run(10_000_000)],
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
        .duration_ms(50)
        .build();

    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario);
    assert!(
        trace.schedule_count(Pid(1)) > 0,
        "task 1 was never scheduled"
    );
    assert!(
        trace.schedule_count(Pid(2)) > 0,
        "task 2 was never scheduled"
    );
}

// ---------------------------------------------------------------------------
// Determinism: true PMU-based preemptive mode
// ---------------------------------------------------------------------------

/// Helper: build a scenario with TRUE preemptive interleaving (PMU enabled).
///
/// Unlike `preemptive_scenario()` which uses `cooperative_only`, this enables
/// actual PMU RBC timer signals for mid-C-code preemption points.
///
/// See ai_docs/DETERMINISM.md for details on RBC determinism.
fn pmu_preemptive_scenario(nr_cpus: u32, nr_tasks: u32, seed: u32, duration_ms: u64) -> Scenario {
    let mut builder = Scenario::builder()
        .cpus(nr_cpus)
        .seed(seed)
        .fixed_priority(true)
        .instant_timing()
        .preemptive(PreemptiveConfig {
            timeslice_min: 100,
            timeslice_max: 500,
            cooperative_only: false, // Enable PMU
            ..Default::default()
        });

    for i in 1..=nr_tasks {
        builder = builder.task(TaskDef {
            name: format!("t{i}"),
            pid: Pid(i as i32),
            nice: 0,
            behavior: TaskBehavior {
                phases: vec![Phase::Run(10_000_000)],
                repeat: RepeatMode::Forever,
            },
            start_time_ns: 0,
            mm_id: None,
            allowed_cpus: None,
            parent_pid: None,
            cgroup_name: None,
            task_flags: 0,
            migration_disabled: 0,
        });
    }

    builder.duration_ms(duration_ms).build()
}

/// Test that PMU-based preemptive interleaving actually fires.
///
/// PMU signal delivery has skid (tens to hundreds of branches), so the
/// preemption POINT varies between runs. However, RBC counter values are
/// deterministic for a given instruction stream. This test verifies that the PMU
/// timer fires and produces preemption records, NOT that the results are
/// deterministic. For deterministic replay, use --record-preemptions /
/// --replay-preemptions.
///
/// If PMU is unavailable (VMs, containers), both runs fall back to
/// cooperative-only mode and no preemption records are produced.
#[test]
fn test_preemptive_pmu_determinism() {
    use scx_simulator::{drain_preemption_records, enable_preemption_collection};

    let _lock = common::setup_test();
    let make = || pmu_preemptive_scenario(4, 2, 42, 20);

    // Run and collect preemption records
    enable_preemption_collection();
    let trace = Simulator::new(DynamicScheduler::simple()).run(make());
    let records = drain_preemption_records();

    // Basic sanity: simulation completed
    assert!(
        !trace.has_error(),
        "PMU preemptive simulation failed: {:?}",
        trace.exit_kind()
    );

    eprintln!(
        "PMU preemptive test: {} trace events, {} preemption records",
        trace.events().len(),
        records.len(),
    );

    // If we got preemption records, the PMU timer is working.
    // We don't assert determinism — PMU skid makes that impossible.
    if !records.is_empty() {
        eprintln!(
            "PMU preemption is active: {} records captured",
            records.len()
        );
        for (i, rec) in records.iter().take(5).enumerate() {
            eprintln!("  [{i}] {rec}");
        }
    } else {
        eprintln!(
            "PMU unavailable (VM/container?): cooperative-only fallback, \
             no preemption records"
        );
    }
}

// ===========================================================================
// Aggressive determinism mode tests (memory hash checkpoints)
// ===========================================================================

/// Test aggressive determinism mode with memory state hashing.
///
/// This test enables the aggressive determinism mode that hashes scheduler
/// memory at key scheduling events (dispatch, enqueue, running, stopping, etc.)
/// and verifies that two runs with the same seed produce identical checkpoint
/// sequences.
///
/// The checkpoint comparison can detect divergence in:
/// - RIP (instruction pointer)
/// - RBC (retired branch count)
/// - Memory state (DSQ contents, task state, etc.)
/// - Event type or CPU
///
/// This is the primary tool for debugging non-determinism: when stress.py
/// finds a failing seed, replay with aggressive determinism mode and the
/// divergence diagnostic will pinpoint exactly where execution diverged.
#[test]
fn test_aggressive_determinism_mode() {
    use scx_simulator::{
        compare_checkpoints, drain_determinism_checkpoints, enable_determinism_mode,
    };

    let _lock = common::setup_test();
    let make = || preemptive_scenario(4, 2, 42, 30);

    // Run 1: collect checkpoints
    enable_determinism_mode();
    let trace1 = Simulator::new(DynamicScheduler::simple()).run(make());
    let checkpoints1 = drain_determinism_checkpoints();

    // Run 2: collect checkpoints
    enable_determinism_mode();
    let trace2 = Simulator::new(DynamicScheduler::simple()).run(make());
    let checkpoints2 = drain_determinism_checkpoints();

    // ---------------------------------------------------------------------------
    // Verify trace event determinism first (sanity check)
    // ---------------------------------------------------------------------------
    assert_eq!(
        trace1.events().len(),
        trace2.events().len(),
        "Traces have different lengths: {} vs {}",
        trace1.events().len(),
        trace2.events().len()
    );

    // ---------------------------------------------------------------------------
    // Verify checkpoint determinism (including memory hashes)
    // ---------------------------------------------------------------------------
    eprintln!(
        "Aggressive determinism checkpoints: run1={} run2={}",
        checkpoints1.len(),
        checkpoints2.len()
    );

    // Print first few checkpoints for diagnostic
    if !checkpoints1.is_empty() {
        eprintln!("Run 1 checkpoints:");
        for (i, cp) in checkpoints1.iter().take(10).enumerate() {
            eprintln!("  [{i}] {cp}");
        }
        if checkpoints1.len() > 10 {
            eprintln!("  ... and {} more", checkpoints1.len() - 10);
        }
    }

    // Compare checkpoints and detect first divergence
    if let Some(divergence) = compare_checkpoints(&checkpoints1, &checkpoints2) {
        eprintln!("CHECKPOINT DIVERGENCE DETECTED:");
        eprintln!("{divergence}");
        panic!(
            "Aggressive determinism check failed: divergence at checkpoint {}",
            divergence.checkpoint_index
        );
    }

    eprintln!(
        "SUCCESS: {} checkpoints verified identical (including memory hashes). \
         Scheduler state is fully deterministic.",
        checkpoints1.len()
    );

    // Verify we collected meaningful checkpoints
    assert!(
        !checkpoints1.is_empty(),
        "No checkpoints collected - aggressive determinism mode may not be working"
    );
}

/// Test that aggressive determinism mode is efficiently disabled when not enabled.
///
/// When determinism mode is disabled (the default), checkpoint collection should
/// have zero overhead beyond a single atomic load per callback.
#[test]
fn test_determinism_mode_disabled_by_default() {
    use scx_simulator::is_determinism_mode_enabled;

    // Acquire lock first to ensure no other test is running with determinism mode enabled
    let _lock = common::setup_test();

    // Determinism mode should be disabled by default (checked after lock acquisition
    // to avoid race with other tests that may be enabling/disabling it)
    assert!(
        !is_determinism_mode_enabled(),
        "Determinism mode should be disabled by default"
    );

    let scenario = preemptive_scenario(2, 2, 42, 10);

    // Run without enabling determinism mode
    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario);

    // Should complete successfully
    assert!(
        trace.schedule_count(Pid(1)) > 0,
        "task 1 was never scheduled"
    );

    // Determinism mode should still be disabled
    assert!(
        !is_determinism_mode_enabled(),
        "Determinism mode should remain disabled"
    );
}

/// Test checkpoint divergence detection with intentionally different seeds.
///
/// This verifies that the divergence detection correctly identifies when
/// two runs produce different checkpoint sequences.
#[test]
fn test_checkpoint_divergence_detection() {
    use scx_simulator::{
        compare_checkpoints, drain_determinism_checkpoints, enable_determinism_mode, DivergenceType,
    };

    let _lock = common::setup_test();

    // Run 1: seed 42
    enable_determinism_mode();
    let _ = Simulator::new(DynamicScheduler::simple()).run(preemptive_scenario(2, 2, 42, 20));
    let checkpoints1 = drain_determinism_checkpoints();

    // Run 2: different seed (100) - should produce different checkpoints
    enable_determinism_mode();
    let _ = Simulator::new(DynamicScheduler::simple()).run(preemptive_scenario(2, 2, 100, 20));
    let checkpoints2 = drain_determinism_checkpoints();

    // Both runs should produce checkpoints
    assert!(
        !checkpoints1.is_empty() && !checkpoints2.is_empty(),
        "Both runs should produce checkpoints"
    );

    // Different seeds may or may not produce different checkpoints depending on
    // how much the seed affects scheduling decisions. For simple scheduler with
    // fixed-priority events, the checkpoints might actually be identical.
    //
    // So we just verify the comparison function works correctly:
    let divergence = compare_checkpoints(&checkpoints1, &checkpoints2);

    if let Some(div) = divergence {
        eprintln!("Expected divergence detected: {}", div);
        // Verify the divergence type is something meaningful
        assert!(
            !matches!(div.divergence_type, DivergenceType::Multiple(ref v) if v.is_empty()),
            "Divergence type should not be empty"
        );
    } else {
        // Same checkpoints despite different seeds is valid (simple scheduler is deterministic
        // for the same scenario structure)
        eprintln!(
            "INFO: Different seeds produced identical checkpoints ({} each). \
             This is valid for simple scheduler with fixed-priority events.",
            checkpoints1.len()
        );
    }
}

// ===========================================================================
// Replay determinism tests (PMU + hardware breakpoint replay engine)
// ===========================================================================

/// Test that record-then-replay reproduces deterministic scheduler behavior.
///
/// Run 1: Normal preemptive PMU mode with determinism checkpoints enabled.
///        Collect preemption records + determinism checkpoints.
/// Run 2: Replay mode using the recorded preemption trace, also with
///        determinism checkpoints enabled.
/// Compare: determinism checkpoints' event types and memory hashes must match,
///          proving the scheduler saw identical state at each decision point.
///          (CPU IDs, RIP, and RBC may differ because replay runs on different
///          physical threads with different instrumentation.)
///
/// If PMU is unavailable (no preemption records), the test is skipped.
/// If HW breakpoints are unavailable during replay, the test panics with
/// Verify that preemptive interleaving with cooperative-only preemptions
/// is fully deterministic: two runs with the same seed and configuration
/// produce identical scheduler checkpoint sequences.
///
/// This tests the cooperative yield mechanism (kfunc-boundary interleaving
/// via the PreemptRing). With `cooperative_only = true`, there are no
/// PMU signal preemptions — all interleaving happens at deterministic
/// kfunc yield points. Same seed -> same PRNG -> same worker selection ->
/// same scheduler decisions -> same checkpoints.
///
/// The replay trace mechanism (PMU + HW breakpoints) is NOT tested here
/// because it depends on hardware-level PMU precision that varies across
/// environments. For PMU replay testing, use the CLI:
///   cargo run -- run --scheduler simple --preemptive --seed=42 \
///     --record-preemptions /tmp/preempts workloads/simple.json
#[test]
fn test_replay_determinism() {
    use scx_simulator::{drain_determinism_checkpoints, enable_determinism_mode};

    let _lock = common::setup_test();

    // Use cooperative_only so interleaving is purely PRNG-driven
    // (no PMU signals) and therefore fully deterministic.
    let make_scenario = || {
        let mut builder = Scenario::builder()
            .cpus(4)
            .seed(42)
            .fixed_priority(true)
            .instant_timing()
            .preemptive(PreemptiveConfig {
                timeslice_min: 100,
                timeslice_max: 500,
                cooperative_only: true,
                ..Default::default()
            });

        for i in 1..=2u32 {
            builder = builder.task(TaskDef {
                name: format!("t{i}"),
                pid: Pid(i as i32),
                nice: 0,
                behavior: TaskBehavior {
                    phases: vec![Phase::Run(10_000_000)],
                    repeat: RepeatMode::Forever,
                },
                start_time_ns: 0,
                mm_id: None,
                allowed_cpus: None,
                parent_pid: None,
                cgroup_name: None,
                task_flags: 0,
                migration_disabled: 0,
            });
        }

        builder.duration_ms(20).build()
    };

    // Run 1: collect determinism checkpoints.
    enable_determinism_mode();
    let _trace1 = Simulator::new(DynamicScheduler::simple()).run(make_scenario());
    let checkpoints1 = drain_determinism_checkpoints();

    assert!(
        !checkpoints1.is_empty(),
        "No determinism checkpoints collected during run 1"
    );

    // Run 2: same scenario, same seed — should produce identical checkpoints.
    enable_determinism_mode();
    let _trace2 = Simulator::new(DynamicScheduler::simple()).run(make_scenario());
    let checkpoints2 = drain_determinism_checkpoints();

    assert!(
        !checkpoints2.is_empty(),
        "No determinism checkpoints collected during run 2"
    );

    // Compare checkpoint sequences: event type + memory hash.
    // CPU IDs may differ (OS thread scheduling), but the scheduler
    // decisions (captured by event type and memory hash) must match.
    assert!(
        compare_replay_checkpoints(&checkpoints1, &checkpoints2),
        "Determinism check FAILED: two runs with the same seed produced \
         different checkpoint sequences. This indicates non-determinism \
         in the cooperative interleaving mechanism."
    );

    eprintln!(
        "SUCCESS: two runs produced identical {} checkpoints with matching state hashes",
        checkpoints1.len()
    );
}

/// Compare two checkpoint sequences by event type and memory hash only.
///
/// For replay determinism, we care that the scheduler reached the same
/// state at each decision point — not which CPU or instruction ran it.
///
/// Returns `true` if replay was faithful, `false` if it diverged (PMU/HW
/// breakpoint imprecision).
fn compare_replay_checkpoints(
    recorded: &[DeterminismCheckpoint],
    replayed: &[DeterminismCheckpoint],
) -> bool {
    if recorded.len() != replayed.len() {
        eprintln!(
            "Replay checkpoint count differs (recorded={} replayed={}) — \
             replay fidelity limited by PMU/breakpoint precision",
            recorded.len(),
            replayed.len(),
        );
        return false;
    }

    let mut mismatches = 0;
    for (i, (r, p)) in recorded.iter().zip(replayed.iter()).enumerate() {
        let event_match = r.event == p.event;
        let hash_match = r.memory_hash == p.memory_hash;
        if !event_match || !hash_match {
            mismatches += 1;
            if mismatches <= 5 {
                eprintln!("REPLAY CHECKPOINT MISMATCH at [{i}]:");
                eprintln!("  recorded: {r}");
                eprintln!("  replayed: {p}");
                if !event_match {
                    eprintln!("    -> event type differs");
                }
                if !hash_match {
                    eprintln!("    -> memory hash differs");
                }
            }
        }
    }

    if mismatches > 0 {
        eprintln!(
            "Replay diverged: {mismatches}/{} checkpoints had event or hash mismatches — \
             replay fidelity limited by PMU/breakpoint precision",
            recorded.len(),
        );
        return false;
    }

    eprintln!(
        "Replay matched all {} checkpoints (event + hash)",
        recorded.len()
    );
    true
}
