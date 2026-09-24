//! Tick-sampled cputime accounting: `p->utime` / `p->stime`.
//!
//! The engine mirrors the kernel's default `CONFIG_TICK_CPU_ACCOUNTING`:
//! every tick that interrupts a running task charges one full tick to
//! `p->utime` if the task is in user compute ([`Phase::Run`]) and to
//! `p->stime` if it is in task-context kernel time ([`Phase::SystemCpu`]).
//!
//! scx_cosmos (upstream sched-ext/scx 49c65ba6d) derives its per-CPU busy
//! state from `p->utime` deltas and deliberately ignores system time. If the
//! substrate left these at zero, cosmos would silently never leave
//! round-robin mode; if it lumped system time into `utime`, sleep-intensive
//! workloads would wrongly look busy.

use std::collections::HashMap;

use scx_simulator::*;

#[macro_use]
mod common;

/// Tick length the engine charges (HZ=250).
const TICK_NS: u64 = 4_000_000;

/// Latest (utime, stime) seen for each task at `ops.stopping()`.
#[derive(Default)]
struct CputimeMonitor {
    last: HashMap<Pid, (u64, u64)>,
}

impl Monitor for CputimeMonitor {
    fn sample(&mut self, ctx: &ProbeContext) {
        if ctx.point == ProbePoint::Stopping {
            let cputime = (task_get_utime(ctx.task_raw), task_get_stime(ctx.task_raw));
            self.last.insert(ctx.pid, cputime);
        }
    }
}

#[test]
fn ticks_charge_user_and_system_time_by_phase() {
    let _lock = common::setup_test();
    let work_ns = 40_000_000u64;

    let scenario = Scenario::builder()
        .cpus(1)
        .instant_timing()
        .add_task(
            "user",
            0,
            TaskBehavior {
                phases: vec![Phase::Run(work_ns)],
                repeat: RepeatMode::Once,
            },
        )
        .add_task(
            "kernel",
            0,
            TaskBehavior {
                phases: vec![Phase::SystemCpu(work_ns)],
                repeat: RepeatMode::Once,
            },
        )
        .duration_ms(200)
        .build();

    let mut monitor = CputimeMonitor::default();
    let result = Simulator::new(DynamicScheduler::simple()).run_monitored(scenario, &mut monitor);
    assert_eq!(result.trace.exit_kind(), &ExitKind::Normal);

    let (user_utime, user_stime) = monitor.last[&Pid(1)];
    let (kernel_utime, kernel_stime) = monitor.last[&Pid(2)];

    // Each tick lands on exactly one side of the split.
    assert_eq!(user_stime, 0, "Run-only task was charged system time");
    assert_eq!(kernel_utime, 0, "SystemCpu-only task was charged user time");

    // Charges are whole ticks.
    assert_eq!(
        user_utime % TICK_NS,
        0,
        "utime {user_utime} is not whole ticks"
    );
    assert_eq!(
        kernel_stime % TICK_NS,
        0,
        "stime {kernel_stime} is not whole ticks"
    );

    // Tick sampling over 40ms of work: within one tick at each end of
    // every stint. The simple scheduler's slice lets each task run in a few
    // stints, so allow two ticks of slop per task.
    for (what, charged) in [("utime", user_utime), ("stime", kernel_stime)] {
        assert!(
            charged + 2 * TICK_NS >= work_ns && charged <= work_ns + 2 * TICK_NS,
            "{what} {charged} is not within two ticks of {work_ns}ns of work"
        );
    }

    // The single CPU was busy for 80ms; every tick in that window charged
    // exactly one of the two tasks.
    let busy = 2 * work_ns;
    let total = user_utime + kernel_stime;
    assert!(
        total + TICK_NS >= busy && total <= busy + TICK_NS,
        "utime+stime {total} is not within one tick of {busy}ns busy time"
    );
}
