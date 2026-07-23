//! LAVD coverage-gap tests.
//!
//! These tests target LAVD C code paths that the existing `lavd.rs` suite does
//! not reach, identified by the 2026-07-22 coverage audit
//! (`scx-sim/ai_docs/COVERAGE_AUDIT_20260722.md`). Each test toggles a LAVD
//! `const volatile` global that no existing test exercises, then drives a
//! workload through the resulting branch. The task notes record the measured
//! `llvm-cov` net-new-line delta for each.
//!
//! Scope note: several audit-identified uncovered *functions* are NOT
//! test-addressable and are deliberately out of scope — the futex
//! priority-boost subsystem (`lock.bpf.c`) and the execve
//! `set_aggressive_migration` hook are BPF-substrate gaps (the simulator does
//! not deliver those tracepoints), `set_power_profile` is a `SEC("syscall")`
//! prog, `get_cpuperf_cap`/`conv_wall_to_invr` are dead code (no live caller),
//! and `idle.bpf.c::migrate_to_neighbor` requires an internal state
//! combination (a task sticky to a saturated domain while a neighbor has a
//! fully idle core, with `is_stealee` set at the select_cpu instant) that the
//! deterministic scenario API cannot reliably co-produce. See task notes.

use scx_simulator::*;

#[macro_use]
mod common;

// ---------------------------------------------------------------------------
// Whitebox helpers (mirror the dlsym pattern in lavd.rs)
// ---------------------------------------------------------------------------

/// Write a bool value to a named LAVD global variable.
unsafe fn lavd_set_bool(sched: &DynamicScheduler, name: &str, val: bool) {
    let sym: libloading::Symbol<'_, *mut bool> = sched
        .get_symbol(name.as_bytes())
        .unwrap_or_else(|| panic!("symbol {name} not found"));
    std::ptr::write_volatile(*sym, val);
}

/// Write a u8 value to a named LAVD global variable.
unsafe fn lavd_set_u8(sched: &DynamicScheduler, name: &str, val: u8) {
    let sym: libloading::Symbol<'_, *mut u8> = sched
        .get_symbol(name.as_bytes())
        .unwrap_or_else(|| panic!("symbol {name} not found"));
    std::ptr::write_volatile(*sym, val);
}

/// Set up a minimal valid PCO (power/core-order) table so the core-compaction
/// and load-balancing paths that consult `get_cpu_order()` do not error with
/// "Incorrect PCO state" (`power.bpf.c:201`). Mirrors `lavd.rs::lavd_setup_pco`.
unsafe fn setup_pco(sched: &DynamicScheduler, nr_cpus: u32) {
    const LAVD_CPU_ID_MAX: usize = 512;
    lavd_set_u8(sched, "nr_pco_states\0", 1);

    let pco_sym: libloading::Symbol<'_, *mut u16> = sched
        .get_symbol(b"pco_table\0")
        .expect("pco_table not found");
    let pco = *pco_sym;
    for i in 0..nr_cpus.min(LAVD_CPU_ID_MAX as u32) {
        std::ptr::write_volatile(pco.add(i as usize), i as u16);
    }

    let bounds_sym: libloading::Symbol<'_, *mut u32> = sched
        .get_symbol(b"pco_bounds\0")
        .expect("pco_bounds not found");
    std::ptr::write_volatile(*bounds_sym, u32::MAX);

    let primary_sym: libloading::Symbol<'_, *mut u16> = sched
        .get_symbol(b"pco_nr_primary\0")
        .expect("pco_nr_primary not found");
    std::ptr::write_volatile(*primary_sym, nr_cpus as u16);
}

/// Build an imbalanced two-domain workload: `n_dom0` CPU-bound hogs pinned to
/// domain 0 (CPUs 0..split) and `n_dom1` pinned to domain 1 (split..nr_cpus),
/// plus a couple of free-floating I/O tasks. This is the classic cross-domain
/// stealing setup used throughout `lavd.rs`.
fn imbalanced_two_domain_scenario(
    nr_cpus: u32,
    split: u32,
    n_dom0: i32,
    n_dom1: i32,
    duration_ms: u64,
) -> Scenario {
    let dom0: Vec<CpuId> = (0..split).map(CpuId).collect();
    let dom1: Vec<CpuId> = (split..nr_cpus).map(CpuId).collect();
    let mut b = Scenario::builder()
        .cpus(nr_cpus)
        .seed(24601)
        .detect_bpf_errors();

    let mut pid = 1i32;
    for _ in 0..n_dom0 {
        b = b.task(TaskDef {
            name: format!("d0_hog{pid}"),
            pid: Pid(pid),
            nice: 0,
            behavior: workloads::cpu_bound(20_000_000),
            start_time_ns: 0,
            mm_id: None,
            allowed_cpus: Some(dom0.clone()),
            parent_pid: None,
            cgroup_name: None,
            task_flags: 0,
            migration_disabled: 0,
        });
        pid += 1;
    }
    for _ in 0..n_dom1 {
        b = b.task(TaskDef {
            name: format!("d1_hog{pid}"),
            pid: Pid(pid),
            nice: 0,
            behavior: workloads::cpu_bound(20_000_000),
            start_time_ns: 0,
            mm_id: None,
            allowed_cpus: Some(dom1.clone()),
            parent_pid: None,
            cgroup_name: None,
            task_flags: 0,
            migration_disabled: 0,
        });
        pid += 1;
    }
    // Two free-floating I/O tasks that can be donated/stolen across domains.
    for _ in 0..2 {
        b = b.add_task(
            &format!("io{pid}"),
            -5,
            workloads::io_bound(50_000, 200_000),
        );
        pid += 1;
    }
    b.duration_ms(duration_ms).build()
}

// ---------------------------------------------------------------------------
// Target: balance.bpf.c `no_fast_lb` slow-path branches (15 gated sites,
// 0 executions at audit — no existing test sets no_fast_lb).
// ---------------------------------------------------------------------------

/// Exercise LAVD's *slow* load-balancing path by enabling `no_fast_lb`.
///
/// With `no_fast_lb = true`, `plan_x_cpdom_migration()`, `consume_dsq()`, and
/// `try_to_steal_task()` in `balance.bpf.c` take their slow-path branches
/// (skip the DSQ peek / task-load estimation, use budget-only decisions).
/// Those `if (no_fast_lb) { ... }` arms are never taken by the default suite
/// (which always runs with `no_fast_lb = false`), so an imbalanced multi-domain
/// workload with the flag set covers previously-unreached branches.
#[test]
fn test_lavd_no_fast_lb_slow_path() {
    let _lock = common::setup_test();

    let nr_cpus = 8u32;
    let sched = DynamicScheduler::lavd_multi_domain(nr_cpus, 2);
    unsafe {
        setup_pco(&sched, nr_cpus);
        lavd_set_bool(&sched, "no_fast_lb\0", true);
    }

    // Domain 0 oversubscribed (6 hogs / 4 CPUs), domain 1 light (1 hog) — a
    // persistent imbalance that keeps the balancer classifying and moving load.
    let scenario = imbalanced_two_domain_scenario(nr_cpus, 4, 6, 1, 400);

    let trace = Simulator::new(sched).run(scenario);
    assert!(
        !trace.has_error(),
        "unexpected error: {:?}",
        trace.exit_kind()
    );

    // All hogs on both domains must make progress.
    for pid in 1..=7i32 {
        assert!(
            trace.total_runtime(Pid(pid)) > 0,
            "task pid={pid} got no runtime under no_fast_lb"
        );
    }
}

/// `no_fast_lb` combined with per-CPU DSQ mode and a fixed migration-delta
/// threshold, to reach the slow-path arms that also depend on the DSQ mode.
#[test]
fn test_lavd_no_fast_lb_per_cpu_dsq() {
    let _lock = common::setup_test();

    let nr_cpus = 8u32;
    let sched = DynamicScheduler::lavd_multi_domain(nr_cpus, 2);
    unsafe {
        setup_pco(&sched, nr_cpus);
        lavd_set_bool(&sched, "no_fast_lb\0", true);
    }
    // per_cpu_dsq = true, no dual-DSQ, fixed 20% migration threshold.
    sched.lavd_configure(true, 0, 20);

    let scenario = imbalanced_two_domain_scenario(nr_cpus, 4, 5, 2, 400);

    let trace = Simulator::new(sched).run(scenario);
    assert!(
        !trace.has_error(),
        "unexpected error: {:?}",
        trace.exit_kind()
    );
    for pid in 1..=7i32 {
        assert!(
            trace.total_runtime(Pid(pid)) > 0,
            "task pid={pid} got no runtime under no_fast_lb+per_cpu_dsq"
        );
    }
}

// ---------------------------------------------------------------------------
// Target: `no_slice_boost` branches (untested global) in the slice/preemption
// path.
// ---------------------------------------------------------------------------

/// Disable slice boosting via `no_slice_boost` and run a latency-sensitive
/// ping-pong alongside CPU hogs. With boosting off, the slice-calculation path
/// takes the non-boost branch that the default suite (boost enabled) never
/// exercises.
#[test]
fn test_lavd_no_slice_boost() {
    let _lock = common::setup_test();

    let nr_cpus = 4u32;
    let sched = DynamicScheduler::lavd(nr_cpus);
    unsafe {
        lavd_set_bool(&sched, "no_slice_boost\0", true);
    }

    let (ping, pong) = workloads::ping_pong(Pid(1), Pid(2), 200_000);
    let mut b = Scenario::builder()
        .cpus(nr_cpus)
        .seed(99)
        .detect_bpf_errors();
    b = b.task(TaskDef {
        name: "ping".into(),
        pid: Pid(1),
        nice: -5,
        behavior: ping,
        start_time_ns: 0,
        mm_id: Some(MmId(1)),
        allowed_cpus: None,
        parent_pid: None,
        cgroup_name: None,
        task_flags: 0,
        migration_disabled: 0,
    });
    b = b.task(TaskDef {
        name: "pong".into(),
        pid: Pid(2),
        nice: -5,
        behavior: pong,
        start_time_ns: 0,
        mm_id: Some(MmId(1)),
        allowed_cpus: None,
        parent_pid: None,
        cgroup_name: None,
        task_flags: 0,
        migration_disabled: 0,
    });
    for i in 0..4 {
        b = b.add_task(&format!("hog{i}"), 0, workloads::cpu_bound(20_000_000));
    }

    let trace = Simulator::new(sched).run(b.duration_ms(300).build());
    assert!(
        !trace.has_error(),
        "unexpected error: {:?}",
        trace.exit_kind()
    );
    assert!(trace.schedule_count(Pid(1)) > 0, "ping never scheduled");
    assert!(trace.schedule_count(Pid(2)) > 0, "pong never scheduled");
}
