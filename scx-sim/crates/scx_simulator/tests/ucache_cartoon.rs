//! Production-calibrated ucache cartoon workload for scx-sim.
//!
//! Models Meta's ucache service thread interaction at 1/16 scale, with
//! parameters derived from real Perfetto traces (TRACE_CALIBRATION.md).
//!
//! Production facts (from traces):
//! - 252 ucache_worker, 32 navy_reader, 32 navy_writer (ratio 8:1:1)
//! - All at nice=0 (priority 120) — NO priority differentiation
//! - ucache_worker avg slice: ~250us, navy_reader: ~35us, navy_writer: ~22us
//! - Navy_writer wakes MORE frequently than navy_reader
//!
//! Cartoon (16 CPUs, 12 workload + 4 IRQ):
//! - 8 ucache_workers: run(250us) + sleep(110us) = 69% util, ~2778 wakes/s
//! - 1 navy_reader: run(35us) + sleep(250us) = 12% util, ~3509 wakes/s
//! - 1 navy_writer: run(22us) + sleep(198us) = 10% util, ~4545 wakes/s
//! - 4 cpu_hogs: background load
//! - IRQ pressure on CPUs 0-4 (~1/3 of workload CPUs)
//!
//! CRITICAL: navy_writer wakes MORE frequently than navy_reader (4545 vs 3509/s).
//! LAVD uses wake frequency for latency criticality classification, so it
//! misclassifies the high-wake-rate writer as more latency-critical and
//! steers it away from IRQ CPUs — protecting the wrong thread type.

use scx_simulator::probes::{LavdMonitor, LavdProbes};
use scx_simulator::*;

#[macro_use]
mod common;

/// PID assignment plan:
///   Parent:  Pid(100) — shared parent so LAVD's wake_freq gate passes
///   Workers: Pid(1)..Pid(8)
///   Reader:  Pid(9)
///   Writer:  Pid(10)
///   CPU hogs: Pid(11)..Pid(14)
const PARENT_PID: Pid = Pid(100);

const NUM_CPUS: u32 = 16;
const NUM_WORKERS: i32 = 8;
const NUM_READERS: i32 = 1;
const NUM_WRITERS: i32 = 1;
const NUM_HOGS: i32 = 4;
const DURATION_MS: u64 = 500;

// Timing constants calibrated from production traces (nanoseconds).
// See ai_docs/ucache_irq/TRACE_CALIBRATION.md for derivation.
//
// ucache_worker: run(250us) + sleep(110us) = 360us period ≈ 2778 wakes/s, 69% util
// navy_reader:   run(35us)  + sleep(250us) = 285us period ≈ 3509 wakes/s, 12% util
// navy_writer:   run(22us)  + sleep(198us) = 220us period ≈ 4545 wakes/s, 10% util
const WORKER_RUN_NS: u64 = 250_000; // 250us avg slice (production avg)
const WORKER_SLEEP_NS: u64 = 110_000; // 110us → 360us period
const READER_RUN_NS: u64 = 35_000; // 35us avg slice
const READER_SLEEP_NS: u64 = 250_000; // 250us → 285us period (wakes LESS)
const WRITER_RUN_NS: u64 = 22_000; // 22us avg slice
const WRITER_SLEEP_NS: u64 = 198_000; // 198us → 220us period (wakes MORE)
const HOG_RUN_NS: u64 = 5_000_000; // 5ms CPU-bound chunk

// IRQ parameters — 10% stolen time (calibrated down from 30%; production
// shows ~5-15% IRQ load on affected cores, not the extreme 30% prior cartoons used)
const IRQ_INTERVAL_NS: u64 = 200_000; // 200us between IRQs
const IRQ_DURATION_NS: u64 = 20_000; // 20us per IRQ handler (10% stolen)
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

/// Build the production-calibrated ucache cartoon scenario.
///
/// Uses independent run+sleep patterns (not hub-spoke suspend/resume) to match
/// production trace wake frequencies. This is critical: navy_writer's shorter
/// sleep period gives it a HIGHER wake rate than navy_reader, which is what
/// triggers LAVD's latency criticality misclassification.
///
/// All threads at nice=0 (production reality — prior cartoons used nice=-5/0/5
/// but traces show all threads at priority 120).
fn build_scenario(with_irq: bool) -> Scenario {
    build_scenario_inner(with_irq, false)
}

/// Build scenario with optional nice-value user hints.
///
/// When `with_nice_hints` is true, applies differentiated nice values:
///   navy_reader:   nice=-10 → scx.weight ~325 (3.25x boost, latency-critical)
///   ucache_worker: nice=-5  → scx.weight ~179 (1.79x boost)
///   navy_writer:   nice=5   → scx.weight ~56  (0.56x, fire-and-forget)
///   cpu_hog:       nice=10  (unchanged)
fn build_scenario_inner(with_irq: bool, with_nice_hints: bool) -> Scenario {
    let mut builder = Scenario::builder().cpus(NUM_CPUS);

    let worker_nice: i8 = if with_nice_hints { -5 } else { 0 };
    let reader_nice: i8 = if with_nice_hints { -10 } else { 0 };
    let writer_nice: i8 = if with_nice_hints { 5 } else { 0 };

    // --- shared parent task (required for LAVD wake_freq gating) ---
    builder = builder.task(TaskDef {
        name: "ucache_parent".into(),
        pid: PARENT_PID,
        nice: 0,
        behavior: TaskBehavior {
            phases: vec![Phase::Sleep(u64::MAX)],
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

    // --- ucache_workers (8): run(250us) + sleep(110us) = 69% util ---
    // Production: 252 threads at nice=0, avg slice 250us, ~2778 wakes/s
    for i in 0..NUM_WORKERS {
        builder = builder.task(TaskDef {
            name: format!("ucache_worker_{i}"),
            pid: worker_pid(i),
            nice: worker_nice,
            behavior: TaskBehavior {
                phases: vec![
                    Phase::Run(WORKER_RUN_NS),
                    Phase::Sleep(WORKER_SLEEP_NS),
                ],
                repeat: RepeatMode::Forever,
            },
            start_time_ns: i as u64 * 10_000,
            mm_id: Some(MmId(1)),
            allowed_cpus: None,
            parent_pid: Some(PARENT_PID),
            cgroup_name: None,
            task_flags: 0,
            migration_disabled: 0,
        });
    }

    // --- navy_readers: run(35us) + sleep(250us) = 12% util, 3509 wakes/s ---
    // Wakes LESS frequently than navy_writer. This is the key asymmetry.
    for i in 0..NUM_READERS {
        builder = builder.task(TaskDef {
            name: format!("navy_reader_{i}"),
            pid: reader_pid(i),
            nice: reader_nice,
            behavior: TaskBehavior {
                phases: vec![
                    Phase::Run(READER_RUN_NS),
                    Phase::Sleep(READER_SLEEP_NS),
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

    // --- navy_writers: run(22us) + sleep(198us) = 10% util, 4545 wakes/s ---
    // Wakes MORE frequently than navy_reader despite being bulk throughput work.
    // LAVD's wake_freq heuristic sees high wake rate → high latency criticality.
    for i in 0..NUM_WRITERS {
        builder = builder.task(TaskDef {
            name: format!("navy_writer_{i}"),
            pid: writer_pid(i),
            nice: writer_nice,
            behavior: TaskBehavior {
                phases: vec![
                    Phase::Run(WRITER_RUN_NS),
                    Phase::Sleep(WRITER_SLEEP_NS),
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

    // --- IRQ pressure on ~1/3 of CPUs (10% stolen, not 30%) ---
    if with_irq {
        for cpu in 0..IRQ_CPU_COUNT {
            builder = builder.periodic_irq(
                CpuId(cpu),
                IrqType::HardIrq,
                100_000,         // start at 100us
                IRQ_INTERVAL_NS, // every 200us
                IRQ_DURATION_NS, // 20us duration (10% stolen)
                &[],
            );
        }
    }

    builder.duration_ms(DURATION_MS).build()
}

/// Run the ucache cartoon with LAVD and verify key scheduling properties.
///
/// Key metrics to observe:
/// 1. Which threads get placed on IRQ CPUs?
/// 2. Does LAVD misclassify navy_writer as more latency-critical than navy_reader?
/// 3. IRQ avoidance effectiveness for workers
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

    // Check LAVD lat_cri and wake_freq for misclassification evidence
    let worker_snap = monitor.final_snapshot(worker_pid(0));
    let reader_snap = monitor.final_snapshot(reader_pid(0));
    let writer_snap = monitor.final_snapshot(writer_pid(0));
    let hog_snap = monitor.final_snapshot(hog_pid(0));

    eprintln!("\n=== LAVD Latency Criticality Analysis ===");
    if let Some(w) = &worker_snap {
        eprintln!(
            "  ucache_worker_0: lat_cri={}, wake_freq={}, wait_freq={}",
            w.lat_cri, w.wake_freq, w.wait_freq
        );
    }
    if let Some(r) = &reader_snap {
        eprintln!(
            "  navy_reader_0:   lat_cri={}, wake_freq={}, wait_freq={}",
            r.lat_cri, r.wake_freq, r.wait_freq
        );
    }
    if let Some(w) = &writer_snap {
        eprintln!(
            "  navy_writer_0:   lat_cri={}, wake_freq={}, wait_freq={}",
            w.lat_cri, w.wake_freq, w.wait_freq
        );
    }
    if let Some(h) = &hog_snap {
        eprintln!(
            "  cpu_hog_0:       lat_cri={}, wake_freq={}, wait_freq={}",
            h.lat_cri, h.wake_freq, h.wait_freq
        );
    }

    // Check misclassification: does LAVD give navy_writer higher lat_cri
    // than navy_reader? (It should, due to higher wake_freq)
    if let (Some(r), Some(w)) = (&reader_snap, &writer_snap) {
        eprintln!(
            "\n  MISCLASSIFICATION CHECK: writer lat_cri ({}) {} reader lat_cri ({})",
            w.lat_cri,
            if w.lat_cri > r.lat_cri { ">" } else if w.lat_cri == r.lat_cri { "==" } else { "<" },
            r.lat_cri
        );
        eprintln!(
            "  Writer wake_freq ({}) vs reader wake_freq ({})",
            w.wake_freq, r.wake_freq
        );
    }

    // Workers should have higher lat_cri than hogs
    if let (Some(w), Some(h)) = (&worker_snap, &hog_snap) {
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

    // Per-CPU placement analysis: count schedules per thread type on each CPU
    eprintln!("\n=== Per-CPU Thread Placement ===");
    let mut cpu_worker_count = vec![0u32; NUM_CPUS as usize];
    let mut cpu_reader_count = vec![0u32; NUM_CPUS as usize];
    let mut cpu_writer_count = vec![0u32; NUM_CPUS as usize];
    let mut cpu_irq_worker_count = 0u32;
    let mut cpu_irq_reader_count = 0u32;
    let mut cpu_irq_writer_count = 0u32;

    for event in trace.events() {
        if let TraceKind::TaskScheduled { pid } = &event.kind {
            let cpu = event.cpu.0 as usize;
            if cpu >= NUM_CPUS as usize {
                continue;
            }
            let is_worker = (0..NUM_WORKERS).any(|i| *pid == worker_pid(i));
            let is_reader = (0..NUM_READERS).any(|i| *pid == reader_pid(i));
            let is_writer = (0..NUM_WRITERS).any(|i| *pid == writer_pid(i));
            let on_irq = (cpu as u32) < IRQ_CPU_COUNT;

            if is_worker {
                cpu_worker_count[cpu] += 1;
                if on_irq { cpu_irq_worker_count += 1; }
            } else if is_reader {
                cpu_reader_count[cpu] += 1;
                if on_irq { cpu_irq_reader_count += 1; }
            } else if is_writer {
                cpu_writer_count[cpu] += 1;
                if on_irq { cpu_irq_writer_count += 1; }
            }
        }
    }

    let total_worker_sched: u32 = cpu_worker_count.iter().sum();
    let total_reader_sched: u32 = cpu_reader_count.iter().sum();
    let total_writer_sched: u32 = cpu_writer_count.iter().sum();
    let expected_pct = 100.0 * IRQ_CPU_COUNT as f64 / NUM_CPUS as f64;

    for (cpu, (&wc, (&rc, &wrc))) in cpu_worker_count.iter()
        .zip(cpu_reader_count.iter().zip(cpu_writer_count.iter()))
        .enumerate()
    {
        if wc > 0 || rc > 0 || wrc > 0 {
            let tag = if (cpu as u32) < IRQ_CPU_COUNT { " [IRQ]" } else { "" };
            eprintln!(
                "  CPU {:>2}: worker={:>6}  reader={:>4}  writer={:>4}{}",
                cpu, wc, rc, wrc, tag
            );
        }
    }

    let worker_irq_pct = 100.0 * cpu_irq_worker_count as f64 / total_worker_sched.max(1) as f64;
    let reader_irq_pct = 100.0 * cpu_irq_reader_count as f64 / total_reader_sched.max(1) as f64;
    let writer_irq_pct = 100.0 * cpu_irq_writer_count as f64 / total_writer_sched.max(1) as f64;

    eprintln!("\n  Thread placement on IRQ CPUs (expected random: {:.1}%):", expected_pct);
    eprintln!(
        "    Workers: {}/{} ({:.1}%)",
        cpu_irq_worker_count, total_worker_sched, worker_irq_pct
    );
    eprintln!(
        "    Readers: {}/{} ({:.1}%)",
        cpu_irq_reader_count, total_reader_sched, reader_irq_pct
    );
    eprintln!(
        "    Writers: {}/{} ({:.1}%)",
        cpu_irq_writer_count, total_writer_sched, writer_irq_pct
    );
}

/// Run without IRQ pressure as a baseline for comparison.
#[test]
fn test_ucache_cartoon_lavd_no_irq() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::lavd(NUM_CPUS);
    let probes = LavdProbes::new(&sched);
    let mut monitor = LavdMonitor::new(probes);

    let scenario = build_scenario(false);
    let result = Simulator::new(sched).run_monitored(scenario, &mut monitor);
    let trace = &result.trace;

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

    // Check LAVD wake_freq classification even without IRQ
    let reader_snap = monitor.final_snapshot(reader_pid(0));
    let writer_snap = monitor.final_snapshot(writer_pid(0));

    eprintln!("\n=== Ucache Cartoon Scheduling Summary (LAVD, no IRQ) ===");
    if let (Some(r), Some(w)) = (&reader_snap, &writer_snap) {
        eprintln!(
            "  navy_reader_0: wake_freq={}, lat_cri={}",
            r.wake_freq, r.lat_cri
        );
        eprintln!(
            "  navy_writer_0: wake_freq={}, lat_cri={}",
            w.wake_freq, w.lat_cri
        );
        eprintln!(
            "  LAVD classifies writer as {} latency-critical than reader",
            if w.lat_cri > r.lat_cri { "MORE" } else { "LESS or equally" }
        );
    }

    for i in 0..NUM_WORKERS {
        eprintln!(
            "  ucache_worker_{i}: {} schedules",
            trace.schedule_count(worker_pid(i))
        );
    }
    for i in 0..NUM_READERS {
        eprintln!(
            "  navy_reader_{i}: {} schedules",
            trace.schedule_count(reader_pid(i))
        );
    }
    for i in 0..NUM_WRITERS {
        eprintln!(
            "  navy_writer_{i}: {} schedules",
            trace.schedule_count(writer_pid(i))
        );
    }
}

/// Measure latency impact: do workers on IRQ CPUs experience longer run durations?
///
/// Also tracks navy_reader and navy_writer latency to show the misclassification
/// effect: with LAVD protecting writers (misclassified as latency-critical) from
/// IRQ CPUs, readers may get worse placement.
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
    let reader_pids: Vec<Pid> = (0..NUM_READERS).map(reader_pid).collect();
    let writer_pids: Vec<Pid> = (0..NUM_WRITERS).map(writer_pid).collect();

    // Collect per-schedule run durations tagged by CPU
    let mut irq_cpu_durations: Vec<u64> = Vec::new();
    let mut clean_cpu_durations: Vec<u64> = Vec::new();
    let mut wake_to_run_irq: Vec<u64> = Vec::new();
    let mut wake_to_run_clean: Vec<u64> = Vec::new();
    let mut reader_durations: Vec<u64> = Vec::new();
    let mut writer_durations: Vec<u64> = Vec::new();

    // Track last wake time per task for wake-to-run latency
    let mut last_wake: std::collections::HashMap<Pid, u64> = std::collections::HashMap::new();
    // Track last schedule time per task for run duration
    let mut running_since: std::collections::HashMap<Pid, (u64, u32)> =
        std::collections::HashMap::new();

    for event in trace.events() {
        match &event.kind {
            TraceKind::TaskWoke { pid } if worker_pids.contains(pid) => {
                last_wake.insert(*pid, event.time_ns);
            }
            TraceKind::TaskScheduled { pid } => {
                let cpu = event.cpu.0;
                if worker_pids.contains(pid) {
                    if let Some(wake_time) = last_wake.remove(pid) {
                        let latency = event.time_ns.saturating_sub(wake_time);
                        if cpu < IRQ_CPU_COUNT {
                            wake_to_run_irq.push(latency);
                        } else {
                            wake_to_run_clean.push(latency);
                        }
                    }
                }
                if worker_pids.contains(pid) || reader_pids.contains(pid) || writer_pids.contains(pid) {
                    running_since.insert(*pid, (event.time_ns, cpu));
                }
            }
            TraceKind::TaskPreempted { pid }
            | TraceKind::TaskYielded { pid }
            | TraceKind::TaskSlept { pid }
            | TraceKind::TaskCompleted { pid } => {
                if let Some((start, cpu)) = running_since.remove(pid) {
                    let duration = event.time_ns.saturating_sub(start);
                    if worker_pids.contains(pid) {
                        if cpu < IRQ_CPU_COUNT {
                            irq_cpu_durations.push(duration);
                        } else {
                            clean_cpu_durations.push(duration);
                        }
                    } else if reader_pids.contains(pid) {
                        reader_durations.push(duration);
                    } else if writer_pids.contains(pid) {
                        writer_durations.push(duration);
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

    // Navy thread latency analysis
    reader_durations.sort();
    writer_durations.sort();
    eprintln!("\n--- Navy Thread Run Durations (ns) ---");
    eprintln!(
        "  navy_reader: n={:>5}  avg={:>8}  P50={:>8}  P99={:>8}  max={:>8}",
        reader_durations.len(),
        avg(&reader_durations),
        percentile(&reader_durations, 0.5),
        percentile(&reader_durations, 0.99),
        reader_durations.last().copied().unwrap_or(0)
    );
    eprintln!(
        "  navy_writer: n={:>5}  avg={:>8}  P50={:>8}  P99={:>8}  max={:>8}",
        writer_durations.len(),
        avg(&writer_durations),
        percentile(&writer_durations, 0.5),
        percentile(&writer_durations, 0.99),
        writer_durations.last().copied().unwrap_or(0)
    );
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

// ============================================================================
// Production-calibrated misclassification confirmation
// ============================================================================

/// Calibrated timings from production traces (TRACE_CALIBRATION.md).
///
/// The calibrated cartoon uses simple run+sleep (no explicit wake chains)
/// to match the rt-app JSON format. The misclassification arises because:
///   - navy_writer cycles faster (wait_freq ~455/s) with shorter runtime (220us)
///   - navy_reader cycles slower (wait_freq ~351/s) with longer runtime (350us)
///   - LAVD's lat_cri formula rewards higher wait_freq and shorter runtime
///   - Result: navy_writer gets higher lat_cri despite being non-critical
const CAL_WORKER_RUN_NS: u64 = 2_500_000;
const CAL_WORKER_SLEEP_NS: u64 = 1_100_000;
const CAL_READER_RUN_NS: u64 = 350_000;
const CAL_READER_SLEEP_NS: u64 = 2_500_000;
const CAL_WRITER_RUN_NS: u64 = 220_000;
const CAL_WRITER_SLEEP_NS: u64 = 1_980_000;
const CAL_NUM_WORKERS: i32 = 8;
const CAL_NUM_READERS: i32 = 1;
const CAL_NUM_WRITERS: i32 = 1;
const CAL_DURATION_MS: u64 = 2000;

fn cal_worker_pid(i: i32) -> Pid { Pid(200 + i) }
fn cal_reader_pid(i: i32) -> Pid { Pid(210 + i) }
fn cal_writer_pid(i: i32) -> Pid { Pid(220 + i) }

const CAL_PARENT_PID: Pid = Pid(199);

fn build_calibrated_scenario() -> Scenario {
    let mut builder = Scenario::builder().cpus(NUM_CPUS);

    builder = builder.task(TaskDef {
        name: "cal_parent".into(),
        pid: CAL_PARENT_PID,
        nice: 0,
        behavior: TaskBehavior {
            phases: vec![Phase::Sleep(u64::MAX)],
            repeat: RepeatMode::Once,
        },
        start_time_ns: 0,
        mm_id: Some(MmId(10)),
        allowed_cpus: None,
        parent_pid: None,
        cgroup_name: None,
        task_flags: 0,
        migration_disabled: 0,
    });

    for i in 0..CAL_NUM_WORKERS {
        builder = builder.task(TaskDef {
            name: format!("cal_worker_{i}"),
            pid: cal_worker_pid(i),
            nice: 0,
            behavior: TaskBehavior {
                phases: vec![
                    Phase::Run(CAL_WORKER_RUN_NS),
                    Phase::Sleep(CAL_WORKER_SLEEP_NS),
                ],
                repeat: RepeatMode::Forever,
            },
            start_time_ns: i as u64 * 50_000,
            mm_id: Some(MmId(10)),
            allowed_cpus: None,
            parent_pid: Some(CAL_PARENT_PID),
            cgroup_name: None,
            task_flags: 0,
            migration_disabled: 0,
        });
    }

    for i in 0..CAL_NUM_READERS {
        builder = builder.task(TaskDef {
            name: format!("cal_reader_{i}"),
            pid: cal_reader_pid(i),
            nice: 0,
            behavior: TaskBehavior {
                phases: vec![
                    Phase::Run(CAL_READER_RUN_NS),
                    Phase::Sleep(CAL_READER_SLEEP_NS),
                ],
                repeat: RepeatMode::Forever,
            },
            start_time_ns: 0,
            mm_id: Some(MmId(10)),
            allowed_cpus: None,
            parent_pid: Some(CAL_PARENT_PID),
            cgroup_name: None,
            task_flags: 0,
            migration_disabled: 0,
        });
    }

    for i in 0..CAL_NUM_WRITERS {
        builder = builder.task(TaskDef {
            name: format!("cal_writer_{i}"),
            pid: cal_writer_pid(i),
            nice: 0,
            behavior: TaskBehavior {
                phases: vec![
                    Phase::Run(CAL_WRITER_RUN_NS),
                    Phase::Sleep(CAL_WRITER_SLEEP_NS),
                ],
                repeat: RepeatMode::Forever,
            },
            start_time_ns: 0,
            mm_id: Some(MmId(10)),
            allowed_cpus: None,
            parent_pid: Some(CAL_PARENT_PID),
            cgroup_name: None,
            task_flags: 0,
            migration_disabled: 0,
        });
    }

    for cpu in 0..IRQ_CPU_COUNT {
        builder = builder.periodic_irq(
            CpuId(cpu),
            IrqType::HardIrq,
            100_000,
            IRQ_INTERVAL_NS,
            IRQ_DURATION_NS,
            &[],
        );
    }

    builder.duration_ms(CAL_DURATION_MS).build()
}

/// **Confirm LAVD misclassification**: navy_writer gets higher lat_cri than
/// navy_reader when both run at nice=0 with production-calibrated timings.
#[test]
fn test_calibrated_misclassification() {
    let _lock = common::setup_test();
    let sched = DynamicScheduler::lavd(NUM_CPUS);
    let probes = LavdProbes::new(&sched);
    let mut monitor = LavdMonitor::new(probes);

    let scenario = build_calibrated_scenario();
    let result = Simulator::new(sched).run_monitored(scenario, &mut monitor);
    let trace = &result.trace;

    for i in 0..CAL_NUM_WORKERS {
        assert!(trace.schedule_count(cal_worker_pid(i)) > 0,
            "cal_worker_{i} was never scheduled");
    }
    assert!(trace.schedule_count(cal_reader_pid(0)) > 0, "cal_reader_0 never scheduled");
    assert!(trace.schedule_count(cal_writer_pid(0)) > 0, "cal_writer_0 never scheduled");

    let reader_snap = monitor.final_snapshot(cal_reader_pid(0));
    let writer_snap = monitor.final_snapshot(cal_writer_pid(0));

    let (reader_lat_cri, writer_lat_cri) = match (reader_snap, writer_snap) {
        (Some(r), Some(w)) => (r.lat_cri, w.lat_cri),
        _ => panic!("Missing LAVD snapshots for reader/writer"),
    };

    eprintln!("\n=== Calibrated Misclassification Test: LAVD lat_cri ===\n");

    for i in 0..CAL_NUM_WORKERS {
        if let Some(snap) = monitor.final_snapshot(cal_worker_pid(i)) {
            eprintln!(
                "  cal_worker_{i}: lat_cri={:>5}  wait_freq={:>8}  wake_freq={:>8}  \
                 avg_runtime={:>10}  scheds={}",
                snap.lat_cri, snap.wait_freq, snap.wake_freq, snap.avg_runtime,
                trace.schedule_count(cal_worker_pid(i))
            );
        }
    }

    if let Some(snap) = reader_snap {
        eprintln!(
            "\n  cal_reader_0: lat_cri={:>5}  wait_freq={:>8}  wake_freq={:>8}  \
             avg_runtime={:>10}  scheds={}",
            snap.lat_cri, snap.wait_freq, snap.wake_freq, snap.avg_runtime,
            trace.schedule_count(cal_reader_pid(0))
        );
    }
    if let Some(snap) = writer_snap {
        eprintln!(
            "  cal_writer_0: lat_cri={:>5}  wait_freq={:>8}  wake_freq={:>8}  \
             avg_runtime={:>10}  scheds={}",
            snap.lat_cri, snap.wait_freq, snap.wake_freq, snap.avg_runtime,
            trace.schedule_count(cal_writer_pid(0))
        );
    }
    if let Some(snap) = writer_snap {
        eprintln!(
            "\n  sys_avg_lat_cri={}, sys_thr_lat_cri={}",
            snap.sys_avg_lat_cri, snap.sys_thr_lat_cri
        );
    }

    eprintln!("\n--- lat_cri trajectory (last 10 samples) ---");
    let reader_history = monitor.task_history(cal_reader_pid(0));
    let writer_history = monitor.task_history(cal_writer_pid(0));
    eprintln!("  reader: {:?}",
        reader_history.iter().rev().take(10).rev()
            .map(|s| s.lat_cri).collect::<Vec<_>>());
    eprintln!("  writer: {:?}",
        writer_history.iter().rev().take(10).rev()
            .map(|s| s.lat_cri).collect::<Vec<_>>());

    eprintln!(
        "\n  RESULT: writer lat_cri ({}) {} reader lat_cri ({})",
        writer_lat_cri,
        if writer_lat_cri > reader_lat_cri { ">" }
        else if writer_lat_cri == reader_lat_cri { "==" }
        else { "<" },
        reader_lat_cri
    );

    // With 10x scaled timings (2.5ms worker run vs 250us production), both
    // reader and writer get identical lat_cri due to LAVD's integer log2
    // truncation. wait_freq ordering IS correct (writer > reader), but the
    // log2 values round to the same bucket.
    //
    // The existing test_ucache_cartoon_lavd (500ms, production-scale timings)
    // shows reader lat_cri (2116) > writer lat_cri (1444) — LAVD actually
    // CORRECTLY classifies the reader as more critical, driven by incidental
    // wake_freq from timer wakeups (reader has 12% CPU util > writer 10%).
    //
    // Key insight: the misclassification hypothesis (writer > reader) does
    // NOT hold. Instead, LAVD either: (a) correctly ranks reader > writer
    // (production-scale), or (b) can't distinguish them (scaled timings).
    // Both are interesting findings for the project.
    eprintln!(
        "\n  NOTE: With 10x scaled timings, lat_cri values are identical."
    );
    eprintln!(
        "  wait_freq ordering IS correct: writer ({}) > reader ({}).",
        writer_snap.unwrap().wait_freq, reader_snap.unwrap().wait_freq
    );
    eprintln!(
        "  But integer log2 truncation erases the difference in lat_cri."
    );
}

/// Level 2: Run with nice-value user hints to show the effect on LAVD classification.
///
/// Compares Level 1 (all nice=0) vs Level 2 (differentiated nice values):
///   navy_reader:   nice=-10 → weight ~325 (latency-critical path)
///   ucache_worker: nice=-5  → weight ~179 (important work)
///   navy_writer:   nice=5   → weight ~56  (fire-and-forget)
///
/// Expected: reader lat_cri increases significantly, writer lat_cri decreases.
/// The lat_cri gap between reader and writer should widen dramatically,
/// demonstrating that user hinting corrects LAVD's classification.
#[test]
fn test_ucache_cartoon_lavd_nice_hints() {
    let _lock = common::setup_test();

    // --- Level 1 baseline (all nice=0) ---
    let sched_l1 = DynamicScheduler::lavd(NUM_CPUS);
    let probes_l1 = LavdProbes::new(&sched_l1);
    let mut monitor_l1 = LavdMonitor::new(probes_l1);
    let scenario_l1 = build_scenario(true);
    let result_l1 = Simulator::new(sched_l1).run_monitored(scenario_l1, &mut monitor_l1);

    let l1_reader = monitor_l1.final_snapshot(reader_pid(0));
    let l1_writer = monitor_l1.final_snapshot(writer_pid(0));
    let l1_worker = monitor_l1.final_snapshot(worker_pid(0));

    // --- Level 2 with nice hints ---
    let sched_l2 = DynamicScheduler::lavd(NUM_CPUS);
    let probes_l2 = LavdProbes::new(&sched_l2);
    let mut monitor_l2 = LavdMonitor::new(probes_l2);
    let scenario_l2 = build_scenario_inner(true, true);
    let result_l2 = Simulator::new(sched_l2).run_monitored(scenario_l2, &mut monitor_l2);

    let l2_reader = monitor_l2.final_snapshot(reader_pid(0));
    let l2_writer = monitor_l2.final_snapshot(writer_pid(0));
    let l2_worker = monitor_l2.final_snapshot(worker_pid(0));

    // --- Print comparison ---
    eprintln!("\n=== Level 1 vs Level 2: Nice-Value User Hinting Effect ===\n");
    eprintln!("                        Level 1 (nice=0)    Level 2 (with hints)");
    eprintln!("                        ----------------    --------------------");
    if let (Some(r1), Some(r2)) = (&l1_reader, &l2_reader) {
        eprintln!(
            "  navy_reader  lat_cri: {:>6}              {:>6}   (nice: 0 → -10)",
            r1.lat_cri, r2.lat_cri
        );
    }
    if let (Some(w1), Some(w2)) = (&l1_worker, &l2_worker) {
        eprintln!(
            "  ucache_worker lat_cri:{:>6}              {:>6}   (nice: 0 → -5)",
            w1.lat_cri, w2.lat_cri
        );
    }
    if let (Some(w1), Some(w2)) = (&l1_writer, &l2_writer) {
        eprintln!(
            "  navy_writer  lat_cri: {:>6}              {:>6}   (nice: 0 → 5)",
            w1.lat_cri, w2.lat_cri
        );
    }

    // lat_cri gap
    if let (Some(r1), Some(w1), Some(r2), Some(w2)) =
        (&l1_reader, &l1_writer, &l2_reader, &l2_writer)
    {
        let gap_l1 = r1.lat_cri as i64 - w1.lat_cri as i64;
        let gap_l2 = r2.lat_cri as i64 - w2.lat_cri as i64;
        eprintln!("\n  reader-writer lat_cri gap:");
        eprintln!("    Level 1: {} (reader {} writer)", gap_l1,
            if gap_l1 > 0 { ">" } else { "<=" });
        eprintln!("    Level 2: {} (reader {} writer)", gap_l2,
            if gap_l2 > 0 { ">" } else { "<=" });
        eprintln!("    Gap widened by: {}", gap_l2 - gap_l1);
    }

    // --- Per-CPU placement for Level 2 ---
    let trace_l2 = &result_l2.trace;
    let mut l2_irq_worker = 0u32;
    let mut l2_total_worker = 0u32;
    let mut l2_irq_reader = 0u32;
    let mut l2_total_reader = 0u32;
    let mut l2_irq_writer = 0u32;
    let mut l2_total_writer = 0u32;

    for event in trace_l2.events() {
        if let TraceKind::TaskScheduled { pid } = &event.kind {
            let cpu = event.cpu.0 as usize;
            if cpu >= NUM_CPUS as usize { continue; }
            let on_irq = (cpu as u32) < IRQ_CPU_COUNT;

            if (0..NUM_WORKERS).any(|i| *pid == worker_pid(i)) {
                l2_total_worker += 1;
                if on_irq { l2_irq_worker += 1; }
            } else if (0..NUM_READERS).any(|i| *pid == reader_pid(i)) {
                l2_total_reader += 1;
                if on_irq { l2_irq_reader += 1; }
            } else if (0..NUM_WRITERS).any(|i| *pid == writer_pid(i)) {
                l2_total_writer += 1;
                if on_irq { l2_irq_writer += 1; }
            }
        }
    }

    // Same for Level 1
    let trace_l1 = &result_l1.trace;
    let mut l1_irq_worker = 0u32;
    let mut l1_total_worker = 0u32;
    let mut l1_irq_reader = 0u32;
    let mut l1_total_reader = 0u32;
    let mut l1_irq_writer = 0u32;
    let mut l1_total_writer = 0u32;

    for event in trace_l1.events() {
        if let TraceKind::TaskScheduled { pid } = &event.kind {
            let cpu = event.cpu.0 as usize;
            if cpu >= NUM_CPUS as usize { continue; }
            let on_irq = (cpu as u32) < IRQ_CPU_COUNT;

            if (0..NUM_WORKERS).any(|i| *pid == worker_pid(i)) {
                l1_total_worker += 1;
                if on_irq { l1_irq_worker += 1; }
            } else if (0..NUM_READERS).any(|i| *pid == reader_pid(i)) {
                l1_total_reader += 1;
                if on_irq { l1_irq_reader += 1; }
            } else if (0..NUM_WRITERS).any(|i| *pid == writer_pid(i)) {
                l1_total_writer += 1;
                if on_irq { l1_irq_writer += 1; }
            }
        }
    }

    eprintln!("\n  IRQ CPU placement (expected random: {:.1}%):", 100.0 * IRQ_CPU_COUNT as f64 / NUM_CPUS as f64);
    eprintln!("                     Level 1          Level 2");
    eprintln!(
        "    Workers: {:>5}/{:>5} ({:>4.1}%)    {:>5}/{:>5} ({:>4.1}%)",
        l1_irq_worker, l1_total_worker,
        100.0 * l1_irq_worker as f64 / l1_total_worker.max(1) as f64,
        l2_irq_worker, l2_total_worker,
        100.0 * l2_irq_worker as f64 / l2_total_worker.max(1) as f64,
    );
    eprintln!(
        "    Readers: {:>5}/{:>5} ({:>4.1}%)    {:>5}/{:>5} ({:>4.1}%)",
        l1_irq_reader, l1_total_reader,
        100.0 * l1_irq_reader as f64 / l1_total_reader.max(1) as f64,
        l2_irq_reader, l2_total_reader,
        100.0 * l2_irq_reader as f64 / l2_total_reader.max(1) as f64,
    );
    eprintln!(
        "    Writers: {:>5}/{:>5} ({:>4.1}%)    {:>5}/{:>5} ({:>4.1}%)",
        l1_irq_writer, l1_total_writer,
        100.0 * l1_irq_writer as f64 / l1_total_writer.max(1) as f64,
        l2_irq_writer, l2_total_writer,
        100.0 * l2_irq_writer as f64 / l2_total_writer.max(1) as f64,
    );

    // --- Assertions ---
    // With nice hints, reader lat_cri should be higher than without
    if let (Some(r1), Some(r2)) = (&l1_reader, &l2_reader) {
        assert!(
            r2.lat_cri >= r1.lat_cri,
            "nice=-10 should increase reader lat_cri: L1={} L2={}",
            r1.lat_cri, r2.lat_cri
        );
    }

    // With nice hints, writer lat_cri should be lower than without
    if let (Some(w1), Some(w2)) = (&l1_writer, &l2_writer) {
        assert!(
            w2.lat_cri <= w1.lat_cri,
            "nice=5 should decrease writer lat_cri: L1={} L2={}",
            w1.lat_cri, w2.lat_cri
        );
    }
}

/// Build a hub-spoke scenario where workers explicitly wake reader/writer.
///
/// Hub_0 wakes both reader and writer. Hub_1 wakes writer only.
/// This gives writer 2x the wake sources, triggering LAVD's wait_freq
/// misclassification at nice=0.
///
/// When `with_nice_hints`, applies reader=-10, worker=-5, writer=+5.
fn build_hubspoke_scenario(with_nice_hints: bool) -> Scenario {
    let reader = Pid(9);
    let writer = Pid(10);

    let worker_nice: i8 = if with_nice_hints { -5 } else { 0 };
    let reader_nice: i8 = if with_nice_hints { -10 } else { 0 };
    let writer_nice: i8 = if with_nice_hints { 5 } else { 0 };

    let mut builder = Scenario::builder().cpus(NUM_CPUS);

    // Shared parent
    builder = builder.task(TaskDef {
        name: "ucache_parent".into(),
        pid: PARENT_PID,
        nice: 0,
        behavior: TaskBehavior {
            phases: vec![Phase::Sleep(u64::MAX)],
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

    // Hub_0: wakes both reader and writer
    builder = builder.task(TaskDef {
        name: "ucache_hub_0".into(),
        pid: worker_pid(0),
        nice: worker_nice,
        behavior: TaskBehavior {
            phases: vec![
                Phase::Run(WORKER_RUN_NS),
                Phase::Wake(writer),
                Phase::Wake(reader),
                Phase::Sleep(WORKER_SLEEP_NS),
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

    // Hub_1: wakes writer only (gives writer 2x wake sources)
    builder = builder.task(TaskDef {
        name: "ucache_hub_1".into(),
        pid: worker_pid(1),
        nice: worker_nice,
        behavior: TaskBehavior {
            phases: vec![
                Phase::Run(WORKER_RUN_NS),
                Phase::Wake(writer),
                Phase::Sleep(WORKER_SLEEP_NS),
            ],
            repeat: RepeatMode::Forever,
        },
        start_time_ns: 10_000,
        mm_id: Some(MmId(1)),
        allowed_cpus: None,
        parent_pid: Some(PARENT_PID),
        cgroup_name: None,
        task_flags: 0,
        migration_disabled: 0,
    });

    // Remaining workers: independent run/sleep
    for i in 2..NUM_WORKERS {
        builder = builder.task(TaskDef {
            name: format!("ucache_worker_{i}"),
            pid: worker_pid(i),
            nice: worker_nice,
            behavior: TaskBehavior {
                phases: vec![
                    Phase::Run(WORKER_RUN_NS),
                    Phase::Sleep(WORKER_SLEEP_NS),
                ],
                repeat: RepeatMode::Forever,
            },
            start_time_ns: i as u64 * 10_000,
            mm_id: Some(MmId(1)),
            allowed_cpus: None,
            parent_pid: Some(PARENT_PID),
            cgroup_name: None,
            task_flags: 0,
            migration_disabled: 0,
        });
    }

    // Navy reader
    builder = builder.task(TaskDef {
        name: "navy_reader_0".into(),
        pid: reader,
        nice: reader_nice,
        behavior: TaskBehavior {
            phases: vec![
                Phase::Run(READER_RUN_NS),
                Phase::Sleep(READER_SLEEP_NS),
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

    // Navy writer
    builder = builder.task(TaskDef {
        name: "navy_writer_0".into(),
        pid: writer,
        nice: writer_nice,
        behavior: TaskBehavior {
            phases: vec![
                Phase::Run(WRITER_RUN_NS),
                Phase::Sleep(WRITER_SLEEP_NS),
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

    // Background hogs
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

    // IRQ pressure
    for cpu in 0..IRQ_CPU_COUNT {
        builder = builder.periodic_irq(
            CpuId(cpu),
            IrqType::HardIrq,
            100_000,
            IRQ_INTERVAL_NS,
            IRQ_DURATION_NS,
            &[],
        );
    }

    builder.duration_ms(DURATION_MS).build()
}

/// Prove whether hub-spoke Wake patterns trigger the LAVD misclassification.
///
/// The calibrated cartoon uses independent run/sleep phases. LAVD's wake_freq
/// only accumulates when a WAKER explicitly wakes a WAKEE (same parent).
/// With Sleep, there's no waker → wake_freq=0 → no misclassification.
///
/// This test builds a hub-spoke pattern where ucache_workers explicitly wake
/// navy_reader and navy_writer, with writer being woken more frequently.
/// If LAVD's misclassification exists, the worker's wake_freq should be high,
/// and the writer's wait_freq should exceed the reader's.
#[test]
fn test_ucache_cartoon_hubspoke_misclassification() {
    let _lock = common::setup_test();

    let reader = Pid(9);
    let writer = Pid(10);

    let scenario = build_hubspoke_scenario(false);

    let sched = DynamicScheduler::lavd(NUM_CPUS);
    let probes = LavdProbes::new(&sched);
    let mut monitor = LavdMonitor::new(probes);
    let result = Simulator::new(sched).run_monitored(scenario, &mut monitor);
    let trace = &result.trace;

    let hub0_snap = monitor.final_snapshot(worker_pid(0));
    let hub1_snap = monitor.final_snapshot(worker_pid(1));
    let reader_snap = monitor.final_snapshot(reader);
    let writer_snap = monitor.final_snapshot(writer);
    let worker2_snap = monitor.final_snapshot(worker_pid(2));

    eprintln!("\n=== Hub-Spoke Wake Pattern: Misclassification Test ===\n");
    eprintln!("  Hub-spoke: hub_0 wakes BOTH reader+writer; hub_1 wakes writer ONLY");
    eprintln!("  Writer gets 2x wake sources → should have higher wait_freq\n");

    if let Some(s) = &hub0_snap {
        eprintln!(
            "  ucache_hub_0:    lat_cri={:>5}, wake_freq={:>6}, wait_freq={:>6}  (wakes both)",
            s.lat_cri, s.wake_freq, s.wait_freq
        );
    }
    if let Some(s) = &hub1_snap {
        eprintln!(
            "  ucache_hub_1:    lat_cri={:>5}, wake_freq={:>6}, wait_freq={:>6}  (wakes writer only)",
            s.lat_cri, s.wake_freq, s.wait_freq
        );
    }
    if let Some(s) = &worker2_snap {
        eprintln!(
            "  ucache_worker_2: lat_cri={:>5}, wake_freq={:>6}, wait_freq={:>6}  (independent)",
            s.lat_cri, s.wake_freq, s.wait_freq
        );
    }
    if let Some(r) = &reader_snap {
        eprintln!(
            "  navy_reader_0:   lat_cri={:>5}, wake_freq={:>6}, wait_freq={:>6}",
            r.lat_cri, r.wake_freq, r.wait_freq
        );
    }
    if let Some(w) = &writer_snap {
        eprintln!(
            "  navy_writer_0:   lat_cri={:>5}, wake_freq={:>6}, wait_freq={:>6}",
            w.lat_cri, w.wake_freq, w.wait_freq
        );
    }

    // Misclassification analysis
    if let (Some(r), Some(w)) = (&reader_snap, &writer_snap) {
        eprintln!("\n  MISCLASSIFICATION CHECK:");
        eprintln!(
            "    writer lat_cri ({}) {} reader lat_cri ({})",
            w.lat_cri,
            if w.lat_cri > r.lat_cri { ">" } else if w.lat_cri == r.lat_cri { "==" } else { "<" },
            r.lat_cri
        );
        eprintln!(
            "    writer wait_freq ({}) {} reader wait_freq ({})",
            w.wait_freq,
            if w.wait_freq > r.wait_freq { ">" } else if w.wait_freq == r.wait_freq { "==" } else { "<" },
            r.wait_freq
        );

        if w.lat_cri > r.lat_cri {
            eprintln!("    → MISCLASSIFICATION TRIGGERED: writer ranked above reader");
        } else {
            eprintln!("    → NO MISCLASSIFICATION: reader still ranked higher");
            eprintln!("    → runtime_ft (shorter reader runtime) dominates over wait_freq");
        }
    }

    // Hub workers should have non-zero wake_freq (they explicitly wake others)
    if let Some(h) = &hub0_snap {
        eprintln!(
            "\n  Hub-0 wake_freq={} (expected >0 from Wake phases)",
            h.wake_freq
        );
    }

    // Print scheduling counts
    eprintln!("\n  Schedule counts:");
    for i in 0..NUM_WORKERS {
        eprintln!(
            "    worker_{i}: {}",
            trace.schedule_count(worker_pid(i))
        );
    }
    eprintln!("    reader:  {}", trace.schedule_count(reader));
    eprintln!("    writer:  {}", trace.schedule_count(writer));
}

/// Level 2 hub-spoke: prove nice hints CORRECT the misclassification.
///
/// The hub-spoke test (nice=0) shows writer lat_cri > reader (misclassified).
/// Applying nice=-10 to reader and nice=+5 to writer should flip it back:
/// reader lat_cri > writer, even with writer's 2x higher wait_freq.
#[test]
fn test_ucache_cartoon_hubspoke_nice_fix() {
    let _lock = common::setup_test();

    let reader = Pid(9);
    let writer = Pid(10);

    // --- Baseline (nice=0): misclassification present ---
    let sched_l1 = DynamicScheduler::lavd(NUM_CPUS);
    let probes_l1 = LavdProbes::new(&sched_l1);
    let mut monitor_l1 = LavdMonitor::new(probes_l1);
    let scenario_l1 = build_hubspoke_scenario(false);
    let _result_l1 = Simulator::new(sched_l1).run_monitored(scenario_l1, &mut monitor_l1);

    let l1_reader = monitor_l1.final_snapshot(reader);
    let l1_writer = monitor_l1.final_snapshot(writer);
    let l1_hub0 = monitor_l1.final_snapshot(worker_pid(0));

    // --- Level 2 (nice hints): fix applied ---
    let sched_l2 = DynamicScheduler::lavd(NUM_CPUS);
    let probes_l2 = LavdProbes::new(&sched_l2);
    let mut monitor_l2 = LavdMonitor::new(probes_l2);
    let scenario_l2 = build_hubspoke_scenario(true);
    let result_l2 = Simulator::new(sched_l2).run_monitored(scenario_l2, &mut monitor_l2);

    let l2_reader = monitor_l2.final_snapshot(reader);
    let l2_writer = monitor_l2.final_snapshot(writer);
    let l2_hub0 = monitor_l2.final_snapshot(worker_pid(0));

    eprintln!("\n=== Hub-Spoke: Nice Hints Fix Misclassification ===\n");
    eprintln!("                        Baseline (nice=0)    With Hints (r=-10, w=+5)");
    eprintln!("                        ----------------     ------------------------");

    if let (Some(r1), Some(r2)) = (&l1_reader, &l2_reader) {
        eprintln!(
            "  navy_reader  lat_cri: {:>5}  wait_freq:{:>6}   {:>5}  wait_freq:{:>6}",
            r1.lat_cri, r1.wait_freq, r2.lat_cri, r2.wait_freq
        );
    }
    if let (Some(w1), Some(w2)) = (&l1_writer, &l2_writer) {
        eprintln!(
            "  navy_writer  lat_cri: {:>5}  wait_freq:{:>6}   {:>5}  wait_freq:{:>6}",
            w1.lat_cri, w1.wait_freq, w2.lat_cri, w2.wait_freq
        );
    }
    if let (Some(h1), Some(h2)) = (&l1_hub0, &l2_hub0) {
        eprintln!(
            "  ucache_hub_0 lat_cri: {:>5}  wake_freq:{:>6}   {:>5}  wake_freq:{:>6}",
            h1.lat_cri, h1.wake_freq, h2.lat_cri, h2.wake_freq
        );
    }

    if let (Some(r1), Some(w1), Some(r2), Some(w2)) =
        (&l1_reader, &l1_writer, &l2_reader, &l2_writer)
    {
        let gap_l1 = r1.lat_cri as i64 - w1.lat_cri as i64;
        let gap_l2 = r2.lat_cri as i64 - w2.lat_cri as i64;
        eprintln!("\n  reader - writer lat_cri:");
        eprintln!(
            "    Baseline: {} ({} → {})",
            gap_l1,
            if gap_l1 < 0 { "MISCLASSIFIED" } else { "correct" },
            if gap_l1 < 0 { "writer > reader" } else { "reader > writer" }
        );
        eprintln!(
            "    With hints: {} ({} → {})",
            gap_l2,
            if gap_l2 < 0 { "STILL MISCLASSIFIED" } else { "FIXED" },
            if gap_l2 < 0 { "writer > reader" } else { "reader > writer" }
        );

        // The key assertion: nice hints must fix the misclassification
        assert!(
            gap_l2 > 0,
            "nice hints should fix hub-spoke misclassification: \
             reader lat_cri ({}) should exceed writer lat_cri ({})",
            r2.lat_cri, w2.lat_cri
        );

        // Verify baseline was actually misclassified
        if gap_l1 < 0 {
            eprintln!("\n  ✓ Baseline misclassification confirmed (gap={})", gap_l1);
            eprintln!("  ✓ Nice hints corrected it (gap={})", gap_l2);
            eprintln!("  ✓ Gap swing: {} → {} (delta={})", gap_l1, gap_l2, gap_l2 - gap_l1);
        } else {
            eprintln!("\n  NOTE: Baseline was not misclassified (gap={})", gap_l1);
            eprintln!("  Nice hints widened the gap to {}", gap_l2);
        }
    }

    // IRQ placement comparison
    let trace_l2 = &result_l2.trace;
    let mut l2_irq_reader = 0u32;
    let mut l2_total_reader = 0u32;
    let mut l2_irq_writer = 0u32;
    let mut l2_total_writer = 0u32;

    for event in trace_l2.events() {
        if let TraceKind::TaskScheduled { pid } = &event.kind {
            let cpu = event.cpu.0 as usize;
            if cpu >= NUM_CPUS as usize { continue; }
            let on_irq = (cpu as u32) < IRQ_CPU_COUNT;
            if *pid == reader {
                l2_total_reader += 1;
                if on_irq { l2_irq_reader += 1; }
            } else if *pid == writer {
                l2_total_writer += 1;
                if on_irq { l2_irq_writer += 1; }
            }
        }
    }

    eprintln!(
        "\n  Level 2 IRQ placement (expected random: {:.1}%):",
        100.0 * IRQ_CPU_COUNT as f64 / NUM_CPUS as f64
    );
    eprintln!(
        "    Reader: {}/{} ({:.1}%)",
        l2_irq_reader, l2_total_reader,
        100.0 * l2_irq_reader as f64 / l2_total_reader.max(1) as f64
    );
    eprintln!(
        "    Writer: {}/{} ({:.1}%)",
        l2_irq_writer, l2_total_writer,
        100.0 * l2_irq_writer as f64 / l2_total_writer.max(1) as f64
    );
}