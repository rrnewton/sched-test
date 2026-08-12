//! Tests for IRQ disruption with cgroup cpu.max fairness (accounting test).
//!
//! This is an A/B test of bogo ops throughput on two workers - one on a victim CPU
//! with IRQ load, and one on a control CPU without IRQ load.
//!
//! # Environment Variables
//!
//! - `SCHTEST_IRQ_MODE`: Controls the IRQ disruption strategy.
//!   Valid values: `none`, `futex`, `pmu`, `timer`, `combined` (case-insensitive).
//!   Defaults to `timer` if not set.
//!
//! - `SCHTEST_IRQ_DURATION`: Test duration in seconds.
//!   Must be a positive integer. Defaults to `10` if not set or invalid.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use cgroups_rs::fs::cgroup_builder::CgroupBuilder;
use cgroups_rs::fs::hierarchies;

use crate::util::shared::{BumpAllocator, SharedBox};
use crate::util::system::System;

use super::irq_common::{
    get_disruption_mode, get_test_duration, launch_cgroup_hog, launch_futex_ipi_disruption,
    launch_pmu_irq_disruption, launch_timer_irq_disruption, log_perf_sysctls, log_scheduler_info,
    InterruptSnapshot, IrqDisruptionHandle, IrqDisruptionMode, IrqDisruptionStats,
    DEFAULT_IRQ_HZ, NUM_TIMERS,
};

/// Test IRQ disruption impact on cgroup cpu.max fairness.
///
/// This test creates two CPU hogs on CPU 1 and CPU 2, both limited to by cpu.max.
/// Additionally, PMU sampling generates high-frequency PMIs (Performance Monitoring
/// Interrupts) on CPU 1 (victim) at IRQ_HZ frequency. PMIs are NMI-like interrupts
/// that preempt almost everything, simulating heavy IRQ load.
fn irq_disruption_targeted() -> Result<()> {
    const CPU_1: i32 = 1;
    const CPU_2: i32 = 2;
    const WAKER_CPU: i32 = 0; // Only used for Futex mode
    const CPU_MAX_PERCENT: f64 = 50.0;
    const IRQ_HZ: u64 = DEFAULT_IRQ_HZ;
    let disruption_mode: IrqDisruptionMode = get_disruption_mode();

    let system = System::load()?;

    // Collect all hyperthreads (logical CPUs)
    let mut all_cpus = Vec::new();
    for core in system.cores() {
        for ht in core.hyperthreads() {
            all_cpus.push(ht.clone());
        }
    }

    eprintln!("Found {} logical CPUs", all_cpus.len());
    eprintln!("Testing IRQ disruption impact on cpu.max fairness:");
    eprintln!(
        "  CPU {} and CPU {} both limited to {}%",
        CPU_1, CPU_2, CPU_MAX_PERCENT
    );
    match disruption_mode {
        IrqDisruptionMode::None => {
            eprintln!("  NO IRQ disruption - baseline measurement");
        }
        IrqDisruptionMode::Futex => {
            eprintln!(
                "  Futex-based IPI disruption: Waker on CPU {} -> Receiver on CPU {} at {} Hz",
                WAKER_CPU, CPU_1, IRQ_HZ
            );
        }
        IrqDisruptionMode::Pmu => {
            eprintln!(
                "  PMU sampling on CPU {} at {} Hz (PMI interrupts)",
                CPU_1, IRQ_HZ
            );
        }
        IrqDisruptionMode::Timer => {
            eprintln!(
                "  Timer interrupts on CPU {} at {} Hz x {} timers = {} Hz effective",
                CPU_1,
                IRQ_HZ,
                NUM_TIMERS,
                IRQ_HZ * NUM_TIMERS as u64
            );
        }
        IrqDisruptionMode::Combined => {
            eprintln!("  COMBINED mode: PMU sampling + Futex IPI + Timer interrupts");
            eprintln!("    PMU: CPU {} at {} Hz (PMI interrupts)", CPU_1, IRQ_HZ);
            eprintln!(
                "    Futex: Waker on CPU {} -> Receiver on CPU {} at {} Hz",
                WAKER_CPU, CPU_1, IRQ_HZ
            );
            eprintln!(
                "    Timer: CPU {} at {} Hz x {} timers",
                CPU_1, IRQ_HZ, NUM_TIMERS
            );
        }
    }

    // Log scheduler and perf information
    log_scheduler_info();
    log_perf_sysctls();

    // Find the CPUs
    let cpu_1_ht = all_cpus
        .iter()
        .find(|ht| ht.id() == CPU_1)
        .ok_or_else(|| anyhow::anyhow!("CPU {} not found", CPU_1))?
        .clone();

    let cpu_2_ht = all_cpus
        .iter()
        .find(|ht| ht.id() == CPU_2)
        .ok_or_else(|| anyhow::anyhow!("CPU {} not found", CPU_2))?
        .clone();

    let waker_ht = all_cpus
        .iter()
        .find(|ht| ht.id() == WAKER_CPU)
        .ok_or_else(|| anyhow::anyhow!("CPU {} not found", WAKER_CPU))?
        .clone();

    // Create shared memory for start signal and counters
    let allocator = BumpAllocator::new("cpu_max_test", 2 * 1024 * 1024)?;
    let start_signal = SharedBox::new(allocator.clone(), AtomicU32::new(0))?;

    // Create counters for both CPUs
    let bogo_ops_cpu1 = SharedBox::new(allocator.clone(), AtomicU64::new(0))?;
    let scheduled_ns_cpu1 = SharedBox::new(allocator.clone(), AtomicU64::new(0))?;
    let bogo_ops_cpu2 = SharedBox::new(allocator.clone(), AtomicU64::new(0))?;
    let scheduled_ns_cpu2 = SharedBox::new(allocator.clone(), AtomicU64::new(0))?;

    // Create cgroups with cpu.max limits
    let period_us = 100000u64; // 100ms period
    let quota_us = (period_us as f64 * CPU_MAX_PERCENT / 100.0) as i64;

    eprintln!(
        "\nCreating cgroups with cpu.max={}/{} ({}%)",
        quota_us, period_us, CPU_MAX_PERCENT
    );

    let cgroup_cpu1 = CgroupBuilder::new("schtest_cpu_max_cpu1")
        .cpu()
        .quota(quota_us)
        .period(period_us)
        .done()
        .build(hierarchies::auto())
        .context("Failed to create cgroup for CPU 1")?;

    let cgroup_cpu2 = CgroupBuilder::new("schtest_cpu_max_cpu2")
        .cpu()
        .quota(quota_us)
        .period(period_us)
        .done()
        .build(hierarchies::auto())
        .context("Failed to create cgroup for CPU 2")?;

    // Capture interrupt counts before the test
    eprintln!("\nCapturing interrupt baseline...");
    let interrupts_before = InterruptSnapshot::capture()?;

    // Launch IRQ disruption based on selected mode
    let irq_handle = match disruption_mode {
        IrqDisruptionMode::None => {
            eprintln!("\nSkipping IRQ disruption (baseline mode)");
            IrqDisruptionHandle::None
        }
        IrqDisruptionMode::Futex => {
            eprintln!("\nLaunching IRQ disruption...");
            eprintln!(
                "  Futex mode: Receiver on CPU {}, Waker on CPU {} at {} Hz",
                CPU_1, WAKER_CPU, IRQ_HZ
            );
            let handle = launch_futex_ipi_disruption(
                allocator.clone(),
                &cpu_1_ht,
                &waker_ht,
                start_signal.clone(),
                IRQ_HZ,
            )?;
            IrqDisruptionHandle::Futex(handle)
        }
        IrqDisruptionMode::Pmu => {
            eprintln!("\nLaunching IRQ disruption...");
            eprintln!("  PMU mode: Sampling on CPU {} at {} Hz", CPU_1, IRQ_HZ);
            let handle = launch_pmu_irq_disruption(
                allocator.clone(),
                &cpu_1_ht,
                start_signal.clone(),
                IRQ_HZ,
            )?;
            IrqDisruptionHandle::Pmu(handle)
        }
        IrqDisruptionMode::Combined => {
            eprintln!("\nLaunching IRQ disruption...");
            eprintln!("  Combined mode: Launching both PMU and Futex...");
            let pmu_handle = launch_pmu_irq_disruption(
                allocator.clone(),
                &cpu_1_ht,
                start_signal.clone(),
                IRQ_HZ,
            )?;
            let futex_handle = launch_futex_ipi_disruption(
                allocator.clone(),
                &cpu_1_ht,
                &waker_ht,
                start_signal.clone(),
                IRQ_HZ,
            )?;
            let timer_handle = launch_timer_irq_disruption(
                allocator.clone(),
                &cpu_1_ht,
                start_signal.clone(),
                IRQ_HZ,
            )?;
            IrqDisruptionHandle::Combined {
                futex: futex_handle,
                pmu: pmu_handle,
                timer: timer_handle,
            }
        }
        IrqDisruptionMode::Timer => {
            eprintln!("\nLaunching IRQ disruption...");
            eprintln!(
                "  Timer mode: {} timers on CPU {} at {} Hz each",
                NUM_TIMERS, CPU_1, IRQ_HZ
            );
            let handle = launch_timer_irq_disruption(
                allocator.clone(),
                &cpu_1_ht,
                start_signal.clone(),
                IRQ_HZ,
            )?;
            IrqDisruptionHandle::Timer(handle)
        }
    };

    let hog_duration = get_test_duration();
    eprintln!("\nLaunching 2 CPU hogs for {:?}...", hog_duration);

    // Launch hog on CPU 1 (victim - receives IPIs)
    let mut child_cpu1 = launch_cgroup_hog(
        CPU_1,
        &cpu_1_ht,
        "schtest_cpu_max_cpu1",
        "victim",
        hog_duration,
        start_signal.clone(),
        bogo_ops_cpu1.clone(),
        scheduled_ns_cpu1.clone(),
    )?;

    // Launch hog on CPU 2 (control - no IPI load)
    let mut child_cpu2 = launch_cgroup_hog(
        CPU_2,
        &cpu_2_ht,
        "schtest_cpu_max_cpu2",
        "control",
        hog_duration,
        start_signal.clone(),
        bogo_ops_cpu2.clone(),
        scheduled_ns_cpu2.clone(),
    )?;

    // Give all threads a moment to initialize
    std::thread::sleep(Duration::from_millis(100));

    // Signal all threads to start simultaneously
    eprintln!("Signaling all threads to START");
    start_signal.store(1, Ordering::Release);

    // Wait for both hogs to complete
    eprintln!("Waiting for hogs to complete...");
    if let Some(result) = child_cpu1.wait(true, false) {
        result.context(format!("Hog on CPU {} failed", CPU_1))?;
    }
    if let Some(result) = child_cpu2.wait(true, false) {
        result.context(format!("Hog on CPU {} failed", CPU_2))?;
    }
    eprintln!("Both hogs completed successfully");

    // Stop IRQ disruption
    eprintln!("Stopping IRQ disruption...");
    let stats = irq_handle.stop()?;

    // Capture interrupt counts after the test
    let interrupts_after = InterruptSnapshot::capture()?;
    let interrupt_delta = interrupts_after.delta(&interrupts_before);

    // Report stats based on mode
    match stats {
        IrqDisruptionStats::None => {
            eprintln!("\n=== No IRQ Disruption (Baseline) ===");
            eprintln!("No artificial IRQ load applied");
        }
        IrqDisruptionStats::Futex {
            wakeup_count,
            futex_wait_calls,
            futex_wait_blocks,
            futex_wait_eagain,
        } => {
            eprintln!("\n=== Futex IPI Disruption Stats ===");
            eprintln!("Waker sent {} wakeups total", wakeup_count);
            eprintln!("Receiver futex_wait calls:  {}", futex_wait_calls);
            eprintln!("  Actual blocks:             {}", futex_wait_blocks);
            eprintln!("  EAGAIN returns:            {}", futex_wait_eagain);

            let block_pct = if futex_wait_calls > 0 {
                (futex_wait_blocks as f64 / futex_wait_calls as f64) * 100.0
            } else {
                0.0
            };
            eprintln!("  Block rate:                {:.2}%", block_pct);
        }
        IrqDisruptionStats::Pmu => {
            eprintln!("\n=== PMU Disruption Stats ===");
            eprintln!("PMU sampling configured at {} Hz on CPU {}", IRQ_HZ, CPU_1);
            eprintln!("(PMIs delivered as NMI-like interrupts throughout test)");
        }
        IrqDisruptionStats::Timer { timer_wakeups } => {
            eprintln!("\n=== Timer Disruption Stats ===");
            eprintln!(
                "Timer: {} wakeups total at {} Hz x {} timers = {} Hz effective on CPU {}",
                timer_wakeups,
                IRQ_HZ,
                NUM_TIMERS,
                IRQ_HZ * NUM_TIMERS as u64,
                CPU_1
            );
        }
        IrqDisruptionStats::Combined {
            wakeup_count,
            futex_wait_calls,
            futex_wait_blocks,
            futex_wait_eagain,
            timer_wakeups,
        } => {
            eprintln!("\n=== COMBINED Disruption Stats ===");
            eprintln!("PMU: Sampling configured at {} Hz on CPU {}", IRQ_HZ, CPU_1);
            eprintln!("     (PMIs delivered as NMI-like interrupts throughout test)");
            eprintln!("\nFutex: Waker sent {} wakeups total", wakeup_count);
            eprintln!("  Receiver futex_wait calls:  {}", futex_wait_calls);
            eprintln!("    Actual blocks:             {}", futex_wait_blocks);
            eprintln!("    EAGAIN returns:            {}", futex_wait_eagain);

            let block_pct = if futex_wait_calls > 0 {
                (futex_wait_blocks as f64 / futex_wait_calls as f64) * 100.0
            } else {
                0.0
            };
            eprintln!("    Block rate:                {:.2}%", block_pct);
            eprintln!(
                "\nTimer: {} wakeups total ({} timers at {} Hz each)",
                timer_wakeups, NUM_TIMERS, IRQ_HZ
            );
        }
    }

    // Report interrupt deltas per CPU
    eprintln!("\n=== Interrupt Counts (Delta During Test) ===");
    eprintln!(
        "{:>6} {:>15} {:>15} {:>15} {:>15}",
        "CPU", "Total", "Reschedule", "Func Call", "TLB"
    );

    // Collect key CPUs
    let key_cpus = vec![
        (WAKER_CPU as usize, "WAKER"),
        (CPU_1 as usize, "VICTIM"),
        (CPU_2 as usize, "CONTROL"),
    ];

    for (cpu, label) in &key_cpus {
        let total = *interrupt_delta.total.get(cpu).unwrap_or(&0);
        let reschedule = *interrupt_delta.reschedule.get(cpu).unwrap_or(&0);
        let function_call = *interrupt_delta.function_call.get(cpu).unwrap_or(&0);
        let tlb = *interrupt_delta.tlb.get(cpu).unwrap_or(&0);

        eprintln!(
            "{:>6} {:>15} {:>15} {:>15} {:>15}  <-- {}",
            cpu, total, reschedule, function_call, tlb, label
        );
    }

    // Calculate IPI totals (reschedule + function_call + tlb)
    let victim_total = *interrupt_delta.total.get(&(CPU_1 as usize)).unwrap_or(&0);
    let victim_ipis = *interrupt_delta
        .reschedule
        .get(&(CPU_1 as usize))
        .unwrap_or(&0)
        + *interrupt_delta
            .function_call
            .get(&(CPU_1 as usize))
            .unwrap_or(&0)
        + *interrupt_delta.tlb.get(&(CPU_1 as usize)).unwrap_or(&0);

    let control_total = *interrupt_delta.total.get(&(CPU_2 as usize)).unwrap_or(&0);
    let control_ipis = *interrupt_delta
        .reschedule
        .get(&(CPU_2 as usize))
        .unwrap_or(&0)
        + *interrupt_delta
            .function_call
            .get(&(CPU_2 as usize))
            .unwrap_or(&0)
        + *interrupt_delta.tlb.get(&(CPU_2 as usize)).unwrap_or(&0);

    eprintln!("\nInterrupt Summary:");
    eprintln!(
        "  Victim CPU {} total interrupts: {} (IPIs: {})",
        CPU_1, victim_total, victim_ipis
    );
    eprintln!(
        "  Control CPU {} total interrupts: {} (IPIs: {})",
        CPU_2, control_total, control_ipis
    );

    if victim_ipis > control_ipis * 2 {
        eprintln!(
            "  ✓ Victim CPU has {}x more IPIs than control",
            victim_ipis / control_ipis.max(1)
        );
    } else {
        eprintln!(
            "  ⚠ Victim CPU IPI rate similar to control (ratio: {:.2}x)",
            victim_ipis as f64 / control_ipis.max(1) as f64
        );
    }

    // Clean up cgroups
    drop(cgroup_cpu1);
    drop(cgroup_cpu2);

    // Collect results
    let results: Vec<(i32, u64, u64)> = vec![
        (
            CPU_1,
            bogo_ops_cpu1.load(Ordering::Acquire),
            scheduled_ns_cpu1.load(Ordering::Acquire),
        ),
        (
            CPU_2,
            bogo_ops_cpu2.load(Ordering::Acquire),
            scheduled_ns_cpu2.load(Ordering::Acquire),
        ),
    ];

    // Calculate statistics
    let bogo_ops_results: Vec<(i32, u64)> =
        results.iter().map(|(cpu, ops, _)| (*cpu, *ops)).collect();
    let scheduled_ns_results: Vec<(i32, u64)> =
        results.iter().map(|(cpu, _, ns)| (*cpu, *ns)).collect();

    // Bogo ops statistics
    let mut bogo_ops_only: Vec<u64> = bogo_ops_results.iter().map(|(_, ops)| *ops).collect();
    bogo_ops_only.sort_unstable();

    let min_bogo_ops = bogo_ops_only[0];
    let max_bogo_ops = bogo_ops_only[1];
    let avg_bogo_ops = (min_bogo_ops + max_bogo_ops) / 2;
    let p50_bogo_ops = avg_bogo_ops;
    let bogo_ops_skew = max_bogo_ops - min_bogo_ops;
    let bogo_ops_skew_pct = if max_bogo_ops > 0 {
        (bogo_ops_skew as f64 / max_bogo_ops as f64) * 100.0
    } else {
        0.0
    };

    let (min_bogo_cpu, _) = bogo_ops_results.iter().min_by_key(|(_, ops)| ops).unwrap();
    let (max_bogo_cpu, _) = bogo_ops_results.iter().max_by_key(|(_, ops)| ops).unwrap();
    let p50_bogo_cpu = *min_bogo_cpu;

    // Scheduled nanoseconds statistics
    let mut scheduled_ns_only: Vec<u64> = scheduled_ns_results.iter().map(|(_, ns)| *ns).collect();
    scheduled_ns_only.sort_unstable();

    let min_scheduled_ns = scheduled_ns_only[0];
    let max_scheduled_ns = scheduled_ns_only[1];
    let avg_scheduled_ns = (min_scheduled_ns + max_scheduled_ns) / 2;
    let p50_scheduled_ns = avg_scheduled_ns;
    let scheduled_ns_skew = max_scheduled_ns - min_scheduled_ns;
    let scheduled_ns_skew_pct = if max_scheduled_ns > 0 {
        (scheduled_ns_skew as f64 / max_scheduled_ns as f64) * 100.0
    } else {
        0.0
    };

    let (min_ns_cpu, _) = scheduled_ns_results
        .iter()
        .min_by_key(|(_, ns)| ns)
        .unwrap();
    let (max_ns_cpu, _) = scheduled_ns_results
        .iter()
        .max_by_key(|(_, ns)| ns)
        .unwrap();
    let p50_ns_cpu = *min_ns_cpu;

    // Bogo ops per millisecond
    let bogo_ops_per_ms: Vec<(i32, f64)> = results
        .iter()
        .map(|(cpu, ops, ns)| {
            let ms = *ns as f64 / 1_000_000.0;
            let ops_per_ms = if ms > 0.0 { *ops as f64 / ms } else { 0.0 };
            (*cpu, ops_per_ms)
        })
        .collect();

    let mut ops_per_ms_only: Vec<f64> = bogo_ops_per_ms.iter().map(|(_, rate)| *rate).collect();
    ops_per_ms_only.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let min_ops_per_ms = ops_per_ms_only[0];
    let max_ops_per_ms = ops_per_ms_only[1];
    let avg_ops_per_ms = (min_ops_per_ms + max_ops_per_ms) / 2.0;
    let p50_ops_per_ms = avg_ops_per_ms;
    let ops_per_ms_skew = max_ops_per_ms - min_ops_per_ms;
    let ops_per_ms_skew_pct = if max_ops_per_ms > 0.0 {
        (ops_per_ms_skew / max_ops_per_ms) * 100.0
    } else {
        0.0
    };

    let (min_ops_ms_cpu, _) = bogo_ops_per_ms
        .iter()
        .min_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap())
        .unwrap();
    let (max_ops_ms_cpu, _) = bogo_ops_per_ms
        .iter()
        .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap())
        .unwrap();
    let p50_ops_ms_cpu = *min_ops_ms_cpu;

    // Print detailed results
    eprintln!("\n=== Per-CPU Results ===");
    eprintln!(
        "{:>6} {:>20} {:>20} {:>20}",
        "CPU", "bogo_ops", "scheduled_ns", "bogo_ops/ms"
    );
    for (cpu, ops, ns) in &results {
        let ops_per_ms = bogo_ops_per_ms
            .iter()
            .find(|(id, _)| id == cpu)
            .map(|(_, rate)| *rate)
            .unwrap_or(0.0);
        let marker = if *cpu == CPU_1 {
            " <-- VICTIM (IPI load)"
        } else {
            " <-- CONTROL"
        };
        eprintln!(
            "{:>6} {:>20} {:>20} {:>20.2}{}",
            cpu, ops, ns, ops_per_ms, marker
        );
    }

    eprintln!("\n=== Bogo Ops Statistics, Per Core ===");
    eprintln!("Min:       {:>20} (CPU {})", min_bogo_ops, min_bogo_cpu);
    eprintln!("Avg:       {:>20}", avg_bogo_ops);
    eprintln!("P50:       {:>20} (CPU {})", p50_bogo_ops, p50_bogo_cpu);
    eprintln!("Max:       {:>20} (CPU {})", max_bogo_ops, max_bogo_cpu);
    eprintln!(
        "Max Skew:  {:>20} ({:.2}%)",
        bogo_ops_skew, bogo_ops_skew_pct
    );

    eprintln!("\n=== Scheduled Time (ns) Statistics, Per Core ===");
    eprintln!("Min:       {:>20} (CPU {})", min_scheduled_ns, min_ns_cpu);
    eprintln!("Avg:       {:>20}", avg_scheduled_ns);
    eprintln!("P50:       {:>20} (CPU {})", p50_scheduled_ns, p50_ns_cpu);
    eprintln!("Max:       {:>20} (CPU {})", max_scheduled_ns, max_ns_cpu);
    eprintln!(
        "Max Skew:  {:>20} ({:.2}%)",
        scheduled_ns_skew, scheduled_ns_skew_pct
    );

    eprintln!("\n=== Bogo Ops/ms Statistics, Per Core ===");
    eprintln!(
        "Min:       {:>20.2} (CPU {})",
        min_ops_per_ms, min_ops_ms_cpu
    );
    eprintln!("Avg:       {:>20.2}", avg_ops_per_ms);
    eprintln!(
        "P50:       {:>20.2} (CPU {})",
        p50_ops_per_ms, p50_ops_ms_cpu
    );
    eprintln!(
        "Max:       {:>20.2} (CPU {})",
        max_ops_per_ms, max_ops_ms_cpu
    );
    eprintln!(
        "Max Skew:  {:>20.2} ({:.2}%)",
        ops_per_ms_skew, ops_per_ms_skew_pct
    );

    // Assert that CPU 1 (victim) has lower bogo_ops than CPU 2 (control)
    if bogo_ops_results
        .iter()
        .find(|(cpu, _)| *cpu == CPU_1)
        .unwrap()
        .1
        >= bogo_ops_results
            .iter()
            .find(|(cpu, _)| *cpu == CPU_2)
            .unwrap()
            .1
    {
        eprintln!(
            "\n⚠ WARNING: Victim CPU {} did not have lower bogo_ops than control CPU {}",
            CPU_1, CPU_2
        );
        eprintln!(
            "   This suggests the interrupt disruption strategy may not be working as expected."
        );
    } else {
        eprintln!(
            "\n✓ Victim CPU {} has lower bogo_ops than control CPU {} (interrupt impact detected)",
            CPU_1, CPU_2
        );
    }

    Ok(())
}

test!("irq_disruption_targeted", irq_disruption_targeted);
