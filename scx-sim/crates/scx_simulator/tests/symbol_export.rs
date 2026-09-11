//! Characterization test for the dlopen kfunc-export link contract.
//!
//! Scheduler `.so` files resolve a set of symbols from the loading binary's
//! dynamic symbol table at dlopen time (each `.so` carries them as `UND`);
//! `build.rs` forces them into the binary via `-rdynamic` + `-Wl,--undefined`
//! and exports the exact set as `SCXSIM_EXPORTED_SYMS` (single source of
//! truth). If a required symbol is missing from the process image, a
//! scheduler `.so` SIGSEGVs at its first kfunc call.
//!
//! SCOPE (what this test actually guards): it asserts every symbol in
//! `SCXSIM_EXPORTED_SYMS` resolves in the process image. Of those, only the
//! symbols Rust never references itself are held in SOLELY by `--undefined`
//! (today: scx_test_map_lookup_elem, scx_arena_subprog_init, sim_arena_buf,
//! sim_arena_offset) — these can silently vanish when the C build moves to
//! `cc::Build`, so this is their only guard. The rest are also pulled in
//! by a live Rust `#[no_mangle]`/reference and exported via `-rdynamic`
//! regardless, so dropping their `--undefined` would NOT fail here. The
//! complementary half of the contract — per-scheduler Rust kfuncs that only
//! lavd/cosmos/mitosis/tickless call (held in by `-rdynamic` + a live Rust
//! ref) — is exercised by the per-scheduler runtime suites (loader_path +
//! lavd/cosmos/mitosis/tickless load+run each `.so`, which crash on any
//! unresolved symbol); keep those suites if pruning tests.
//!
//! NOTE: `build.rs`'s link-args (and the C static libs) only apply to a test
//! binary that actually LINKS `scx_simulator`. A bare test binary that never
//! references the crate links as a downstream and lacks these symbols
//! entirely — the same `cargo:rustc-link-arg` non-transitivity an external
//! embedder (ktstr) must handle by re-emitting the set itself. We run a real
//! simulation to force the representative linkage.

use libloading::os::unix::Library;
use scx_simulator::*;

mod common;

/// Comma-separated `--undefined` symbol set emitted by `build.rs`.
const EXPORTED_SYMS: &str = env!("SCXSIM_EXPORTED_SYMS");

#[test]
fn test_exported_symbols_resolve_in_process_image() {
    let _lock = common::setup_test();

    // Run a real simulation through a dlopen'd scheduler. This (a) forces the
    // test binary to link scx_simulator + its C static libs + build.rs
    // link-args, and (b) exercises kfuncs at `.so` load, so an absent
    // binary-resolved symbol would already crash before the explicit check.
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
        })
        .duration_ms(50)
        .build();
    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario);
    assert_eq!(
        trace.exit_kind(),
        &ExitKind::Normal,
        "sim did not complete normally"
    );

    // Explicitly assert every required export resolves in the process image.
    let syms: Vec<&str> = EXPORTED_SYMS.split(',').filter(|s| !s.is_empty()).collect();
    assert!(
        !syms.is_empty(),
        "SCXSIM_EXPORTED_SYMS is empty — build.rs did not emit the export set"
    );

    // `Library::this()` is `dlopen(NULL, ...)`: resolves against the running
    // program image first, which is where the `--undefined` C symbols live.
    let this = Library::this();
    let mut missing = Vec::new();
    for sym in &syms {
        let mut name = sym.as_bytes().to_vec();
        name.push(0); // dlsym needs a NUL-terminated name
                      // SAFETY: we only take the symbol's address and never call through it
                      // or assume a type; a lookup error means the symbol is absent.
        let resolved = unsafe { this.get::<*const std::ffi::c_void>(&name).is_ok() };
        if !resolved {
            missing.push(*sym);
        }
    }

    assert!(
        missing.is_empty(),
        "exported symbols missing from the process image (a dropped \
         build.rs --undefined would SIGSEGV a scheduler .so at first kfunc \
         call): {missing:?}"
    );
}
