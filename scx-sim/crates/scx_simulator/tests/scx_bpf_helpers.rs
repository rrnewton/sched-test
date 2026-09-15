//! Return-value / API-contract tests for the simulated SCX BPF helper kfuncs
//! (`scx_bpf_*`), as they are actually invoked by the linked-in BPF schedulers.
//!
//! Every kfunc in `unsafe_impl/kfuncs.rs` is a `#[no_mangle] extern "C"` symbol
//! the compiled scheduler `.so` resolves against at load time (per the No-Stub
//! rule, the *scheduler's own* BPF code calls these — nothing is faked). Many
//! of the helpers record the exact value they returned into the trace via a
//! dedicated `TraceKind` (`HelperNow`, `HelperTaskCpu`, `DsqNrQueued`,
//! `CreateDsq`, `KickCpu`, `DsqInsert`/`DsqInsertVtime`/`DsqMoveToLocal`). This
//! file asserts those recorded return values match the documented BPF API
//! behavior, complementing the sibling files that test *policy* rather than the
//! helper contract:
//!   - `dsq_operations.rs`  — DSQ lifecycle / vtime ordering / backpressure.
//!   - `idle_cpu_selection.rs` — idle notification and kick *presence*.
//!   - `topology.rs`        — CPU-topology spread.
//!
//! Coverage of the six task items:
//!   1. `scx_bpf_dsq_insert` / `_dsq_insert_vtime` (a.k.a. `scx_bpf_dispatch`)
//!      → `test_dsq_insert_deferred_delivers` (slice > 0, insert ⇒ run).
//!   2. `scx_bpf_dsq_move_to_local` (a.k.a. `scx_bpf_consume`)
//!      → `test_dsq_move_to_local_consume_conservation`.
//!   3. `scx_bpf_kick_cpu` → `test_kick_cpu_targets_valid_cpu`.
//!   4. `scx_bpf_task_running` / `scx_bpf_task_cgroup`
//!      → `test_task_cgroup_null_pointer_contract` (the only externally
//!      reachable contract; see the test's doc comment for why the non-null
//!      path and `scx_bpf_task_running` have no integration-trace surface and
//!      are covered by the in-crate unit tests in `kfuncs.rs`).
//!   5. `scx_bpf_nr_cpu_ids` + topology → `test_nr_cpu_ids_bounds_and_scaling`.
//!   6. "return values match documented BPF API behavior" — the unifying theme;
//!      `test_now_*`, `test_task_cpu_*`, `test_dsq_nr_queued_*`,
//!      `test_create_dsq_*` each pin one helper's return contract.
//!
//! Which schedulers exercise which helper was probed empirically; assertions
//! are scoped to the schedulers that actually call the helper (e.g. `simple`
//! never emits `HelperNow`/`KickCpu`, so it is excluded from those, matching
//! its no-preemption / no-clock-probe design).

use std::collections::BTreeSet;
use std::ffi::c_void;

use scx_simulator::*;

#[macro_use]
mod common;

// ---------------------------------------------------------------------------
// Shared fixtures
// ---------------------------------------------------------------------------

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

fn new_sched(label: &str, cpus: u32) -> DynamicScheduler {
    match label {
        "simple" => DynamicScheduler::simple(),
        "lavd" => DynamicScheduler::lavd(cpus),
        "cosmos" => DynamicScheduler::cosmos(cpus),
        _ => unreachable!("unknown scheduler {label}"),
    }
}

/// Run `label` on `cpus` CPUs with `scenario`, asserting a clean exit.
fn run(label: &str, cpus: u32, scenario: Scenario) -> Trace {
    let trace = Simulator::new(new_sched(label, cpus)).run(scenario);
    assert!(
        !trace.has_error(),
        "[{label}] simulation error: {:?}",
        trace.exit_kind()
    );
    trace
}

/// `n` always-runnable CPU hogs on `cpus` CPUs — maximal DSQ / kick pressure.
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
            workloads::cpu_bound(100_000_000),
        ));
    }
    b.duration_ms(150).build()
}

/// `n` run/sleep tasks on `cpus` CPUs — exercises wakeups, idle transitions,
/// and the local fast path alongside the shared DSQ path.
fn mixed(cpus: u32, n: i32, seed: u32) -> Scenario {
    let mut b = Scenario::builder()
        .cpus(cpus)
        .seed(seed)
        .detect_bpf_errors();
    for i in 0..n {
        b = b.task(task(
            &format!("t{i}"),
            1 + i,
            0,
            TaskBehavior {
                phases: vec![Phase::Run(2_000_000), Phase::Sleep(1_000_000)],
                repeat: RepeatMode::Forever,
            },
        ));
    }
    b.duration_ms(120).build()
}

const DURATION_MS: u64 = 150;

// ---------------------------------------------------------------------------
// 1. scx_bpf_now(): per-CPU monotonic clock, returns the current time.
//    Contract: returns the current per-CPU `local_clock`. Within the running
//    simulation the clock is monotonic non-decreasing on each CPU and stays
//    within the window. There is exactly one legitimate exception per CPU: the
//    boundary between the init/enable epoch (ops.init, cgroup_init, enable —
//    which advance a nominal sub-microsecond clock) and the start of real task
//    execution, where the CPU's clock resets to ~0 for the run epoch. We allow
//    a single such reset per CPU, and only to a near-zero value; any other
//    backward step is a real clock regression and fails.
//    Only lavd is covered. cosmos does NOT call scx_bpf_now() at all -- it
//    reads time via bpf_ktime_get_ns() (zero occurrences of scx_bpf_now in
//    scx_cosmos/src/bpf/main.bpf.c). This test used to assert cosmos called
//    it, and passed only because the wrapper's hand-written scx_pmu_* stubs
//    called scx_bpf_now() to fabricate PMU counter values -- it was measuring
//    the simulator's own stub, not the scheduler. simple never calls it.
// ---------------------------------------------------------------------------

/// A backward clock step is only tolerated if it lands here (init→run epoch
/// reset near time 0); the real run epoch quickly climbs into the milliseconds.
const EPOCH_RESET_CEILING_NS: u64 = 100_000;

#[test]
fn test_now_returns_monotonic_per_cpu_clock() {
    let _lock = common::setup_test();
    for label in ["lavd"] {
        let trace = run(label, 4, mixed(4, 8, 1));

        let mut saw_now = false;
        // Per CPU: last observed return value, and how many epoch resets seen.
        let mut last_per_cpu: std::collections::HashMap<u32, u64> =
            std::collections::HashMap::new();
        let mut resets_per_cpu: std::collections::HashMap<u32, u32> =
            std::collections::HashMap::new();
        let max_ns = DURATION_MS * 1_000_000 + 5_000_000; // window + slack
        for e in trace.events() {
            if let TraceKind::HelperNow { ret_ns } = e.kind {
                saw_now = true;
                // The helper reports the CPU's own clock: the event timestamp
                // and the returned value are one and the same reading.
                assert_eq!(
                    ret_ns, e.time_ns,
                    "[{label}] scx_bpf_now ret {ret_ns} != recording time {}",
                    e.time_ns
                );
                assert!(
                    ret_ns <= max_ns,
                    "[{label}] scx_bpf_now returned {ret_ns} > window {max_ns}"
                );
                let last = last_per_cpu.entry(e.cpu.0).or_insert(0);
                if ret_ns < *last {
                    // Only the init→run epoch boundary may go backwards, once
                    // per CPU, and only to a near-zero value.
                    let resets = resets_per_cpu.entry(e.cpu.0).or_insert(0);
                    *resets += 1;
                    assert!(
                        *resets == 1 && ret_ns < EPOCH_RESET_CEILING_NS,
                        "[{label}] scx_bpf_now regressed on cpu{}: {ret_ns} < {last} \
                         (reset #{resets}, not an init→run epoch boundary)",
                        e.cpu.0
                    );
                }
                *last = ret_ns;
            }
        }
        assert!(saw_now, "[{label}] scheduler never called scx_bpf_now");
    }
}

// ---------------------------------------------------------------------------
// 2. scx_bpf_task_cpu(): returns a valid CPU id.
//    Contract: returns the task's assigned CPU — always within
//    [0, nr_cpu_ids). (lavd/cosmos call it; simple does not.)
// ---------------------------------------------------------------------------

#[test]
fn test_task_cpu_returns_valid_cpu() {
    let _lock = common::setup_test();
    let cpus = 4u32;
    for label in ["lavd", "cosmos"] {
        let trace = run(label, cpus, hogs(cpus, 8, 2));

        let mut saw = false;
        for e in trace.events() {
            if let TraceKind::HelperTaskCpu { ret_cpu, .. } = e.kind {
                saw = true;
                assert!(
                    ret_cpu.0 < cpus,
                    "[{label}] scx_bpf_task_cpu returned cpu{} >= nr_cpu_ids {cpus}",
                    ret_cpu.0
                );
            }
        }
        assert!(saw, "[{label}] scheduler never called scx_bpf_task_cpu");
    }
}

// ---------------------------------------------------------------------------
// 3. scx_bpf_dsq_nr_queued(): non-negative depth bounded by the runnable set.
//    Contract: returns the count of tasks queued in the DSQ — never negative,
//    never more than the tasks that exist, and under contention it must
//    actually observe a backlog (> 0), proving it reflects real depth rather
//    than a constant. (lavd polls queue depth; simple/cosmos do not.)
// ---------------------------------------------------------------------------

#[test]
fn test_dsq_nr_queued_return_value_bounds() {
    let _lock = common::setup_test();
    let n_tasks = 16i32;
    // Heavy contention on 2 CPUs so a shared DSQ genuinely backs up.
    let trace = run("lavd", 2, hogs(2, n_tasks, 3));

    let mut saw = false;
    let mut max_ret = 0i32;
    for e in trace.events() {
        if let TraceKind::DsqNrQueued { ret, .. } = e.kind {
            saw = true;
            assert!(ret >= 0, "scx_bpf_dsq_nr_queued returned negative {ret}");
            assert!(
                ret <= n_tasks,
                "scx_bpf_dsq_nr_queued returned {ret} > total tasks {n_tasks}"
            );
            max_ret = max_ret.max(ret);
        }
    }
    assert!(saw, "lavd never called scx_bpf_dsq_nr_queued");
    assert!(
        max_ret >= 1,
        "scx_bpf_dsq_nr_queued never observed a backlog under 16-hog contention (max {max_ret})"
    );
}

// ---------------------------------------------------------------------------
// 4. scx_bpf_create_dsq(): success return code + valid NUMA node.
//    Contract: returns 0 on success; a healthy scheduler init creates each DSQ
//    exactly once (no -1 double-create), and the requested node is a real node
//    id or -1 ("any node"). Every supported scheduler creates >=1 DSQ.
// ---------------------------------------------------------------------------

#[test]
fn test_create_dsq_return_code_and_node() {
    let _lock = common::setup_test();
    for label in ["simple", "lavd", "cosmos"] {
        let trace = run(label, 4, mixed(4, 6, 4));

        let mut created: BTreeSet<u64> = BTreeSet::new();
        for e in trace.events() {
            if let TraceKind::CreateDsq { dsq_id, node, rc } = e.kind {
                assert_eq!(
                    rc, 0,
                    "[{label}] scx_bpf_create_dsq({}) returned rc={rc} (double-create or failure)",
                    dsq_id.0
                );
                assert!(
                    node >= -1,
                    "[{label}] scx_bpf_create_dsq requested invalid node {node}"
                );
                created.insert(dsq_id.0);
            }
        }
        assert!(
            !created.is_empty(),
            "[{label}] scheduler created no DSQs via scx_bpf_create_dsq"
        );
    }
}

// ---------------------------------------------------------------------------
// 5. scx_bpf_kick_cpu(): targets an in-range CPU.
//    Contract: the kicked CPU id is always < nr_cpu_ids. Under hog contention
//    lavd/cosmos issue cross-CPU kicks; every target must be a real CPU.
//    (Presence is already covered by idle_cpu_selection.rs; here we pin the
//    *target validity* of the argument the scheduler passed.)
// ---------------------------------------------------------------------------

#[test]
fn test_kick_cpu_targets_valid_cpu() {
    let _lock = common::setup_test();
    let cpus = 4u32;
    // lavd kicks under saturation (preemption-driven); cosmos only issues
    // SCX_KICK_IDLE, so it needs an idle CPU to exist before it kicks at all.
    // See idle_cpu_selection::test_wakeup_kick_cpu_paths for why cosmos is no
    // longer expected to kick on a saturated 8/4 workload.
    for (label, nr_tasks) in [("lavd", 8), ("cosmos", 2)] {
        let trace = run(label, cpus, hogs(cpus, nr_tasks, 5));

        let mut kicks = 0usize;
        for e in trace.events() {
            if let TraceKind::KickCpu { target_cpu } = e.kind {
                kicks += 1;
                assert!(
                    target_cpu.0 < cpus,
                    "[{label}] scx_bpf_kick_cpu targeted cpu{} >= nr_cpu_ids {cpus}",
                    target_cpu.0
                );
            }
        }
        assert!(kicks > 0, "[{label}] scheduler issued no scx_bpf_kick_cpu");
    }
}

// ---------------------------------------------------------------------------
// 6. scx_bpf_nr_cpu_ids(): the scheduler's CPU-count view bounds every CPU id
//    it ever references, and it scales the CPUs it uses to the count it sees.
//
//    scx_bpf_nr_cpu_ids() returns sim.cpus.len(); it is only meaningful inside
//    a live sim context, so rather than call it in isolation we validate the
//    invariant it underpins: with N CPUs configured, no event (scheduling,
//    kick target, task_cpu return, idle update) ever names a CPU >= N, and a
//    contended run spreads across more CPUs when N grows.
// ---------------------------------------------------------------------------

/// Highest CPU id named anywhere in the trace (event site + helper returns).
fn max_cpu_referenced(trace: &Trace) -> u32 {
    let mut m = 0u32;
    for e in trace.events() {
        m = m.max(e.cpu.0);
        match e.kind {
            TraceKind::KickCpu { target_cpu } => m = m.max(target_cpu.0),
            TraceKind::HelperTaskCpu { ret_cpu, .. } => m = m.max(ret_cpu.0),
            TraceKind::UpdateIdle { cpu, .. } => m = m.max(cpu.0),
            _ => {}
        }
    }
    m
}

/// Distinct CPUs a task actually ran on.
fn distinct_cpus_used(trace: &Trace) -> BTreeSet<u32> {
    trace
        .events()
        .iter()
        .filter_map(|e| match e.kind {
            TraceKind::TaskScheduled { .. } => Some(e.cpu.0),
            _ => None,
        })
        .collect()
}

#[test]
fn test_nr_cpu_ids_bounds_and_scaling() {
    let _lock = common::setup_test();
    for label in ["lavd", "cosmos"] {
        // Bounds: no CPU id may exceed the configured count, for any topology.
        for n in [1u32, 2, 4, 8] {
            let trace = run(label, n, hogs(n, 8, 6));
            let max_cpu = max_cpu_referenced(&trace);
            assert!(
                max_cpu < n,
                "[{label}] referenced cpu{max_cpu} with only {n} CPUs (nr_cpu_ids violation)"
            );
        }

        // Scaling: 8 hogs on 8 CPUs must spread over more CPUs than on 2.
        let narrow = distinct_cpus_used(&run(label, 2, hogs(2, 8, 7)));
        let wide = distinct_cpus_used(&run(label, 8, hogs(8, 8, 7)));
        assert!(
            wide.len() > narrow.len(),
            "[{label}] no extra CPU spread at 8 CPUs (wide={} narrow={}); \
             scheduler is not honoring nr_cpu_ids",
            wide.len(),
            narrow.len()
        );
    }
}

// ---------------------------------------------------------------------------
// 7. scx_bpf_dsq_insert / scx_bpf_dsq_insert_vtime (== scx_bpf_dispatch):
//    deferred insert, positive slice, and every inserted task reaches a CPU.
//    Contract: the insert hands the task a positive time slice and enqueues it
//    for dispatch; a task that is inserted must subsequently be scheduled.
// ---------------------------------------------------------------------------

#[test]
fn test_dsq_insert_deferred_delivers() {
    let _lock = common::setup_test();
    for label in ["simple", "lavd", "cosmos"] {
        let trace = run(label, 4, mixed(4, 8, 8));

        let mut inserted: BTreeSet<i32> = BTreeSet::new();
        let mut scheduled: BTreeSet<i32> = BTreeSet::new();
        let mut inserts = 0usize;
        for e in trace.events() {
            match e.kind {
                TraceKind::DsqInsert { pid, slice, .. } => {
                    assert!(slice > 0, "[{label}] scx_bpf_dsq_insert slice was 0");
                    inserted.insert(pid.0);
                    inserts += 1;
                }
                TraceKind::DsqInsertVtime { pid, slice, .. } => {
                    assert!(slice > 0, "[{label}] scx_bpf_dsq_insert_vtime slice was 0");
                    inserted.insert(pid.0);
                    inserts += 1;
                }
                TraceKind::TaskScheduled { pid } => {
                    scheduled.insert(pid.0);
                }
                _ => {}
            }
        }
        assert!(inserts > 0, "[{label}] no scx_bpf_dsq_insert[_vtime] calls");
        // Deferred insert ⇒ eventual dispatch: no inserted task is stranded.
        let stranded: Vec<i32> = inserted.difference(&scheduled).copied().collect();
        assert!(
            stranded.is_empty(),
            "[{label}] tasks inserted but never scheduled: {stranded:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// 8. scx_bpf_dsq_move_to_local (== scx_bpf_consume): consume conservation.
//    Contract: a successful move (success=true) delivers one queued task to the
//    current CPU's local DSQ; you cannot consume more tasks than were inserted.
//    (simple/lavd consume from a shared DSQ; cosmos inserts directly to local
//    DSQs and barely uses move-to-local, so it is excluded.)
// ---------------------------------------------------------------------------

#[test]
fn test_dsq_move_to_local_consume_conservation() {
    let _lock = common::setup_test();
    for label in ["simple", "lavd"] {
        let trace = run(label, 2, hogs(2, 12, 9));

        let mut inserts = 0usize;
        let mut ok_moves = 0usize;
        let mut total_moves = 0usize;
        for e in trace.events() {
            match e.kind {
                TraceKind::DsqInsert { .. } | TraceKind::DsqInsertVtime { .. } => inserts += 1,
                TraceKind::DsqMoveToLocal { success, .. } => {
                    total_moves += 1;
                    if success {
                        ok_moves += 1;
                    }
                }
                _ => {}
            }
        }
        assert!(
            ok_moves > 0,
            "[{label}] no successful scx_bpf_dsq_move_to_local (moves={total_moves})"
        );
        // Conservation: cannot consume more than was ever inserted.
        assert!(
            ok_moves <= inserts,
            "[{label}] consumed {ok_moves} tasks but only {inserts} were inserted"
        );
    }
}

// ---------------------------------------------------------------------------
// 9. scx_bpf_task_cgroup(NULL): defensive null-pointer contract.
//
//    This is the one helper contract reachable from an integration test via a
//    direct kfunc call: a NULL task pointer resolves to a NULL cgroup with no
//    simulator context required (the early-return path in kfuncs.rs), matching
//    the kernel's defensive handling.
//
//    The non-null `scx_bpf_task_cgroup` path and `scx_bpf_task_running` have no
//    integration-trace surface with the currently supported schedulers:
//      * cgroup_bw (post-776ae41e) threads the raw `cgrp_id` instead of calling
//        scx_bpf_task_cgroup, so lavd emits no HelperTaskCgroup events; and
//      * scx_bpf_task_running (called internally by cosmos's select_cpu, see
//        schedulers/cosmos/cosmos_main_patched.c) returns a bool with no trace
//        event.
//    Both are exercised by the in-crate unit tests in `unsafe_impl/kfuncs.rs`
//    (which can construct a SimulatorState), and cosmos running correctly under
//    every test here transitively depends on scx_bpf_task_running behaving.
// ---------------------------------------------------------------------------

#[test]
fn test_task_cgroup_null_pointer_contract() {
    let _lock = common::setup_test();
    // extern "C" kfunc, safe to call; NULL in ⇒ NULL out, no sim context used.
    let ret = scx_bpf_task_cgroup(std::ptr::null_mut::<c_void>(), 0);
    assert!(
        ret.is_null(),
        "scx_bpf_task_cgroup(NULL) must return NULL, got {ret:?}"
    );
}
