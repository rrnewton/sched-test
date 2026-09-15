//! Tests for idle-CPU selection and power-aware scheduling across schedulers.
//!
//! Validates, via the trace, the idle-CPU handling paths: the `ops.update_idle`
//! notification, CPU selection when many CPUs are idle, idle detection under
//! different topologies (SMT / multi-LLC), the all-idle vs all-busy contrast,
//! the cross-CPU wakeup (IPI/KickCpu) path, and operation under lavd's power
//! modes.
//!
//! Observed behavior the assertions are built on (empirically probed):
//! - `UpdateIdle` events fire for every scheduler; the `CpuIdle` TraceKind is
//!   not emitted by these schedulers, so idle notification is asserted via
//!   `UpdateIdle`.
//! - simple never issues an explicit `KickCpu` (it has no preemption/IPI path);
//!   lavd issues KickCpu IPIs under contention; cosmos only issues
//!   SCX_KICK_IDLE, so it kicks only when an idle CPU exists to wake.
//! - lavd runs correctly in Performance/Balanced/Powersave modes. (Core
//!   compaction does not demonstrably concentrate load through the scenario
//!   API, so that is not asserted — see task notes.)

use std::collections::HashSet;

use scx_simulator::*;

#[macro_use]
mod common;

fn task(name: &str, pid: i32, nice: i8, behavior: TaskBehavior) -> TaskDef {
    TaskDef {
        name: name.into(),
        pid: Pid(pid),
        nice,
        behavior,
        start_time_ns: 0,
        mm_id: None,
        allowed_cpus: None,
        parent_pid: None,
        cgroup_name: None,
        task_flags: 0,
        migration_disabled: 0,
        thread_group_leader: None,
        uid: Uid(0),
        gid: Gid(0),
        fork_cpu: None,
    }
}

fn count(trace: &Trace, pred: impl Fn(&TraceKind) -> bool) -> usize {
    trace.events().iter().filter(|e| pred(&e.kind)).count()
}

fn update_idle_count(trace: &Trace) -> usize {
    count(trace, |k| matches!(k, TraceKind::UpdateIdle { .. }))
}

fn kick_count(trace: &Trace) -> usize {
    count(trace, |k| matches!(k, TraceKind::KickCpu { .. }))
}

fn cpus_used(trace: &Trace) -> usize {
    trace
        .events()
        .iter()
        .filter_map(|e| match e.kind {
            TraceKind::TaskScheduled { .. } => Some(e.cpu),
            _ => None,
        })
        .collect::<HashSet<_>>()
        .len()
}

fn new_sched(label: &str, cpus: u32) -> DynamicScheduler {
    match label {
        "simple" => DynamicScheduler::simple(),
        "lavd" => DynamicScheduler::lavd(cpus),
        "cosmos" => DynamicScheduler::cosmos(cpus),
        _ => unreachable!(),
    }
}

const ALL: [&str; 3] = ["simple", "lavd", "cosmos"];

/// Set up a minimal valid PCO (power/core-order) table so lavd's
/// core-compaction path (engaged by Balanced/Powersave modes) does not error
/// with "Incorrect PCO state" (power.bpf.c:201). Mirrors lavd.rs::lavd_setup_pco.
unsafe fn lavd_setup_pco(sched: &DynamicScheduler, nr_cpus: u32) {
    const LAVD_CPU_ID_MAX: usize = 512;
    let ns: libloading::Symbol<'_, *mut u8> = sched
        .get_symbol(b"nr_pco_states\0")
        .expect("nr_pco_states not found");
    std::ptr::write_volatile(*ns, 1);
    let pco: libloading::Symbol<'_, *mut u16> = sched
        .get_symbol(b"pco_table\0")
        .expect("pco_table not found");
    let pco = *pco;
    for i in 0..nr_cpus.min(LAVD_CPU_ID_MAX as u32) {
        std::ptr::write_volatile(pco.add(i as usize), i as u16);
    }
    let bounds: libloading::Symbol<'_, *mut u32> = sched
        .get_symbol(b"pco_bounds\0")
        .expect("pco_bounds not found");
    std::ptr::write_volatile(*bounds, u32::MAX);
    let primary: libloading::Symbol<'_, *mut u16> = sched
        .get_symbol(b"pco_nr_primary\0")
        .expect("pco_nr_primary not found");
    std::ptr::write_volatile(*primary, nr_cpus as u16);
}

/// `n` CPU hogs on `cpus` CPUs.
fn hogs(cpus: u32, n: i32, seed: u32) -> Scenario {
    let mut b = Scenario::builder()
        .cpus(cpus)
        .seed(seed)
        .detect_bpf_errors();
    for i in 0..n {
        b = b.task(task(
            &format!("hog{i}"),
            1 + i,
            0,
            workloads::cpu_bound(50_000_000),
        ));
    }
    b.duration_ms(120).build()
}

// ---------------------------------------------------------------------------
// 1. Idle-CPU notification: ops.update_idle fires and CPU selection happens.
// ---------------------------------------------------------------------------

#[test]
fn test_idle_notification_and_selection() {
    let _lock = common::setup_test();
    for label in ALL {
        // Under-loaded: 2 tasks on 4 CPUs → CPUs transition idle/busy.
        let trace = Simulator::new(new_sched(label, 4)).run(hogs(4, 2, 1));
        assert!(
            !trace.has_error(),
            "[{label}] error: {:?}",
            trace.exit_kind()
        );
        assert!(
            update_idle_count(&trace) > 0,
            "[{label}] no UpdateIdle (idle-notification) events"
        );
        assert!(
            count(&trace, |k| matches!(k, TraceKind::SelectTaskRq { .. })) > 0,
            "[{label}] no SelectTaskRq (CPU-selection) events"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. Selection when many CPUs are idle: tasks spread onto distinct idle CPUs.
// ---------------------------------------------------------------------------

#[test]
fn test_multiple_idle_cpu_selection_spreads() {
    let _lock = common::setup_test();
    for label in ALL {
        // 4 always-runnable tasks, 8 idle CPUs — idle selection should place
        // them on several distinct CPUs rather than piling onto one.
        let trace = Simulator::new(new_sched(label, 8)).run(hogs(8, 4, 2));
        assert!(
            !trace.has_error(),
            "[{label}] error: {:?}",
            trace.exit_kind()
        );
        let used = cpus_used(&trace);
        assert!(
            used >= 2,
            "[{label}] 4 tasks on 8 idle CPUs used only {used} CPU(s)"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. Idle detection across topologies (flat, SMT, multi-LLC).
// ---------------------------------------------------------------------------

#[test]
fn test_idle_detection_across_topologies() {
    let _lock = common::setup_test();
    for label in ALL {
        for topo in ["flat", "smt", "llc"] {
            let make = || {
                let mut b = Scenario::builder().cpus(8).seed(3).detect_bpf_errors();
                b = match topo {
                    "smt" => b.smt(2),          // 4 cores × 2 threads
                    "llc" => b.cpus_per_llc(4), // 2 LLC domains
                    _ => b,
                };
                for i in 0..4i32 {
                    b = b.task(task(
                        &format!("t{i}"),
                        1 + i,
                        0,
                        workloads::cpu_bound(50_000_000),
                    ));
                }
                b.duration_ms(120).build()
            };
            let trace = Simulator::new(new_sched(label, 8)).run(make());
            assert!(
                !trace.has_error(),
                "[{label}/{topo}] error: {:?}",
                trace.exit_kind()
            );
            assert!(
                update_idle_count(&trace) > 0,
                "[{label}/{topo}] no idle detection (UpdateIdle) events"
            );
            // With 4 tasks and 8 CPUs the idle-aware placement should use
            // multiple CPUs/cores regardless of topology.
            assert!(
                cpus_used(&trace) >= 2,
                "[{label}/{topo}] tasks did not spread across idle CPUs"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 4. All-idle vs all-busy: an under-loaded system leaves CPUs idle; a
//    saturated one activates them all.
// ---------------------------------------------------------------------------

#[test]
fn test_all_idle_vs_all_busy() {
    let _lock = common::setup_test();
    for label in ALL {
        // All-idle: a single task on 4 CPUs — most CPUs stay idle.
        let idle = Simulator::new(new_sched(label, 4)).run(hogs(4, 1, 4));
        assert!(
            !idle.has_error(),
            "[{label}] idle error: {:?}",
            idle.exit_kind()
        );
        let idle_used = cpus_used(&idle);
        assert!(
            update_idle_count(&idle) > 0,
            "[{label}] no UpdateIdle on an under-loaded system"
        );

        // All-busy: 8 tasks on 4 CPUs — every CPU is activated.
        let busy = Simulator::new(new_sched(label, 4)).run(hogs(4, 8, 4));
        assert!(
            !busy.has_error(),
            "[{label}] busy error: {:?}",
            busy.exit_kind()
        );
        let busy_used = cpus_used(&busy);

        assert!(
            busy_used > idle_used,
            "[{label}] expected more CPUs active when saturated: idle={idle_used} busy={busy_used}"
        );
        assert!(
            busy_used >= 3,
            "[{label}] saturated system used only {busy_used}/4 CPUs"
        );
    }
}

// ---------------------------------------------------------------------------
// 5. Wakeup / IPI path: lavd and cosmos kick idle CPUs under contention.
//    simple has no explicit kick path (documented) — asserted to be zero.
// ---------------------------------------------------------------------------

#[test]
fn test_wakeup_kick_cpu_paths() {
    let _lock = common::setup_test();

    // lavd kicks even when every CPU is busy: its kick path is driven by
    // preemption, not by the existence of an idle CPU. 8 tasks / 4 CPUs.
    let trace = Simulator::new(new_sched("lavd", 4)).run(hogs(4, 8, 5));
    assert!(!trace.has_error(), "[lavd] error: {:?}", trace.exit_kind());
    assert!(
        kick_count(&trace) > 0,
        "[lavd] no KickCpu (IPI wakeup) events under contention"
    );

    // cosmos only ever issues SCX_KICK_IDLE, so it can only kick when a CPU is
    // actually idle to be woken. Under-load it: 2 tasks / 4 CPUs.
    //
    // This used to be asserted on the same saturated 8/4 workload as lavd and
    // passed only because the build manifest forced perf_config=1 and the
    // wrapper fed cosmos fabricated PMU counts, which made is_event_heavy()
    // permanently true and pushed every enqueue down the pick_idle_cpu()
    // branch. With the real no-PMU path restored, a saturated cosmos correctly
    // issues no IPI wakeups -- there is no idle CPU to wake. Measured kicks by
    // load on 4 CPUs: cosmos 7/12/20 at 1/2/3 tasks and 0 at 4+; lavd 1/2/3/4
    // and 4 at every saturated load. Both directions are asserted below so the
    // distinction cannot silently regress again.
    let trace = Simulator::new(new_sched("cosmos", 4)).run(hogs(4, 2, 5));
    assert!(
        !trace.has_error(),
        "[cosmos] error: {:?}",
        trace.exit_kind()
    );
    assert!(
        kick_count(&trace) > 0,
        "[cosmos] no KickCpu (IPI wakeup) events with idle CPUs available"
    );

    let trace = Simulator::new(new_sched("cosmos", 4)).run(hogs(4, 8, 5));
    assert!(
        !trace.has_error(),
        "[cosmos] error: {:?}",
        trace.exit_kind()
    );
    assert_eq!(
        kick_count(&trace),
        0,
        "[cosmos] issued SCX_KICK_IDLE with every CPU busy — there is no idle \
         CPU to wake, so this indicates fabricated PMU input has returned"
    );
    // simple issues no explicit KickCpu — matches its no-preemption design.
    let trace = Simulator::new(DynamicScheduler::simple()).run(hogs(4, 8, 5));
    assert!(
        !trace.has_error(),
        "[simple] error: {:?}",
        trace.exit_kind()
    );
    assert_eq!(
        kick_count(&trace),
        0,
        "[simple] unexpectedly issued KickCpu events (simple has no kick path)"
    );
}

// ---------------------------------------------------------------------------
// 6. Power-aware scheduling: lavd operates correctly in every power mode and
//    still performs idle-CPU selection in each.
// ---------------------------------------------------------------------------

#[test]
fn test_lavd_power_modes_operate() {
    let _lock = common::setup_test();
    for mode in [
        LavdPowerMode::Performance,
        LavdPowerMode::Balanced,
        LavdPowerMode::Powersave,
    ] {
        let sched = DynamicScheduler::lavd(4);
        // Balanced/Powersave engage core compaction, which reads the PCO table.
        unsafe {
            lavd_setup_pco(&sched, 4);
        }
        sched.lavd_set_power_mode(mode);
        let mut b = Scenario::builder().cpus(4).seed(6).detect_bpf_errors();
        for i in 0..4i32 {
            b = b.task(task(
                &format!("t{i}"),
                1 + i,
                0,
                TaskBehavior {
                    phases: vec![Phase::Run(2_000_000), Phase::Sleep(2_000_000)],
                    repeat: RepeatMode::Forever,
                },
            ));
        }
        let trace = Simulator::new(sched).run(b.duration_ms(150).build());
        assert!(
            !trace.has_error(),
            "[{mode:?}] error: {:?}",
            trace.exit_kind()
        );
        // Every task runs and idle-aware CPU selection still occurs.
        for pid in 1..=4i32 {
            assert!(
                trace.schedule_count(Pid(pid)) > 0,
                "[{mode:?}] task pid={pid} never scheduled"
            );
        }
        assert!(
            count(&trace, |k| matches!(k, TraceKind::SelectTaskRq { .. })) > 0,
            "[{mode:?}] no CPU-selection events"
        );
    }
}
