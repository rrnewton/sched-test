//! Characterization tests for the path-driven scheduler loader.
//!
//! These pin the contract that the path-agnostic loader API
//! (`discover_schedulers` + `DynamicScheduler::load`) behaves
//! identically to the compile-time `env!(SCHEDULER_SO_DIR)`-baked named
//! constructor. A later change migrates the named constructors off `env!` to runtime
//! path resolution (`simple()` is expected to delegate to `load()` with a
//! runtime-resolved path); these tests must keep passing across that
//! refactor, so any divergence in the loader seam fails loudly here.

use std::path::Path;

use scx_simulator::*;

mod common;

/// The build script bakes the scheduler `.so` output directory into this
/// compile-time env var (`build.rs` emits `cargo:rustc-env=SCHEDULER_SO_DIR`).
const SO_DIR: &str = env!("SCHEDULER_SO_DIR");

/// A small, fully-explicit scenario: one CPU-bound task on one CPU under
/// instant timing. Every knob is set on the builder, so the resulting trace
/// is a function of (scheduler, scenario) only and does not depend on any
/// `SCX_SIM_*` process environment.
fn fixed_scenario() -> Scenario {
    Scenario::builder()
        .cpus(1)
        .instant_timing()
        .task(TaskDef {
            name: "worker".into(),
            pid: Pid(1),
            nice: 0,
            behavior: TaskBehavior {
                phases: vec![Phase::Run(5_000_000)],
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
        })
        .duration_ms(100)
        .build()
}

fn names(found: &[SchedulerInfo]) -> Vec<String> {
    found.iter().map(|s| s.name.clone()).collect()
}

/// `discover_schedulers` finds the built `libscx_simple.so`, derives its
/// short name by stripping the `libscx_`/`.so` affixes, and returns the set
/// sorted by derived name.
#[test]
fn test_discover_schedulers_finds_simple() {
    let _lock = common::setup_test();
    let found = discover_schedulers(Path::new(SO_DIR));
    assert!(
        !found.is_empty(),
        "discover_schedulers found no libscx_*.so in {SO_DIR}"
    );

    let simple = found
        .iter()
        .find(|s| s.name == "simple")
        .unwrap_or_else(|| panic!("no 'simple' scheduler among {:?}", names(&found)));
    // Derived name is the affix-stripped form, not the filename.
    assert!(
        !simple.name.contains("libscx_"),
        "name retains prefix: {}",
        simple.name
    );
    assert!(
        !simple.name.contains(".so"),
        "name retains suffix: {}",
        simple.name
    );
    assert!(
        simple.path.ends_with("libscx_simple.so"),
        "unexpected path for simple: {:?}",
        simple.path
    );

    // Results are sorted by derived name.
    let got = names(&found);
    let mut want = got.clone();
    want.sort();
    assert_eq!(
        got, want,
        "discover_schedulers result is not sorted by name"
    );
}

/// The path-driven `DynamicScheduler::load` produces a byte-identical trace
/// to the `env!(SCHEDULER_SO_DIR)`-baked `DynamicScheduler::simple()` over a
/// fixed scenario. `simple()` is literally `load("{env}/libscx_simple.so",
/// "simple", 1)` today; this pins that equivalence so the env!->runtime
/// pivot cannot silently change behavior. The two simulators run
/// sequentially under one `SIM_LOCK` guard; each is dropped (dlclose -> C
/// global state reset) before the next loads.
#[test]
fn test_load_matches_named_ctor() {
    let _lock = common::setup_test();

    let via_ctor = Simulator::new(DynamicScheduler::simple()).run(fixed_scenario());
    let so = format!("{SO_DIR}/libscx_simple.so");
    let via_load = Simulator::new(DynamicScheduler::load(&so, "simple", 1)).run(fixed_scenario());

    assert_eq!(
        via_ctor.exit_kind(),
        &ExitKind::Normal,
        "ctor run did not complete normally"
    );
    assert_eq!(
        via_load.exit_kind(),
        &ExitKind::Normal,
        "load run did not complete normally"
    );

    let a = via_ctor.events();
    let b = via_load.events();
    assert_eq!(
        a.len(),
        b.len(),
        "event count differs: ctor={} load={}",
        a.len(),
        b.len()
    );
    for (i, (e1, e2)) in a.iter().zip(b.iter()).enumerate() {
        assert_eq!(e1.time_ns, e2.time_ns, "event {i}: time_ns differs");
        assert_eq!(e1.cpu, e2.cpu, "event {i}: cpu differs");
        assert_eq!(e1.kind, e2.kind, "event {i}: kind differs");
    }
}
