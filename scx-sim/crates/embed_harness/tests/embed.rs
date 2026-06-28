//! End-to-end proof of the dlopen kfunc-export link contract for a DOWNSTREAM
//! embedder.
//!
//! `embed_harness` depends on `scx_simulator` (so its test binary links the C
//! static libs that DEFINE the exported kfunc/SDT/arena symbols, and is a
//! downstream crate that does NOT inherit scx_simulator's `-rdynamic` /
//! `-Wl,--undefined` link args -- those are non-transitive). The harness'
//! `build.rs` re-emits that set from `scxsim_build::EXPORTED_SYMS`. This test
//! loads the harness-built `libscx_simple.so` through
//! `DynamicScheduler::load_with_definition` (which dlopens with RTLD_NOW, so all
//! of the `.so`'s undefined symbols are resolved eagerly at load) and runs a
//! real 1-CPU simulation. `simple.so` UND-references 3 of the 13 EXPORTED_SYMS
//! (scx_test_map_lookup_elem, sim_arena_buf, sim_arena_offset); `sim_arena_offset`
//! is held in SOLELY by the re-emitted `--undefined`, so reaching
//! `ExitKind::Normal` proves the re-emission is load-bearing and non-transitive
//! for the simple-reachable subset. The remaining EXPORTED_SYMS (e.g.
//! scx_arena_subprog_init, the scx_task_* SDT path) are exercised by
//! scx_simulator's own per-scheduler runtime suites, which load the schedulers
//! that reference them. (Removing the re-emission from build.rs makes the load
//! fail with `undefined symbol: sim_arena_offset` -- the implicit negative
//! control, verified once manually.)

// One import: the curated embed surface (re-exports scxsim_build::SchedulerDefinition).
use scx_simulator::prelude::*;

#[test]
fn embedder_built_simple_so_loads_and_runs() {
    let so = format!("{}/libscx_simple.so", env!("HARNESS_SO_DIR"));

    // An embedder constructs the definition from scratch (not the bundled
    // standalone_definitions): `simple`'s source is local, so it overrides
    // new()'s strip_const/scx_bpf_dir defaults.
    let mut def = SchedulerDefinition::new("simple");
    def.strip_const = false;
    def.scx_bpf_dir = false;

    let sched = DynamicScheduler::load_with_definition(&so, &def, 1);
    let scenario = Scenario::builder()
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
        })
        .duration_ms(50)
        .build();
    let trace = Simulator::new(sched).run(scenario);
    assert_eq!(
        trace.exit_kind(),
        &ExitKind::Normal,
        "embedder-built simple.so did not run to normal completion -- without the \
         re-emitted link args the RTLD_NOW load fails on an undefined symbol"
    );
}
