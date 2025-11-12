//! Binary wrapper for CPU benchmark
//!
//! This delegates to the benchmark implementation in the library.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use std::thread;
use schtest::workloads::spinner_utilization;
use nix::unistd::{fork, ForkResult};
use nix::sys::wait::waitpid;
use libc;
use clap::Parser;
use rand::Rng;

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

/// Print rusage for given who (RUSAGE_SELF or RUSAGE_CHILDREN)
fn print_rusage(label: &str, who: i32) {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::getrusage(who, &mut usage) };
    if ret == 0 {
        let user_secs = usage.ru_utime.tv_sec as f64 + (usage.ru_utime.tv_usec as f64 / 1e6);
        let sys_secs = usage.ru_stime.tv_sec as f64 + (usage.ru_stime.tv_usec as f64 / 1e6);
        eprintln!("{} => user: {:.3}s, sys: {:.3}s", label, user_secs, sys_secs);
    } else {
        eprintln!("getrusage failed for {}", label);
    }
}

/// Run the benchmark inside a cgroup with CPU bandwidth limit
fn run_with_cgroup(cpu_pct: u64, duration: Duration, verbose: bool) {
    let tsc_hz = spinner_utilization::get_tsc_hz(verbose);

    // Create shared memory for shutdown signal + ready flag using anonymous mmap
    let prot = libc::PROT_READ | libc::PROT_WRITE;
    let flags = libc::MAP_SHARED | libc::MAP_ANONYMOUS;
    let ptr = unsafe {
        libc::mmap(std::ptr::null_mut(), 4096, prot, flags, -1, 0) as *mut AtomicU32
    };
    if ptr.is_null() || ptr as isize == -1 {
        eprintln!("Failed to mmap shared memory");
        std::process::exit(1);
    }

    let shutdown_flag = ptr;
    let ready_flag = unsafe { ptr.add(1) };
    unsafe {
        (*shutdown_flag).store(0, Ordering::Relaxed);
        (*ready_flag).store(0, Ordering::Relaxed);
    }

    // Create cgroup at root level with a unique name
    let mut rng = rand::thread_rng();
    let name = format!("schtest-{}", rng.gen::<u32>());
    let cg_path = std::path::PathBuf::from("/sys/fs/cgroup").join(&name);
    
    if let Err(e) = std::fs::create_dir_all(&cg_path) {
        eprintln!("Failed to create cgroup directory: {}", e);
        std::process::exit(1);
    }
    
    if verbose {
        eprintln!("Created cgroup at: {:?}", cg_path);
    }

    // Configure CPU cap based on percentage (cgroup v2 cpu.max or v1 cfs quota/period)
    let quota = cpu_pct * 1000;  // e.g., 50% -> 50000 per 100000
    let period = 100000u64;
    
    let cpu_max = cg_path.join("cpu.max");
    if cpu_max.exists() {
        // format: quota period
        if verbose {
            eprintln!("Writing to cpu.max: {} {}", quota, period);
        }
        if let Err(e) = std::fs::write(&cpu_max, format!("{} {}", quota, period)) {
            eprintln!("Failed to write cpu.max: {}", e);
            std::process::exit(1);
        }
    } else {
        let cfs_quota = cg_path.join("cpu.cfs_quota_us");
        let cfs_period = cg_path.join("cpu.cfs_period_us");
        if cfs_quota.exists() && cfs_period.exists() {
            if verbose {
                eprintln!("Writing to cfs_quota_us: {}", quota);
            }
            let _ = std::fs::write(&cfs_quota, quota.to_string());
            let _ = std::fs::write(&cfs_period, period.to_string());
        } else {
            eprintln!("Error: neither cpu.max nor cfs_quota_us found!");
            std::process::exit(1);
        }
    }

    let child_pid = match unsafe { fork() } {
        Ok(ForkResult::Parent { child }) => child,
        Ok(ForkResult::Child) => {
            // Child: wait for parent to set ready flag (meaning we're in the cgroup)
            while unsafe { (*ready_flag).load(Ordering::Acquire) == 0 } {
                std::hint::spin_loop();
            }
            
            // Now run the benchmark until shutdown flag set
            let results = spinner_utilization::run_spinner_with_shutdown(shutdown_flag, tsc_hz, verbose);
            
            // Output JSON to stdout
            println!("{}", serde_json::to_string_pretty(&results).unwrap());
            
            print_rusage("Child rusage", libc::RUSAGE_SELF);
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("Fork failed: {}", e);
            std::process::exit(1);
        }
    };

    // Parent: add child to cgroup
    let procs = if cg_path.join("cgroup.procs").exists() {
        cg_path.join("cgroup.procs")
    } else {
        cg_path.join("tasks")
    };
    if let Err(e) = std::fs::write(&procs, format!("{}", child_pid.as_raw())) {
        eprintln!("Failed to add child to cgroup: {}", e);
        std::process::exit(1);
    }

    // Signal child it's ready to run
    unsafe {
        (*ready_flag).store(1, Ordering::Release);
    }

    // Sleep for the specified duration
    thread::sleep(duration);

    // Signal child to stop
    unsafe {
        (*shutdown_flag).store(1, Ordering::Relaxed);
    }

    // Wait for child
    if let Err(e) = waitpid(child_pid, None) {
        eprintln!("waitpid failed: {}", e);
    }

    // Print rusage for child
    print_rusage("Parent rusage (should show child's time)", libc::RUSAGE_CHILDREN);

    // Cleanup cgroup
    let _ = std::fs::remove_dir_all(&cg_path);

    // Cleanup shared memory
    unsafe {
        libc::munmap(ptr as *mut libc::c_void, 4096);
    }
}

fn main() {
    let args = Args::parse();
    let duration = Duration::from_secs(args.seconds);

    if let Some(cpu_pct) = args.cgroup_cpu {
        // Fork mode: run benchmark in cgroup
        run_with_cgroup(cpu_pct, duration, args.verbose);
    } else {
        // Direct mode: run benchmark in current process
        let tsc_hz = spinner_utilization::get_tsc_hz(args.verbose);
        let results = spinner_utilization::run_spinner(duration, tsc_hz, args.verbose);
        
        // Output JSON to stdout
        println!("{}", serde_json::to_string_pretty(&results).unwrap());
        
        print_rusage("Self rusage", libc::RUSAGE_SELF);
    }
}
