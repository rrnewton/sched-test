//! Common infrastructure for IRQ disruption tests.
//!
//! This module provides shared utilities for tests that measure the impact of
//! IRQ load on scheduler behavior. It includes:
//! - Interrupt snapshot capture from /proc/interrupts
//! - Multiple IRQ disruption strategies (timer, futex IPI, PMU sampling)
//! - Helper functions for environment variables and logging

use std::collections::HashMap;
use std::env::VarError;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::util::child::Child;
use crate::util::shared::{BumpAllocator, SharedBox};
use crate::util::system::{default_scheduler_name, CPUMask, CPUSet, Hyperthread};

/// Per-CPU interrupt counts parsed from /proc/interrupts
#[derive(Debug, Clone)]
pub struct InterruptSnapshot {
    /// Total interrupt count per CPU (sum of all interrupt types)
    pub per_cpu_total: HashMap<usize, u64>,
    /// Rescheduling interrupts (RES) - IPIs for rescheduling
    pub per_cpu_reschedule: HashMap<usize, u64>,
    /// Function call interrupts (CAL) - IPIs for function calls
    pub per_cpu_function_call: HashMap<usize, u64>,
    /// TLB shootdown interrupts (TLB) - IPIs for TLB invalidation
    pub per_cpu_tlb: HashMap<usize, u64>,
    /// Number of CPUs detected
    pub num_cpus: usize,
}

impl InterruptSnapshot {
    /// Parse /proc/interrupts and sum all interrupt types per CPU
    pub fn capture() -> Result<Self> {
        let data = std::fs::read_to_string("/proc/interrupts")
            .context("Failed to read /proc/interrupts")?;

        let mut lines = data.lines();

        // First line contains CPU headers: "CPU0  CPU1  CPU2  ..."
        let header = lines
            .next()
            .ok_or_else(|| anyhow::anyhow!("Empty /proc/interrupts"))?;
        let num_cpus = header.split_whitespace().count();

        let mut per_cpu_total: HashMap<usize, u64> = HashMap::new();
        let mut per_cpu_reschedule: HashMap<usize, u64> = HashMap::new();
        let mut per_cpu_function_call: HashMap<usize, u64> = HashMap::new();
        let mut per_cpu_tlb: HashMap<usize, u64> = HashMap::new();

        for cpu in 0..num_cpus {
            per_cpu_total.insert(cpu, 0);
            per_cpu_reschedule.insert(cpu, 0);
            per_cpu_function_call.insert(cpu, 0);
            per_cpu_tlb.insert(cpu, 0);
        }

        // Parse each interrupt line
        for line in lines {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.is_empty() {
                continue;
            }

            // Identify interrupt type
            let is_reschedule = line.contains("Rescheduling interrupts");
            let is_function_call = line.contains("Function call interrupts");
            let is_tlb = line.contains("TLB shootdowns");

            // Skip the interrupt number/name (first column)
            // Next num_cpus columns are the counts per CPU
            for (cpu_idx, count_str) in parts.iter().skip(1).take(num_cpus).enumerate() {
                if let Ok(count) = count_str.parse::<u64>() {
                    *per_cpu_total.entry(cpu_idx).or_insert(0) += count;

                    if is_reschedule {
                        *per_cpu_reschedule.entry(cpu_idx).or_insert(0) += count;
                    } else if is_function_call {
                        *per_cpu_function_call.entry(cpu_idx).or_insert(0) += count;
                    } else if is_tlb {
                        *per_cpu_tlb.entry(cpu_idx).or_insert(0) += count;
                    }
                }
            }
        }

        Ok(InterruptSnapshot {
            per_cpu_total,
            per_cpu_reschedule,
            per_cpu_function_call,
            per_cpu_tlb,
            num_cpus,
        })
    }

    /// Calculate delta from another snapshot (self - other)
    pub fn delta(&self, other: &InterruptSnapshot) -> InterruptDelta {
        let mut total = HashMap::new();
        let mut reschedule = HashMap::new();
        let mut function_call = HashMap::new();
        let mut tlb = HashMap::new();

        for cpu in 0..self.num_cpus {
            total.insert(
                cpu,
                *self.per_cpu_total.get(&cpu).unwrap_or(&0) as i64
                    - *other.per_cpu_total.get(&cpu).unwrap_or(&0) as i64,
            );
            reschedule.insert(
                cpu,
                *self.per_cpu_reschedule.get(&cpu).unwrap_or(&0) as i64
                    - *other.per_cpu_reschedule.get(&cpu).unwrap_or(&0) as i64,
            );
            function_call.insert(
                cpu,
                *self.per_cpu_function_call.get(&cpu).unwrap_or(&0) as i64
                    - *other.per_cpu_function_call.get(&cpu).unwrap_or(&0) as i64,
            );
            tlb.insert(
                cpu,
                *self.per_cpu_tlb.get(&cpu).unwrap_or(&0) as i64
                    - *other.per_cpu_tlb.get(&cpu).unwrap_or(&0) as i64,
            );
        }

        InterruptDelta {
            total,
            reschedule,
            function_call,
            tlb,
        }
    }
}

/// Delta of interrupt counts between two snapshots
#[derive(Debug)]
pub struct InterruptDelta {
    pub total: HashMap<usize, i64>,
    pub reschedule: HashMap<usize, i64>,
    pub function_call: HashMap<usize, i64>,
    pub tlb: HashMap<usize, i64>,
}

/// Different methods for generating IRQ load on the victim CPU
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IrqDisruptionMode {
    /// No IRQ disruption - baseline measurement
    None,
    /// Use futex_wait/futex_wake to generate cross-core IPI wakeups
    Futex,
    /// Use perf_event_open to generate PMI (Performance Monitoring Interrupt) sampling
    Pmu,
    /// Use timerfd for high-frequency timer interrupts
    #[default]
    Timer,
    /// Use all methods simultaneously for maximum IRQ pressure
    Combined,
}

/// Unified handle for any IRQ disruption strategy
pub enum IrqDisruptionHandle {
    None,
    Futex(FutexIpiDisruptionHandle),
    Pmu(PmuIrqHandle),
    Timer(TimerIrqHandle),
    Combined {
        futex: FutexIpiDisruptionHandle,
        pmu: PmuIrqHandle,
        timer: TimerIrqHandle,
    },
}

impl IrqDisruptionHandle {
    pub fn stop(self) -> Result<IrqDisruptionStats> {
        match self {
            IrqDisruptionHandle::None => Ok(IrqDisruptionStats::None),
            IrqDisruptionHandle::Futex(handle) => handle.stop(),
            IrqDisruptionHandle::Pmu(handle) => {
                let _ = handle.stop()?;
                Ok(IrqDisruptionStats::Pmu)
            }
            IrqDisruptionHandle::Timer(handle) => {
                let timer_wakeups = handle.stop()?;
                Ok(IrqDisruptionStats::Timer { timer_wakeups })
            }
            IrqDisruptionHandle::Combined { futex, pmu, timer } => {
                let futex_stats = futex.stop()?;
                let _ = pmu.stop()?;
                let timer_wakeups = timer.stop()?;

                match futex_stats {
                    IrqDisruptionStats::Futex {
                        wakeup_count,
                        futex_wait_calls,
                        futex_wait_blocks,
                        futex_wait_eagain,
                    } => Ok(IrqDisruptionStats::Combined {
                        wakeup_count,
                        futex_wait_calls,
                        futex_wait_blocks,
                        futex_wait_eagain,
                        timer_wakeups,
                    }),
                    _ => unreachable!(),
                }
            }
        }
    }
}

/// Statistics from IRQ disruption
pub enum IrqDisruptionStats {
    None,
    Futex {
        wakeup_count: u64,
        futex_wait_calls: u64,
        futex_wait_blocks: u64,
        futex_wait_eagain: u64,
    },
    Pmu,
    Timer {
        timer_wakeups: u64,
    },
    Combined {
        wakeup_count: u64,
        futex_wait_calls: u64,
        futex_wait_blocks: u64,
        futex_wait_eagain: u64,
        timer_wakeups: u64,
    },
}

/// Handle for futex-based IPI disruption strategy
pub struct FutexIpiDisruptionHandle {
    waker_child: Child,
    receiver_child: Child,
    stop_signal: SharedBox<AtomicU32>,
    wakeup_count: SharedBox<AtomicU64>,
    // Instrumentation counters
    futex_wait_calls: SharedBox<AtomicU64>,
    futex_wait_blocks: SharedBox<AtomicU64>,
    futex_wait_eagain: SharedBox<AtomicU64>,
}

impl FutexIpiDisruptionHandle {
    /// Stop the IPI disruption and return stats
    pub fn stop(self) -> Result<IrqDisruptionStats> {
        self.stop_signal.store(1, Ordering::Release);
        std::thread::sleep(Duration::from_millis(100));
        drop(self.waker_child);
        drop(self.receiver_child);

        Ok(IrqDisruptionStats::Futex {
            wakeup_count: self.wakeup_count.load(Ordering::Acquire),
            futex_wait_calls: self.futex_wait_calls.load(Ordering::Acquire),
            futex_wait_blocks: self.futex_wait_blocks.load(Ordering::Acquire),
            futex_wait_eagain: self.futex_wait_eagain.load(Ordering::Acquire),
        })
    }
}

/// Launch futex-based IPI disruption (waker on waker_cpu, receiver on victim_cpu)
///
/// The waker sends futex_wake() at irq_hz frequency to wake the receiver.
/// Each futex_wake() should generate a cross-core IPI to wake the blocked receiver.
pub fn launch_futex_ipi_disruption(
    allocator: std::sync::Arc<BumpAllocator>,
    victim_cpu: &Hyperthread,
    waker_cpu: &Hyperthread,
    start_signal: SharedBox<AtomicU32>,
    irq_hz: u64,
) -> Result<FutexIpiDisruptionHandle> {
    let stop_signal = SharedBox::new(allocator.clone(), AtomicU32::new(0))?;
    let futex_word = SharedBox::new(allocator.clone(), AtomicU32::new(0))?;
    let wakeup_count = SharedBox::new(allocator.clone(), AtomicU64::new(0))?;

    // Create instrumentation counters
    let futex_wait_calls = SharedBox::new(allocator.clone(), AtomicU64::new(0))?;
    let futex_wait_blocks = SharedBox::new(allocator.clone(), AtomicU64::new(0))?;
    let futex_wait_eagain = SharedBox::new(allocator.clone(), AtomicU64::new(0))?;

    // Launch receiver thread on victim CPU
    let receiver_mask = CPUMask::new(victim_cpu);
    let receiver_futex = futex_word.clone();
    let receiver_stop = stop_signal.clone();
    let receiver_start = start_signal.clone();
    let receiver_calls = futex_wait_calls.clone();
    let receiver_blocks = futex_wait_blocks.clone();
    let receiver_eagain = futex_wait_eagain.clone();

    let receiver_child = Child::run(
        move || {
            receiver_mask.run(|| {
                // Wait for start signal
                while receiver_start.load(Ordering::Acquire) == 0 {
                    std::hint::spin_loop();
                }

                let futex_ptr = receiver_futex.as_ptr() as *mut u32;

                loop {
                    // Check if we should stop
                    if receiver_stop.load(Ordering::Acquire) != 0 {
                        break;
                    }

                    // Read current futex value and wait on it (standard futex pattern)
                    let futex_val = receiver_futex.load(Ordering::Acquire);

                    // futex_wait: blocks in kernel until woken by futex_wake
                    // Wait for the CURRENT value to change, not for it to be 0
                    receiver_calls.fetch_add(1, Ordering::Relaxed);
                    unsafe {
                        let ret = libc::syscall(
                            libc::SYS_futex,
                            futex_ptr,
                            libc::FUTEX_WAIT, // No PRIVATE flag - shared across processes
                            futex_val,        // Wait on current value
                            std::ptr::null::<libc::timespec>(),
                            std::ptr::null::<u32>(),
                            0u32,
                        );

                        // Track whether we actually blocked or got EAGAIN
                        if ret == -1 {
                            let errno = *libc::__errno_location();
                            if errno == libc::EAGAIN {
                                receiver_eagain.fetch_add(1, Ordering::Relaxed);
                            } else if errno != libc::EINTR {
                                eprintln!("futex_wait failed: errno={}", errno);
                            }
                        } else {
                            // Successful block and wakeup
                            receiver_blocks.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            })?;
            Ok(())
        },
        None,
    )?;

    // Launch waker thread
    let waker_mask = CPUMask::new(waker_cpu);
    let waker_start = start_signal;
    let waker_stop = stop_signal.clone();
    let waker_futex = futex_word.clone();
    let waker_count_shared = wakeup_count.clone();

    let waker_child = Child::run(
        move || {
            waker_mask.run(|| {
                use std::time::Instant;

                // Wait for start signal
                while waker_start.load(Ordering::Acquire) == 0 {
                    std::hint::spin_loop();
                }

                let start_time = Instant::now();
                let target_interval = Duration::from_nanos(1_000_000_000 / irq_hz);
                let mut wakeups_sent = 0u64;
                let futex_ptr = waker_futex.as_ptr() as *mut u32;

                loop {
                    // Check if we should stop
                    if waker_stop.load(Ordering::Acquire) != 0 {
                        break;
                    }

                    // Calculate when the next wakeup should happen
                    let target_time = start_time + target_interval * (wakeups_sent as u32);

                    // Spin until it's time
                    while Instant::now() < target_time {
                        std::hint::spin_loop();
                    }

                    // Increment futex word and wake the receiver
                    waker_futex.fetch_add(1, Ordering::Release);

                    unsafe {
                        libc::syscall(
                            libc::SYS_futex,
                            futex_ptr,
                            libc::FUTEX_WAKE, // No PRIVATE flag - shared across processes
                            1i32,
                            std::ptr::null::<libc::timespec>(),
                            std::ptr::null::<u32>(),
                            0u32,
                        );
                    }

                    wakeups_sent += 1;

                    // Record count every 1000 wakeups to avoid overhead
                    if wakeups_sent.is_multiple_of(1000) {
                        waker_count_shared.store(wakeups_sent, Ordering::Release);
                    }
                }

                // Final count
                waker_count_shared.store(wakeups_sent, Ordering::Release);
            })?;
            Ok(())
        },
        None,
    )?;

    // Wake the receiver one last time so it can see the stop signal when stopping
    let stop_signal_clone = stop_signal.clone();
    let futex_word_clone = futex_word.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(10)); // Should be longer than test
        if stop_signal_clone.load(Ordering::Acquire) != 0 {
            futex_word_clone.fetch_add(1, Ordering::Release);
            unsafe {
                let futex_ptr = futex_word_clone.as_ptr() as *mut u32;
                libc::syscall(
                    libc::SYS_futex,
                    futex_ptr,
                    libc::FUTEX_WAKE, // No PRIVATE flag - shared across processes
                    1i32,
                    std::ptr::null::<libc::timespec>(),
                    std::ptr::null::<u32>(),
                    0u32,
                );
            }
        }
    });

    Ok(FutexIpiDisruptionHandle {
        waker_child,
        receiver_child,
        stop_signal,
        wakeup_count,
        futex_wait_calls,
        futex_wait_blocks,
        futex_wait_eagain,
    })
}

/// Handle for PMU-based IRQ disruption
pub struct PmuIrqHandle {
    perf_child: Child,
    stop_signal: SharedBox<AtomicU32>,
    sample_count: SharedBox<AtomicU64>,
}

impl PmuIrqHandle {
    /// Stop the PMU disruption and return sample count
    pub fn stop(self) -> Result<u64> {
        self.stop_signal.store(1, Ordering::Release);
        std::thread::sleep(Duration::from_millis(100));
        drop(self.perf_child);
        Ok(self.sample_count.load(Ordering::Acquire))
    }
}

/// Handle for timer-based IRQ disruption
pub struct TimerIrqHandle {
    timer_child: Child,
    stop_signal: SharedBox<AtomicU32>,
    wakeup_count: SharedBox<AtomicU64>,
}

/// Number of parallel timers to create for interrupt load per CPU
pub const NUM_TIMERS: usize = 4;

impl TimerIrqHandle {
    /// Stop the timer disruption and return wakeup count
    pub fn stop(self) -> Result<u64> {
        self.stop_signal.store(1, Ordering::Release);
        std::thread::sleep(Duration::from_millis(100));
        drop(self.timer_child);
        Ok(self.wakeup_count.load(Ordering::Acquire))
    }
}

/// Launch timer-based IRQ disruption on victim CPU
///
/// Creates NUM_TIMERS (8) separate POSIX timers using real-time signals
/// to generate high-frequency timer interrupts. Real-time signals can be
/// queued, allowing for much higher effective interrupt rates than SIGALRM.
pub fn launch_timer_irq_disruption(
    allocator: std::sync::Arc<BumpAllocator>,
    victim_cpu: &Hyperthread,
    start_signal: SharedBox<AtomicU32>,
    timer_hz: u64,
) -> Result<TimerIrqHandle> {
    let stop_signal = SharedBox::new(allocator.clone(), AtomicU32::new(0))?;
    let wakeup_count = SharedBox::new(allocator.clone(), AtomicU64::new(0))?;

    let cpu_mask = CPUMask::new(victim_cpu);
    let cpu_id = victim_cpu.id(); // Capture CPU ID before moving into closure
    let timer_start = start_signal.clone();
    let timer_stop = stop_signal.clone();
    let timer_count = wakeup_count.clone();

    let timer_child = Child::run(
        move || {
            cpu_mask.run(|| {
                // Wait for start signal
                while timer_start.load(Ordering::Acquire) == 0 {
                    std::hint::spin_loop();
                }

                // Set up signal handlers for real-time signals
                // Real-time signals (SIGRTMIN+n) can be queued unlike standard signals
                static SIGNAL_COUNT: AtomicU64 = AtomicU64::new(0);

                extern "C" fn timer_signal_handler(_sig: i32) {
                    // Minimal work - just count the signal
                    SIGNAL_COUNT.fetch_add(1, Ordering::Relaxed);
                }

                // Install signal handlers for all our real-time signals
                for i in 0..NUM_TIMERS {
                    let signo = libc::SIGRTMIN() + i as i32;
                    let sa = libc::sigaction {
                        sa_sigaction: timer_signal_handler as *const () as usize,
                        sa_mask: unsafe { std::mem::zeroed() },
                        sa_flags: libc::SA_RESTART,
                        sa_restorer: None,
                    };

                    let ret = unsafe { libc::sigaction(signo, &sa, std::ptr::null_mut()) };

                    if ret < 0 {
                        let errno = unsafe { *libc::__errno_location() };
                        panic!("sigaction for SIGRTMIN+{} failed: errno={}", i, errno);
                    }
                }

                // Calculate timer interval in nanoseconds
                let interval_ns = 1_000_000_000 / timer_hz;
                let interval_sec = interval_ns / 1_000_000_000;
                let interval_nsec = interval_ns % 1_000_000_000;

                // Create NUM_TIMERS POSIX timers, each firing at timer_hz
                let mut timer_ids: Vec<libc::timer_t> = Vec::with_capacity(NUM_TIMERS);

                for i in 0..NUM_TIMERS {
                    let mut timer_id: libc::timer_t = std::ptr::null_mut();
                    let mut sev: libc::sigevent = unsafe { std::mem::zeroed() };
                    sev.sigev_notify = libc::SIGEV_SIGNAL;
                    sev.sigev_signo = libc::SIGRTMIN() + i as i32;
                    sev.sigev_value.sival_ptr = std::ptr::null_mut();

                    let ret = unsafe {
                        libc::timer_create(libc::CLOCK_MONOTONIC, &mut sev, &mut timer_id)
                    };

                    if ret < 0 {
                        let errno = unsafe { *libc::__errno_location() };
                        panic!("timer_create {} failed: errno={}", i, errno);
                    }

                    // Stagger initial expiry to spread interrupts across time
                    // Each timer starts at offset (i * interval_ns / NUM_TIMERS)
                    let stagger_ns = (i as u64 * interval_ns) / NUM_TIMERS as u64;
                    let initial_nsec = if stagger_ns == 0 {
                        interval_nsec as i64
                    } else {
                        stagger_ns as i64
                    };

                    let timer_spec = libc::itimerspec {
                        it_interval: libc::timespec {
                            tv_sec: interval_sec as i64,
                            tv_nsec: interval_nsec as i64,
                        },
                        it_value: libc::timespec {
                            tv_sec: 0,
                            tv_nsec: initial_nsec,
                        },
                    };

                    let ret = unsafe {
                        libc::timer_settime(timer_id, 0, &timer_spec, std::ptr::null_mut())
                    };

                    if ret < 0 {
                        let errno = unsafe { *libc::__errno_location() };
                        panic!("timer_settime {} failed: errno={}", i, errno);
                    }

                    timer_ids.push(timer_id);
                }

                eprintln!(
                    "Timer interrupts enabled on CPU {} at {} Hz x {} timers = {} Hz effective (using SIGRTMIN signals)",
                    cpu_id, timer_hz, NUM_TIMERS, timer_hz * NUM_TIMERS as u64
                );

                // Spin in userspace handling signals
                loop {
                    if timer_stop.load(Ordering::Acquire) != 0 {
                        break;
                    }

                    // Update shared counter
                    let count = SIGNAL_COUNT.load(Ordering::Relaxed);
                    timer_count.store(count, Ordering::Release);

                    // Use nanosleep for a very short time to let signals interrupt
                    let sleep_spec = libc::timespec {
                        tv_sec: 0,
                        tv_nsec: 10_000, // 10 microseconds
                    };
                    unsafe {
                        libc::nanosleep(&sleep_spec, std::ptr::null_mut());
                    }
                }

                // Delete all timers
                for timer_id in timer_ids {
                    unsafe {
                        libc::timer_delete(timer_id);
                    }
                }

                // Store final count
                let final_count = SIGNAL_COUNT.load(Ordering::Relaxed);
                timer_count.store(final_count, Ordering::Release);

                eprintln!(
                    "Timer interrupts stopped, {} signals delivered",
                    final_count
                );
            })?;
            Ok(())
        },
        None,
    )?;

    Ok(TimerIrqHandle {
        timer_child,
        stop_signal,
        wakeup_count,
    })
}

/// Determine disruption mode from environment, defaulting to Timer.
/// Set SCHTEST_IRQ_MODE=none|futex|pmu|timer|combined to select mode.
pub fn get_disruption_mode() -> IrqDisruptionMode {
    match std::env::var("SCHTEST_IRQ_MODE") {
        Ok(val) if val.eq_ignore_ascii_case("none") => IrqDisruptionMode::None,
        Ok(val) if val.eq_ignore_ascii_case("futex") => IrqDisruptionMode::Futex,
        Ok(val) if val.eq_ignore_ascii_case("pmu") => IrqDisruptionMode::Pmu,
        Ok(val) if val.eq_ignore_ascii_case("timer") => IrqDisruptionMode::Timer,
        Ok(val) if val.eq_ignore_ascii_case("combined") => IrqDisruptionMode::Combined,
        Ok(val) if val.is_empty() => IrqDisruptionMode::default(),
        Err(VarError::NotPresent) => IrqDisruptionMode::default(),
        Ok(val) => {
            panic!(
                "Invalid SCHTEST_IRQ_MODE value '{}'. Valid options: none, futex, pmu, timer, combined",
                val
            );
        }
        Err(oth) => {
            panic!("Error reading SCHTEST_IRQ_MODE: {}", oth);
        }
    }
}

/// Get test duration from environment variable SCHTEST_IRQ_DURATION (in seconds).
/// Defaults to 10 seconds if not set or invalid.
pub fn get_test_duration() -> Duration {
    match std::env::var("SCHTEST_IRQ_DURATION") {
        Ok(val) => match val.parse::<u64>() {
            Ok(secs) if secs > 0 => Duration::from_secs(secs),
            _ => {
                eprintln!(
                    "Invalid SCHTEST_IRQ_DURATION value '{}', using default 10 seconds",
                    val
                );
                Duration::from_secs(10)
            }
        },
        Err(_) => Duration::from_secs(10),
    }
}

/// Get the reserved tracing core from environment variable SCHTEST_IRQ_RESERVE_TRACING_CORE.
/// Returns None if not set or invalid. When set, this CPU will not receive IRQ disruption,
/// allowing a tracer (e.g., wprof) to run on it without interference.
pub fn get_reserved_tracing_core() -> Option<usize> {
    match std::env::var("SCHTEST_IRQ_RESERVE_TRACING_CORE") {
        Ok(val) => match val.parse::<usize>() {
            Ok(cpu) => Some(cpu),
            Err(_) => {
                eprintln!(
                    "Warning: SCHTEST_IRQ_RESERVE_TRACING_CORE='{}' is not a valid CPU ID",
                    val
                );
                None
            }
        },
        Err(_) => None,
    }
}

/// Read the kernel's maximum allowed perf sample rate
pub fn get_max_perf_sample_rate() -> Result<u64> {
    let rate_str = std::fs::read_to_string("/proc/sys/kernel/perf_event_max_sample_rate")
        .context("Failed to read perf_event_max_sample_rate")?;
    rate_str
        .trim()
        .parse::<u64>()
        .context("Failed to parse perf_event_max_sample_rate")
}

/// Detect and report scheduler information
pub fn log_scheduler_info() {
    fn read_sysfs(path: &str) -> Option<String> {
        std::fs::read_to_string(path)
            .ok()
            .map(|s| s.trim().to_string())
    }

    eprintln!("\nScheduler Information:");

    // Check if sched_ext is active by looking for /sys/kernel/sched_ext
    let sched_ext_path = "/sys/kernel/sched_ext";
    let sched_ext_active = std::path::Path::new(sched_ext_path).exists();

    if sched_ext_active {
        eprintln!("  sched_ext:                    ACTIVE");

        // Try to read the current scheduler name
        if let Some(scheduler) = read_sysfs("/sys/kernel/sched_ext/root/ops") {
            eprintln!("  Current scheduler:            {}", scheduler);
        } else {
            // sched_ext is available but no scheduler loaded - using default kernel scheduler
            let state = read_sysfs("/sys/kernel/sched_ext/state").unwrap_or_default();
            eprintln!("  sched_ext state:              {}", state);
            eprintln!("  Scheduler:                    {} (default)", default_scheduler_name());
        }
    } else {
        eprintln!("  sched_ext:                    NOT ACTIVE");
        eprintln!("  Scheduler:                    {} (default)", default_scheduler_name());
    }

    // Check scheduler features
    if let Some(features) = read_sysfs("/sys/kernel/debug/sched/features") {
        eprintln!(
            "  Scheduler features:           {}",
            if features.len() > 60 {
                &features[..60]
            } else {
                &features
            }
        );
    }
}

/// Log relevant perf sysctls to aid debugging PMU sampling behavior.
pub fn log_perf_sysctls() {
    fn read_sysctl(path: &str) -> Option<String> {
        std::fs::read_to_string(path)
            .ok()
            .map(|s| s.trim().to_string())
    }
    let paranoid = read_sysctl("/proc/sys/kernel/perf_event_paranoid");
    let max_rate = read_sysctl("/proc/sys/kernel/perf_event_max_sample_rate");
    eprintln!("\nPerf sysctls:");
    match paranoid {
        Some(v) => eprintln!("  perf_event_paranoid           = {}", v),
        None => eprintln!("  perf_event_paranoid           = <unavailable>"),
    }
    match max_rate {
        Some(v) => eprintln!("  perf_event_max_sample_rate    = {}", v),
        None => eprintln!("  perf_event_max_sample_rate    = <unavailable>"),
    }
}

/// Launch PMU-based IRQ disruption on victim CPU
///
/// Uses perf_event_open to configure high-frequency PMU sampling on the victim CPU.
/// PMIs (Performance Monitoring Interrupts) are delivered as NMI-like interrupts.
pub fn launch_pmu_irq_disruption(
    allocator: std::sync::Arc<BumpAllocator>,
    victim_cpu: &Hyperthread,
    start_signal: SharedBox<AtomicU32>,
    target_freq_hz: u64,
) -> Result<PmuIrqHandle> {
    // Check kernel's maximum allowed sample rate and cap our request
    let max_kernel_rate = get_max_perf_sample_rate()?;
    let actual_freq_hz = target_freq_hz.min(max_kernel_rate);

    if actual_freq_hz < target_freq_hz {
        eprintln!(
            "WARNING: Requested sample rate {} Hz exceeds kernel limit {} Hz",
            target_freq_hz, max_kernel_rate
        );
        eprintln!("         Using capped rate: {} Hz", actual_freq_hz);
    }

    let stop_signal = SharedBox::new(allocator.clone(), AtomicU32::new(0))?;
    let sample_count = SharedBox::new(allocator.clone(), AtomicU64::new(0))?;

    let cpu_id = victim_cpu.id();
    // Pin the perf child to the victim CPU so PMIs/NMIs are delivered on that core.
    let perf_mask = CPUMask::new(victim_cpu);
    let perf_start = start_signal.clone();
    let perf_stop = stop_signal.clone();
    let perf_count = sample_count.clone();

    let perf_child = Child::run(
        move || {
            perf_mask.run(|| {
                // Wait for start signal
                while perf_start.load(Ordering::Acquire) == 0 {
                    std::hint::spin_loop();
                }

                // perf_event_attr structure for perf_event_open
                // Must match kernel struct - see /usr/include/linux/perf_event.h
                #[repr(C)]
                struct PerfEventAttr {
                    type_: u32,
                    size: u32,
                    config: u64,
                    sample_freq: u64, // Union with sample_period
                    sample_type: u64,
                    read_format: u64,
                    // Bitfield flags packed into u64
                    flags: u64,
                    // Rest of the structure
                    _rest: [u8; 1024], // Padding for remaining fields
                }

                const PERF_TYPE_HARDWARE: u32 = 0;
                const PERF_COUNT_HW_CPU_CYCLES: u64 = 0;
                const PERF_SAMPLE_IP: u64 = 1 << 0; // Sample instruction pointer

                let mut attr: PerfEventAttr = unsafe { std::mem::zeroed() };
                attr.type_ = PERF_TYPE_HARDWARE;
                attr.size = 128; // PERF_ATTR_SIZE_VER7 - a safe modern size
                attr.config = PERF_COUNT_HW_CPU_CYCLES;
                attr.sample_freq = actual_freq_hz;
                attr.sample_type = PERF_SAMPLE_IP; // Must sample something when using sampling mode
                attr.read_format = 0;

                // Build flags bitfield: freq=1 (bit 10), others=0
                attr.flags = 0;
                attr.flags |= 1 << 10; // freq mode

                // perf_event_open(attr, pid, cpu, group_fd, flags)
                let perf_fd = unsafe {
                    libc::syscall(
                        libc::SYS_perf_event_open,
                        &attr as *const PerfEventAttr,
                        -1i32,  // pid = -1 (all processes)
                        cpu_id, // cpu
                        -1i32,  // group_fd = -1
                        0u64,   // flags
                    ) as i32
                };

                if perf_fd < 0 {
                    let errno = unsafe { *libc::__errno_location() };
                    panic!(
                        "perf_event_open failed on CPU {} with {} Hz: errno={}",
                        cpu_id, actual_freq_hz, errno
                    );
                }

                // Enable the event
                let enable_ret = unsafe {
                    libc::ioctl(perf_fd, 9216, 0) // PERF_EVENT_IOC_ENABLE
                };

                if enable_ret < 0 {
                    let errno = unsafe { *libc::__errno_location() };
                    panic!(
                        "PERF_EVENT_IOC_ENABLE failed on CPU {}: errno={}",
                        cpu_id, errno
                    );
                }

                eprintln!(
                    "PMU sampling enabled on CPU {} at {} Hz",
                    cpu_id, actual_freq_hz
                );

                // Keep the perf event alive until stop signal
                // PMIs will be generated automatically by the hardware
                loop {
                    if perf_stop.load(Ordering::Acquire) != 0 {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }

                // Try to read final count (this may not work for all event types)
                let mut count: u64 = 0;
                let samples = unsafe {
                    let ret = libc::read(
                        perf_fd,
                        &mut count as *mut u64 as *mut libc::c_void,
                        std::mem::size_of::<u64>(),
                    );
                    if ret == std::mem::size_of::<u64>() as isize {
                        count
                    } else {
                        0
                    }
                };

                perf_count.store(samples, Ordering::Release);

                // Disable and close
                unsafe {
                    libc::ioctl(perf_fd, 9217, 0); // PERF_EVENT_IOC_DISABLE
                    libc::close(perf_fd);
                }

                eprintln!("PMU sampling stopped");
            })?;
            Ok(())
        },
        None,
    )?;

    Ok(PmuIrqHandle {
        perf_child,
        stop_signal,
        sample_count,
    })
}

/// Default IRQ frequency for timer-based disruption (140kHz - kernel max for hrtimer)
pub const DEFAULT_IRQ_HZ: u64 = 140 * 1000;

/// Handle for background lat_cap measurement workers
pub struct LatCapWorkersHandle {
    workers: Vec<Child>,
    stop_signal: SharedBox<AtomicU32>,
}

impl LatCapWorkersHandle {
    /// Stop all background workers
    pub fn stop(self) -> Result<()> {
        self.stop_signal.store(1, Ordering::Release);
        std::thread::sleep(Duration::from_millis(100));
        // Workers will exit on their own when they see stop signal
        drop(self.workers);
        Ok(())
    }
}

/// Default duty cycle for lat_cap measurement workers (5% active, 95% sleeping)
pub const LAT_CAP_WORKER_DUTY_CYCLE_PCT: u64 = 5;

/// Default period for lat_cap measurement workers (100ms cycle)
pub const LAT_CAP_WORKER_PERIOD_MS: u64 = 100;

/// Launch low duty-cycle background workers pinned to every specified CPU.
///
/// These workers ensure LAVD's stolen_time_est gets sampled on every core.
/// Each worker:
/// - Sleeps for (100 - duty_cycle)% of each period
/// - Spins for duty_cycle% of each period
/// - Repeats until stop signal
///
/// This is necessary because LAVD only updates stolen_time_est when tasks
/// actually run on a CPU. Without these workers, CPUs with no tasks would
/// have stale/zero lat_capacity values.
pub fn launch_lat_cap_workers(
    allocator: std::sync::Arc<BumpAllocator>,
    cpus: &[Hyperthread],
    start_signal: SharedBox<AtomicU32>,
    duty_cycle_pct: u64,
    period_ms: u64,
) -> Result<LatCapWorkersHandle> {
    let stop_signal = SharedBox::new(allocator.clone(), AtomicU32::new(0))?;
    let mut workers = Vec::new();

    // Calculate spin and sleep durations
    let spin_ms = (period_ms * duty_cycle_pct) / 100;
    let sleep_ms = period_ms - spin_ms;

    for cpu in cpus {
        let cpu_mask = CPUMask::new(cpu);
        let cpu_id = cpu.id();
        let worker_start = start_signal.clone();
        let worker_stop = stop_signal.clone();

        let worker = Child::run(
            move || {
                cpu_mask.run(|| {
                    // Set thread name for identification
                    unsafe {
                        let name = std::ffi::CString::new("background").unwrap();
                        libc::prctl(libc::PR_SET_NAME, name.as_ptr());
                    }
                    let _ = cpu_id; // Silence unused warning

                    // Wait for start signal
                    while worker_start.load(Ordering::Acquire) == 0 {
                        std::hint::spin_loop();
                    }

                    // Low duty-cycle loop: sleep, then spin briefly
                    loop {
                        if worker_stop.load(Ordering::Acquire) != 0 {
                            break;
                        }

                        // Sleep phase (most of the period)
                        std::thread::sleep(Duration::from_millis(sleep_ms));

                        if worker_stop.load(Ordering::Acquire) != 0 {
                            break;
                        }

                        // Spin phase (brief work to sample stolen_time)
                        let spin_until = std::time::Instant::now()
                            + Duration::from_millis(spin_ms);
                        while std::time::Instant::now() < spin_until {
                            std::hint::spin_loop();
                        }
                    }
                })?;
                Ok(())
            },
            None,
        )?;
        workers.push(worker);
    }

    eprintln!(
        "Launched {} lat_cap measurement workers ({}% duty cycle, {}ms period)",
        workers.len(),
        duty_cycle_pct,
        period_ms
    );

    Ok(LatCapWorkersHandle {
        workers,
        stop_signal,
    })
}

/// Handle for ping-pong latency probe workers
pub struct PingPongProbeHandle {
    probe_a: Child,
    probe_b: Child,
    stop_signal: SharedBox<AtomicU32>,
    /// Final CPU for probe A
    pub final_cpu_a: SharedBox<AtomicI32>,
    /// Final CPU for probe B
    pub final_cpu_b: SharedBox<AtomicI32>,
    /// Ping-pong iterations completed
    pub iterations: SharedBox<AtomicU64>,
}

impl PingPongProbeHandle {
    /// Stop the ping-pong probes and return final CPU locations
    pub fn stop(self) -> Result<(i32, i32, u64)> {
        self.stop_signal.store(1, Ordering::Release);
        // Wake both threads one more time so they can see stop signal
        std::thread::sleep(Duration::from_millis(100));
        drop(self.probe_a);
        drop(self.probe_b);
        Ok((
            self.final_cpu_a.load(Ordering::Acquire),
            self.final_cpu_b.load(Ordering::Acquire),
            self.iterations.load(Ordering::Acquire),
        ))
    }
}

/// Default ping-pong frequency (10 kHz = 100us between wakeups)
pub const PING_PONG_HZ: u64 = 10_000;

/// Launch ping-pong latency probe workers.
///
/// These workers wake each other in a tight loop, making them latency-critical
/// in LAVD's eyes:
/// - High wake_freq (woken frequently by each other)
/// - Short avg_runtime (minimal work between wakes)
/// - High normalized_lat_cri (above LAVD_LC_LATENCY_SENSITIVE_THRESH)
///
/// Both threads start pinned to initial_cpu, then unpin themselves.
/// The scheduler should migrate them to the CPU with best lat_capacity.
pub fn launch_ping_pong_probes(
    allocator: std::sync::Arc<BumpAllocator>,
    initial_cpu: &Hyperthread,
    start_signal: SharedBox<AtomicU32>,
    ping_pong_hz: u64,
) -> Result<PingPongProbeHandle> {
    let stop_signal = SharedBox::new(allocator.clone(), AtomicU32::new(0))?;
    let final_cpu_a = SharedBox::new(allocator.clone(), AtomicI32::new(-1))?;
    let final_cpu_b = SharedBox::new(allocator.clone(), AtomicI32::new(-1))?;
    let iterations = SharedBox::new(allocator.clone(), AtomicU64::new(0))?;

    // Shared futex words for ping-pong communication
    let futex_a = SharedBox::new(allocator.clone(), AtomicU32::new(0))?;
    let futex_b = SharedBox::new(allocator.clone(), AtomicU32::new(0))?;

    let initial_mask = CPUMask::new(initial_cpu);
    let interval_ns = 1_000_000_000 / ping_pong_hz;

    // Probe A: waits on futex_a, wakes futex_b
    let probe_a_start = start_signal.clone();
    let probe_a_stop = stop_signal.clone();
    let probe_a_futex_wait = futex_a.clone();
    let probe_a_futex_wake = futex_b.clone();
    let probe_a_final_cpu = final_cpu_a.clone();
    let probe_a_iterations = iterations.clone();
    let probe_a_mask = initial_mask.clone();

    let probe_a = Child::run(
        move || {
            probe_a_mask.run(|| {
                // Set thread name for tracing
                unsafe {
                    let name = std::ffi::CString::new("probe1").unwrap();
                    libc::prctl(libc::PR_SET_NAME, name.as_ptr());
                }

                // Set nice value to -10 for higher priority
                unsafe {
                    libc::setpriority(libc::PRIO_PROCESS, 0, -10);
                }

                // Wait for start signal
                while probe_a_start.load(Ordering::Acquire) == 0 {
                    std::hint::spin_loop();
                }

                // Unpin ourselves
                unsafe {
                    let mut mask: libc::cpu_set_t = std::mem::zeroed();
                    for i in 0..libc::CPU_SETSIZE as usize {
                        libc::CPU_SET(i, &mut mask);
                    }
                    libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mask);
                }

                let wait_ptr = probe_a_futex_wait.as_ptr() as *mut u32;
                let wake_ptr = probe_a_futex_wake.as_ptr() as *mut u32;
                let mut local_iter = 0u64;

                // Kick off the ping-pong by waking B first
                probe_a_futex_wake.fetch_add(1, Ordering::Release);
                unsafe {
                    libc::syscall(
                        libc::SYS_futex,
                        wake_ptr,
                        libc::FUTEX_WAKE,
                        1i32,
                        std::ptr::null::<libc::timespec>(),
                        std::ptr::null::<u32>(),
                        0u32,
                    );
                }

                loop {
                    if probe_a_stop.load(Ordering::Acquire) != 0 {
                        break;
                    }

                    // Wait on our futex
                    let futex_val = probe_a_futex_wait.load(Ordering::Acquire);
                    unsafe {
                        libc::syscall(
                            libc::SYS_futex,
                            wait_ptr,
                            libc::FUTEX_WAIT,
                            futex_val,
                            std::ptr::null::<libc::timespec>(),
                            std::ptr::null::<u32>(),
                            0u32,
                        );
                    }

                    if probe_a_stop.load(Ordering::Acquire) != 0 {
                        break;
                    }

                    // Brief spin to simulate minimal work and control frequency
                    let spin_until = std::time::Instant::now()
                        + Duration::from_nanos(interval_ns / 2);
                    while std::time::Instant::now() < spin_until {
                        std::hint::spin_loop();
                    }

                    // Wake the other thread
                    probe_a_futex_wake.fetch_add(1, Ordering::Release);
                    unsafe {
                        libc::syscall(
                            libc::SYS_futex,
                            wake_ptr,
                            libc::FUTEX_WAKE,
                            1i32,
                            std::ptr::null::<libc::timespec>(),
                            std::ptr::null::<u32>(),
                            0u32,
                        );
                    }

                    local_iter += 1;
                    if local_iter.is_multiple_of(1000) {
                        probe_a_iterations.store(local_iter * 2, Ordering::Release);
                    }
                }

                // Record final CPU
                let cpu = unsafe { libc::sched_getcpu() };
                probe_a_final_cpu.store(cpu, Ordering::Release);
                probe_a_iterations.store(local_iter * 2, Ordering::Release);
            })?;
            Ok(())
        },
        None,
    )?;

    // Probe B: waits on futex_b, wakes futex_a
    let probe_b_start = start_signal.clone();
    let probe_b_stop = stop_signal.clone();
    let probe_b_futex_wait = futex_b.clone();
    let probe_b_futex_wake = futex_a.clone();
    let probe_b_final_cpu = final_cpu_b.clone();
    let probe_b_mask = initial_mask.clone();

    let probe_b = Child::run(
        move || {
            probe_b_mask.run(|| {
                // Set thread name for tracing
                unsafe {
                    let name = std::ffi::CString::new("probe2").unwrap();
                    libc::prctl(libc::PR_SET_NAME, name.as_ptr());
                }

                // Set nice value to -10 for higher priority
                unsafe {
                    libc::setpriority(libc::PRIO_PROCESS, 0, -10);
                }

                // Wait for start signal
                while probe_b_start.load(Ordering::Acquire) == 0 {
                    std::hint::spin_loop();
                }

                // Unpin ourselves
                unsafe {
                    let mut mask: libc::cpu_set_t = std::mem::zeroed();
                    for i in 0..libc::CPU_SETSIZE as usize {
                        libc::CPU_SET(i, &mut mask);
                    }
                    libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mask);
                }

                let wait_ptr = probe_b_futex_wait.as_ptr() as *mut u32;
                let wake_ptr = probe_b_futex_wake.as_ptr() as *mut u32;

                loop {
                    if probe_b_stop.load(Ordering::Acquire) != 0 {
                        break;
                    }

                    // Wait on our futex
                    let futex_val = probe_b_futex_wait.load(Ordering::Acquire);
                    unsafe {
                        libc::syscall(
                            libc::SYS_futex,
                            wait_ptr,
                            libc::FUTEX_WAIT,
                            futex_val,
                            std::ptr::null::<libc::timespec>(),
                            std::ptr::null::<u32>(),
                            0u32,
                        );
                    }

                    if probe_b_stop.load(Ordering::Acquire) != 0 {
                        break;
                    }

                    // Brief spin to simulate minimal work and control frequency
                    let spin_until = std::time::Instant::now()
                        + Duration::from_nanos(interval_ns / 2);
                    while std::time::Instant::now() < spin_until {
                        std::hint::spin_loop();
                    }

                    // Wake the other thread
                    probe_b_futex_wake.fetch_add(1, Ordering::Release);
                    unsafe {
                        libc::syscall(
                            libc::SYS_futex,
                            wake_ptr,
                            libc::FUTEX_WAKE,
                            1i32,
                            std::ptr::null::<libc::timespec>(),
                            std::ptr::null::<u32>(),
                            0u32,
                        );
                    }
                }

                // Record final CPU
                let cpu = unsafe { libc::sched_getcpu() };
                probe_b_final_cpu.store(cpu, Ordering::Release);
            })?;
            Ok(())
        },
        None,
    )?;

    // Spawn a helper thread to wake both probes when stop signal is set
    let stop_signal_clone = stop_signal.clone();
    let futex_a_clone = futex_a.clone();
    let futex_b_clone = futex_b.clone();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(100));
            if stop_signal_clone.load(Ordering::Acquire) != 0 {
                // Wake both probes so they can exit
                futex_a_clone.fetch_add(1, Ordering::Release);
                futex_b_clone.fetch_add(1, Ordering::Release);
                unsafe {
                    let ptr_a = futex_a_clone.as_ptr() as *mut u32;
                    let ptr_b = futex_b_clone.as_ptr() as *mut u32;
                    libc::syscall(
                        libc::SYS_futex,
                        ptr_a,
                        libc::FUTEX_WAKE,
                        1i32,
                        std::ptr::null::<libc::timespec>(),
                        std::ptr::null::<u32>(),
                        0u32,
                    );
                    libc::syscall(
                        libc::SYS_futex,
                        ptr_b,
                        libc::FUTEX_WAKE,
                        1i32,
                        std::ptr::null::<libc::timespec>(),
                        std::ptr::null::<u32>(),
                        0u32,
                    );
                }
                break;
            }
        }
    });

    eprintln!(
        "Launched ping-pong latency probes at {} Hz ({}us between wakeups)",
        ping_pong_hz,
        1_000_000 / ping_pong_hz
    );

    Ok(PingPongProbeHandle {
        probe_a,
        probe_b,
        stop_signal,
        final_cpu_a,
        final_cpu_b,
        iterations,
    })
}

/// Maximum number of CPU transitions to record
pub const MAX_CPU_TRANSITIONS: usize = 1024;

/// A single CPU transition event
#[derive(Debug, Clone, Copy, Default)]
#[repr(C)]
pub struct CpuTransition {
    /// Iteration number when transition occurred
    pub iteration: u64,
    /// Elapsed nanoseconds since probe start
    pub elapsed_ns: u64,
    /// CPU before the transition
    pub from_cpu: i32,
    /// CPU after the transition
    pub to_cpu: i32,
}

/// Shared memory structure for CPU transition history
#[repr(C)]
pub struct CpuTransitionLog {
    /// Number of transitions recorded
    pub count: AtomicU32,
    /// Transition entries (fixed-size array)
    pub entries: [CpuTransition; MAX_CPU_TRANSITIONS],
}

impl Default for CpuTransitionLog {
    fn default() -> Self {
        Self {
            count: AtomicU32::new(0),
            entries: [CpuTransition::default(); MAX_CPU_TRANSITIONS],
        }
    }
}

/// Handle for a single spinner probe worker
pub struct SpinnerProbeHandle {
    probe: Child,
    stop_signal: SharedBox<AtomicU32>,
    /// Final CPU for the probe
    pub final_cpu: SharedBox<AtomicI32>,
    /// Spin iterations completed
    pub iterations: SharedBox<AtomicU64>,
    /// CPU transition log
    transition_log: SharedBox<CpuTransitionLog>,
}

impl SpinnerProbeHandle {
    /// Stop the spinner probe and return (final_cpu, iterations, transitions)
    pub fn stop(self) -> Result<(i32, u64, Vec<CpuTransition>)> {
        self.stop_signal.store(1, Ordering::Release);
        std::thread::sleep(Duration::from_millis(100));
        drop(self.probe);

        let final_cpu = self.final_cpu.load(Ordering::Acquire);
        let iterations = self.iterations.load(Ordering::Acquire);

        // Copy transitions from shared memory
        let count = self.transition_log.count.load(Ordering::Acquire) as usize;
        let count = count.min(MAX_CPU_TRANSITIONS);
        let transitions: Vec<CpuTransition> = self.transition_log.entries[..count].to_vec();

        Ok((final_cpu, iterations, transitions))
    }
}

/// Launch a single spinner probe worker.
///
/// This worker spins continuously, tracking its CPU location and recording
/// CPU transitions (migrations) with timestamps.
///
/// It starts pinned to initial_cpu, then unpins itself on start signal.
/// The scheduler may migrate it to a CPU with better characteristics.
///
/// The probe is named "probe1" and has nice value -10 for higher priority.
pub fn launch_spinner_probe(
    allocator: std::sync::Arc<BumpAllocator>,
    initial_cpu: &Hyperthread,
    start_signal: SharedBox<AtomicU32>,
) -> Result<SpinnerProbeHandle> {
    let stop_signal = SharedBox::new(allocator.clone(), AtomicU32::new(0))?;
    let final_cpu = SharedBox::new(allocator.clone(), AtomicI32::new(-1))?;
    let iterations = SharedBox::new(allocator.clone(), AtomicU64::new(0))?;
    let transition_log = SharedBox::new(allocator.clone(), CpuTransitionLog::default())?;

    let initial_mask = CPUMask::new(initial_cpu);

    let probe_start = start_signal.clone();
    let probe_stop = stop_signal.clone();
    let probe_final_cpu = final_cpu.clone();
    let probe_iterations = iterations.clone();
    let probe_transition_log = transition_log.clone();
    let probe_mask = initial_mask.clone();

    let probe = Child::run(
        move || {
            probe_mask.run(|| {
                // Set thread name for tracing
                unsafe {
                    let name = std::ffi::CString::new("probe1").unwrap();
                    libc::prctl(libc::PR_SET_NAME, name.as_ptr());
                }

                // Set nice value to -10 for higher priority
                unsafe {
                    libc::setpriority(libc::PRIO_PROCESS, 0, -10);
                }

                // Wait for start signal
                while probe_start.load(Ordering::Acquire) == 0 {
                    std::hint::spin_loop();
                }

                // Record start time for elapsed calculations
                let start_time = std::time::Instant::now();

                // Unpin ourselves
                unsafe {
                    let mut mask: libc::cpu_set_t = std::mem::zeroed();
                    for i in 0..libc::CPU_SETSIZE as usize {
                        libc::CPU_SET(i, &mut mask);
                    }
                    libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mask);
                }

                let mut local_iter = 0u64;
                let mut current_cpu = unsafe { libc::sched_getcpu() };
                probe_final_cpu.store(current_cpu, Ordering::Release);

                // Helper to record a transition
                let record_transition = |log: &CpuTransitionLog, iter: u64, elapsed_ns: u64, from: i32, to: i32| {
                    let idx = log.count.fetch_add(1, Ordering::AcqRel) as usize;
                    if idx < MAX_CPU_TRANSITIONS {
                        // Safety: we're the only writer to this index, using ptr::write
                        let entry_ptr = log.entries.as_ptr().wrapping_add(idx) as *mut CpuTransition;
                        unsafe {
                            std::ptr::write(entry_ptr, CpuTransition {
                                iteration: iter,
                                elapsed_ns,
                                from_cpu: from,
                                to_cpu: to,
                            });
                        }
                    }
                };

                // Spin loop
                loop {
                    if probe_stop.load(Ordering::Acquire) != 0 {
                        break;
                    }

                    // Check current CPU and detect transitions
                    let cpu = unsafe { libc::sched_getcpu() };
                    if cpu != current_cpu {
                        let elapsed_ns = start_time.elapsed().as_nanos() as u64;
                        record_transition(&probe_transition_log, local_iter, elapsed_ns, current_cpu, cpu);
                        current_cpu = cpu;
                        probe_final_cpu.store(cpu, Ordering::Release);
                    }

                    // Tight spin with compiler fence
                    for _ in 0..10000 {
                        std::sync::atomic::compiler_fence(Ordering::SeqCst);
                    }

                    local_iter += 1;
                    if local_iter.is_multiple_of(1000) {
                        probe_iterations.store(local_iter, Ordering::Release);
                    }
                }

                // Record final state
                let cpu = unsafe { libc::sched_getcpu() };
                probe_final_cpu.store(cpu, Ordering::Release);
                probe_iterations.store(local_iter, Ordering::Release);
            })?;
            Ok(())
        },
        None,
    )?;

    eprintln!("Launched spinner probe (nice -10, tracking CPU transitions)");

    Ok(SpinnerProbeHandle {
        probe,
        stop_signal,
        final_cpu,
        iterations,
        transition_log,
    })
}

/// Helper function to launch a CPU hog and add it to a cgroup
#[allow(clippy::too_many_arguments)]
pub fn launch_cgroup_hog(
    _cpu_id: i32,
    cpu_ht: &Hyperthread,
    cgroup_name: &str,
    worker_name: &str,
    hog_duration: Duration,
    start_signal: SharedBox<AtomicU32>,
    bogo_ops_out: SharedBox<AtomicU64>,
    scheduled_ns_out: SharedBox<AtomicU64>,
) -> Result<crate::util::child::Child> {
    use crate::workloads::spinner_utilization;

    let cpu_mask = CPUMask::new(cpu_ht);
    let name = worker_name.to_string();

    let child = crate::util::child::Child::run(
        move || {
            cpu_mask.run(|| {
                spinner_utilization::cpu_hog_workload(
                    &name,
                    hog_duration,
                    start_signal,
                    scheduled_ns_out,
                    Some(bogo_ops_out),
                );
            })?;
            Ok(())
        },
        None,
    )?;

    // Add process to its cgroup
    let pid = child.pid().as_raw();
    let procs_path = std::path::Path::new("/sys/fs/cgroup")
        .join(cgroup_name)
        .join("cgroup.procs");
    std::fs::write(&procs_path, pid.to_string())
        .context(format!("Failed to write PID {} to {:?}", pid, procs_path))?;

    Ok(child)
}
