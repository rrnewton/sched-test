//! Integration tests for engine-side `cpu.max` enforcement (Diff 3 wiring).
//!
//! These tests exercise the throttle → refill → resume cycle implemented
//! by the [`scx_simulator::cgroup_bw::BandwidthManager`] hooks in the
//! engine tick/dispatch loop:
//!
//! - charging at `stop_and_reenqueue` and `handle_task_phase_complete`
//! - admission gate at `post_dispatch_run` / `handle_dsq_consume`
//! - slice cap from remaining quota in `start_running`
//! - refill events scheduled at scenario load and re-armed by
//!   [`scx_simulator::engine::EventKind::CgroupBwRefill`]
//!
//! The tests are scheduler-agnostic in spirit (the bandwidth model lives in
//! the engine, not the scheduler) but use `scx_simple` because it has the
//! simplest dispatch path and the lowest interference between bw events
//! and scheduler decisions.

use scx_simulator::*;

#[macro_use]
mod common;

/// Tight quota: a single CPU-bound task in a cgroup with `quota=10ms /
/// period=50ms` should run, exhaust quota, throttle, and resume on refill.
///
/// We assert on the trace shape rather than precise schedule counts so the
/// test stays robust to scheduler-overhead drift:
///
/// 1. At least one `CgroupBwCharge` event was recorded for the task.
/// 2. At least one `CgroupBwThrottle` event was recorded for the cgroup
///    (proving the budget actually exhausted).
/// 3. At least one `CgroupBwDenied` event was recorded (proving the
///    admission gate refused dispatch while throttled).
/// 4. At least one `CgroupBwRefill` event was recorded (proving the
///    refill timer fired and the cgroup transitioned out of throttled).
/// 5. The task was scheduled BOTH before AND after the first refill —
///    i.e., it survived the throttle window and ran again, demonstrating
///    the full throttle → refill → resume cycle.
#[test]
fn test_engine_throttles_and_refills_tight_quota() {
    let _lock = common::setup_test();

    // 50ms period, 10ms quota, no burst → 20% bandwidth ceiling.
    // 4 CPUs available, 1 CPU-bound task with 200ms of work means the task
    // would finish in 200ms wall time without throttling, but with the
    // 20% cap it should need ~1s and clearly hit multiple throttle/refill
    // cycles in the 400ms simulated window.
    let nr_cpus = 4u32;
    let scenario = Scenario::builder()
        .cpus(nr_cpus)
        .cgroup_with_bandwidth(
            "tight",
            &[CpuId(0), CpuId(1), CpuId(2), CpuId(3)],
            50_000, // period_us = 50ms
            10_000, // quota_us  = 10ms
            0,      // burst_us  = 0
        )
        .add_task_in_cgroup("hog", 0, workloads::cpu_bound(200_000_000), "tight")
        .duration_ms(400)
        .build();

    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario);

    // Walk the trace, tallying the new bw event kinds and remembering the
    // first refill timestamp + task-scheduled timestamps so we can assert
    // on the throttle → refill → resume invariant.
    let mut n_charge = 0u32;
    let mut n_throttle = 0u32;
    let mut n_denied = 0u32;
    let mut n_refill = 0u32;
    let mut first_refill_ns: Option<u64> = None;
    let mut sched_before_refill = 0u32;
    let mut sched_after_refill = 0u32;

    for ev in trace.events() {
        match &ev.kind {
            TraceKind::CgroupBwCharge { .. } => n_charge += 1,
            TraceKind::CgroupBwThrottle { .. } => n_throttle += 1,
            TraceKind::CgroupBwDenied { .. } => n_denied += 1,
            TraceKind::CgroupBwRefill { .. } => {
                n_refill += 1;
                if first_refill_ns.is_none() {
                    first_refill_ns = Some(ev.time_ns);
                }
            }
            TraceKind::TaskScheduled { pid } if pid.0 == 1 => match first_refill_ns {
                None => sched_before_refill += 1,
                Some(_) => sched_after_refill += 1,
            },
            _ => {}
        }
    }

    assert!(
        n_charge > 0,
        "expected at least one CgroupBwCharge event, got {n_charge} \
         (the engine never charged the cgroup for consumed CPU time)"
    );
    assert!(
        n_throttle > 0,
        "expected at least one CgroupBwThrottle event, got {n_throttle} \
         (the cgroup never exhausted its quota — is charge wired?)"
    );
    assert!(
        n_denied > 0,
        "expected at least one CgroupBwDenied event, got {n_denied} \
         (the admission gate never refused a throttled task)"
    );
    assert!(
        n_refill > 0,
        "expected at least one CgroupBwRefill event, got {n_refill} \
         (the refill timer never fired)"
    );
    assert!(
        sched_before_refill > 0,
        "task was never scheduled BEFORE the first refill — \
         engine never let the task start running at all?"
    );
    assert!(
        sched_after_refill > 0,
        "task was never scheduled AFTER the first refill — \
         throttle → refill → resume cycle is broken"
    );
}

/// Unlimited cgroup (no `cpu.max`) should produce ZERO bw trace events.
///
/// This is the negative-case sanity check: tasks not in a tracked cgroup
/// must not be charged, throttled, denied, or affected by refill events.
#[test]
fn test_engine_no_enforcement_without_cpu_max() {
    let _lock = common::setup_test();

    let scenario = Scenario::builder()
        .cpus(2)
        .add_task("free", 0, workloads::cpu_bound(50_000_000))
        .duration_ms(100)
        .build();

    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario);

    for ev in trace.events() {
        match &ev.kind {
            TraceKind::CgroupBwCharge { .. }
            | TraceKind::CgroupBwThrottle { .. }
            | TraceKind::CgroupBwDenied { .. }
            | TraceKind::CgroupBwRefill { .. } => {
                panic!("unexpected bw event in untracked-cgroup run: {:?}", ev.kind);
            }
            _ => {}
        }
    }
}
