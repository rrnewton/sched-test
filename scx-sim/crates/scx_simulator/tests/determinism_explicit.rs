//! Determinism characterization with FULLY-EXPLICIT config.
//!
//! `common/mod.rs::test_determinism` proves same-seed -> same-trace, but it
//! runs both traces in one process+env and lets seed/noise/overhead default
//! from `SCX_SIM_*` process env. That misses a systematic shift present in
//! both runs (the failure mode a loader/C-build/lowering change introduces)
//! AND the env-config leakage that breaks isolation when ktstr embeds the
//! simulator in-process. These tests pin the embed-critical property: with
//! every knob set on the builder, the trace is a function of (scheduler,
//! scenario) ONLY — immune to `SCX_SIM_*`.

use scx_simulator::*;

mod common;

/// A scenario with every determinism-relevant knob set explicitly, so it
/// never consults `SCX_SIM_*` process env. Two equal-ish tasks on one CPU
/// give enough scheduling activity to expose any nondeterminism.
fn explicit_scenario(seed: u32) -> Scenario {
    Scenario::builder()
        .cpus(1)
        .instant_timing()
        .seed(seed)
        .noise(false)
        .overhead(false)
        .sched_overhead_rbc_ns(None)
        .task(TaskDef {
            name: "t1".into(),
            pid: Pid(1),
            nice: 0,
            behavior: TaskBehavior {
                phases: vec![Phase::Run(10_000_000)],
                repeat: RepeatMode::Forever,
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
        })
        .task(TaskDef {
            name: "t2".into(),
            pid: Pid(2),
            nice: -3,
            behavior: TaskBehavior {
                phases: vec![Phase::Run(10_000_000)],
                repeat: RepeatMode::Forever,
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
        })
        .duration_ms(50)
        .build()
}

/// `SCX_SIM_*` vars the builder consults for defaults, each paired with a
/// HOSTILE value that would change the trace if it leaked past the explicit
/// setters. `Scenario::builder()` bakes `NoiseConfig::from_env` (SCX_SIM_NOISE,
/// SCX_SIM_INSTANT_TIMING, SCX_SIM_RUN_JITTER_CV_PPM), `OverheadConfig::from_env`
/// (SCX_SIM_OVERHEAD + 9 per-parameter `*_NS`), and `seed_from_env`
/// (SCX_SIM_SEED). `.noise(false)`/`.overhead(false)` set only `.enabled`; the
/// per-param `*_NS` fields RETAIN these values, so the trace is immune only
/// because every consumer gates on `.enabled` — exercising the full set proves
/// that gate-dominance rather than just the top-level toggles (a regression
/// that moved an overhead read outside the `.enabled` gate would slip past a
/// 4-var check). `SCX_SIM_RBC_NS` is intentionally omitted: it is NOT
/// builder-consulted — only `load_rtapp` reads it — and this scenario builds
/// via the builder, not rt-app.
const HOSTILE_SIM_ENV: &[(&str, &str)] = &[
    ("SCX_SIM_SEED", "987654321"),
    ("SCX_SIM_NOISE", "1"),
    ("SCX_SIM_INSTANT_TIMING", "0"),
    ("SCX_SIM_RUN_JITTER_CV_PPM", "1000000"),
    ("SCX_SIM_OVERHEAD", "1"),
    ("SCX_SIM_VOL_CSW_NS", "999999"),
    ("SCX_SIM_INVOL_CSW_NS", "999999"),
    ("SCX_SIM_CSW_JITTER_NS", "999999"),
    ("SCX_SIM_IPI_NS", "999999"),
    ("SCX_SIM_DSQ_CONSUME_NS", "999999"),
    ("SCX_SIM_WAKEUP_FLOOR_NS", "999999"),
    ("SCX_SIM_WAKEUP_JITTER_NS", "999999"),
    ("SCX_SIM_MIGRATION_PENALTY_NS", "999999"),
    ("SCX_SIM_CROSS_LLC_PENALTY_NS", "999999"),
];

fn clear_sim_env() {
    for (k, _) in HOSTILE_SIM_ENV {
        std::env::remove_var(k);
    }
}

fn assert_traces_eq(a: &Trace, b: &Trace, ctx: &str) {
    let (ea, eb) = (a.events(), b.events());
    assert_eq!(
        ea.len(),
        eb.len(),
        "{ctx}: event count differs: {} vs {}",
        ea.len(),
        eb.len()
    );
    for (i, (e1, e2)) in ea.iter().zip(eb.iter()).enumerate() {
        assert_eq!(e1.time_ns, e2.time_ns, "{ctx}: event {i} time_ns differs");
        assert_eq!(e1.cpu, e2.cpu, "{ctx}: event {i} cpu differs");
        assert_eq!(e1.kind, e2.kind, "{ctx}: event {i} kind differs");
    }
}

/// Golden determinism: an explicit-config scenario produces a byte-identical
/// event stream on rerun (cross-instance). Unlike the env-defaulted variant,
/// this is reproducible regardless of the ambient `SCX_SIM_*` environment.
#[test]
fn test_explicit_config_determinism() {
    let _lock = common::setup_test();
    let t1 = Simulator::new(DynamicScheduler::simple()).run(explicit_scenario(42));
    let t2 = Simulator::new(DynamicScheduler::simple()).run(explicit_scenario(42));
    assert_eq!(t1.exit_kind(), &ExitKind::Normal);
    assert_eq!(t2.exit_kind(), &ExitKind::Normal);
    assert_traces_eq(&t1, &t2, "explicit-config rerun");
}

/// Embed isolation: an explicit-config scenario is immune to `SCX_SIM_*`.
/// Setting hostile env values that WOULD change the trace if they leaked past
/// the builder's explicit setters must not perturb the result. Env is
/// restored before the assertion so a failure cannot contaminate other tests.
#[test]
fn test_explicit_config_immune_to_env() {
    let _lock = common::setup_test();

    // This test mutates process-global env; it relies on nextest's
    // process-per-test isolation (the mandated runner). Under `cargo test`
    // (one shared process) a sibling test reading `SCX_SIM_*` on another
    // thread could observe the transient hostile values — SIM_LOCK serializes
    // sim tests but not pure from_env/parser tests. Do not run under `cargo test`.
    clear_sim_env();
    let clean = Simulator::new(DynamicScheduler::simple()).run(explicit_scenario(42));

    for (k, v) in HOSTILE_SIM_ENV {
        std::env::set_var(k, v);
    }
    let dirty = Simulator::new(DynamicScheduler::simple()).run(explicit_scenario(42));

    // Restore env BEFORE asserting so a failure cannot contaminate siblings.
    clear_sim_env();

    assert_eq!(clean.exit_kind(), &ExitKind::Normal);
    assert_eq!(dirty.exit_kind(), &ExitKind::Normal);
    assert_traces_eq(
        &clean,
        &dirty,
        "explicit config must override SCX_SIM_* env (embed isolation)",
    );
}
