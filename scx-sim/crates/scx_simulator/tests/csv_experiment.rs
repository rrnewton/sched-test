//! CSV-emitting experiment runner for the ucache cartoon workload.
//!
//! Outputs metrics in METRICS_SPECIFICATION.md CSV format to stdout.
//! Designed to be called from scripts/modes/simulator.sh.
//!
//! Configuration via environment variables:
//!   SCX_SIM_CORES       - Number of simulated CPUs (default: 16)
//!   SCX_SIM_SCHEDULER   - "lavd" or "tickless" (default: "lavd")
//!   SCX_SIM_CONDITION   - "level1_nice0" or "level2_nice_hints" (default: "level1_nice0")
//!   SCX_SIM_DURATION_MS - Simulation duration in ms (default: 500)
//!   SCX_SIM_SEED        - PRNG seed (default: 42)
//!   SCX_SIM_PERFETTO    - If set, write Perfetto trace to this path
//!   SCX_SIM_CSV_HEADER  - If "1", print CSV header line first

use std::collections::HashMap;

use scx_simulator::probes::{LavdMonitor, LavdProbes};
use scx_simulator::*;

#[macro_use]
mod common;

// ---- Constants (same as ucache_cartoon_scxsim_direct.rs) ----

const PARENT_PID: Pid = Pid(100);
// Thread counts computed from nr_cpus in thread_counts().
// Target: 0.9 threads/CPU matching production (284 threads on 316 CPUs).
// At 16 CPUs: 10 workers, 1 reader, 1 writer, 2 hogs (= 14 threads, 0.88/CPU)
// At 48 CPUs: 32 workers, 4 readers, 4 writers, 3 hogs (= 43 threads, 0.90/CPU)

const WORKER_RUN_NS: u64 = 250_000;
const WORKER_SLEEP_NS: u64 = 110_000;
const READER_RUN_NS: u64 = 35_000;
const READER_SLEEP_NS: u64 = 250_000;
const WRITER_RUN_NS: u64 = 5_000_000; // 5ms burst write (coalesced flush)
const WRITER_SLEEP_NS: u64 = 25_000_000; // 25ms sleep (batching before flush, ~33 wakes/sec)
const HOG_RUN_NS: u64 = 200_000;
const HOG_SLEEP_NS: u64 = 750_000;

const IRQ_INTERVAL_NS: u64 = 10_000_000; // 10ms period (matching rt-app)
const IRQ_DURATION_NS: u64 = 3_300_000; // 3.3ms burst (33% duty cycle, matching VM BROAD profile)

/// Compute thread counts scaled to nr_cpus (ratio: 1.5 threads/CPU total).
/// Thread counts: 0.9 threads/CPU default, or explicit override via env vars
/// (SCX_SIM_WORKERS, SCX_SIM_READERS, SCX_SIM_WRITERS, SCX_SIM_HOGS).
fn thread_counts(nr_cpus: u32) -> (i32, i32, i32, i32) {
    fn env_i32(name: &str) -> Option<i32> {
        std::env::var(name).ok()?.parse().ok()
    }
    if let Some(w) = env_i32("SCX_SIM_WORKERS") {
        let r = env_i32("SCX_SIM_READERS").unwrap_or(2);
        let wr = env_i32("SCX_SIM_WRITERS").unwrap_or(2);
        let h = env_i32("SCX_SIM_HOGS").unwrap_or(4);
        return (w, r, wr, h);
    }
    let workers = ((nr_cpus * 2 / 3) as i32).max(4);
    let readers = ((nr_cpus / 12) as i32).max(1);
    let writers = ((nr_cpus / 12) as i32).max(1);
    let target_total = (nr_cpus as f64 * 0.9) as i32;
    let hogs = (target_total - workers - readers - writers).max(1);
    (workers, readers, writers, hogs)
}

/// Compute IRQ target CPUs: even-numbered CPUs up to ~1/3 of total.
fn irq_cpus(nr_cpus: u32) -> Vec<u32> {
    let n_irq = (nr_cpus / 3).max(2);
    (0..nr_cpus)
        .filter(|c| c % 2 == 0)
        .take(n_irq as usize)
        .collect()
}

fn worker_pid(i: i32) -> Pid {
    Pid(1 + i)
}
fn reader_pid(i: i32, num_workers: i32) -> Pid {
    Pid(1 + num_workers + i)
}
fn writer_pid(i: i32, num_workers: i32, num_readers: i32) -> Pid {
    Pid(1 + num_workers + num_readers + i)
}
fn hog_pid(i: i32, num_workers: i32, num_readers: i32, num_writers: i32) -> Pid {
    Pid(1 + num_workers + num_readers + num_writers + i)
}

fn build_scenario(
    nr_cpus: u32,
    cpus_per_llc: u32,
    with_nice_hints: bool,
    duration_ms: u64,
) -> Scenario {
    let mut builder = Scenario::builder().cpus(nr_cpus);
    if cpus_per_llc > 0 {
        builder = builder.cpus_per_llc(cpus_per_llc);
    }

    let worker_nice: i8 = if with_nice_hints { -5 } else { 0 };
    let reader_nice: i8 = if with_nice_hints { -10 } else { 0 };
    let writer_nice: i8 = if with_nice_hints { 5 } else { 0 };

    let (num_workers, num_readers, num_writers, num_hogs) =
        if std::env::var("SCX_SIM_FIXED_THREADS").ok().as_deref() == Some("1") {
            // Fixed 16-CPU thread counts regardless of nr_cpus.
            // Use for isolation experiments that vary CPUs while holding threads constant.
            (16_i32, 2, 2, 4)
        } else {
            thread_counts(nr_cpus)
        };
    let irq_cpu_list = irq_cpus(nr_cpus);

    // Shared parent task
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

    for i in 0..num_workers {
        builder = builder.task(TaskDef {
            name: format!("ucache_worker_{i}"),
            pid: worker_pid(i),
            nice: worker_nice,
            behavior: TaskBehavior {
                phases: vec![Phase::Run(WORKER_RUN_NS), Phase::Sleep(WORKER_SLEEP_NS)],
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

    for i in 0..num_readers {
        builder = builder.task(TaskDef {
            name: format!("navy_reader_{i}"),
            pid: reader_pid(i, num_workers),
            nice: reader_nice,
            behavior: TaskBehavior {
                phases: vec![Phase::Run(READER_RUN_NS), Phase::Sleep(READER_SLEEP_NS)],
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

    for i in 0..num_writers {
        builder = builder.task(TaskDef {
            name: format!("navy_writer_{i}"),
            pid: writer_pid(i, num_workers, num_readers),
            nice: writer_nice,
            behavior: TaskBehavior {
                phases: vec![Phase::Run(WRITER_RUN_NS), Phase::Sleep(WRITER_SLEEP_NS)],
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

    for i in 0..num_hogs {
        builder = builder.task(TaskDef {
            name: format!("cpu_hog_{i}"),
            pid: hog_pid(i, num_workers, num_readers, num_writers),
            nice: 10,
            behavior: TaskBehavior {
                phases: vec![Phase::Run(HOG_RUN_NS), Phase::Sleep(HOG_SLEEP_NS)],
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

    // IRQ pressure on even-numbered CPUs (33% duty, matching VM BROAD profile)
    for &cpu in irq_cpu_list.iter() {
        builder = builder.periodic_irq(
            CpuId(cpu),
            IrqType::HardIrq,
            100_000,
            IRQ_INTERVAL_NS,
            IRQ_DURATION_NS,
            &[],
        );
    }

    builder.duration_ms(duration_ms).build()
}

// ---- Metric extraction ----

fn pctl(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[((sorted.len() as f64 * p) as usize).min(sorted.len() - 1)]
}

/// Compute cycle times (inter-schedule intervals) per PID.
fn compute_cycle_times(trace: &Trace, pids: &[Pid], warmup_ns: u64) -> Vec<u64> {
    let mut last_sched: HashMap<Pid, u64> = HashMap::new();
    let mut cycles: Vec<u64> = Vec::new();
    for event in trace.events() {
        if let TraceKind::TaskScheduled { pid } = &event.kind {
            if pids.contains(pid) {
                if let Some(prev) = last_sched.insert(*pid, event.time_ns) {
                    if event.time_ns >= warmup_ns {
                        let delta = event.time_ns.saturating_sub(prev);
                        if delta > 0 {
                            cycles.push(delta);
                        }
                    }
                }
            }
        }
    }
    cycles.sort();
    cycles
}

/// Compute scheduling latencies (wake-to-scheduled) for given PIDs.
fn compute_sched_latencies(trace: &Trace, pids: &[Pid], warmup_ns: u64) -> Vec<u64> {
    let mut last_wake: HashMap<Pid, u64> = HashMap::new();
    let mut latencies: Vec<u64> = Vec::new();
    for event in trace.events() {
        match &event.kind {
            TraceKind::TaskWoke { pid } if pids.contains(pid) => {
                last_wake.insert(*pid, event.time_ns);
            }
            TraceKind::TaskScheduled { pid } if pids.contains(pid) => {
                if let Some(wake_time) = last_wake.remove(pid) {
                    if event.time_ns >= warmup_ns {
                        latencies.push(event.time_ns.saturating_sub(wake_time));
                    }
                }
            }
            _ => {}
        }
    }
    latencies.sort();
    latencies
}

/// Compute time-weighted IRQ exposure for given PIDs.
/// Returns (runtime_on_irq_ns, total_runtime_ns).
fn compute_irq_exposure(
    trace: &Trace,
    pids: &[Pid],
    warmup_ns: u64,
    irq_cpu_list: &[u32],
) -> (u64, u64) {
    let mut running_since: HashMap<Pid, (u64, u32)> = HashMap::new();
    let mut irq_ns: u64 = 0;
    let mut total_ns: u64 = 0;

    for event in trace.events() {
        match &event.kind {
            TraceKind::TaskScheduled { pid } if pids.contains(pid) => {
                running_since.insert(*pid, (event.time_ns, event.cpu.0));
            }
            TraceKind::TaskPreempted { pid }
            | TraceKind::TaskYielded { pid }
            | TraceKind::TaskSlept { pid }
            | TraceKind::TaskCompleted { pid }
                if pids.contains(pid) =>
            {
                if let Some((start, cpu)) = running_since.remove(pid) {
                    // Clamp start to warmup boundary
                    let effective_start = start.max(warmup_ns);
                    if event.time_ns > effective_start {
                        let dur = event.time_ns - effective_start;
                        total_ns += dur;
                        if irq_cpu_list.contains(&cpu) {
                            irq_ns += dur;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    (irq_ns, total_ns)
}

fn emit_csv_row(
    timestamp: &str,
    scheduler: &str,
    condition: &str,
    thread_type: &str,
    thread_id: i32,
    metric: &str,
    percentile: &str,
    value: f64,
    unit: &str,
    n: usize,
    rep: u32,
    notes: &str,
) {
    println!(
        "{},simulator,{},{},{},{},{},{},{},{},{},{},{}",
        timestamp,
        scheduler,
        condition,
        thread_type,
        thread_id,
        metric,
        percentile,
        value,
        unit,
        n,
        rep,
        notes
    );
}

// ---- Percentile metrics ----

fn emit_latency_percentiles(
    timestamp: &str,
    scheduler: &str,
    condition: &str,
    thread_type: &str,
    thread_id: i32,
    metric_name: &str,
    sorted: &[u64],
    rep: u32,
    notes: &str,
) {
    let n = sorted.len();
    if n == 0 {
        return;
    }
    emit_csv_row(
        timestamp,
        scheduler,
        condition,
        thread_type,
        thread_id,
        metric_name,
        "p50",
        pctl(sorted, 0.50) as f64,
        "ns",
        n,
        rep,
        notes,
    );
    emit_csv_row(
        timestamp,
        scheduler,
        condition,
        thread_type,
        thread_id,
        metric_name,
        "p90",
        pctl(sorted, 0.90) as f64,
        "ns",
        n,
        rep,
        notes,
    );
    emit_csv_row(
        timestamp,
        scheduler,
        condition,
        thread_type,
        thread_id,
        metric_name,
        "p99",
        pctl(sorted, 0.99) as f64,
        "ns",
        n,
        rep,
        notes,
    );
    emit_csv_row(
        timestamp,
        scheduler,
        condition,
        thread_type,
        thread_id,
        metric_name,
        "p999",
        pctl(sorted, 0.999) as f64,
        "ns",
        n,
        rep,
        notes,
    );
    emit_csv_row(
        timestamp,
        scheduler,
        condition,
        thread_type,
        thread_id,
        metric_name,
        "max",
        *sorted.last().unwrap() as f64,
        "ns",
        n,
        rep,
        notes,
    );
}

// ---- Main test ----

#[test]
fn csv_experiment_run() {
    let _lock = common::setup_test();

    let nr_cpus: u32 = std::env::var("SCX_SIM_CORES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(16);
    let scheduler = std::env::var("SCX_SIM_SCHEDULER").unwrap_or_else(|_| "lavd".into());
    let condition = std::env::var("SCX_SIM_CONDITION").unwrap_or_else(|_| "level1_nice0".into());
    let duration_ms: u64 = std::env::var("SCX_SIM_DURATION_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(500);
    let seed: u32 = std::env::var("SCX_SIM_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(42);
    let perfetto_path = std::env::var("SCX_SIM_PERFETTO").ok();
    let print_header = std::env::var("SCX_SIM_CSV_HEADER").as_deref() == Ok("1");
    let rep: u32 = std::env::var("SCX_SIM_REP")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let warmup_ms: u64 = std::env::var("SCX_SIM_WARMUP_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let warmup_ns: u64 = warmup_ms * 1_000_000;

    let with_nice_hints = condition.contains("nice_hints") || condition.contains("level2");
    let timestamp = std::env::var("SCX_SIM_TIMESTAMP").unwrap_or_else(|_| {
        // Fall back to current date via system command
        let output = std::process::Command::new("date")
            .arg("+%Y-%m-%d")
            .output()
            .expect("failed to run date");
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    });
    let sched_label = match scheduler.as_str() {
        "lavd" => "LAVD",
        "tickless" => "Tickless",
        _ => &scheduler,
    };

    // Build scenario
    let cpus_per_llc: u32 = std::env::var("SCX_SIM_CPUS_PER_LLC")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(if nr_cpus > 16 { 12 } else { 0 });
    let mut scenario = build_scenario(nr_cpus, cpus_per_llc, with_nice_hints, duration_ms);
    scenario.seed = seed;

    let (num_workers, num_readers, num_writers, _num_hogs) =
        if std::env::var("SCX_SIM_FIXED_THREADS").ok().as_deref() == Some("1") {
            (16_i32, 2, 2, 4)
        } else {
            thread_counts(nr_cpus)
        };
    let irq_cpu_list = irq_cpus(nr_cpus);

    // Create scheduler and optionally attach LAVD monitor
    let use_lavd = scheduler == "lavd";
    let nr_domains = if cpus_per_llc > 0 {
        nr_cpus / cpus_per_llc
    } else {
        1
    };

    if print_header {
        println!("timestamp,mode,scheduler,condition,thread_type,thread_id,metric_name,percentile,value,unit,sample_count,rep,notes");
    }

    let trace = if use_lavd {
        // Match production LAVD config:
        //   --performance --slice-min-us 3000 --slice-max-us 10000 --mig-delta-pct 15
        // Production does NOT use --per-cpu-dsq or --pinned-slice-us.
        // per_cpu_dsq=false, pinned_slice_ns=0 → all tasks use shared cpdom DSQs.
        let sched = if nr_domains > 1 {
            let s = DynamicScheduler::lavd_multi_domain(nr_cpus, nr_domains);
            s.lavd_configure(false, 0, 15);
            s
        } else {
            let s = DynamicScheduler::lavd(nr_cpus);
            s.lavd_configure(false, 0, 15);
            s
        };
        let probes = LavdProbes::new(&sched);
        let mut monitor = LavdMonitor::new(probes);
        let result = Simulator::new(sched).run_monitored(scenario, &mut monitor);

        // Emit LAVD-specific metrics
        let thread_map: Vec<(&str, i32, Pid)> = {
            let mut v = Vec::new();
            for i in 0..num_workers {
                v.push(("cache_worker", i, worker_pid(i)));
            }
            for i in 0..num_readers {
                v.push(("ssd_reader", i, reader_pid(i, num_workers)));
            }
            for i in 0..num_writers {
                v.push(("ssd_writer", i, writer_pid(i, num_workers, num_readers)));
            }
            v
        };
        for (ttype, tid, pid) in &thread_map {
            if let Some(snap) = monitor.final_snapshot(*pid) {
                emit_csv_row(
                    &timestamp,
                    sched_label,
                    &condition,
                    ttype,
                    *tid,
                    "lat_cri",
                    "",
                    snap.lat_cri as f64,
                    "dimensionless",
                    1,
                    rep,
                    "final snapshot",
                );
                emit_csv_row(
                    &timestamp,
                    sched_label,
                    &condition,
                    ttype,
                    *tid,
                    "wake_freq",
                    "",
                    snap.wake_freq as f64,
                    "dimensionless",
                    1,
                    rep,
                    "final snapshot",
                );
                emit_csv_row(
                    &timestamp,
                    sched_label,
                    &condition,
                    ttype,
                    *tid,
                    "wait_freq",
                    "",
                    snap.wait_freq as f64,
                    "dimensionless",
                    1,
                    rep,
                    "final snapshot",
                );
            }
        }

        result.trace
    } else {
        let sched = DynamicScheduler::tickless(nr_cpus);
        Simulator::new(sched).run(scenario)
    };

    // Write Perfetto trace if requested
    if let Some(ref path) = perfetto_path {
        let mut file = std::fs::File::create(path)
            .unwrap_or_else(|e| panic!("failed to create perfetto file {path}: {e}"));
        trace
            .write_perfetto_json(&mut file)
            .unwrap_or_else(|e| panic!("failed to write perfetto trace: {e}"));
        eprintln!("wrote perfetto trace to {path}");
    }

    // ---- E2E cycle times ----
    let worker_pids: Vec<Pid> = (0..num_workers).map(worker_pid).collect();
    let reader_pids: Vec<Pid> = (0..num_readers)
        .map(|i| reader_pid(i, num_workers))
        .collect();
    let _writer_pids: Vec<Pid> = (0..num_writers)
        .map(|i| writer_pid(i, num_workers, num_readers))
        .collect();

    let worker_cycles = compute_cycle_times(&trace, &worker_pids, warmup_ns);
    let reader_cycles = compute_cycle_times(&trace, &reader_pids, warmup_ns);

    // E2E for cache_worker (aggregate across all worker PIDs)
    emit_latency_percentiles(
        &timestamp,
        sched_label,
        &condition,
        "cache_worker",
        0,
        "e2e_latency",
        &worker_cycles,
        rep,
        "runsleep model",
    );

    // E2E for ssd_reader
    emit_latency_percentiles(
        &timestamp,
        sched_label,
        &condition,
        "ssd_reader",
        0,
        "e2e_latency",
        &reader_cycles,
        rep,
        "runsleep model",
    );

    // ---- Scheduling latency ----
    let worker_sched_lat = compute_sched_latencies(&trace, &worker_pids, warmup_ns);
    let reader_sched_lat = compute_sched_latencies(&trace, &reader_pids, warmup_ns);

    emit_latency_percentiles(
        &timestamp,
        sched_label,
        &condition,
        "cache_worker",
        0,
        "sched_latency",
        &worker_sched_lat,
        rep,
        "",
    );

    emit_latency_percentiles(
        &timestamp,
        sched_label,
        &condition,
        "ssd_reader",
        0,
        "sched_latency",
        &reader_sched_lat,
        rep,
        "",
    );

    // ---- IRQ exposure ----
    let (worker_irq_ns, worker_total_ns) =
        compute_irq_exposure(&trace, &worker_pids, warmup_ns, &irq_cpu_list);
    let (reader_irq_ns, reader_total_ns) =
        compute_irq_exposure(&trace, &reader_pids, warmup_ns, &irq_cpu_list);

    if worker_total_ns > 0 {
        let pct = 100.0 * worker_irq_ns as f64 / worker_total_ns as f64;
        emit_csv_row(
            &timestamp,
            sched_label,
            &condition,
            "cache_worker",
            0,
            "irq_exposure",
            "",
            pct,
            "pct",
            worker_total_ns as usize,
            rep,
            "time-weighted",
        );
    }
    if reader_total_ns > 0 {
        let pct = 100.0 * reader_irq_ns as f64 / reader_total_ns as f64;
        emit_csv_row(
            &timestamp,
            sched_label,
            &condition,
            "ssd_reader",
            0,
            "irq_exposure",
            "",
            pct,
            "pct",
            reader_total_ns as usize,
            rep,
            "time-weighted",
        );
    }

    // Also emit count-weighted IRQ exposure
    let mut worker_irq_count = 0u32;
    let mut worker_total_count = 0u32;
    let mut reader_irq_count = 0u32;
    let mut reader_total_count = 0u32;
    for event in trace.events() {
        if event.time_ns < warmup_ns {
            continue;
        }
        if let TraceKind::TaskScheduled { pid } = &event.kind {
            let on_irq = irq_cpu_list.contains(&event.cpu.0);
            if worker_pids.contains(pid) {
                worker_total_count += 1;
                if on_irq {
                    worker_irq_count += 1;
                }
            } else if reader_pids.contains(pid) {
                reader_total_count += 1;
                if on_irq {
                    reader_irq_count += 1;
                }
            }
        }
    }
    if worker_total_count > 0 {
        let pct = 100.0 * worker_irq_count as f64 / worker_total_count as f64;
        emit_csv_row(
            &timestamp,
            sched_label,
            &condition,
            "cache_worker",
            0,
            "irq_exposure_count",
            "",
            pct,
            "pct",
            worker_total_count as usize,
            rep,
            "count-weighted",
        );
    }
    if reader_total_count > 0 {
        let pct = 100.0 * reader_irq_count as f64 / reader_total_count as f64;
        emit_csv_row(
            &timestamp,
            sched_label,
            &condition,
            "ssd_reader",
            0,
            "irq_exposure_count",
            "",
            pct,
            "pct",
            reader_total_count as usize,
            rep,
            "count-weighted",
        );
    }

    // ---- DSQ routing diagnostic ----
    {
        use std::collections::HashMap;
        let mut dsq_counts: HashMap<u64, usize> = HashMap::new();
        for event in trace.events() {
            if event.time_ns < warmup_ns {
                continue;
            }
            match &event.kind {
                TraceKind::DsqInsert { dsq_id, .. } | TraceKind::DsqInsertVtime { dsq_id, .. } => {
                    *dsq_counts.entry(dsq_id.0).or_insert(0) += 1;
                }
                _ => {}
            }
        }

        let mut local = 0usize;
        let mut cpdom = 0usize;
        let mut percpu = 0usize;
        for (&id, &count) in &dsq_counts {
            if id & DsqId::FLAG_BUILTIN != 0 {
                local += count;
            } else if id & (1 << 12) != 0 {
                cpdom += count;
            } else {
                percpu += count;
            }
        }
        let total = local + cpdom + percpu;
        if total > 0 {
            eprintln!(
                "  DSQ routing: local={} ({:.0}%) cpdom={} ({:.0}%) percpu={} ({:.0}%)",
                local,
                100.0 * local as f64 / total as f64,
                cpdom,
                100.0 * cpdom as f64 / total as f64,
                percpu,
                100.0 * percpu as f64 / total as f64,
            );
        }
    }

    eprintln!(
        "csv_experiment: scheduler={} condition={} cores={} cpus_per_llc={} domains={} threads={} duration={}ms seed={}",
        scheduler, condition, nr_cpus, cpus_per_llc, nr_domains,
        num_workers + num_readers + num_writers + _num_hogs, duration_ms, seed
    );
}
