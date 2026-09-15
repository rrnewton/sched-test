//! Tests for IRQ-induced task migration.
//!
//! This test measures whether the scheduler migrates tasks away from IRQ-heavy
//! CPUs to a quiet control CPU. The test:
//!
//! 1. Launches low duty-cycle workers on ALL CPUs to ensure lat_cap measurement
//! 2. Victimizes ALL physical cores except one control CPU with timer IRQs
//! 3. Starts a spinner probe task on a victim CPU
//! 4. Unpins it and measures whether it migrates to the control CPU
//!
//! The spinner probe has nice -10 for higher priority. LAVD should recognize
//! the IRQ load on victim CPUs and potentially migrate the probe to the
//! CPU with best lat_capacity (the control CPU without IRQ load).
//!
//! # Environment Variables
//!
//! - `SCHTEST_IRQ_DURATION`: Test duration in seconds. Defaults to `10`.
//! - `SCHTEST_IRQ_RESERVE_TRACING_CORE`: CPU ID to reserve from IRQ storm for tracing
//!   (e.g., for wprof). When set, this CPU will not receive IRQ disruption, allowing
//!   a tracer to run on it without interference.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use anyhow::Result;

use crate::util::shared::{BumpAllocator, SharedBox};
use crate::util::system::System;

use super::irq_common::{
    get_reserved_tracing_core, get_test_duration, launch_lat_cap_workers, launch_spinner_probe,
    launch_timer_irq_disruption, log_scheduler_info, InterruptSnapshot, TimerIrqHandle,
    DEFAULT_IRQ_HZ, LAT_CAP_WORKER_DUTY_CYCLE_PCT, LAT_CAP_WORKER_PERIOD_MS, NUM_TIMERS,
};

/// Test whether the scheduler migrates tasks from IRQ-heavy CPUs to a quiet CPU.
///
/// This test:
/// 1. Picks one CPU as the "control" (no IRQ load)
/// 2. Launches lat_cap measurement workers on ALL CPUs (to enable LAVD's stolen_time sampling)
/// 3. Launches timer IRQ disruption on ALL other physical cores
/// 4. Starts a spinner probe on a victim CPU, then unpins it
/// 5. Measures which CPU the probe ends up on
fn irq_migration_test() -> Result<()> {
    let system = System::load()?;

    // Collect all physical cores (we'll victimize all but one)
    let cores: Vec<_> = system.cores().iter().collect();
    if cores.len() < 2 {
        anyhow::bail!("Need at least 2 physical cores for this test");
    }

    // Pick control core (last physical core) - we'll leave ALL its hyperthreads quiet
    let control_core = &cores[cores.len() - 1];
    let control_core_id = control_core.id();
    let _control_cpu = control_core.hyperthreads().first().unwrap().clone();

    // Pick initial victim (first physical core's first hyperthread)
    let initial_victim_core = &cores[0];
    let initial_victim_cpu = initial_victim_core.hyperthreads().first().unwrap().clone();
    let initial_victim_cpu_id = initial_victim_cpu.id();

    // Check if a CPU should be reserved for tracing (e.g., for wprof)
    let reserved_tracing_cpu = get_reserved_tracing_core();

    // Collect all victim CPUs: ALL hyperthreads of each core, excluding control core
    // This ensures we stress both hyperthreads so the task can't escape to a sibling
    let mut victim_cpus = Vec::new();
    for core in &cores {
        // Skip the control core entirely (all its hyperthreads)
        if core.id() == control_core_id {
            continue;
        }
        // Add ALL hyperthreads of this core
        for ht in core.hyperthreads() {
            // Skip the reserved tracing core if set
            if let Some(reserved_cpu) = reserved_tracing_cpu {
                if ht.id() == reserved_cpu as i32 {
                    continue;
                }
            }
            victim_cpus.push(ht.clone());
        }
    }

    eprintln!("=== IRQ Migration Test (Spinner Probe) ===");
    eprintln!("Physical cores: {}", cores.len());
    eprintln!("Control core: {} (all hyperthreads quiet)", control_core_id);
    eprintln!("Control CPUs: {:?}", control_core.hyperthreads().iter().map(|h| h.id()).collect::<Vec<_>>());
    if let Some(reserved_cpu) = reserved_tracing_cpu {
        eprintln!("Reserved tracing CPU: {} (no IRQ load)", reserved_cpu);
    }
    let victim_cpu_ids: Vec<i32> = victim_cpus.iter().map(|h| h.id()).collect();
    eprintln!(
        "Victim CPUs: {} (all hyperthreads, excluding control core{})",
        victim_cpus.len(),
        if reserved_tracing_cpu.is_some() { " and tracing" } else { "" }
    );
    eprintln!("Victim CPU IDs: {:?}", victim_cpu_ids);
    eprintln!("Initial probe CPU: {} (will be unpinned)", initial_victim_cpu_id);
    eprintln!(
        "IRQ load: {} Hz x {} timers = {} Hz per victim CPU",
        DEFAULT_IRQ_HZ,
        NUM_TIMERS,
        DEFAULT_IRQ_HZ * NUM_TIMERS as u64
    );

    log_scheduler_info();

    // Create shared memory
    let allocator = BumpAllocator::new("irq_migration_test", 4 * 1024 * 1024)?;
    let start_signal = SharedBox::new(allocator.clone(), AtomicU32::new(0))?;

    // Collect ALL CPUs for lat_cap measurement (including control CPU)
    let all_cpus: Vec<_> = cores
        .iter()
        .flat_map(|core| core.hyperthreads().iter().cloned())
        .collect();

    // Launch lat_cap measurement workers on ALL CPUs before the test
    // This ensures LAVD has accurate stolen_time_est measurements for every CPU
    eprintln!("\nLaunching lat_cap measurement workers on all {} CPUs...", all_cpus.len());
    let lat_cap_workers = launch_lat_cap_workers(
        allocator.clone(),
        &all_cpus,
        start_signal.clone(),
        LAT_CAP_WORKER_DUTY_CYCLE_PCT,
        LAT_CAP_WORKER_PERIOD_MS,
    )?;

    // Start lat_cap workers and let them run briefly to establish baseline lat_cap values
    eprintln!("Starting lat_cap workers for warm-up period...");
    start_signal.store(1, Ordering::Release);

    // Warm-up period: let lat_cap workers run for ~1 second so LAVD can establish
    // baseline stolen_time_est values on all CPUs before the IRQ storm starts
    let warmup_duration = Duration::from_secs(1);
    eprintln!("Warming up lat_cap measurements for {:?}...", warmup_duration);
    std::thread::sleep(warmup_duration);

    // Reset start signal for the actual test workers
    start_signal.store(0, Ordering::Release);
    let test_start_signal = SharedBox::new(allocator.clone(), AtomicU32::new(0))?;

    // Capture interrupt baseline
    eprintln!("\nCapturing interrupt baseline...");
    let interrupts_before = InterruptSnapshot::capture()?;

    // Launch timer IRQ disruption on ALL victim CPUs
    eprintln!("\nLaunching timer IRQ disruption on {} victim CPUs...", victim_cpus.len());
    let mut irq_handles: Vec<TimerIrqHandle> = Vec::new();

    for victim_cpu in &victim_cpus {
        let handle = launch_timer_irq_disruption(
            allocator.clone(),
            victim_cpu,
            test_start_signal.clone(),
            DEFAULT_IRQ_HZ,
        )?;
        irq_handles.push(handle);
    }

    let test_duration = get_test_duration();
    eprintln!("\nLaunching spinner probe on CPU {} (will unpin after start)...", initial_victim_cpu_id);

    // Launch spinner probe
    // This task spins continuously with nice -10
    let spinner_probe = launch_spinner_probe(
        allocator.clone(),
        &initial_victim_cpu,
        test_start_signal.clone(),
    )?;

    // Give everything a moment to initialize
    std::thread::sleep(Duration::from_millis(100));

    // Signal start for test workers (lat_cap workers already running from warm-up)
    eprintln!("Signaling START (probes will unpin and run for {:?})", test_duration);
    test_start_signal.store(1, Ordering::Release);

    // Let the test run
    eprintln!("Test running...");
    std::thread::sleep(test_duration);
    eprintln!("Test duration complete");

    // Stop spinner probe and get final CPU position + transition history
    eprintln!("Stopping spinner probe...");
    let (final_cpu, spinner_iterations, transitions) = spinner_probe.stop()?;

    // Stop all IRQ disruption
    eprintln!("Stopping IRQ disruption on {} CPUs...", irq_handles.len());
    let mut total_timer_wakeups = 0u64;
    for handle in irq_handles {
        total_timer_wakeups += handle.stop()?;
    }

    // Stop lat_cap measurement workers
    eprintln!("Stopping lat_cap measurement workers...");
    lat_cap_workers.stop()?;

    // Capture interrupt counts after
    let interrupts_after = InterruptSnapshot::capture()?;
    let interrupt_delta = interrupts_after.delta(&interrupts_before);

    // Collect all control CPU IDs (all hyperthreads of control core)
    let control_cpu_ids: Vec<i32> = control_core.hyperthreads().iter().map(|h| h.id()).collect();

    // Results
    eprintln!("\n=== Results ===");
    eprintln!("Total timer wakeups across all victim CPUs: {}", total_timer_wakeups);
    eprintln!("Spinner iterations: {}", spinner_iterations);
    eprintln!();
    eprintln!("Initial CPU: {}", initial_victim_cpu_id);
    eprintln!("Final CPU: {}", final_cpu);
    eprintln!("Control CPUs: {:?}", control_cpu_ids);

    // Print CPU transition history
    eprintln!("\n=== CPU Transitions ({} total) ===", transitions.len());
    if transitions.is_empty() {
        eprintln!("  No CPU transitions detected (probe stayed on CPU {})", initial_victim_cpu_id);
    } else {
        eprintln!("{:>12} {:>12} {:>8} {:>8}  Note", "Iteration", "Elapsed", "From", "To");
        for t in &transitions {
            let elapsed_ms = t.elapsed_ns as f64 / 1_000_000.0;
            let note = if control_cpu_ids.contains(&t.to_cpu) {
                "-> CONTROL"
            } else if control_cpu_ids.contains(&t.from_cpu) {
                "<- left CONTROL"
            } else {
                ""
            };
            eprintln!("{:>12} {:>10.1}ms {:>8} {:>8}  {}",
                      t.iteration, elapsed_ms, t.from_cpu, t.to_cpu, note);
        }
    }

    // CPU residence time summary (always print)
    let test_duration_ns = test_duration.as_nanos() as u64;
    let mut cpu_time: std::collections::HashMap<i32, u64> = std::collections::HashMap::new();
    let mut prev_cpu = initial_victim_cpu_id;
    let mut prev_time: u64 = 0;
    for t in &transitions {
        let duration = t.elapsed_ns - prev_time;
        *cpu_time.entry(prev_cpu).or_insert(0) += duration;
        prev_cpu = t.to_cpu;
        prev_time = t.elapsed_ns;
    }
    // Add final segment
    if prev_time < test_duration_ns {
        *cpu_time.entry(prev_cpu).or_insert(0) += test_duration_ns - prev_time;
    }

    eprintln!("\n=== CPU Residence Time ===");
    eprintln!("{:>6} {:>12} {:>8}  Role", "CPU", "Time", "Percent");
    eprintln!("{}", "-".repeat(45));
    let mut cpu_times: Vec<_> = cpu_time.into_iter().collect();
    cpu_times.sort_by(|(_, a), (_, b)| b.cmp(a));  // Sort by time descending
    for (cpu, time_ns) in &cpu_times {
        let time_ms = *time_ns as f64 / 1_000_000.0;
        let pct = *time_ns as f64 / test_duration_ns as f64 * 100.0;
        let role = if control_cpu_ids.contains(cpu) {
            "CONTROL"
        } else if victim_cpu_ids.contains(cpu) {
            "VICTIM"
        } else {
            ""
        };
        eprintln!("{:>6} {:>10.1}ms {:>7.1}%  {}", cpu, time_ms, pct, role);
    }

    // Check if probe migrated to control core (any of its hyperthreads)
    let probe_on_control = control_cpu_ids.contains(&final_cpu);
    let probe_migrated = final_cpu != initial_victim_cpu_id;
    let ever_on_control = transitions.iter().any(|t| control_cpu_ids.contains(&t.to_cpu));

    eprintln!();
    if probe_on_control {
        eprintln!("✓ SUCCESS: Probe migrated to control core (CPU {})", final_cpu);
    } else if ever_on_control {
        eprintln!("⚠ PARTIAL: Probe visited control core but left");
    } else if probe_migrated {
        eprintln!("⚠ PARTIAL: Probe migrated but not to control core");
        eprintln!("  Probe: {} -> {}", initial_victim_cpu_id, final_cpu);
    } else {
        eprintln!("✗ FAILED: Probe did not migrate from victim CPU {}",
                  initial_victim_cpu_id);
    }

    // Print interrupt summary for key CPUs
    eprintln!("\n=== Interrupt Counts (Delta) ===");
    eprintln!(
        "{:>6} {:>15} {:>15}  Role",
        "CPU", "Total", "Reschedule"
    );

    // Show initial victim, final CPU, and control
    let mut interesting_cpus = vec![
        (initial_victim_cpu_id as usize, "INITIAL"),
        (final_cpu as usize, "FINAL"),
    ];
    for &cpu_id in &control_cpu_ids {
        interesting_cpus.push((cpu_id as usize, "CONTROL"));
    }

    // Deduplicate
    interesting_cpus.sort_by_key(|(cpu, _)| *cpu);
    interesting_cpus.dedup_by_key(|(cpu, _)| *cpu);

    for (cpu, label) in &interesting_cpus {
        let total = *interrupt_delta.total.get(cpu).unwrap_or(&0);
        let reschedule = *interrupt_delta.reschedule.get(cpu).unwrap_or(&0);
        eprintln!("{:>6} {:>15} {:>15}  {}", cpu, total, reschedule, label);
    }

    // Summary stats
    let control_interrupts: i64 = control_cpu_ids
        .iter()
        .map(|&cpu| *interrupt_delta.total.get(&(cpu as usize)).unwrap_or(&0))
        .sum::<i64>() / control_cpu_ids.len().max(1) as i64;

    let victim_avg_interrupts: i64 = if !victim_cpus.is_empty() {
        let sum: i64 = victim_cpus
            .iter()
            .map(|cpu| *interrupt_delta.total.get(&(cpu.id() as usize)).unwrap_or(&0))
            .sum();
        sum / victim_cpus.len() as i64
    } else {
        0
    };

    eprintln!("\nInterrupt Summary:");
    eprintln!("  Control CPUs avg interrupts: {}", control_interrupts);
    eprintln!("  Victim CPUs avg interrupts: {}", victim_avg_interrupts);

    if victim_avg_interrupts > control_interrupts * 10 {
        eprintln!("  ✓ Victim CPUs have ~{}x more interrupts than control",
                  victim_avg_interrupts / control_interrupts.max(1));
    }

    Ok(())
}

test!("irq_migration", irq_migration_test);
