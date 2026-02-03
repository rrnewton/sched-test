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

/// Estimate CPU frequency in Hz (cycles/sec)
fn estimate_cpu_hz() -> u64 {
    // Crude estimation: measure cycles over 100ms
    let start = rdtsc();
    let t0 = Instant::now();
    thread::sleep(Duration::from_millis(100));
    let elapsed = t0.elapsed().as_secs_f64();
    let end = rdtsc();
    let cycles = end - start;
    (cycles as f64 / elapsed) as u64
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

/// Worker loop: runs until shutdown, records time slices
fn worker_shared(shutdown_flag: *const AtomicU32, cpu_hz: u64) {
    let worker_start = Instant::now();
    let mut count: u64 = 0;
    let mut slices = Vec::new();
    let mut last_cycle = rdtsc();
    let mut last_time = Instant::now();
    let mut in_slice = false;
    let mut slice_start = 0u64;
    let mut slice_start_time = last_time;
    while unsafe { (*shutdown_flag).load(Ordering::Relaxed) == 0 } {
        count += 1;
        let cur_cycle = rdtsc();
        let cur_time = Instant::now();
        let gap = cur_cycle - last_cycle;
        // If gap is large, treat as deschedule event
        if gap > MIN_DESCHEDULE_CYCLES {
            if in_slice {
                let slice_end = last_cycle;
                let slice_end_time = last_time;
                slices.push((slice_start, slice_end, slice_start_time, slice_end_time));
                in_slice = false;
            }
        } else {
            if !in_slice {
                slice_start = cur_cycle;
                slice_start_time = cur_time;
                in_slice = true;
            }
        }
        last_cycle = cur_cycle;
        last_time = cur_time;
    }
    // Final slice
    if in_slice {
        slices.push((slice_start, last_cycle, slice_start_time, last_time));
    }
    // Print results
    let elapsed_secs = worker_start.elapsed().as_secs_f64();
    let total_cycles: u64 = slices.iter().map(|(start, end, _, _)| end - start).sum();
    let total_time: f64 = slices
        .iter()
        .map(|(_, _, t0, t1)| t1.duration_since(*t0).as_secs_f64())
        .sum();
    let scheduled_secs = total_time;
    println!("Elapsed time: {:.3} seconds", elapsed_secs);
    println!("Worker was scheduled for {:.3} seconds, {} iterations", scheduled_secs, format_with_commas(count));
    println!("Total cycles: {}", format_with_commas(total_cycles));
    // Windowed utilization
    let window = Duration::from_millis(WINDOW_SIZE_MS);
    let mut windows = Vec::new();
    let mut cur_win_start = slices.first().map(|(_,_,t0,_)| *t0).unwrap_or(Instant::now());
    let mut cur_win_end = cur_win_start + window;
    let mut cur_win_cycles = 0u64;
    for (start, end, t0, t1) in &slices {
        let mut seg_start = *t0;
        let seg_end = *t1;
        let mut seg_cycles = end - start;
        while seg_end > cur_win_end {
            let seg_dur = cur_win_end.duration_since(seg_start).as_secs_f64();
            let seg_frac = seg_dur / t1.duration_since(*t0).as_secs_f64();
            let win_cycles = (seg_cycles as f64 * seg_frac) as u64;
            cur_win_cycles += win_cycles;
            windows.push(cur_win_cycles);
            cur_win_start = cur_win_end;
            cur_win_end += window;
            cur_win_cycles = 0;
            seg_start = cur_win_start;
            seg_cycles -= win_cycles;
        }
        cur_win_cycles += seg_cycles;
    }
    if cur_win_cycles > 0 {
        windows.push(cur_win_cycles);
    }
    // Compute percentiles
    let mut utilizations: Vec<f64> = windows.iter().map(|&cyc| cyc as f64 / (cpu_hz as f64 * window.as_secs_f64())).collect();
    utilizations.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let percentiles = [1, 25, 50, 75, 99];
    println!("Windowed utilization percentiles ({}ms windows):", window.as_millis());
    for &p in &percentiles {
        let idx = ((utilizations.len() as f64) * (p as f64 / 100.0)).floor() as usize;
        let idx = idx.min(utilizations.len().saturating_sub(1));
        println!("p{:02}: {:.2}%", p, utilizations.get(idx).copied().unwrap_or(0.0) * 100.0);
    }
}

/// Main benchmark entry point
fn main() {
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

    // Configure ~50% CPU cap (cgroup v2 cpu.max or v1 cfs quota/period)
    let cpu_max = cg_path.join("cpu.max");
    if cpu_max.exists() {
        // format: quota period
        let _ = std::fs::write(&cpu_max, "50000 100000");
    } else {
        let cfs_quota = cg_path.join("cpu.cfs_quota_us");
        let cfs_period = cg_path.join("cpu.cfs_period_us");
        if cfs_quota.exists() && cfs_period.exists() {
            let _ = std::fs::write(&cfs_quota, "50000");
            let _ = std::fs::write(&cfs_period, "100000");
        }
    }

    // Fork child (shares address space mappings)
    match unsafe { fork() } {
        Ok(ForkResult::Child) => {
            let cpu_hz = estimate_cpu_hz();
            worker_shared(shutdown_ptr, cpu_hz);
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

            // Let it run for 3 seconds
            thread::sleep(Duration::from_secs(3));

            // Signal shutdown via shared flag
            unsafe { (*(shutdown_ptr as *mut AtomicU32)).store(1, Ordering::Relaxed); }

            // Wait for child to exit
            let _ = waitpid(Pid::from_raw(child.as_raw()), None);
            // Cleanup by dropping cg
        }
        Err(e) => {
            eprintln!("fork failed: {}", e);
            std::process::exit(1);
        }
    }
}
