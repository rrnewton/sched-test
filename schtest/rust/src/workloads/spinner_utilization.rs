//! CPU utilization benchmark using rdtsc-based spin loop
//!
//! This module provides a CPU-intensive workload that measures its own scheduling
//! behavior by tracking time slices and descheduling events via TSC (Time Stamp Counter).

use crate::util::shared::SharedBox;
use serde::{Deserialize, Serialize};
use std::arch::asm;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Minimum TSC cycle gap to consider as a descheduling event
const MIN_DESCHEDULE_CYCLES: u64 = 3000;

/// Window size in milliseconds for utilization calculation
const WINDOW_SIZE_MS: u64 = 100;

/// Per-window statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowStat {
    pub start_time_ns: u64,
    pub num_slices: usize,
    pub max_gap_ns: u64,
    pub util_pct: f64,
}

/// Percentile statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Percentiles {
    pub p01: f64,
    pub p25: f64,
    pub p50: f64,
    pub p75: f64,
    pub p99: f64,
}

/// Complete benchmark results
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkResults {
    pub tsc_frequency: u64,
    pub time_scheduled_ns: u64,
    /// Number of loop iterations completed (bogo ops) - represents work done,
    /// distinct from time_scheduled_ns which measures how long the process was scheduled
    pub bogo_ops: u64,
    pub total_tsc_ticks: u64,
    pub per_window_stats: Vec<WindowStat>,
    pub utilization_pcts: Percentiles,
    pub timeslice_pcts: Percentiles,
}

/// Read the current cycle count using rdtsc
#[inline(always)]
pub fn rdtsc() -> u64 {
    unsafe {
        let low: u32;
        let high: u32;
        asm!("rdtsc", out("eax") low, out("edx") high);
        ((high as u64) << 32) | (low as u64)
    }
}

/// Read invariant TSC frequency (Hz) from sysfs if available
pub fn read_tsc_hz() -> Option<u64> {
    let path = "/sys/devices/system/cpu/cpu0/tsc_freq_khz";
    if let Ok(khz_str) = std::fs::read_to_string(path) {
        if let Ok(khz) = khz_str.trim().parse::<u64>() {
            return Some(khz * 1000);
        }
    }
    None
}

/// Calibrate TSC frequency over a short interval; returns measured Hz and elapsed seconds
pub fn calibrate_tsc_short(duration_ms: u64) -> (u64, f64) {
    let target = Duration::from_millis(duration_ms);
    let t0 = Instant::now();
    let c0 = rdtsc();
    // Busy-wait until target elapsed; Instant is vDSO-accelerated
    loop {
        if t0.elapsed() >= target {
            break;
        }
        std::hint::spin_loop();
    }
    let c1 = rdtsc();
    let elapsed = t0.elapsed().as_secs_f64();
    let measured_hz = ((c1 - c0) as f64 / elapsed) as u64;
    (measured_hz, elapsed)
}

/// Get TSC frequency: prefer sysfs, fallback to calibration
pub fn get_tsc_hz(verbose: bool) -> u64 {
    match read_tsc_hz() {
        Some(hz) => {
            if verbose {
                eprintln!("TSC frequency: {} Hz (from sysfs)", format_with_commas(hz));
            }
            hz
        }
        None => {
            if verbose {
                eprintln!("TSC frequency: sysfs unavailable, calibrating...");
            }
            let calib_ms = 5u64;
            let (measured_hz, elapsed) = calibrate_tsc_short(calib_ms);
            if verbose {
                eprintln!(
                    "TSC frequency: {} Hz (measured over {:.3} ms)",
                    format_with_commas(measured_hz),
                    elapsed * 1000.0
                );
            }
            measured_hz
        }
    }
}

/// Format a number with comma separators for readability
pub fn format_with_commas(n: u64) -> String {
    n.to_string()
        .as_bytes()
        .rchunks(3)
        .rev()
        .map(|chunk| std::str::from_utf8(chunk).unwrap())
        .collect::<Vec<_>>()
        .join(",")
}

/// Worker loop: runs for specified duration, records time slices
pub fn run_spinner(duration: Duration, tsc_hz: u64, verbose: bool) -> BenchmarkResults {
    let worker_start = Instant::now();
    let deadline = worker_start + duration;
    let mut count: u64 = 0;
    let mut slices = Vec::new();
    let mut last_cycle = rdtsc();
    let mut in_slice = false;
    let mut slice_start_cycle = 0u64;

    while Instant::now() < deadline {
        count += 1;
        let cur_cycle = rdtsc();
        let gap = cur_cycle - last_cycle;

        // If gap is large, treat as deschedule event
        if gap > MIN_DESCHEDULE_CYCLES {
            if in_slice {
                let slice_end_cycle = last_cycle;
                slices.push((slice_start_cycle, slice_end_cycle));
                in_slice = false;
            }
        } else if !in_slice {
            slice_start_cycle = cur_cycle;
            in_slice = true;
        }
        last_cycle = cur_cycle;
    }

    // Final slice
    if in_slice {
        slices.push((slice_start_cycle, last_cycle));
    }

    compute_results(worker_start, slices, count, tsc_hz, verbose)
}

/// Worker loop with shutdown flag: runs until flag is set, records time slices
///
/// # Safety
/// The caller must ensure that `shutdown_flag` points to a valid `AtomicU32`
/// that remains valid for the duration of this function call.
pub unsafe fn run_spinner_with_shutdown(
    shutdown_flag: *const std::sync::atomic::AtomicU32,
    tsc_hz: u64,
    verbose: bool,
) -> BenchmarkResults {
    use std::sync::atomic::Ordering;

    let worker_start = Instant::now();
    let mut count: u64 = 0;
    let mut slices = Vec::new();
    let mut last_cycle = rdtsc();
    let mut in_slice = false;
    let mut slice_start_cycle = 0u64;

    while unsafe { (*shutdown_flag).load(Ordering::Relaxed) == 0 } {
        count += 1;
        let cur_cycle = rdtsc();
        let gap = cur_cycle - last_cycle;

        // If gap is large, treat as deschedule event
        if gap > MIN_DESCHEDULE_CYCLES {
            if in_slice {
                let slice_end_cycle = last_cycle;
                slices.push((slice_start_cycle, slice_end_cycle));
                in_slice = false;
            }
        } else if !in_slice {
            slice_start_cycle = cur_cycle;
            in_slice = true;
        }
        last_cycle = cur_cycle;
    }

    // Final slice
    if in_slice {
        slices.push((slice_start_cycle, last_cycle));
    }

    compute_results(worker_start, slices, count, tsc_hz, verbose)
}

/// Compute benchmark results and statistics
pub fn compute_results_from_slices(
    worker_start: Instant,
    slices: Vec<(u64, u64)>,
    count: u64,
    tsc_hz: u64,
    verbose: bool,
) -> BenchmarkResults {
    compute_results(worker_start, slices, count, tsc_hz, verbose)
}

/// Compute benchmark results and statistics
fn compute_results(
    worker_start: Instant,
    slices: Vec<(u64, u64)>,
    count: u64,
    tsc_hz: u64,
    verbose: bool,
) -> BenchmarkResults {
    let elapsed_secs = worker_start.elapsed().as_secs_f64();
    let total_cycles: u64 = slices.iter().map(|(start, end)| end - start).sum();

    // Convert total cycles to scheduled time
    let scheduled_secs = total_cycles as f64 / tsc_hz as f64;
    let scheduled_ns = (scheduled_secs * 1e9) as u64;

    if verbose {
        eprintln!("Elapsed time: {:.3} seconds", elapsed_secs);
        eprintln!(
            "Worker was scheduled for {:.3} seconds, {} iterations",
            scheduled_secs,
            format_with_commas(count)
        );
        eprintln!("Total TSC ticks: {}", format_with_commas(total_cycles));
    }

    // Windowed utilization with optional per-window stats
    let window_cycles = (tsc_hz as f64 * (WINDOW_SIZE_MS as f64 / 1000.0)) as u64;
    let mut windows = Vec::new();

    if !slices.is_empty() {
        let first_cycle = slices[0].0;

        // Internal window stats for computation
        struct InternalWindowStats {
            start_time_secs: f64,
            slices: usize,
            max_gap_cycles: u64,
            cycles: u64,
        }
        let mut window_stats = Vec::new();

        let mut cur_win_start = first_cycle;
        let mut cur_win_end = cur_win_start + window_cycles;
        let mut cur_win_cycles = 0u64;
        let mut cur_win_slices = 0usize;
        let mut cur_win_max_gap_cycles = 0u64;

        for (slice_idx, &(start_cycle, end_cycle)) in slices.iter().enumerate() {
            let mut seg_start = start_cycle;
            let mut seg_cycles = end_cycle - start_cycle;

            // Skip windows that end before this segment starts
            while cur_win_end <= seg_start {
                // Finalize current window even if empty
                if cur_win_cycles > 0 {
                    windows.push(cur_win_cycles);
                    let start_time_secs = (cur_win_start - first_cycle) as f64 / tsc_hz as f64;
                    window_stats.push(InternalWindowStats {
                        start_time_secs,
                        slices: cur_win_slices,
                        max_gap_cycles: cur_win_max_gap_cycles,
                        cycles: cur_win_cycles,
                    });
                }
                // Move to next window
                cur_win_start = cur_win_end;
                cur_win_end += window_cycles;
                cur_win_cycles = 0;
                cur_win_slices = 0;
                cur_win_max_gap_cycles = 0;
            }

            // Handle case where slice spans multiple windows
            while seg_start + seg_cycles > cur_win_end {
                let win_cycles = cur_win_end - seg_start;
                cur_win_cycles += win_cycles;
                cur_win_slices += 1;

                // Calculate gap to next slice in cycles
                if slice_idx + 1 < slices.len() {
                    let next_start = slices[slice_idx + 1].0;
                    let gap_cycles = next_start - end_cycle;
                    cur_win_max_gap_cycles = cur_win_max_gap_cycles.max(gap_cycles);
                }

                windows.push(cur_win_cycles);

                let start_time_secs = (cur_win_start - first_cycle) as f64 / tsc_hz as f64;
                window_stats.push(InternalWindowStats {
                    start_time_secs,
                    slices: cur_win_slices,
                    max_gap_cycles: cur_win_max_gap_cycles,
                    cycles: cur_win_cycles,
                });

                // Move to next window
                cur_win_start = cur_win_end;
                cur_win_end += window_cycles;
                cur_win_cycles = 0;
                cur_win_slices = 0;
                cur_win_max_gap_cycles = 0;

                seg_start = cur_win_start;
                seg_cycles -= win_cycles;
            }

            // Add remaining segment to current window
            cur_win_cycles += seg_cycles;
            cur_win_slices += 1;
        }

        // Final partial window
        let has_partial_window = cur_win_cycles > 0;
        if has_partial_window {
            windows.push(cur_win_cycles);
            let start_time_secs = (cur_win_start - first_cycle) as f64 / tsc_hz as f64;
            window_stats.push(InternalWindowStats {
                start_time_secs,
                slices: cur_win_slices,
                max_gap_cycles: cur_win_max_gap_cycles,
                cycles: cur_win_cycles,
            });
        }

        // Export stats excluding partial final window
        let stats_to_export = if has_partial_window && window_stats.len() > 1 {
            &window_stats[..window_stats.len() - 1]
        } else {
            &window_stats[..]
        };

        if verbose && !stats_to_export.is_empty() {
            eprintln!("\nPer-window statistics:");
            for stat in stats_to_export {
                let utilization = stat.cycles as f64 / window_cycles as f64;
                let max_gap_ns = (stat.max_gap_cycles as f64 / tsc_hz as f64 * 1e9) as u64;
                eprintln!(
                    "  Window {:.1}s: {} slices, max gap: {} ns, util: {:.2}%",
                    stat.start_time_secs,
                    stat.slices,
                    format_with_commas(max_gap_ns),
                    utilization * 100.0
                );
            }
        }

        // Compute percentiles, excluding final partial window if present
        let utilization_windows = if has_partial_window && windows.len() > 1 {
            &windows[..windows.len() - 1]
        } else {
            &windows[..]
        };

        let percentile_values = if !utilization_windows.is_empty() {
            let mut utilizations: Vec<f64> = utilization_windows
                .iter()
                .map(|&cyc| cyc as f64 / window_cycles as f64)
                .collect();
            utilizations.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let percentile_keys = [1, 25, 50, 75, 99];

            if verbose {
                eprintln!(
                    "Windowed utilization percentiles ({}ms windows, {} complete windows):",
                    WINDOW_SIZE_MS,
                    utilizations.len()
                );
            }

            let mut pcts = [0.0; 5];
            for (i, &p) in percentile_keys.iter().enumerate() {
                let idx = ((utilizations.len() as f64) * (p as f64 / 100.0)).floor() as usize;
                let idx = idx.min(utilizations.len().saturating_sub(1));
                let val = utilizations.get(idx).copied().unwrap_or(0.0) * 100.0;
                pcts[i] = val;
                if verbose {
                    eprintln!("p{:02}: {:.2}%", p, val);
                }
            }

            Percentiles {
                p01: pcts[0],
                p25: pcts[1],
                p50: pcts[2],
                p75: pcts[3],
                p99: pcts[4],
            }
        } else {
            Percentiles {
                p01: 0.0,
                p25: 0.0,
                p50: 0.0,
                p75: 0.0,
                p99: 0.0,
            }
        };

        // Compute timeslice length percentiles
        let timeslice_percentiles = if !slices.is_empty() {
            let mut slice_lengths_ms: Vec<f64> = slices
                .iter()
                .map(|(start, end)| {
                    let cycles = end - start;
                    cycles as f64 / tsc_hz as f64 * 1e3
                })
                .collect();
            slice_lengths_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let percentile_keys = [1, 25, 50, 75, 99];

            if verbose {
                eprintln!(
                    "\nTimeslice length percentiles ({} slices):",
                    slice_lengths_ms.len()
                );
            }

            let mut pcts = [0.0; 5];
            for (i, &p) in percentile_keys.iter().enumerate() {
                let idx = ((slice_lengths_ms.len() as f64) * (p as f64 / 100.0)).floor() as usize;
                let idx = idx.min(slice_lengths_ms.len().saturating_sub(1));
                let val_ms = slice_lengths_ms.get(idx).copied().unwrap_or(0.0);
                pcts[i] = val_ms;
                if verbose {
                    eprintln!("p{:02}: {:.3} ms", p, val_ms);
                }
            }

            Percentiles {
                p01: pcts[0],
                p25: pcts[1],
                p50: pcts[2],
                p75: pcts[3],
                p99: pcts[4],
            }
        } else {
            Percentiles {
                p01: 0.0,
                p25: 0.0,
                p50: 0.0,
                p75: 0.0,
                p99: 0.0,
            }
        };

        // Build window stats for export
        let json_window_stats: Vec<WindowStat> = stats_to_export
            .iter()
            .map(|stat| {
                let start_time_ns = (stat.start_time_secs * 1e9) as u64;
                let max_gap_ns = (stat.max_gap_cycles as f64 / tsc_hz as f64 * 1e9) as u64;
                let util_pct = stat.cycles as f64 / window_cycles as f64 * 100.0;
                WindowStat {
                    start_time_ns,
                    num_slices: stat.slices,
                    max_gap_ns,
                    util_pct,
                }
            })
            .collect();

        BenchmarkResults {
            tsc_frequency: tsc_hz,
            time_scheduled_ns: scheduled_ns,
            bogo_ops: count,
            total_tsc_ticks: total_cycles,
            per_window_stats: json_window_stats,
            utilization_pcts: percentile_values,
            timeslice_pcts: timeslice_percentiles,
        }
    } else {
        // No slices, empty results
        BenchmarkResults {
            tsc_frequency: tsc_hz,
            time_scheduled_ns: scheduled_ns,
            bogo_ops: count,
            total_tsc_ticks: total_cycles,
            per_window_stats: vec![],
            utilization_pcts: Percentiles {
                p01: 0.0,
                p25: 0.0,
                p50: 0.0,
                p75: 0.0,
                p99: 0.0,
            },
            timeslice_pcts: Percentiles {
                p01: 0.0,
                p25: 0.0,
                p50: 0.0,
                p75: 0.0,
                p99: 0.0,
            },
        }
    }
}

/// CPU hog workload using spinner_utilization to measure actual scheduled time.
///
/// This function waits for a start signal, then runs a CPU-intensive spinner
/// for the specified duration, measuring the actual time scheduled via TSC.
/// The scheduled time (in nanoseconds) and bogo_ops are written to shared memory.
/// Upon completion, prints summary statistics including slice count.
///
/// # Arguments
/// * `name` - Descriptive name for this worker (e.g., "victim", "control")
/// * `duration` - How long to run the spinner
/// * `start_signal` - Shared atomic flag; spins until this becomes non-zero
/// * `scheduled_ns_out` - Shared atomic where the scheduled time (ns) will be written
/// * `bogo_ops_out` - Optional shared atomic where the bogo ops count will be written
pub fn cpu_hog_workload(
    name: &str,
    duration: Duration,
    start_signal: SharedBox<AtomicU32>,
    scheduled_ns_out: SharedBox<AtomicU64>,
    bogo_ops_out: Option<SharedBox<AtomicU64>>,
) {
    // Get TSC frequency (non-verbose)
    let tsc_hz = get_tsc_hz(false);

    // Wait for start signal
    while start_signal.load(Ordering::Acquire) == 0 {
        std::hint::spin_loop();
    }

    // Run spinner for the specified duration
    let results = run_spinner(duration, tsc_hz, false);

    // Write scheduled time (in nanoseconds) to shared memory
    scheduled_ns_out.store(results.time_scheduled_ns, Ordering::Release);

    // Write bogo_ops if output location provided
    if let Some(bogo_ops) = bogo_ops_out {
        bogo_ops.store(results.bogo_ops, Ordering::Release);
    }

    // Add small random delay (0.0-0.5 seconds) to space out parallel worker summaries
    let delay_ms = (rand::random::<f64>() * 500.0) as u64;
    std::thread::sleep(Duration::from_millis(delay_ms));

    // Build summary output in a buffer for atomic printing
    use std::fmt::Write;
    let mut output = String::new();

    writeln!(output, "\n=== Worker '{}' Summary ===", name).unwrap();
    writeln!(output, "  Scheduled time:  {:.3} seconds", results.time_scheduled_ns as f64 / 1e9).unwrap();
    writeln!(output, "  Bogo ops:        {}", format_with_commas(results.bogo_ops)).unwrap();
    writeln!(output, "  Total slices:    {}", results.per_window_stats.iter().map(|w| w.num_slices).sum::<usize>()).unwrap();
    writeln!(output, "  Windows:         {}", results.per_window_stats.len()).unwrap();

    // Find max gap across all windows
    let max_gap_ns = results.per_window_stats
        .iter()
        .map(|w| w.max_gap_ns)
        .max()
        .unwrap_or(0);
    writeln!(output, "  Max gap:         {} ns", format_with_commas(max_gap_ns)).unwrap();

    // Print utilization percentiles
    writeln!(output, "  Utilization percentiles:").unwrap();
    writeln!(output, "    p01: {:.2}%", results.utilization_pcts.p01).unwrap();
    writeln!(output, "    p25: {:.2}%", results.utilization_pcts.p25).unwrap();
    writeln!(output, "    p50: {:.2}%", results.utilization_pcts.p50).unwrap();
    writeln!(output, "    p75: {:.2}%", results.utilization_pcts.p75).unwrap();
    writeln!(output, "    p99: {:.2}%", results.utilization_pcts.p99).unwrap();

    // Print timeslice length percentiles
    writeln!(output, "  Timeslice length percentiles:").unwrap();
    writeln!(output, "    p01: {:.3} ms", results.timeslice_pcts.p01).unwrap();
    writeln!(output, "    p25: {:.3} ms", results.timeslice_pcts.p25).unwrap();
    writeln!(output, "    p50: {:.3} ms", results.timeslice_pcts.p50).unwrap();
    writeln!(output, "    p75: {:.3} ms", results.timeslice_pcts.p75).unwrap();
    writeln!(output, "    p99: {:.3} ms", results.timeslice_pcts.p99).unwrap();

    // Atomically print the entire summary with a single eprintln! call
    eprintln!("{}", output);
}
