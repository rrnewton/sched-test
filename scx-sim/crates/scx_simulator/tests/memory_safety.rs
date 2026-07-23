//! Memory-safety stress tests for the C scheduler substrate
//! (tg test-sim-memory-safety).
//!
//! These tests drive the simulator down the code paths most likely to expose
//! memory-safety bugs in the *compiled-in C* — the deterministic bump arena
//! (`csrc/sim_arena.c`, 32 MiB), the SDT task-local-storage hash table
//! (`csrc/sim_sdt_stubs.c`, 16 384 slots, open addressing keyed by the
//! `task_struct` pointer), the per-cgroup context allocations, and the BPF
//! scheduler's own `init_task`/`exit_task`/`cgroup_*` callbacks that read and
//! write that memory.
//!
//! Under a plain `cargo test` run these assert the observable contract: the
//! process never crashes (reaching any assertion already proves no
//! segfault/abort escaped the C code), the run does not surface a BPF error,
//! and boundary conditions (arena/map/cgroup ceilings) are handled gracefully
//! rather than corrupting memory. Their real power is as an ASAN/valgrind
//! payload — see the module note below.
//!
//! ## Running under a memory sanitizer (task item 6)
//! The whole point of these scenarios is to be run under a checker that
//! instruments the dlopened C scheduler `.so` and the `csrc/` substrate:
//!
//! ```text
//! # valgrind memcheck (works on the normal stable build; no rebuild needed):
//! valgrind --error-exitcode=99 --leak-check=no \
//!   ./target/debug/deps/memory_safety-<hash> --test-threads=1
//!
//! # or Rust+C AddressSanitizer on nightly:
//! RUSTFLAGS="-Zsanitizer=address" CFLAGS="-fsanitize=address" \
//!   cargo +nightly test -p scx_simulator --test memory_safety --target x86_64-unknown-linux-gnu
//! ```
//!
//! The scenarios are kept modest in *duration* (allocation happens at
//! `init_task`, independent of how long the sim runs) so they stay tractable
//! even at ~10-50x slowdown under valgrind.

use scx_simulator::*;

#[macro_use]
mod common;

/// A named scheduler constructor. `simple` ignores the CPU count.
type NamedSchedFactory = (&'static str, fn(u32) -> DynamicScheduler);

fn schedulers() -> [NamedSchedFactory; 3] {
    [
        ("simple", |_n| DynamicScheduler::simple()),
        ("lavd", DynamicScheduler::lavd),
        ("cosmos", DynamicScheduler::cosmos),
    ]
}

/// A forever-running CPU hog behavior.
fn hog() -> TaskBehavior {
    TaskBehavior {
        phases: vec![Phase::Run(1_000_000_000)],
        repeat: RepeatMode::Forever,
    }
}

/// A one-shot task (runs `run_ns` once, then exits → `exit_task`), with an
/// explicit PID and creation time so create/destroy can be staggered.
fn oneshot(pid: i32, start_ns: TimeNs, run_ns: TimeNs) -> TaskDef {
    TaskDef {
        name: format!("os{pid}"),
        pid: Pid(pid),
        nice: 0,
        behavior: TaskBehavior {
            phases: vec![Phase::Run(run_ns)],
            repeat: RepeatMode::Once,
        },
        start_time_ns: start_ns,
        mm_id: None,
        allowed_cpus: None,
        parent_pid: None,
        cgroup_name: None,
        task_flags: 0,
        migration_disabled: 0,
    }
}

/// Count trace events matching a predicate.
fn count(trace: &Trace, pred: impl Fn(&TraceKind) -> bool) -> usize {
    trace.events().iter().filter(|e| pred(&e.kind)).count()
}

// ---------------------------------------------------------------------------
// (1) Maximum task count — stress the bump arena + SDT hash table.
// ---------------------------------------------------------------------------

/// Allocate a large number of concurrent tasks. Every task takes a taskc slot
/// from the 32 MiB arena and a slot in the 16 384-entry SDT hash table (each
/// scheduler's `init_task` writes into it). A count well inside the documented
/// ~8 192-task ceiling exercises the allocator and the open-addressing probe
/// sequence at real load without hitting the graceful-exhaustion cap. The run
/// must complete without a BPF error or crash.
#[test]
fn max_task_count_stresses_arena() {
    let _lock = common::setup_test();
    let ntasks = 2000;

    for (name, make) in schedulers() {
        let mut b = Scenario::builder().cpus(4).no_watchdog();
        for _ in 0..ntasks {
            b = b.add_task("t", 0, hog());
        }
        // Short duration: taskc allocation happens at init_task regardless.
        let trace = Simulator::new(make(4)).run(b.duration_ms(15).build());

        assert!(
            !trace.has_error(),
            "{name}: {ntasks} tasks surfaced an error: {:?}",
            trace.exit_kind()
        );
        assert!(
            !trace.events().is_empty(),
            "{name}: {ntasks}-task run produced no events"
        );
    }
}

// ---------------------------------------------------------------------------
// (2) Rapid task create/destroy cycles — init_task/exit_task churn.
// ---------------------------------------------------------------------------

/// Hundreds of one-shot tasks whose creation is staggered across the run so
/// that `init_task` (taskc alloc + SDT insert) and `exit_task` (SDT free) fire
/// continuously and interleaved. Verifies the allocator/table survive constant
/// churn and that the tasks actually get created and destroyed.
#[test]
fn rapid_task_create_destroy_cycles() {
    let _lock = common::setup_test();
    let ntasks = 600i32;
    let dur_ns = 60_000_000u64;

    for (name, make) in schedulers() {
        let mut b = Scenario::builder().cpus(4).no_watchdog();
        for i in 0..ntasks {
            // Spread creation across the first ~half of the run.
            let start = (i as u64 * (dur_ns / 2)) / ntasks as u64;
            b = b.task(oneshot(i + 1, start, 150_000));
        }
        let trace = Simulator::new(make(4)).run(b.duration_ns(dur_ns).build());

        assert!(
            !trace.has_error(),
            "{name}: create/destroy churn surfaced an error: {:?}",
            trace.exit_kind()
        );
        // A large fraction of the one-shot tasks completed (were destroyed via
        // exit_task) — proving real create/destroy churn, not just creation.
        let completed = count(&trace, |k| matches!(k, TraceKind::TaskCompleted { .. }));
        assert!(
            completed >= ntasks as usize / 2,
            "{name}: only {completed}/{ntasks} tasks completed — churn too shallow"
        );
    }
}

// ---------------------------------------------------------------------------
// (3) Accessing task data after a task exits — use-after-exit probes.
// ---------------------------------------------------------------------------

/// A waker repeatedly issues `Wake` at a target PID that has already exited
/// (the target is a one-shot task that finishes early). Waking a departed task
/// must be handled without dereferencing freed taskc — `bpf_task_from_pid`
/// returns NULL and the scheduler must cope. The run must stay healthy.
#[test]
fn wake_targeting_exited_task_no_uaf() {
    let _lock = common::setup_test();

    for (name, make) in schedulers() {
        let scenario = Scenario::builder()
            .cpus(2)
            .no_watchdog()
            // Waker (pid 1): keeps waking pid 2 long after pid 2 has exited.
            .add_task(
                "waker",
                0,
                TaskBehavior {
                    phases: vec![
                        Phase::Run(1_000_000),
                        Phase::Wake(Pid(2)),
                        Phase::Sleep(1_000_000),
                    ],
                    repeat: RepeatMode::Forever,
                },
            )
            // Target (pid 2): runs once for 500µs, then exits early.
            .task(oneshot(2, 0, 500_000))
            .duration_ms(50)
            .build();

        let trace = Simulator::new(make(2)).run(scenario);

        assert!(
            !trace.has_error(),
            "{name}: waking an exited task surfaced an error: {:?}",
            trace.exit_kind()
        );
        // The target really did exit, and the waker kept running afterward
        // (so the post-exit wakes were actually attempted).
        assert!(
            count(
                &trace,
                |k| matches!(k, TraceKind::TaskCompleted { pid } if *pid == Pid(2))
            ) >= 1,
            "{name}: target task never exited — UAF path not exercised"
        );
        assert!(
            trace.schedule_count(Pid(1)) > 1,
            "{name}: waker did not keep running past the target's exit"
        );
    }
}

/// Mass task exit under the schedulers that carry per-task BPF timers/iterators
/// (LAVD, cosmos): many one-shot tasks exit while the scheduler's periodic
/// bookkeeping may still hold references. `exit_task` frees the SDT slot; no
/// later callback may touch it. Clean completion is the black-box signal;
/// under valgrind/ASAN this is a prime use-after-free site.
#[test]
fn mass_task_exit_under_timer_schedulers() {
    let _lock = common::setup_test();
    let ntasks = 400i32;

    for (name, make) in [
        (
            "lavd",
            DynamicScheduler::lavd as fn(u32) -> DynamicScheduler,
        ),
        ("cosmos", DynamicScheduler::cosmos),
    ] {
        let mut b = Scenario::builder().cpus(4).no_watchdog();
        // A couple of hogs keep the CPUs (and the scheduler's timers) active
        // while the one-shot tasks churn through exit.
        b = b.add_task("hog_a", 0, hog()).add_task("hog_b", 0, hog());
        for i in 0..ntasks {
            b = b.task(oneshot(i + 3, (i as u64 * 40_000) % 20_000_000, 200_000));
        }
        let trace = Simulator::new(make(4)).run(b.duration_ms(60).build());

        assert!(
            !trace.has_error(),
            "{name}: mass exit surfaced an error: {:?}",
            trace.exit_kind()
        );
        assert!(
            count(&trace, |k| matches!(k, TraceKind::TaskCompleted { .. })) > ntasks as usize / 2,
            "{name}: too few tasks exited to exercise the free path"
        );
    }
}

// ---------------------------------------------------------------------------
// (4) Cgroup operations during task lifecycle transitions.
// ---------------------------------------------------------------------------

/// Interleave the per-cgroup context lifecycle with the per-task lifecycle:
/// tasks live inside cgroups, cgroups are created and destroyed at runtime,
/// tasks migrate between cgroups, and one-shot tasks exit throughout. This
/// stresses cgroup ctx alloc/free (`cgroup_init`/`cgroup_exit`) racing task
/// `init_task`/`exit_task`/`cgroup_move`. Runs on schedulers with cgroup
/// support (simple, lavd).
#[test]
fn cgroup_ops_during_task_lifecycle() {
    let _lock = common::setup_test();

    for (name, make) in [
        (
            "simple",
            (|_n| DynamicScheduler::simple()) as fn(u32) -> DynamicScheduler,
        ),
        ("lavd", DynamicScheduler::lavd),
    ] {
        let mut b = Scenario::builder()
            .cpus(4)
            .no_watchdog()
            .cgroup("cg_a", &[CpuId(0), CpuId(1)])
            .cgroup("cg_b", &[CpuId(2), CpuId(3)]);

        // Long-lived tasks in each cgroup.
        b = b
            .add_task_in_cgroup("a_hog", 0, hog(), "cg_a")
            .add_task_in_cgroup("b_hog", 0, hog(), "cg_b");

        // Many one-shot tasks in cgroups that exit throughout the run.
        for i in 0..120i32 {
            let cg = if i % 2 == 0 { "cg_a" } else { "cg_b" };
            b = b.add_task_in_cgroup(
                &format!("cgos{i}"),
                0,
                TaskBehavior {
                    phases: vec![Phase::Run(200_000)],
                    repeat: RepeatMode::Once,
                },
                cg,
            );
        }

        // Runtime cgroup churn interleaved with the task lifecycle.
        b = b
            .cgroup_create_at("cg_c", None, Some(&[CpuId(0)]), 5_000_000)
            .cgroup_migrate(Pid(1), "cg_a", "cg_b", 10_000_000)
            .cgroup_migrate(Pid(2), "cg_b", "cg_a", 15_000_000)
            .cgroup_destroy_at("cg_c", 25_000_000);

        let trace = Simulator::new(make(4)).run(b.duration_ms(40).build());

        assert!(
            !trace.has_error(),
            "{name}: cgroup lifecycle churn surfaced an error: {:?}",
            trace.exit_kind()
        );
        assert!(
            !trace.events().is_empty(),
            "{name}: cgroup lifecycle run produced no events"
        );
    }
}

// ---------------------------------------------------------------------------
// (5) Map operations at boundary sizes.
// ---------------------------------------------------------------------------

/// Push the SDT task-local-storage hash table toward high load. With ~5 000
/// live tasks the 16 384-slot table sits near a third full, so `init_task`'s
/// open-addressing insert and every per-callback `scx_task_data` lookup walk
/// real probe chains. Boundary behavior must remain correct (no error/crash).
#[test]
fn sdt_map_near_capacity() {
    let _lock = common::setup_test();
    let ntasks = 5000;

    for (name, make) in [
        (
            "simple",
            (|_n| DynamicScheduler::simple()) as fn(u32) -> DynamicScheduler,
        ),
        ("lavd", DynamicScheduler::lavd),
    ] {
        let mut b = Scenario::builder().cpus(4).no_watchdog();
        for _ in 0..ntasks {
            b = b.add_task("t", 0, hog());
        }
        let trace = Simulator::new(make(4)).run(b.duration_ms(10).build());

        assert!(
            !trace.has_error(),
            "{name}: {ntasks}-task SDT-boundary run surfaced an error: {:?}",
            trace.exit_kind()
        );
        assert!(
            !trace.events().is_empty(),
            "{name}: SDT-boundary run produced no events"
        );
    }
}

/// Create many cgroups up to a configured ceiling, then attempt to exceed it.
/// Within the limit the cgroup map/context allocations must all succeed; going
/// over must fail *gracefully* (the engine's `ErrorCgroupExhausted`, or a
/// clean Normal exit if the scheduler declines the extra cgroups) — never a
/// buffer overrun or crash.
#[test]
fn cgroup_map_at_and_over_limit() {
    let _lock = common::setup_test();
    let limit = 64u32;

    // (a) Exactly at the limit: everything succeeds cleanly.
    {
        let mut b = Scenario::builder().cpus(2).no_watchdog().max_cgroups(limit);
        b = b.add_task("hog", 0, hog());
        // limit - 1 runtime cgroups (root occupies one slot).
        for i in 0..(limit - 1) {
            b = b.cgroup_create_at(
                &format!("cg{i}"),
                None,
                Some(&[CpuId(0)]),
                1_000_000 + i as u64 * 100_000,
            );
        }
        let trace = Simulator::new(DynamicScheduler::lavd(2)).run(b.duration_ms(20).build());
        assert!(
            !trace.has_error(),
            "at-limit cgroup creation should succeed cleanly, got {:?}",
            trace.exit_kind()
        );
    }

    // (b) Over the limit: must be handled gracefully, not corrupt memory.
    {
        let mut b = Scenario::builder().cpus(2).no_watchdog().max_cgroups(limit);
        b = b.add_task("hog", 0, hog());
        for i in 0..(limit * 2) {
            b = b.cgroup_create_at(
                &format!("cg{i}"),
                None,
                Some(&[CpuId(0)]),
                1_000_000 + i as u64 * 50_000,
            );
        }
        let trace = Simulator::new(DynamicScheduler::lavd(2)).run(b.duration_ms(20).build());
        // Acceptable outcomes: a clean run (scheduler refused extras via
        // -ENOMEM) or the engine's explicit exhaustion error. Anything that
        // reaches this assertion already proves no crash / memory corruption.
        let ok = matches!(trace.exit_kind(), ExitKind::Normal)
            || matches!(trace.exit_kind(), ExitKind::ErrorCgroupExhausted { .. });
        assert!(
            ok,
            "over-limit cgroup creation was not handled gracefully: {:?}",
            trace.exit_kind()
        );
    }
}
