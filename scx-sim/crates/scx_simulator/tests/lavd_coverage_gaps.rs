//! LAVD coverage-gap tests.
//!
//! These tests target LAVD C code paths that the existing `lavd.rs` suite does
//! not reach, identified by the 2026-07-22 coverage audit
//! (`scx-sim/ai_docs/COVERAGE_AUDIT_20260722.md`). Each test toggles a LAVD
//! `const volatile` global that no existing test exercises, then drives a
//! workload through the resulting branch. The task notes record the measured
//! `llvm-cov` net-new-line delta for each.
//!
//! Scope note (updated by `cover-lavd-power-idle-gaps`): the futex
//! priority-boost subsystem (`lock.bpf.c`) is a BPF-substrate gap (the
//! simulator does not deliver futex enter/exit tracepoints); the execve
//! `set_aggressive_migration` hook is `SEC("?tracepoint/...sys_enter_execve")`
//! and the simulator does not deliver syscall tracepoints, so it and its
//! sole callee `set_aggressive_migration` (a `static`, i.e. non-exported
//! function) cannot be reached; `conv_wall_to_invr` is a `static __inline`
//! with zero live callers so the compiler never emits it (confirmed absent
//! from the `.so` symbol table); `set_cpu_flag` is a `__hidden inline` with
//! zero live callers (never emitted, never dynamically resolvable); and
//! `get_nice_prio` is `__hidden` and only reachable through
//! `introspec.bpf.c::submit_task_ctx`, whose `bpf_ringbuf_reserve` the
//! simulator wrapper stubs to `NULL` — so it returns `-ENOMEM` before ever
//! calling `get_nice_prio`. Each of these was verified against the scx lavd
//! BPF source and the built `libscx_lavd.so` symbol table; see task
//! `cover-lavd-power-idle-gaps` notes.
//!
//! Newly covered here (previously listed as out of scope): the power-profile
//! switch `do_set_power_profile` via the autopilot runtime path
//! (`do_autopilot` → `do_set_power_profile`, PERFORMANCE transition +
//! `update_power_mode_time`), and `get_cpuperf_cap` (exported accessor,
//! exercised over FFI).
//!
//! Still NOT test-addressable and documented below with the empirically
//! confirmed mechanism (each measured 0% under coverage instrumentation): the
//! `set_power_profile` `SEC("syscall")` wrapper (its body needs an installed
//! simulator context and there is no syscall-injection API), and the
//! idle-migration fallbacks `pick_random_cpu` / `cpumask_any_distribute` /
//! `migrate_to_neighbor` (topology / affinity-substrate gaps).

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

/// Read one `u16` slot of a LAVD `u16[]` global (e.g. `cpu_capacity`).
unsafe fn lavd_get_u16_at(sched: &DynamicScheduler, name: &str, idx: usize) -> u16 {
    let sym: libloading::Symbol<'_, *mut u16> = sched
        .get_symbol(name.as_bytes())
        .unwrap_or_else(|| panic!("symbol {name} not found"));
    std::ptr::read_volatile((*sym).add(idx))
}

/// Write one `u16` slot of a LAVD `u16[]` global.
unsafe fn lavd_set_u16_at(sched: &DynamicScheduler, name: &str, idx: usize, val: u16) {
    let sym: libloading::Symbol<'_, *mut u16> = sched
        .get_symbol(name.as_bytes())
        .unwrap_or_else(|| panic!("symbol {name} not found"));
    std::ptr::write_volatile((*sym).add(idx), val);
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
            thread_group_leader: None,
            uid: Uid(0),
            gid: Gid(0),
            fork_cpu: None,
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
            thread_group_leader: None,
            uid: Uid(0),
            gid: Gid(0),
            fork_cpu: None,
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
        thread_group_leader: None,
        uid: Uid(0),
        gid: Gid(0),
        fork_cpu: None,
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
        thread_group_leader: None,
        uid: Uid(0),
        gid: Gid(0),
        fork_cpu: None,
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

// ---------------------------------------------------------------------------
// Target: power.bpf.c `do_set_power_profile` via the autopilot runtime path.
//
// `do_set_power_profile()` is the power-mode switch. It runs from
// `do_autopilot()`, which the periodic `update_sys_stat` timer calls when
// autopilot is enabled: it maps the measured required-capacity to POWERSAVE /
// BALANCED / PERFORMANCE and calls `do_set_power_profile()` to enact the
// change. A workload that starts nearly idle (only light I/O) and then becomes
// saturated (a burst of CPU hogs) makes the required-capacity cross the
// autopilot thresholds, so the switch runs across multiple arms on the *real*
// runtime path (No-Stub Rule: real scheduler C code — not the userspace-init
// helper `wrapper.c::lavd_set_power_mode`, which sets the globals directly and
// bypasses the switch).
//
// Note: the `set_power_profile` `SEC("syscall")` wrapper (a one-line
// `return do_set_power_profile(input->power_mode);`) cannot be covered from a
// test: invoking it directly aborts (its body calls `scx_bpf_now()`, which
// requires an installed simulator context), and the simulator has no API to
// inject a syscall-prog invocation mid-run. That single wrapper line is a
// substrate gap (no syscall-prog injection, mb sim-91a825); its body
// `do_set_power_profile` is what this test exercises.
// ---------------------------------------------------------------------------

/// Autopilot enabled + an idle→saturated load swing drives
/// `do_autopilot()` → `do_set_power_profile()` across power-mode thresholds.
#[test]
fn test_lavd_autopilot_runtime_power_switch() {
    let _lock = common::setup_test();

    let nr_cpus = 4u32;
    let sched = DynamicScheduler::lavd(nr_cpus);
    unsafe {
        setup_pco(&sched, nr_cpus);
    }
    sched.lavd_set_autopilot(true);

    // Autopilot starts BALANCED. Phase 1 (0..~250ms): CPU hogs saturate every
    // CPU → high required-capacity → autopilot climbs to PERFORMANCE, running
    // `do_set_power_profile`'s switch + `update_power_mode_time`. Phase 2
    // (~250ms..600ms): the hogs finish and only two light I/O tasks remain, so
    // the run continues under low load while the autopilot keeps evaluating.
    // (The BALANCED/POWERSAVE arms are not reliably reachable here — the
    // required-capacity EWMA decays too slowly to cross both lower thresholds
    // within the run — so this test targets the PERFORMANCE transition and the
    // surrounding machinery, not all three arms.)
    let mut b = Scenario::builder()
        .cpus(nr_cpus)
        .seed(7)
        .detect_bpf_errors();
    let mut pid = 1i32;
    // Light I/O tasks that persist for the whole run (define the low-load
    // floor after the hogs exit).
    for _ in 0..2 {
        b = b.task(TaskDef {
            name: format!("io{pid}"),
            pid: Pid(pid),
            nice: -5,
            behavior: workloads::io_bound(20_000, 4_000_000),
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
        });
        pid += 1;
    }
    // Finite CPU hogs: run flat-out for ~250ms then exit, collapsing the load.
    for _ in 0..nr_cpus {
        b = b.task(TaskDef {
            name: format!("hog{pid}"),
            pid: Pid(pid),
            nice: 0,
            behavior: TaskBehavior {
                phases: vec![Phase::Run(250_000_000)],
                repeat: RepeatMode::Once,
            },
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
        });
        pid += 1;
    }

    let trace = Simulator::new(sched).run(b.duration_ms(600).build());
    assert!(
        !trace.has_error(),
        "unexpected error: {:?}",
        trace.exit_kind()
    );
    // Sanity: a hog ran to completion (busy phase materialized) and the light
    // I/O tasks kept running afterward (the low-load phase materialized), so
    // the autopilot timer observed both a saturated and a light regime.
    assert!(
        trace.total_runtime(Pid(pid - 1)) > 0,
        "hog made no progress; saturated phase never materialized"
    );
    assert!(
        trace.total_runtime(Pid(1)) > 0,
        "light I/O task never ran; low-load phase never materialized"
    );
}

// ---------------------------------------------------------------------------
// Target: power.bpf.c `get_cpuperf_cap` (exported accessor).
//
// `get_cpuperf_cap(cpu)` returns `cpu_capacity[cpu]`. Its only in-tree caller
// is `conv_wall_to_invr`, which itself has zero callers, so it is dead under
// the current scx source — but it is an *exported* symbol, so we exercise the
// real accessor (and its bounds handling) directly over FFI. This is a
// regression guard for the accessor, not a claim that the live scheduler
// reaches it.
// ---------------------------------------------------------------------------

/// `get_cpuperf_cap(cpu)` reads back the per-CPU capacity table entry.
#[test]
fn test_lavd_get_cpuperf_cap_reads_capacity_table() {
    let _lock = common::setup_test();

    let sched = DynamicScheduler::lavd(4);
    type GetCpuperfCapFn = unsafe extern "C" fn(i32) -> u16;

    unsafe {
        let sym: libloading::Symbol<'_, GetCpuperfCapFn> = sched
            .get_symbol(b"get_cpuperf_cap\0")
            .expect("get_cpuperf_cap symbol not found");

        // Write a known capacity to a slot, then confirm the accessor returns
        // it. Save/restore the slot so we do not perturb the shared `.so`
        // state for subsequent tests under the serialized SIM_LOCK.
        let saved = lavd_get_u16_at(&sched, "cpu_capacity\0", 2);
        lavd_set_u16_at(&sched, "cpu_capacity\0", 2, 777);
        assert_eq!(
            (sym)(2),
            777,
            "get_cpuperf_cap must return the cpu_capacity[] slot"
        );
        lavd_set_u16_at(&sched, "cpu_capacity\0", 2, saved);
        assert_eq!((sym)(2), saved, "capacity slot restored");
    }
}

// ---------------------------------------------------------------------------
// NOT test-addressable: idle.bpf.c `pick_random_cpu`, `cpumask_any_distribute`,
// and `migrate_to_neighbor`.
//
// These were empirically confirmed unreachable under scxsim's current lavd
// substrate (each candidate scenario measured 0% coverage with `coverage.sh`
// instrumentation). The mechanisms:
//
// * `pick_random_cpu` is taken only when `find_sticky_cpu_and_cpdom()` returns
//   `sticky_cpdom < 0`, which requires the task's `prev_cpu` to be *not
//   runnable* (affinity excludes it) with no runnable waker/sticky domain.
//   scxsim models `TaskDef::initial_cpu()` kernel-faithfully — a task's first
//   CPU is always inside its cpumask (`task.rs`: "a new task's cpu field is set
//   to the CPU where it was forked, which is always within its cpumask") — and
//   the scenario API has no runtime affinity-narrowing event (no
//   `Phase::SetAffinity`), so `prev_cpu` is never outside the allowed set. The
//   only other path (the `cpu < 0` fallback at `idle.bpf.c:926`) is the branch
//   the source itself annotates as "impossible". `cpumask_any_distribute` is
//   called only from `pick_random_cpu`, so it inherits the same gap.
//
// * `migrate_to_neighbor` has two call sites, both blocked:
//   - `idle.bpf.c:872` is gated on `!i_smt_empty`, i.e. `is_smt_active` AND a
//     fully-idle SMT core. The lavd scxsim wrapper (`schedulers/lavd/wrapper.c`
//     `lavd_setup`) hardcodes `is_smt_active = false`. Even when forced true
//     (whitebox) alongside a real idle-SMT-mask substrate (`scenario.smt(2)`)
//     and a strongly imbalanced two-domain workload, the required simultaneous
//     state — the selected task's sticky domain is a `stealee` with no fully
//     idle core *while* a neighbor domain has a fully idle core, all at the
//     `lavd_select_cpu` instant — did not co-occur (measured 0%).
//   - `idle.bpf.c:897` requires `LAVD_FLAG_MIGRATION_AGGRESSIVE`, set only by
//     `set_aggressive_migration()` from the execve tracepoint hook, which the
//     simulator does not deliver (see the syscall-tracepoint gap above).
//
// Closing these needs scxsim infrastructure, not a new test:
// * migrate_to_neighbor — propagate `scenario.smt()` into the lavd wrapper's
//   `is_smt_active` + per-CPU SMT sibling topology, plus a stealee/idle-neighbor
//   scenario primitive (mb sim-39706c).
// * pick_random_cpu / cpumask_any_distribute — a runtime affinity-narrowing
//   scenario event so a task can wake with `prev_cpu` outside its cpumask
//   (mb sim-110125).
