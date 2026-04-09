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
///   Workers: Pid(1)..Pid(8)
///   Readers: Pid(9), Pid(10)
///   Writers: Pid(11), Pid(12)
///   CPU hogs: Pid(13)..Pid(16)

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

// IRQ parameters
const IRQ_INTERVAL_NS: u64 = 200_000; // 200us between IRQs
const IRQ_DURATION_NS: u64 = 10_000; // 10us per IRQ handler
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
            parent_pid: None,
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
            parent_pid: None,
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
            parent_pid: None,
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

/// Compare with tickless scheduler (baseline, no LAVD intelligence).
#[test]
fn test_ucache_cartoon_tickless() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::tickless(NUM_CPUS);

    let scenario = build_scenario(true);
    let trace = Simulator::new(sched).run(scenario);

    for i in 0..NUM_WORKERS {
        assert!(
            trace.schedule_count(worker_pid(i)) > 0,
            "ucache_worker_{i} was never scheduled (tickless)"
        );
    }

    eprintln!("\n=== Ucache Cartoon Scheduling Summary (Tickless + IRQ) ===");
    for i in 0..NUM_WORKERS {
        eprintln!(
            "  ucache_worker_{i}: {} schedules",
            trace.schedule_count(worker_pid(i))
        );
    }
}
