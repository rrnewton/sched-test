//! Ucache cartoon workload for scx-sim.
//!
//! Models the core ucache thread interaction pattern at 1/16 scale:
//! - 8 ucache_workers: the hub, high wake frequency, latency-critical
//! - 2 navy_readers: NVM read workers, woken by workers, wake workers back
//! - 2 navy_writers: NVM write workers, fire-and-forget from workers
//! - 4 cpu_hogs: background load to create contention
//! - IRQ pressure on ~1/3 of CPUs (CPUs 0-4)
//!
//! See ai_docs/UCACHE_BACKGROUND.md for the full architecture analysis.

use scx_simulator::probes::{LavdMonitor, LavdProbes};
use scx_simulator::*;

#[macro_use]
mod common;

/// PID assignment plan:
///   Parent:  Pid(100) — shared parent so LAVD's wake_freq gate passes
///   Workers: Pid(1)..Pid(8)
///   Readers: Pid(9), Pid(10)
///   Writers: Pid(11), Pid(12)
///   CPU hogs: Pid(13)..Pid(16)
const PARENT_PID: Pid = Pid(100);

const NUM_CPUS: u32 = 16;
const NUM_WORKERS: i32 = 8;
const NUM_READERS: i32 = 2;
const NUM_WRITERS: i32 = 2;
const NUM_HOGS: i32 = 4;
const DURATION_MS: u64 = 500;

// Timing constants (nanoseconds)
const DRAM_LOOKUP_NS: u64 = 3_000; // 3us DRAM lookup
const REPLY_NS: u64 = 1_000; // 1us reply processing
const SSD_READ_NS: u64 = 100_000; // 100us SSD read
const SSD_WRITE_NS: u64 = 200_000; // 200us SSD write
const WORKER_SLEEP_NS: u64 = 200_000; // 200us batching sleep (fast_pickup_schedule_us)
const HOG_RUN_NS: u64 = 5_000_000; // 5ms CPU-bound chunk

// IRQ parameters — 30% stolen time matches production ucache (~1/3 cores hammered)
const IRQ_INTERVAL_NS: u64 = 200_000; // 200us between IRQs
const IRQ_DURATION_NS: u64 = 60_000; // 60us per IRQ handler (30% stolen)
const IRQ_CPU_COUNT: u32 = 5; // CPUs 0-4 get IRQ pressure

fn worker_pid(i: i32) -> Pid {
    Pid(1 + i)
}
fn reader_pid(i: i32) -> Pid {
    Pid(1 + NUM_WORKERS + i)
}
fn writer_pid(i: i32) -> Pid {
    Pid(1 + NUM_WORKERS + NUM_READERS + i)
}
fn hog_pid(i: i32) -> Pid {
    Pid(1 + NUM_WORKERS + NUM_READERS + NUM_WRITERS + i)
}

/// Build the ucache cartoon scenario.
///
/// Each worker models a simplified request loop:
///   Run(DRAM lookup) → Wake(reader) → Wake(writer) → Sleep(batching)
///   [woken by reader completion] → Run(reply) → repeat
///
/// Each reader:
///   Sleep(forever) → [woken by worker] → Run(SSD read) → Wake(worker) → repeat
///
/// Each writer:
///   Sleep(forever) → [woken by worker] → Run(SSD write) → repeat
fn build_scenario(with_irq: bool) -> Scenario {
    let mut builder = Scenario::builder().cpus(NUM_CPUS);

    // --- shared parent task (required for LAVD wake_freq gating) ---
    // LAVD only updates wake_freq when waker->real_parent == wakee->real_parent.
    // All ucache threads are children of the same process (ucache server).
    builder = builder.task(TaskDef {
        name: "ucache_parent".into(),
        pid: PARENT_PID,
        nice: 0,
        behavior: TaskBehavior {
            phases: vec![Phase::Sleep(u64::MAX)], // sleeps forever, just exists as parent
            repeat: RepeatMode::Once,
        },
        start_time_ns: 0,
        mm_id: Some(MmId(1)),
        allowed_cpus: None,
        parent_pid: None,
        cgroup_name: None,
        task_flags: 0,
        migration_disabled: 0,
    });

    // --- ucache_workers ---
    // Worker i wakes reader (i % NUM_READERS) and writer (i % NUM_WRITERS).
    // After sleeping, worker is woken by its assigned reader.
    for i in 0..NUM_WORKERS {
        let my_reader = reader_pid(i % NUM_READERS);
        let my_writer = writer_pid(i % NUM_WRITERS);
        builder = builder.task(TaskDef {
            name: format!("ucache_worker_{i}"),
            pid: worker_pid(i),
            nice: -5,
            behavior: TaskBehavior {
                phases: vec![
                    Phase::Run(DRAM_LOOKUP_NS),  // DRAM lookup
                    Phase::Wake(my_reader),       // dispatch NVM read
                    Phase::Wake(my_writer),       // dispatch NVM write (async)
                    Phase::Sleep(WORKER_SLEEP_NS), // batching sleep
                    // Reader will wake us back; if sleep expires first, we loop
                    Phase::Run(REPLY_NS),         // process reply
                ],
                repeat: RepeatMode::Forever,
            },
            start_time_ns: i as u64 * 10_000, // stagger starts by 10us
            mm_id: Some(MmId(1)),              // shared address space for wake_freq
            allowed_cpus: None,                // free to migrate
            parent_pid: Some(PARENT_PID),      // shared parent for LAVD wake_freq gate
            cgroup_name: None,
            task_flags: 0,
            migration_disabled: 0,
        });
    }

    // --- navy_readers ---
    // Each reader serves multiple workers. Reader i is woken by workers
    // where (worker_idx % NUM_READERS == i). After SSD read, reader wakes
    // the worker that dispatched it. We approximate this by waking a
    // round-robin worker.
    for i in 0..NUM_READERS {
        // Wake back the first worker that maps to this reader
        let wake_back = worker_pid(i);
        builder = builder.task(TaskDef {
            name: format!("navy_reader_{i}"),
            pid: reader_pid(i),
            nice: 0,
            behavior: TaskBehavior {
                phases: vec![
                    Phase::Sleep(u64::MAX),      // wait for job
                    Phase::Run(SSD_READ_NS),     // SSD read
                    Phase::Wake(wake_back),       // baton_.post() → wake worker
                ],
                repeat: RepeatMode::Forever,
            },
            start_time_ns: 0,
            mm_id: Some(MmId(1)),
            allowed_cpus: None,
            parent_pid: Some(PARENT_PID),
            cgroup_name: None,
            task_flags: 0,
            migration_disabled: 0,
        });
    }

    // --- navy_writers ---
    // Fire-and-forget: woken by workers, run SSD write, sleep again.
    for i in 0..NUM_WRITERS {
        builder = builder.task(TaskDef {
            name: format!("navy_writer_{i}"),
            pid: writer_pid(i),
            nice: 5,
            behavior: TaskBehavior {
                phases: vec![
                    Phase::Sleep(u64::MAX),      // wait for job
                    Phase::Run(SSD_WRITE_NS),    // SSD write
                ],
                repeat: RepeatMode::Forever,
            },
            start_time_ns: 0,
            mm_id: Some(MmId(1)),
            allowed_cpus: None,
            parent_pid: Some(PARENT_PID),
            cgroup_name: None,
            task_flags: 0,
            migration_disabled: 0,
        });
    }

    // --- cpu_hogs (background contention) ---
    for i in 0..NUM_HOGS {
        builder = builder.task(TaskDef {
            name: format!("cpu_hog_{i}"),
            pid: hog_pid(i),
            nice: 10,
            behavior: TaskBehavior {
                phases: vec![Phase::Run(HOG_RUN_NS)],
                repeat: RepeatMode::Forever,
            },
            start_time_ns: 0,
            mm_id: Some(MmId(2)),
            allowed_cpus: None,
            parent_pid: None,
            cgroup_name: None,
            task_flags: 0,
            migration_disabled: 0,
        });
    }

    // --- IRQ pressure on ~1/3 of CPUs ---
    if with_irq {
        for cpu in 0..IRQ_CPU_COUNT {
            builder = builder.periodic_irq(
                CpuId(cpu),
                IrqType::HardIrq,
                100_000,         // start at 100us
                IRQ_INTERVAL_NS, // every 200us
                IRQ_DURATION_NS, // 10us duration
                &[],             // no direct wakeups from IRQ
            );
        }
    }

    builder.duration_ms(DURATION_MS).build()
}

/// Run the ucache cartoon with LAVD and verify key scheduling properties.
#[test]
fn test_ucache_cartoon_lavd() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::lavd(NUM_CPUS);
    let probes = LavdProbes::new(&sched);
    let mut monitor = LavdMonitor::new(probes);

    let scenario = build_scenario(true);
    let result = Simulator::new(sched).run_monitored(scenario, &mut monitor);
    let trace = &result.trace;

    // All task types should be scheduled
    for i in 0..NUM_WORKERS {
        assert!(
            trace.schedule_count(worker_pid(i)) > 0,
            "ucache_worker_{i} was never scheduled"
        );
    }
    for i in 0..NUM_READERS {
        assert!(
            trace.schedule_count(reader_pid(i)) > 0,
            "navy_reader_{i} was never scheduled"
        );
    }
    for i in 0..NUM_WRITERS {
        assert!(
            trace.schedule_count(writer_pid(i)) > 0,
            "navy_writer_{i} was never scheduled"
        );
    }

    // Verify IRQ events were generated on the expected CPUs
    let irq_events: Vec<_> = trace
        .events()
        .iter()
        .filter(|e| matches!(e.kind, TraceKind::IrqStart { .. }))
        .collect();
    assert!(
        irq_events.len() > 100,
        "expected many IRQ events, got {}",
        irq_events.len()
    );

    // Check LAVD lat_cri: workers should have higher lat_cri than hogs
    let worker_snap = monitor.final_snapshot(worker_pid(0));
    let hog_snap = monitor.final_snapshot(hog_pid(0));

    if let (Some(w), Some(h)) = (worker_snap, hog_snap) {
        eprintln!(
            "ucache_worker_0: lat_cri={}, wake_freq={}",
            w.lat_cri, w.wake_freq
        );
        eprintln!(
            "cpu_hog_0: lat_cri={}, wake_freq={}",
            h.lat_cri, h.wake_freq
        );
        // Workers participate in wake chains → higher lat_cri
        assert!(
            w.lat_cri >= h.lat_cri,
            "expected worker lat_cri ({}) >= hog lat_cri ({})",
            w.lat_cri,
            h.lat_cri
        );
    }

    // Print scheduling distribution summary
    eprintln!("\n=== Ucache Cartoon Scheduling Summary (LAVD + IRQ) ===");
    for i in 0..NUM_WORKERS {
        let pid = worker_pid(i);
        eprintln!(
            "  ucache_worker_{i}: {} schedules",
            trace.schedule_count(pid)
        );
    }
    for i in 0..NUM_READERS {
        let pid = reader_pid(i);
        eprintln!(
            "  navy_reader_{i}: {} schedules",
            trace.schedule_count(pid)
        );
    }
    for i in 0..NUM_WRITERS {
        let pid = writer_pid(i);
        eprintln!(
            "  navy_writer_{i}: {} schedules",
            trace.schedule_count(pid)
        );
    }
    eprintln!("  IRQ events: {}", irq_events.len());

    // Per-CPU placement analysis: count worker schedules on each CPU
    eprintln!("\n=== Per-CPU Worker Placement ===");
    let mut cpu_worker_count = vec![0u32; NUM_CPUS as usize];
    let mut cpu_irq_worker_count = 0u32;
    let mut cpu_clean_worker_count = 0u32;
    for event in trace.events() {
        if let TraceKind::TaskScheduled { pid } = &event.kind {
            let is_worker = (0..NUM_WORKERS).any(|i| *pid == worker_pid(i));
            if is_worker {
                let cpu = event.cpu.0 as usize;
                if cpu < cpu_worker_count.len() {
                    cpu_worker_count[cpu] += 1;
                    if (cpu as u32) < IRQ_CPU_COUNT {
                        cpu_irq_worker_count += 1;
                    } else {
                        cpu_clean_worker_count += 1;
                    }
                }
            }
        }
    }
    let total_worker_sched: u32 = cpu_worker_count.iter().sum();
    for (cpu, &count) in cpu_worker_count.iter().enumerate() {
        if count > 0 {
            let pct = 100.0 * count as f64 / total_worker_sched as f64;
            let tag = if (cpu as u32) < IRQ_CPU_COUNT { " [IRQ]" } else { "" };
            eprintln!("  CPU {:>2}: {:>6} ({:>5.1}%){}", cpu, count, pct, tag);
        }
    }
    let irq_pct = 100.0 * cpu_irq_worker_count as f64 / total_worker_sched as f64;
    let expected_pct = 100.0 * IRQ_CPU_COUNT as f64 / NUM_CPUS as f64;
    eprintln!(
        "\n  Workers on IRQ CPUs: {}/{} ({:.1}%, expected random: {:.1}%)",
        cpu_irq_worker_count, total_worker_sched, irq_pct, expected_pct
    );
}

/// Run without IRQ pressure as a baseline for comparison.
#[test]
fn test_ucache_cartoon_lavd_no_irq() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::lavd(NUM_CPUS);

    let scenario = build_scenario(false);
    let trace = Simulator::new(sched).run(scenario);

    // All task types should be scheduled
    for i in 0..NUM_WORKERS {
        assert!(
            trace.schedule_count(worker_pid(i)) > 0,
            "ucache_worker_{i} was never scheduled (no IRQ)"
        );
    }
    for i in 0..NUM_READERS {
        assert!(
            trace.schedule_count(reader_pid(i)) > 0,
            "navy_reader_{i} was never scheduled (no IRQ)"
        );
    }

    // No IRQ events should exist
    let irq_count = trace
        .events()
        .iter()
        .filter(|e| matches!(e.kind, TraceKind::IrqStart { .. }))
        .count();
    assert_eq!(irq_count, 0, "expected no IRQ events in baseline");

    eprintln!("\n=== Ucache Cartoon Scheduling Summary (LAVD, no IRQ) ===");
    for i in 0..NUM_WORKERS {
        eprintln!(
            "  ucache_worker_{i}: {} schedules",
            trace.schedule_count(worker_pid(i))
        );
    }
}

/// Measure latency impact: do workers on IRQ CPUs experience longer run durations?
///
/// This test validates the latency hypothesis: IRQ-stolen time should make
/// run durations longer on IRQ CPUs (CPUs 0-4) vs clean CPUs (5-15).
/// With LAVD's IRQ avoidance, workers should mostly run on clean CPUs,
/// resulting in more consistent run durations.
#[test]
fn test_ucache_cartoon_latency_by_cpu() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::lavd(NUM_CPUS);
    let probes = LavdProbes::new(&sched);
    let mut monitor = LavdMonitor::new(probes);

    let scenario = build_scenario(true);
    let result = Simulator::new(sched).run_monitored(scenario, &mut monitor);
    let trace = &result.trace;

    let worker_pids: Vec<Pid> = (0..NUM_WORKERS).map(worker_pid).collect();

    // Collect per-schedule run durations tagged by CPU
    let mut irq_cpu_durations: Vec<u64> = Vec::new();
    let mut clean_cpu_durations: Vec<u64> = Vec::new();
    let mut wake_to_run_irq: Vec<u64> = Vec::new();
    let mut wake_to_run_clean: Vec<u64> = Vec::new();

    // Track last wake time per worker for wake-to-run latency
    let mut last_wake: std::collections::HashMap<Pid, u64> = std::collections::HashMap::new();
    // Track last schedule time per worker for run duration
    let mut running_since: std::collections::HashMap<Pid, (u64, u32)> =
        std::collections::HashMap::new(); // (time, cpu)

    for event in trace.events() {
        match &event.kind {
            TraceKind::TaskWoke { pid } if worker_pids.contains(pid) => {
                last_wake.insert(*pid, event.time_ns);
            }
            TraceKind::TaskScheduled { pid } if worker_pids.contains(pid) => {
                let cpu = event.cpu.0;
                // Wake-to-run latency
                if let Some(wake_time) = last_wake.remove(pid) {
                    let latency = event.time_ns.saturating_sub(wake_time);
                    if cpu < IRQ_CPU_COUNT {
                        wake_to_run_irq.push(latency);
                    } else {
                        wake_to_run_clean.push(latency);
                    }
                }
                running_since.insert(*pid, (event.time_ns, cpu));
            }
            TraceKind::TaskPreempted { pid }
            | TraceKind::TaskYielded { pid }
            | TraceKind::TaskSlept { pid }
            | TraceKind::TaskCompleted { pid }
                if worker_pids.contains(pid) =>
            {
                if let Some((start, cpu)) = running_since.remove(pid) {
                    let duration = event.time_ns.saturating_sub(start);
                    if cpu < IRQ_CPU_COUNT {
                        irq_cpu_durations.push(duration);
                    } else {
                        clean_cpu_durations.push(duration);
                    }
                }
            }
            _ => {}
        }
    }

    // Sort for percentile computation
    irq_cpu_durations.sort();
    clean_cpu_durations.sort();
    wake_to_run_irq.sort();
    wake_to_run_clean.sort();

    let percentile = |sorted: &[u64], p: f64| -> u64 {
        if sorted.is_empty() {
            return 0;
        }
        let idx = ((sorted.len() as f64 * p) as usize).min(sorted.len() - 1);
        sorted[idx]
    };

    let avg = |data: &[u64]| -> u64 {
        if data.is_empty() {
            return 0;
        }
        data.iter().sum::<u64>() / data.len() as u64
    };

    eprintln!("\n=== Worker Latency: IRQ CPUs vs Clean CPUs ===\n");
    eprintln!("--- Run Durations (ns) ---");
    eprintln!(
        "  IRQ CPUs (0-{}):  n={:>5}  avg={:>8}  P50={:>8}  P90={:>8}  P99={:>8}  max={:>8}",
        IRQ_CPU_COUNT - 1,
        irq_cpu_durations.len(),
        avg(&irq_cpu_durations),
        percentile(&irq_cpu_durations, 0.5),
        percentile(&irq_cpu_durations, 0.9),
        percentile(&irq_cpu_durations, 0.99),
        irq_cpu_durations.last().copied().unwrap_or(0)
    );
    eprintln!(
        "  Clean CPUs ({}-{}): n={:>5}  avg={:>8}  P50={:>8}  P90={:>8}  P99={:>8}  max={:>8}",
        IRQ_CPU_COUNT,
        NUM_CPUS - 1,
        clean_cpu_durations.len(),
        avg(&clean_cpu_durations),
        percentile(&clean_cpu_durations, 0.5),
        percentile(&clean_cpu_durations, 0.9),
        percentile(&clean_cpu_durations, 0.99),
        clean_cpu_durations.last().copied().unwrap_or(0)
    );

    eprintln!("\n--- Wake-to-Run Latency (ns) ---");
    eprintln!(
        "  IRQ CPUs (0-{}):  n={:>5}  avg={:>8}  P50={:>8}  P90={:>8}  P99={:>8}  max={:>8}",
        IRQ_CPU_COUNT - 1,
        wake_to_run_irq.len(),
        avg(&wake_to_run_irq),
        percentile(&wake_to_run_irq, 0.5),
        percentile(&wake_to_run_irq, 0.9),
        percentile(&wake_to_run_irq, 0.99),
        wake_to_run_irq.last().copied().unwrap_or(0)
    );
    eprintln!(
        "  Clean CPUs ({}-{}): n={:>5}  avg={:>8}  P50={:>8}  P90={:>8}  P99={:>8}  max={:>8}",
        IRQ_CPU_COUNT,
        NUM_CPUS - 1,
        wake_to_run_clean.len(),
        avg(&wake_to_run_clean),
        percentile(&wake_to_run_clean, 0.5),
        percentile(&wake_to_run_clean, 0.9),
        percentile(&wake_to_run_clean, 0.99),
        wake_to_run_clean.last().copied().unwrap_or(0)
    );

    // Verify LAVD routes most worker schedules to clean CPUs
    let total_worker_sched = irq_cpu_durations.len() + clean_cpu_durations.len();
    let irq_pct = if total_worker_sched > 0 {
        100.0 * irq_cpu_durations.len() as f64 / total_worker_sched as f64
    } else {
        0.0
    };
    eprintln!(
        "\n  Worker schedules: {} on IRQ CPUs, {} on clean ({:.1}% on IRQ, expected random: {:.1}%)",
        irq_cpu_durations.len(),
        clean_cpu_durations.len(),
        irq_pct,
        100.0 * IRQ_CPU_COUNT as f64 / NUM_CPUS as f64
    );

    // If LAVD avoidance works, fewer than random on IRQ CPUs
    if total_worker_sched > 50 {
        let expected_random_pct = 100.0 * IRQ_CPU_COUNT as f64 / NUM_CPUS as f64;
        eprintln!(
            "  IRQ avoidance ratio: {:.1}x (actual {:.1}% vs expected {:.1}%)",
            expected_random_pct / irq_pct.max(0.1),
            irq_pct,
            expected_random_pct
        );
    }

    // Run duration on IRQ CPUs should be longer due to stolen time
    if !irq_cpu_durations.is_empty() && !clean_cpu_durations.is_empty() {
        let irq_avg = avg(&irq_cpu_durations);
        let clean_avg = avg(&clean_cpu_durations);
        if clean_avg > 0 {
            eprintln!(
                "  Run duration ratio (IRQ/clean): {:.2}x ({} vs {} ns avg)",
                irq_avg as f64 / clean_avg as f64,
                irq_avg,
                clean_avg
            );
        }
    }
}

/// Compare with tickless scheduler (baseline, no LAVD intelligence).
/// Measures the same latency metrics to quantify LAVD's advantage.
#[test]
fn test_ucache_cartoon_tickless() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::tickless(NUM_CPUS);

    let scenario = build_scenario(true);
    let trace = Simulator::new(sched).run(scenario);

    let worker_pids: Vec<Pid> = (0..NUM_WORKERS).map(worker_pid).collect();

    let mut irq_durations: Vec<u64> = Vec::new();
    let mut clean_durations: Vec<u64> = Vec::new();
    let mut running_since: std::collections::HashMap<Pid, (u64, u32)> =
        std::collections::HashMap::new();

    for event in trace.events() {
        match &event.kind {
            TraceKind::TaskScheduled { pid } if worker_pids.contains(pid) => {
                running_since.insert(*pid, (event.time_ns, event.cpu.0));
            }
            TraceKind::TaskPreempted { pid }
            | TraceKind::TaskYielded { pid }
            | TraceKind::TaskSlept { pid }
            | TraceKind::TaskCompleted { pid }
                if worker_pids.contains(pid) =>
            {
                if let Some((start, cpu)) = running_since.remove(pid) {
                    let dur = event.time_ns.saturating_sub(start);
                    if cpu < IRQ_CPU_COUNT {
                        irq_durations.push(dur);
                    } else {
                        clean_durations.push(dur);
                    }
                }
            }
            _ => {}
        }
    }

    irq_durations.sort();
    clean_durations.sort();

    let percentile = |s: &[u64], p: f64| -> u64 {
        if s.is_empty() { return 0; }
        s[((s.len() as f64 * p) as usize).min(s.len() - 1)]
    };
    let avg = |d: &[u64]| -> u64 {
        if d.is_empty() { return 0; }
        d.iter().sum::<u64>() / d.len() as u64
    };

    let total = irq_durations.len() + clean_durations.len();
    let irq_pct = if total > 0 { 100.0 * irq_durations.len() as f64 / total as f64 } else { 0.0 };

    eprintln!("\n=== Tickless Baseline: Worker Latency by CPU ===\n");
    eprintln!(
        "  IRQ CPUs:   n={:>5}  avg={:>8}  P50={:>8}  P99={:>8}  max={:>8}",
        irq_durations.len(), avg(&irq_durations),
        percentile(&irq_durations, 0.5), percentile(&irq_durations, 0.99),
        irq_durations.last().copied().unwrap_or(0)
    );
    eprintln!(
        "  Clean CPUs: n={:>5}  avg={:>8}  P50={:>8}  P99={:>8}  max={:>8}",
        clean_durations.len(), avg(&clean_durations),
        percentile(&clean_durations, 0.5), percentile(&clean_durations, 0.99),
        clean_durations.last().copied().unwrap_or(0)
    );
    eprintln!(
        "  Workers on IRQ CPUs: {:.1}% (expected random: {:.1}%)",
        irq_pct, 100.0 * IRQ_CPU_COUNT as f64 / NUM_CPUS as f64
    );

    for i in 0..NUM_WORKERS {
        assert!(
            trace.schedule_count(worker_pid(i)) > 0,
            "ucache_worker_{i} was never scheduled (tickless)"
        );
    }
}
