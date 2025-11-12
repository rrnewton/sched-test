//! Binary wrapper for CPU benchmark
//!
//! This delegates to the benchmark implementation in the library.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use std::thread;
// no exec/Command; using fork
use schtest::util::cgroups::Cgroup;
use std::arch::asm;
use nix::unistd::{fork, ForkResult, Pid};
use nix::sys::wait::waitpid;
use libc;
use clap::Parser;
// use std::ffi::CStr; // kept for future diagnostics if needed

/// CPU bandwidth benchmark for cgroup scheduling
#[derive(Parser, Debug)]
#[command(name = "benchmark_cpu")]
#[command(about = "Measures CPU scheduling behavior under cgroup bandwidth limits")]
struct Args {
    /// Enable verbose per-window statistics
    #[arg(short, long)]
    verbose: bool,

    /// Duration to run the benchmark in seconds
    #[arg(long, default_value = "3")]
    seconds: u64,

    /// Optional: CPU limit percentage (e.g., 50 for 50%). If specified, forks child into a cgroup with this limit.
    #[arg(long)]
    cgroup_cpu: Option<u64>,
}

/// Minimum TSC cycle gap to consider as a descheduling event
const MIN_DESCHEDULE_CYCLES: u64 = 1000;

/// Window size in milliseconds for utilization calculation
const WINDOW_SIZE_MS: u64 = 100;

/// Read the current cycle count using rdtsc
#[inline(always)]
fn rdtsc() -> u64 {
    unsafe {
        let low: u32;
        let high: u32;
        asm!("rdtsc", out("eax") low, out("edx") high);
        ((high as u64) << 32) | (low as u64)
    }
}

/// Read CPU frequency from Linux system files
fn _read_cpu_hz() -> u64 {
    // Try reading from /proc/cpuinfo first
    if let Ok(cpuinfo) = std::fs::read_to_string("/proc/cpuinfo") {
        for line in cpuinfo.lines() {
            if line.starts_with("cpu MHz") {
                if let Some(mhz_str) = line.split(':').nth(1) {
                    if let Ok(mhz) = mhz_str.trim().parse::<f64>() {
                        return (mhz * 1_000_000.0) as u64;
                    }
                }
            }
        }
    }

    // Fallback: try reading from sysfs (CPU 0)
    if let Ok(khz_str) = std::fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq") {
        if let Ok(khz) = khz_str.trim().parse::<u64>() {
            return khz * 1000;
        }
    }

    // Last resort: assume a common frequency (3 GHz)
    eprintln!("Warning: Could not read CPU frequency, assuming 3.0 GHz");
    3_000_000_000
}

/// Read invariant TSC frequency (Hz) from sysfs if available
fn read_tsc_hz() -> Option<u64> {
    let path = "/sys/devices/system/cpu/cpu0/tsc_freq_khz";
    if let Ok(khz_str) = std::fs::read_to_string(path) {
        if let Ok(khz) = khz_str.trim().parse::<u64>() {
            return Some(khz * 1000);
        }
    }
    None
}

/// Calibrate TSC frequency over a short interval; returns measured Hz and elapsed seconds
fn calibrate_tsc_short(duration_ms: u64) -> (u64, f64) {
    let target = std::time::Duration::from_millis(duration_ms);
    let t0 = Instant::now();
    let c0 = rdtsc();
    // Busy-wait until target elapsed; Instant is vDSO-accelerated
    loop {
        if t0.elapsed() >= target { break; }
        std::hint::spin_loop();
    }
    let c1 = rdtsc();
    let elapsed = t0.elapsed().as_secs_f64();
    let measured_hz = ((c1 - c0) as f64 / elapsed) as u64;
    (measured_hz, elapsed)
}

/// Format a number with comma separators for readability
fn format_with_commas(n: u64) -> String {
    n.to_string()
        .as_bytes()
        .rchunks(3)
        .rev()
        .map(|chunk| std::str::from_utf8(chunk).unwrap())
        .collect::<Vec<_>>()
        .join(",")
}

/// Worker loop: runs for specified duration, records time slices
fn worker(duration: Duration, cpu_hz: u64, verbose: bool) {
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
        } else {
            if !in_slice {
                slice_start_cycle = cur_cycle;
                in_slice = true;
            }
        }
        last_cycle = cur_cycle;
    }

    // Final slice
    if in_slice {
        slices.push((slice_start_cycle, last_cycle));
    }

    print_results(worker_start, slices, count, cpu_hz, verbose);
}

/// Worker loop for fork mode: runs until shutdown flag set, records time slices
fn worker_shared(shutdown_flag: *const AtomicU32, cpu_hz: u64, verbose: bool) {
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
        } else {
            if !in_slice {
                slice_start_cycle = cur_cycle;
                in_slice = true;
            }
        }
        last_cycle = cur_cycle;
    }

    // Final slice
    if in_slice {
        slices.push((slice_start_cycle, last_cycle));
    }

    print_results(worker_start, slices, count, cpu_hz, verbose);
}

/// Print benchmark results with statistics
fn print_results(worker_start: Instant, slices: Vec<(u64, u64)>, count: u64, cpu_hz: u64, verbose: bool) {
    // Print results
    let elapsed_secs = worker_start.elapsed().as_secs_f64();
    let total_cycles: u64 = slices.iter().map(|(start, end)| end - start).sum();

    // Convert total cycles to scheduled time
    let scheduled_secs = total_cycles as f64 / cpu_hz as f64;
    println!("Elapsed time: {:.3} seconds", elapsed_secs);
    println!("Worker was scheduled for {:.3} seconds, {} iterations", scheduled_secs, format_with_commas(count));
    println!("Total cycles: {}", format_with_commas(total_cycles));

    // Windowed utilization with optional per-window stats
    // Now working entirely in cycle space, convert to time later
    let window_cycles = (cpu_hz as f64 * (WINDOW_SIZE_MS as f64 / 1000.0)) as u64;
    let mut windows = Vec::new();

    if !slices.is_empty() {
        let first_cycle = slices[0].0;

        // Store window stats for later printing in chronological order
        struct WindowStats {
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

                if verbose {
                    let start_time_secs = (cur_win_start - first_cycle) as f64 / cpu_hz as f64;
                    window_stats.push(WindowStats {
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
            if verbose {
                let start_time_secs = (cur_win_start - first_cycle) as f64 / cpu_hz as f64;
                window_stats.push(WindowStats {
                    start_time_secs,
                    slices: cur_win_slices,
                    max_gap_cycles: cur_win_max_gap_cycles,
                    cycles: cur_win_cycles,
                });
            }
        }

        // Print window stats in chronological order (excluding partial final window)
        if verbose && !window_stats.is_empty() {
            println!("\nPer-window statistics:");
            let stats_to_print = if has_partial_window && window_stats.len() > 1 {
                &window_stats[..window_stats.len() - 1]  // Exclude last partial window
            } else {
                &window_stats[..]
            };
            for stat in stats_to_print {
                let utilization = stat.cycles as f64 / window_cycles as f64;
                let max_gap_ns = (stat.max_gap_cycles as f64 / cpu_hz as f64 * 1e9) as u64;
                println!("  Window {:.1}s: {} slices, max gap: {} ns, util: {:.2}%",
                    stat.start_time_secs, stat.slices, format_with_commas(max_gap_ns), utilization * 100.0);
            }
        }

        // Compute percentiles, excluding final partial window if present
        let utilization_windows = if has_partial_window && windows.len() > 1 {
            &windows[..windows.len() - 1]  // Exclude last partial window
        } else {
            &windows[..]
        };
        
        if !utilization_windows.is_empty() {
            let mut utilizations: Vec<f64> = utilization_windows.iter().map(|&cyc| cyc as f64 / window_cycles as f64).collect();
            utilizations.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let percentiles = [1, 25, 50, 75, 99];
            println!("Windowed utilization percentiles ({}ms windows, {} complete windows):", 
                WINDOW_SIZE_MS, utilizations.len());
            for &p in &percentiles {
                let idx = ((utilizations.len() as f64) * (p as f64 / 100.0)).floor() as usize;
                let idx = idx.min(utilizations.len().saturating_sub(1));
                println!("p{:02}: {:.2}%", p, utilizations.get(idx).copied().unwrap_or(0.0) * 100.0);
            }
        }
    }
}

/// Print rusage for given who (RUSAGE_SELF or RUSAGE_CHILDREN)
fn print_rusage(label: &str, who: i32) {
    unsafe {
        let mut ru: libc::rusage = std::mem::zeroed();
        if libc::getrusage(who, &mut ru) == 0 {
            let ut = ru.ru_utime.tv_sec as f64 + (ru.ru_utime.tv_usec as f64) / 1e6;
            let st = ru.ru_stime.tv_sec as f64 + (ru.ru_stime.tv_usec as f64) / 1e6;
            eprintln!("rusage {}: user={:.6}s sys={:.6}s", label, ut, st);
        } else {
            eprintln!("rusage {}: getrusage failed", label);
        }
    }
}

/// Main benchmark entry point
fn main() {
    let args = Args::parse();

    // Prefer sysfs TSC if available, else calibrate
    let tsc_hz = match read_tsc_hz() {
        Some(hz) => {
            eprintln!("TSC frequency: {} Hz (from sysfs)", format_with_commas(hz));
            hz
        }
        None => {
            eprintln!("TSC frequency: sysfs unavailable, calibrating...");
            let calib_ms = 5u64;
            let (measured_hz, elapsed) = calibrate_tsc_short(calib_ms);
            eprintln!(
                "TSC frequency: {} Hz (measured over {:.3} ms)",
                format_with_commas(measured_hz),
                elapsed * 1000.0
            );
            measured_hz
        }
    };

    let duration = Duration::from_secs(args.seconds);

    // If --cgroup-cpu specified, fork into a cgroup with specified limit
    if let Some(cpu_percent) = args.cgroup_cpu {
        run_with_cgroup(cpu_percent, duration, tsc_hz, args.verbose);
    } else {
        // Direct worker call, no fork, no cgroup
        worker(duration, tsc_hz, args.verbose);
    }
}

/// Run worker in a forked child with cgroup CPU limit
fn run_with_cgroup(cpu_percent: u64, duration: Duration, tsc_hz: u64, verbose: bool) {
    // Create shared shutdown flag using anonymous shared mmap
    let flag_size = std::mem::size_of::<AtomicU32>();
    let shutdown_ptr = unsafe {
        let addr = libc::mmap(
            std::ptr::null_mut(),
            flag_size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_ANONYMOUS,
            -1,
            0,
        );
        if addr == libc::MAP_FAILED {
            panic!("mmap failed for shutdown flag");
        }
        let flag_ptr = addr as *mut AtomicU32;
        std::ptr::write(flag_ptr, AtomicU32::new(0));
        flag_ptr as *const AtomicU32
    };

    // Parent: create cgroup
    let cg = Cgroup::create().expect("Failed to create cgroup");
    let cg_path = cg.info().path().clone();

    // Configure CPU cap based on percentage (cgroup v2 cpu.max or v1 cfs quota/period)
    let quota = cpu_percent * 1000;  // e.g., 50% -> 50000 per 100000
    let period = 100000u64;
    
    let cpu_max = cg_path.join("cpu.max");
    if cpu_max.exists() {
        // format: quota period
        let _ = std::fs::write(&cpu_max, format!("{} {}", quota, period));
    } else {
        let cfs_quota = cg_path.join("cpu.cfs_quota_us");
        let cfs_period = cg_path.join("cpu.cfs_period_us");
        if cfs_quota.exists() && cfs_period.exists() {
            let _ = std::fs::write(&cfs_quota, quota.to_string());
            let _ = std::fs::write(&cfs_period, period.to_string());
        }
    }

    // Fork child (shares address space mappings)
    match unsafe { fork() } {
        Ok(ForkResult::Child) => {
            // In the child: run worker and print our own rusage before exit so
            // we can compare kernel accounting with external `time` output.
            worker_shared(shutdown_ptr, tsc_hz, verbose);
            print_rusage("child_self", libc::RUSAGE_SELF);
            // Exit explicitly
            std::process::exit(0);
        }
        Ok(ForkResult::Parent { child }) => {
            // Move child to cgroup: prefer cgroup.procs (v2), else tasks (v1)
            let procs = if cg_path.join("cgroup.procs").exists() {
                cg_path.join("cgroup.procs")
            } else {
                cg_path.join("tasks")
            };
            std::fs::write(&procs, format!("{}", child.as_raw()))
                .expect("Failed to add child to cgroup");

            // Let it run for specified duration
            thread::sleep(duration);

            // Signal shutdown via shared flag
            unsafe { (*(shutdown_ptr as *mut AtomicU32)).store(1, Ordering::Relaxed); }

            // Wait for child to exit
            let _ = waitpid(Pid::from_raw(child.as_raw()), None);
            // Print aggregated rusage for children so we can see how the kernel
            // accounted the child's CPU time.
            print_rusage("children", libc::RUSAGE_CHILDREN);
            // Cleanup by dropping cg
        }
        Err(e) => {
            eprintln!("fork failed: {}", e);
            std::process::exit(1);
        }
    }
}
