//! Track 3: LAVD stall reproduction with cgroup bandwidth enforcement.
//!
//! These tests attempt to trigger the scheduling stall observed in production
//! WhatsApp hosts where LAVD + cgroup bandwidth throttling causes tasks to
//! become stuck in dispatch queues for extended periods.
//!
//! The key insight: when many bandwidth-throttled tasks are simultaneously
//! unthrottled at a period boundary, the burst of wakeups can overwhelm
//! LAVD's dispatch logic, causing some tasks to never get scheduled.

use scx_simulator::scenario::PreemptiveConfig;
use scx_simulator::task::{Phase, RepeatMode, TaskBehavior};
use scx_simulator::*;

#[macro_use]
mod common;

/// Helper: build a heavy CPU-bound task behavior (95% duty cycle).
fn heavy_worker() -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(9_500_000), Phase::Sleep(500_000)],
        repeat: RepeatMode::Forever,
    }
}

/// Helper: build a medium task behavior (50% duty cycle).
fn medium_worker() -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(5_000_000), Phase::Sleep(5_000_000)],
        repeat: RepeatMode::Forever,
    }
}

/// Attempt 1: Moderate stall scenario.
///
/// 8 CPUs, 2 cgroups:
/// - "tight": 50ms quota / 100ms period (0.5 CPUs for 10 tasks)
/// - "generous": 400ms quota / 100ms period (4 CPUs for 10 tasks)
#[test]
fn test_stall_moderate_oversubscription() {
    let _lock = common::setup_test();
    let all_cpus: Vec<CpuId> = (0..8).map(CpuId).collect();

    let mut builder = Scenario::builder()
        .cpus(8)
        .duration_ms(5_000)
        .watchdog_timeout_ns(Some(4_000_000_000))
        .cgroup_with_bandwidth("tight", &all_cpus, 100_000, 50_000, 0)
        .cgroup_with_bandwidth("generous", &all_cpus, 100_000, 400_000, 0);

    for i in 0..10 {
        builder = builder.add_task_in_cgroup(&format!("tight_{i:02}"), 0, heavy_worker(), "tight");
    }
    for i in 0..10 {
        builder = builder.add_task_in_cgroup(&format!("gen_{i:02}"), 0, heavy_worker(), "generous");
    }

    let scenario = builder.build();
    eprintln!("=== Stall Attempt: Moderate (10+10 tasks, 8 CPUs) ===");
    eprintln!("  tight: 50ms/100ms (0.5 CPUs for 10 tasks)");
    eprintln!("  generous: 400ms/100ms (4 CPUs for 10 tasks)");

    let trace = Simulator::new(DynamicScheduler::lavd(8)).run(scenario);

    match trace.exit_kind() {
        ExitKind::Normal => {
            eprintln!("  Result: COMPLETED NORMALLY (no stall)");
            for i in 0..10 {
                let pid = Pid(i + 1);
                let rt = trace.total_runtime(pid);
                let sched = trace.schedule_count(pid);
                eprintln!(
                    "  tight_{i:02} (pid {}): runtime={}ms, schedules={sched}",
                    pid.0,
                    rt / 1_000_000
                );
            }
            for i in 0..10 {
                let pid = Pid(i + 11);
                let rt = trace.total_runtime(pid);
                let sched = trace.schedule_count(pid);
                eprintln!(
                    "  gen_{i:02} (pid {}): runtime={}ms, schedules={sched}",
                    pid.0,
                    rt / 1_000_000
                );
            }
        }
        ExitKind::ErrorStall {
            pid,
            runnable_for_ns,
        } => {
            eprintln!(
                "  ⚠️ STALL DETECTED! pid={}, stalled for {}ms",
                pid.0,
                runnable_for_ns / 1_000_000
            );
            trace.dump();
        }
        other => {
            eprintln!("  Result: {other:?}");
            trace.dump();
        }
    }
}

/// Attempt 2: Extreme stall scenario.
///
/// 4 CPUs, 2 cgroups:
/// - "starving": 10ms quota / 100ms period (0.1 CPUs for 30 tasks!)
/// - "fed": 200ms quota / 100ms period (2 CPUs for 10 tasks)
#[test]
fn test_stall_extreme_oversubscription() {
    let _lock = common::setup_test();
    let all_cpus: Vec<CpuId> = (0..4).map(CpuId).collect();

    let mut builder = Scenario::builder()
        .cpus(4)
        .duration_ms(3_000)
        .watchdog_timeout_ns(Some(2_000_000_000))
        .cgroup_with_bandwidth("starving", &all_cpus, 100_000, 10_000, 0)
        .cgroup_with_bandwidth("fed", &all_cpus, 100_000, 200_000, 0);

    for i in 0..30 {
        builder =
            builder.add_task_in_cgroup(&format!("starve_{i:02}"), 0, heavy_worker(), "starving");
    }
    for i in 0..10 {
        builder = builder.add_task_in_cgroup(&format!("fed_{i:02}"), 0, medium_worker(), "fed");
    }

    let scenario = builder.build();
    eprintln!("=== Stall Attempt: Extreme (30+10 tasks, 4 CPUs) ===");
    eprintln!("  starving: 10ms/100ms (0.1 CPUs for 30 tasks)");
    eprintln!("  fed: 200ms/100ms (2 CPUs for 10 tasks)");

    let trace = Simulator::new(DynamicScheduler::lavd(4)).run(scenario);

    match trace.exit_kind() {
        ExitKind::Normal => {
            eprintln!("  Result: COMPLETED NORMALLY (no stall)");
            let mut min_sched = usize::MAX;
            let mut min_task = String::new();
            for i in 0..30 {
                let pid = Pid(i + 1);
                let sched = trace.schedule_count(pid);
                if sched < min_sched {
                    min_sched = sched;
                    min_task = format!("starve_{i:02}");
                }
            }
            eprintln!("  Least-scheduled starving task: {min_task} with {min_sched} schedules");
        }
        ExitKind::ErrorStall {
            pid,
            runnable_for_ns,
        } => {
            eprintln!(
                "  ⚠️⚠️⚠️ STALL DETECTED! pid={}, stalled for {}ms ⚠️⚠️⚠️",
                pid.0,
                runnable_for_ns / 1_000_000
            );
            trace.dump();
        }
        other => {
            eprintln!("  Result: {other:?}");
        }
    }
}

/// Attempt 3: David Dai's configuration (closest to production).
///
/// 8 CPUs, matching stall_v1_tight.json:
/// - "bw_tight": 50ms quota / 100ms period (0.5 CPUs for 30 tasks)
/// - "bw_generous": 800ms quota / 100ms period (8 CPUs for 30 tasks)
#[test]
fn test_stall_david_dai_config() {
    let _lock = common::setup_test();
    let all_cpus: Vec<CpuId> = (0..8).map(CpuId).collect();

    let mut builder = Scenario::builder()
        .cpus(8)
        .duration_ms(10_000)
        .watchdog_timeout_ns(Some(4_000_000_000))
        .cgroup_with_bandwidth("bw_tight", &all_cpus, 100_000, 50_000, 0)
        .cgroup_with_bandwidth("bw_generous", &all_cpus, 100_000, 800_000, 0);

    for i in 0..30 {
        builder =
            builder.add_task_in_cgroup(&format!("tight_{i:02}"), 0, heavy_worker(), "bw_tight");
    }
    for i in 0..30 {
        builder =
            builder.add_task_in_cgroup(&format!("gen_{i:02}"), 0, heavy_worker(), "bw_generous");
    }

    let scenario = builder.build();
    eprintln!("=== Stall Attempt: David Dai Config (30+30 tasks, 8 CPUs) ===");
    eprintln!("  bw_tight: 50ms/100ms (0.5 CPUs for 30 tasks)");
    eprintln!("  bw_generous: 800ms/100ms (8 CPUs for 30 tasks)");

    let trace = Simulator::new(DynamicScheduler::lavd(8)).run(scenario);

    match trace.exit_kind() {
        ExitKind::Normal => {
            eprintln!("  Result: COMPLETED NORMALLY (no stall in 10s)");
            let mut tight_total_rt: u64 = 0;
            let mut gen_total_rt: u64 = 0;
            let mut tight_min_sched = usize::MAX;
            let mut gen_min_sched = usize::MAX;
            for i in 0..30 {
                let pid = Pid(i + 1);
                tight_total_rt += trace.total_runtime(pid);
                tight_min_sched = tight_min_sched.min(trace.schedule_count(pid));
            }
            for i in 0..30 {
                let pid = Pid(i + 31);
                gen_total_rt += trace.total_runtime(pid);
                gen_min_sched = gen_min_sched.min(trace.schedule_count(pid));
            }
            eprintln!("  tight total runtime: {}ms", tight_total_rt / 1_000_000);
            eprintln!("  generous total runtime: {}ms", gen_total_rt / 1_000_000);
            eprintln!("  tight min schedules: {tight_min_sched}");
            eprintln!("  generous min schedules: {gen_min_sched}");
        }
        ExitKind::ErrorStall {
            pid,
            runnable_for_ns,
        } => {
            eprintln!(
                "  ⚠️⚠️⚠️ STALL DETECTED! pid={}, stalled for {}ms ⚠️⚠️⚠️",
                pid.0,
                runnable_for_ns / 1_000_000
            );
            eprintln!("  THIS IS THE PRODUCTION STALL PATTERN");
            trace.dump();
        }
        other => {
            eprintln!("  Result: {other:?}");
        }
    }
}

/// Attempt 4: David Dai config with cooperative interleaving.
///
/// This enables concurrent dispatch callbacks on multiple CPUs,
/// which can expose race conditions in LAVD's BPF code.
#[test]
fn test_stall_interleave_cooperative() {
    let _lock = common::setup_test();
    let all_cpus: Vec<CpuId> = (0..8).map(CpuId).collect();

    let mut builder = Scenario::builder()
        .cpus(8)
        .duration_ms(10_000)
        .watchdog_timeout_ns(Some(4_000_000_000))
        .interleave(true)
        .preemptive(PreemptiveConfig::cooperative_only())
        .cgroup_with_bandwidth("bw_tight", &all_cpus, 100_000, 50_000, 0)
        .cgroup_with_bandwidth("bw_generous", &all_cpus, 100_000, 800_000, 0);

    for i in 0..30 {
        builder =
            builder.add_task_in_cgroup(&format!("tight_{i:02}"), 0, heavy_worker(), "bw_tight");
    }
    for i in 0..30 {
        builder =
            builder.add_task_in_cgroup(&format!("gen_{i:02}"), 0, heavy_worker(), "bw_generous");
    }

    let scenario = builder.build();
    eprintln!("=== Stall Attempt: Interleave+Cooperative (30+30 tasks, 8 CPUs) ===");

    let trace = Simulator::new(DynamicScheduler::lavd(8)).run(scenario);

    match trace.exit_kind() {
        ExitKind::Normal => {
            eprintln!("  Result: COMPLETED NORMALLY (no stall)");
            let mut tight_min = usize::MAX;
            let mut gen_min = usize::MAX;
            for i in 0..30 {
                tight_min = tight_min.min(trace.schedule_count(Pid(i + 1)));
                gen_min = gen_min.min(trace.schedule_count(Pid(i + 31)));
            }
            eprintln!("  tight min schedules: {tight_min}");
            eprintln!("  generous min schedules: {gen_min}");
        }
        ExitKind::ErrorStall {
            pid,
            runnable_for_ns,
        } => {
            eprintln!(
                "  ⚠️⚠️⚠️ STALL DETECTED (interleave)! pid={}, stalled for {}ms ⚠️⚠️⚠️",
                pid.0,
                runnable_for_ns / 1_000_000
            );
            trace.dump();
        }
        other => {
            eprintln!("  Result: {other:?}");
        }
    }
}

/// Attempt 5: Longer simulation (30s) with production-scale parameters.
///
/// Production stalls take ~30s to trigger according to the watchdog.
/// Use a long simulation with extreme oversubscription.
#[test]
fn test_stall_long_duration() {
    let _lock = common::setup_test();
    let all_cpus: Vec<CpuId> = (0..8).map(CpuId).collect();

    let mut builder = Scenario::builder()
        .cpus(8)
        .duration_ms(30_000) // 30 seconds — production watchdog timeout
        .watchdog_timeout_ns(Some(15_000_000_000)) // 15s watchdog
        .cgroup_with_bandwidth("bw_tight", &all_cpus, 100_000, 50_000, 0)
        .cgroup_with_bandwidth("bw_generous", &all_cpus, 100_000, 800_000, 0);

    // 50 tight tasks — even more oversubscription
    for i in 0..50 {
        builder =
            builder.add_task_in_cgroup(&format!("tight_{i:02}"), 0, heavy_worker(), "bw_tight");
    }
    // 30 generous tasks
    for i in 0..30 {
        builder =
            builder.add_task_in_cgroup(&format!("gen_{i:02}"), 0, heavy_worker(), "bw_generous");
    }

    let scenario = builder.build();
    eprintln!("=== Stall Attempt: Long Duration (50+30 tasks, 8 CPUs, 30s) ===");

    let trace = Simulator::new(DynamicScheduler::lavd(8)).run(scenario);

    match trace.exit_kind() {
        ExitKind::Normal => {
            eprintln!("  Result: COMPLETED NORMALLY (no stall in 30s)");
            let mut tight_min = usize::MAX;
            for i in 0..50 {
                tight_min = tight_min.min(trace.schedule_count(Pid(i + 1)));
            }
            eprintln!("  tight min schedules: {tight_min}");
        }
        ExitKind::ErrorStall {
            pid,
            runnable_for_ns,
        } => {
            eprintln!(
                "  ⚠️⚠️⚠️ STALL DETECTED (30s run)! pid={}, stalled for {}ms ⚠️⚠️⚠️",
                pid.0,
                runnable_for_ns / 1_000_000
            );
        }
        other => {
            eprintln!("  Result: {other:?}");
        }
    }
}
