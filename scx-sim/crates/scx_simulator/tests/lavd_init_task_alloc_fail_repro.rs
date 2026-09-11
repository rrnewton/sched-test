//! Production-crash reproduction: scx GitHub #3564 —
//! `scx_lavd crashes with "task_ctx_stor first lookup failed"`.
//!
//! # The production crash
//!
//! Reported upstream at <https://github.com/sched-ext/scx/issues/3564>:
//! on a running system, `scx_lavd` intermittently died with
//!
//! ```text
//! EXIT: scx_bpf_error (src/bpf/main.bpf.c: task_ctx_stor first lookup failed)
//!   bpf_prog_..._lavd_init_task+0x77
//!   bpf__sched_ext_ops_init_task+0x4b
//!   scx_fork+0xc3
//!   copy_process+0xe64
//! ```
//!
//! i.e. during `fork()`, `lavd_init_task` called `scx_task_alloc(p)`, the
//! BPF-arena / SDT task-storage allocation failed and returned NULL, so
//! lavd raised `scx_bpf_error("task_ctx_stor first lookup failed")` and
//! returned `-ENOMEM` (exit kind 1025). See
//! `scx/scheds/rust/scx_lavd/src/bpf/main.bpf.c` `lavd_init_task` ->
//! `scx_task_alloc(p)` -> `if (!taskc) scx_bpf_error("task_ctx_stor first
//! lookup failed")`.
//!
//! # How this test reproduces it
//!
//! scxsim runs the *real* compiled `lavd_init_task` against the same
//! `scx_task_alloc()` allocator (`csrc/sim_sdt_stubs.c`). We arm the
//! allocator's opt-in fault injector (`set_task_alloc_fail_pid`, default
//! off, RBC/determinism-neutral) so that `scx_task_alloc()` returns NULL
//! for our worker's PID — exactly the arena-exhaustion condition from the
//! field report. lavd's real error path then fires
//! `scx_bpf_error("task_ctx_stor first lookup failed")`, which the
//! simulator surfaces as `ExitKind::ErrorBpf`.
//!
//! This also exercises the engine's init-task failure surfacing: an
//! `ops.init_task` that returns non-zero is now reported as
//! `ExitKind::ErrorBpf` instead of panicking the harness (see
//! `safe/engine.rs`), which is what lets a production init-path crash be
//! reproduced as a first-class exit kind at all. Prior to that fix the
//! init loop asserted `rc == 0` and this reproducer was impossible.

use scx_simulator::*;

mod common;

/// A tiny single-worker lavd scenario used by both the armed (crash) and
/// unarmed (baseline) cases below, so the ONLY difference between them is
/// whether the fault injector is armed.
fn tiny_lavd_scenario(worker_pid: Pid) -> Scenario {
    Scenario::builder()
        .cpus(2)
        .task(TaskDef {
            name: "yes".into(),
            pid: worker_pid,
            nice: 0,
            behavior: TaskBehavior {
                phases: vec![Phase::Run(5_000_000)], // 5ms
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
        })
        .duration_ms(50)
        .build()
}

/// scx #3564: arming the allocator fault for the worker's PID reproduces
/// `scx_bpf_error("task_ctx_stor first lookup failed")` from lavd's real
/// `lavd_init_task` path, surfaced as `ExitKind::ErrorBpf`.
#[test]
fn test_lavd_init_task_alloc_fail_reproduces_task_ctx_stor_error() {
    let _lock = common::setup_test();

    let worker = Pid(1);
    let scenario = tiny_lavd_scenario(worker);

    // Arm the arena-exhaustion fault for the worker's init_task.
    set_task_alloc_fail_pid(worker.0);
    let trace = Simulator::new(DynamicScheduler::lavd(2)).run(scenario);
    // Disarm BEFORE asserting so a failed assertion can't leak the fault
    // flag into other tests sharing this process.
    set_task_alloc_fail_pid(0);

    match trace.exit_kind() {
        ExitKind::ErrorBpf(msg) => {
            assert!(
                msg.contains("task_ctx_stor first lookup failed"),
                "expected lavd's #3564 error string, got ErrorBpf({msg:?})"
            );
        }
        other => panic!(
            "expected ExitKind::ErrorBpf(\"...task_ctx_stor first lookup failed...\") \
             reproducing scx #3564, got {other:?}"
        ),
    }
}

/// Guard: the fault injector is strictly opt-in. The identical scenario,
/// run WITHOUT arming the fault, completes normally — proving the knob
/// does not perturb default (production) behavior.
#[test]
fn test_unarmed_baseline_completes_normally() {
    let _lock = common::setup_test();

    // Defensive: ensure disarmed even if a prior test in this binary left
    // it set (tests are serialized by SIM_LOCK, so this is race-free).
    set_task_alloc_fail_pid(0);

    let worker = Pid(1);
    let scenario = tiny_lavd_scenario(worker);
    let trace = Simulator::new(DynamicScheduler::lavd(2)).run(scenario);

    assert!(
        !trace.has_error(),
        "unarmed baseline should not error, got {:?}",
        trace.exit_kind()
    );
    assert_eq!(
        trace.exit_kind(),
        &ExitKind::Normal,
        "unarmed baseline should exit Normal"
    );
}
