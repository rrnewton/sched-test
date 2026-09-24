//! End-to-end proof of the dlopen host-export link contract for a DOWNSTREAM
//! embedder.
//!
//! `embed_harness` depends on `scx_simulator`, so its test binary is linked
//! against the definitions of every `scxsim_build::HOST_EXPORTS` symbol, and it
//! is a downstream crate that does NOT inherit scx_simulator's `-rdynamic` /
//! `-Wl,--undefined` link args (cargo does not pass link arguments on to
//! dependents). The harness' `build.rs` emits them itself with
//! `scxsim_build::emit_host_link_args()`. This test loads the harness-built
//! `libscx_simple.so` through `DynamicScheduler::load_with_definition`, which
//! first checks that every host export is in the process's global symbol scope
//! (refusing with `LoadError::HostSymbolsNotExported` otherwise) and then
//! dlopens with RTLD_NOW, and runs a real 1-CPU simulation. Reaching
//! `ExitKind::Normal` proves the emitted args reached this binary; the
//! `embed_unexported` crate is the negative control, the same dependency
//! without the build-script call, which the loader must refuse.

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
            thread_group_leader: None,
            uid: Uid(0),
            gid: Gid(0),
            fork_cpu: None,
        })
        .duration_ms(50)
        .build();
    let trace = Simulator::new(sched).run(scenario);
    assert_eq!(
        trace.exit_kind(),
        &ExitKind::Normal,
        "embedder-built simple.so did not run to normal completion"
    );
}
