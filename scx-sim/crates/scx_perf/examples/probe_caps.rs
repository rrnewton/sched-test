//! Probe which test capabilities this machine actually has.
//!
//! Prints one line per capability that is **missing**, so a runner script can
//! build the `SCXSIM_ALLOW_MISSING_CAPS` value without guessing:
//!
//! ```sh
//! missing=$(cargo run -q -p scx_perf --example probe_caps | paste -sd,)
//! [ -n "$missing" ] && export SCXSIM_ALLOW_MISSING_CAPS="$missing"
//! ```
//!
//! Exits 0 whether or not anything is missing; absence is reported on stdout,
//! not through the exit status.

use scx_perf::capability;
use scx_perf::{HwBreakpoint, PmuConfig, RbcCounter};

/// Can we create a PMU counter that actually counts?
fn have_pmu() -> bool {
    let Some(config) = PmuConfig::detect() else {
        return false;
    };
    let Ok(counter) = RbcCounter::new(&config) else {
        return false;
    };
    if counter.reset().is_err() || counter.enable().is_err() {
        return false;
    }
    let mut sum = 0u64;
    for i in 0..10_000u64 {
        if i % 2 == 0 {
            sum += i;
        }
    }
    std::hint::black_box(sum);
    let _ = counter.disable();
    // A counter that reads 0 after 10k conditional branches is present but
    // non-functional (seen in some VMs); treat that as missing.
    matches!(counter.read(), Ok(n) if n > 0)
}

/// Can we arm a hardware execution breakpoint on this thread?
fn have_hw_breakpoint() -> bool {
    let target: fn(u64) -> u64 = std::hint::black_box;
    let addr = target as *const () as u64;
    let tid = unsafe { libc::syscall(libc::SYS_gettid) } as libc::pid_t;
    HwBreakpoint::new(addr, tid, libc::SIGTRAP).is_ok()
}

/// Is Perfetto's `trace_processor_shell` reachable?
fn have_trace_processor() -> bool {
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            if dir.join("trace_processor_shell").is_file() {
                return true;
            }
        }
    }
    std::env::var_os("HOME")
        .map(|h| {
            std::path::PathBuf::from(h)
                .join("bin/trace_processor_shell")
                .is_file()
        })
        .unwrap_or(false)
}

/// A capability name paired with its probe.
type Check = (&'static str, fn() -> bool);

fn main() {
    let checks: &[Check] = &[
        (capability::PMU, have_pmu),
        (capability::HW_BREAKPOINT, have_hw_breakpoint),
        (capability::TRACE_PROCESSOR, have_trace_processor),
    ];

    for (name, probe) in checks {
        if !probe() {
            println!("{name}");
        }
    }
}
