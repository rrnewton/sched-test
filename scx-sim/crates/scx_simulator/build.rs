use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_dir: PathBuf = env::var("CARGO_MANIFEST_DIR").unwrap().into();
    // Workspace root is two levels up from crates/scx_simulator
    let workspace_dir = manifest_dir.join("../..").canonicalize().unwrap();
    // Repo root is one level up from the workspace (scx-sim/)
    let root_dir = workspace_dir.join("..").canonicalize().unwrap();
    let out_dir: PathBuf = env::var("OUT_DIR").unwrap().into();

    let coverage = env::var("SCX_SIM_COVERAGE").as_deref() == Ok("1");

    let include_paths: Vec<PathBuf> = vec![
        // Our own C source directory (at workspace root)
        workspace_dir.join("csrc"),
        // Existing unit test infrastructure
        root_dir.join("lib/scxtest"),
        // Scheduler include paths
        root_dir.join("scheds/include"),
        root_dir.join("scheds/include/lib"),
        root_dir.join("scheds/vmlinux"),
        root_dir.join("scheds/vmlinux/arch/x86"),
        root_dir.join("scheds/include/bpf-compat"),
        // libbpf headers
        env::var("DEP_BPF_INCLUDE")
            .expect("libbpf-sys include must be available")
            .into(),
    ];

    // Common compiler: BPF scheduler code compiled as userspace C has
    // inherently unused parameters (fixed BPF ops signatures) and unknown
    // attributes (preserve_access_index from vmlinux.h).
    let compiler = env::var("BPF_CLANG").unwrap_or_else(|_| "clang".into());

    // DRY helper: apply common config to a cc::Build
    let configure_build = |build: &mut cc::Build| {
        build
            .compiler(&compiler)
            .define("SCX_BPF_UNITTEST", None)
            .includes(&include_paths)
            .flag("-Wno-unused-parameter")
            .flag("-Wno-unknown-attributes");
        if coverage {
            build
                .flag("-fprofile-instr-generate")
                .flag("-fcoverage-mapping");
        }
    };

    // ---------------------------------------------------------------
    // Static libraries (linked into the main binary)
    // ---------------------------------------------------------------

    // Build the scxtest support library (map emulation, cpumask, test assert).
    // NOTE: overrides.c is NOT included here — it goes into each .so instead.
    // The .so's weak stubs from overrides.c are overridden by the main binary's
    // strong kfunc symbols and SDT functions exported via -rdynamic.
    let mut scxtest = cc::Build::new();
    scxtest.files([
        root_dir.join("lib/scxtest/scx_test.c"),
        root_dir.join("lib/scxtest/scx_test_map.c"),
        root_dir.join("lib/scxtest/scx_test_cpumask.c"),
    ]);
    configure_build(&mut scxtest);
    scxtest.compile("scxtest");

    // Build the task_struct accessor library
    let mut sim_task = cc::Build::new();
    sim_task.file(workspace_dir.join("csrc/sim_task.c"));
    configure_build(&mut sim_task);
    sim_task.compile("sim_task");

    // Build the SDT / arena per-task storage stubs.
    // Provides strong definitions of scx_task_alloc/data/free that override
    // the __weak stubs in overrides.c. Linked into the main binary for unit
    // tests and exported via -rdynamic for .so schedulers.
    // Build the SDT (per-task data) stubs and deterministic arena allocator.
    // sim_sdt_stubs.c and sim_arena.c are compiled together so they share
    // the arena storage (sim_arena_buf/sim_arena_offset).
    let mut sim_sdt = cc::Build::new();
    sim_sdt.file(workspace_dir.join("csrc/sim_sdt_stubs.c"));
    sim_sdt.file(workspace_dir.join("csrc/sim_arena.c"));
    configure_build(&mut sim_sdt);
    sim_sdt.compile("sim_sdt_stubs");

    // Build the cgroup CSS iterator support.
    // Provides sim_css_next() and related functions for bpf_for_each(css, ...).
    let mut sim_cgroup = cc::Build::new();
    sim_cgroup.file(workspace_dir.join("csrc/sim_cgroup.c"));
    configure_build(&mut sim_cgroup);
    sim_cgroup.compile("sim_cgroup");

    // Build the userspace scx_atq_* shim for Phase 1 BPF infra scale-up
    // item 7 (tg `scxsim-bpf-infra-scale-up-phase1`). Provides the
    // arena-task-queue API surface that Phase 2's compiled-in
    // `scx/lib/cgroup_bw.bpf.c` calls (insert_vtime, pop, peek,
    // nr_queued, cancel, etc.). Lives in the main binary and is
    // exported via -rdynamic so scheduler `.so` files can resolve
    // `scx_atq_*` references at dlopen time.
    let mut sim_atq = cc::Build::new();
    sim_atq.file(workspace_dir.join("csrc/sim_atq.c"));
    configure_build(&mut sim_atq);
    sim_atq.compile("sim_atq");

    // ---------------------------------------------------------------
    // Shared libraries (.so) for schedulers — built via Makefile
    // ---------------------------------------------------------------

    let scheduler_dir = if coverage {
        out_dir.join("schedulers_cov")
    } else {
        out_dir.join("schedulers")
    };
    let bpf_include = env::var("DEP_BPF_INCLUDE").expect("libbpf-sys include must be available");

    let mut make = Command::new("make");
    make.arg("-C")
        .arg(workspace_dir.join("schedulers"))
        .arg(format!("BUILD_DIR={}", scheduler_dir.display()))
        .arg(format!("SIMULATOR_DIR={}", workspace_dir.display()))
        .arg(format!("ROOT_DIR={}", root_dir.display()))
        .arg(format!("BPF_INCLUDE={bpf_include}"))
        .arg(format!("CC={compiler}"));
    if coverage {
        make.arg("SCX_SIM_COVERAGE=1");
    }
    // Phase 2 (tg `compile-scx-cgroup-bw-library-into-scxsim-phase2`):
    // forward the env var into the Make invocation so
    // `schedulers/Makefile` can `ifeq ($(SCXSIM_PHASE2_REAL_CGROUP_BW),1)`
    // and inject `-DSCXSIM_PHASE2_REAL_CGROUP_BW=1` into the LAVD wrapper
    // compile. The `cargo:rerun-if-env-changed=...` below ensures
    // build.rs re-runs when the env var flips.
    if let Ok(v) = env::var("SCXSIM_PHASE2_REAL_CGROUP_BW") {
        make.arg(format!("SCXSIM_PHASE2_REAL_CGROUP_BW={}", v));
    }
    println!("cargo:rerun-if-env-changed=SCXSIM_PHASE2_REAL_CGROUP_BW");
    let status = make.status().expect("failed to run make");

    assert!(status.success(), "scheduler Makefile failed: exit {status}");

    // ---------------------------------------------------------------
    // Linker flags for the main binary
    // ---------------------------------------------------------------

    // Export all symbols so .so can resolve kfuncs and scxtest functions
    println!("cargo:rustc-link-arg=-rdynamic");

    // Force scxtest map functions into the binary even though Rust doesn't
    // reference them directly — the .so's scheduler code calls them via
    // the bpf_map_lookup_elem macro.
    println!("cargo:rustc-link-arg=-Wl,--undefined=scx_test_map_lookup_elem");
    // Also export scx_test_map_clear_all for deterministic re-runs.
    // This clears the thread-local map registry between simulation runs.
    println!("cargo:rustc-link-arg=-Wl,--undefined=scx_test_map_clear_all");

    // Force SDT (per-task storage) functions into the binary. The .so files
    // do not include sim_sdt_stubs.c — they resolve these from the main binary.
    // This ensures there's only one copy of the SDT hash table, allowing
    // sim_sdt_reset() to work correctly for deterministic re-runs.
    println!("cargo:rustc-link-arg=-Wl,--undefined=scx_task_init");
    println!("cargo:rustc-link-arg=-Wl,--undefined=scx_task_alloc");
    println!("cargo:rustc-link-arg=-Wl,--undefined=scx_task_data");
    println!("cargo:rustc-link-arg=-Wl,--undefined=scx_task_free");
    println!("cargo:rustc-link-arg=-Wl,--undefined=scx_arena_subprog_init");

    // Force e9_preempt_yield and E9_SHARED_RBC into the binary so the
    // e9patch-instrumented .so files can resolve them via -rdynamic.
    println!("cargo:rustc-link-arg=-Wl,--undefined=e9_preempt_yield");
    println!("cargo:rustc-link-arg=-Wl,--undefined=E9_SHARED_RBC");
    // Arena allocator symbols used by both sim_sdt_stubs (main binary) and
    // sim_bpf_stubs (.so) — ensure they're exported via -rdynamic.
    println!("cargo:rustc-link-arg=-Wl,--undefined=sim_arena_buf");
    println!("cargo:rustc-link-arg=-Wl,--undefined=sim_arena_offset");

    // sim_atq.c symbols (Phase 1 BPF infra scale-up item 7). No Rust code
    // references them directly today -- Phase 2's compiled-in
    // cgroup_bw.bpf.c is the consumer, and it lives in scheduler `.so`
    // files that resolve via dlopen + -rdynamic. Force the .o into the
    // binary so the symbols are present at .so load time.
    //
    // One `--undefined` forces the whole sim_atq.o translation unit; all
    // scx_atq_* symbols come along for the ride via static-linker
    // semantics. We pin scx_atq_create_internal because it's the only
    // entry point that a consumer can call without already holding an
    // atq pointer.
    println!("cargo:rustc-link-arg=-Wl,--undefined=scx_atq_create_internal");

    // Link the clang profile runtime when coverage is enabled.
    // This provides __llvm_profile_* symbols for the instrumented .so files.
    if coverage {
        let rt_dir_output = Command::new(&compiler)
            .arg("--print-runtime-dir")
            .output()
            .expect("failed to run clang --print-runtime-dir");
        assert!(
            rt_dir_output.status.success(),
            "clang --print-runtime-dir failed"
        );
        let rt_dir = String::from_utf8(rt_dir_output.stdout)
            .expect("non-UTF8 runtime dir")
            .trim()
            .to_string();
        // Detect the actual library name (may or may not have arch suffix)
        let profile_lib =
            if std::path::Path::new(&format!("{rt_dir}/libclang_rt.profile.a")).exists() {
                "clang_rt.profile"
            } else {
                "clang_rt.profile-x86_64"
            };
        println!("cargo:rustc-link-search=native={rt_dir}");
        println!("cargo:rustc-link-lib=static={profile_lib}");
    }

    // Expose scheduler .so directory to Rust code
    println!(
        "cargo:rustc-env=SCHEDULER_SO_DIR={}",
        scheduler_dir.display()
    );

    // Expose bpftrace script path for --bpf-trace option
    println!(
        "cargo:rustc-env=BPFTRACE_SCRIPT={}",
        workspace_dir.join("scripts/trace_scx_ops.bt").display()
    );

    // Rebuild triggers (relative to workspace root, which is ../../ from crate).
    //
    // The list MUST cover every directory whose contents the scheduler `.so`
    // build depends on. If a directory is omitted, swapping the scx submodule
    // SHA can silently leave a STALE `.so` in place: cargo's incremental
    // logic skips the build script, the Makefile is never re-invoked, and
    // the cached `.so` from the previous SHA is reused unchanged.
    //
    // Worked example (the bug this list exists to prevent): pre-fix, only
    // `scx_tickless/src/bpf` and `scx_cosmos/src/bpf` were watched. Swapping
    // the scx submodule between two SHAs that differed only in
    // `scheds/rust/scx_lavd/src/bpf/` produced a 0.06s no-op `cargo build`
    // and the stale `libscx_lavd.so` from the prior SHA was reused. The
    // workaround was `touch crates/scx_simulator/build.rs`. See:
    // - tg `official-scheduler-rebuild-action-replace-touch-build-rs-hack`
    // - experiments/lavd_cpubw_stalls_202604/CPU_BW_STALL_BUG_REPRODUCER_REPORT.md
    //
    // When a NEW scheduler is added under `scx-sim/schedulers/`, audit its
    // `wrapper.c` for `#include "../../scx/...` lines and any
    // `*_BPF_DIR := $(ROOT_DIR)/...` in its `config.mk`, and add the
    // corresponding directory to this list. Today's wrappers transitively
    // depend on the dirs below.
    let rerun_dirs: &[&str] = &[
        // scx-sim local source — Makefile + wrapper.c per scheduler
        "schedulers",
        "csrc",
    ];
    for d in rerun_dirs {
        println!("cargo:rerun-if-changed={}", workspace_dir.join(d).display());
    }
    let scx_rerun_dirs: &[&str] = &[
        // Test infrastructure (lives at sched-test root, not under scx submodule)
        "lib/scxtest",
        // scx submodule subtrees pulled in by wrapper.c per-scheduler #includes
        // and by the include_paths above. Watching each subtree forces a
        // rebuild whenever the submodule is swapped to a SHA that touched
        // those files.
        "scx/lib",                              // ravg.bpf.c, cgroup_bw.bpf.c, ...
        "scx/scheds/rust/scx_lavd/src/bpf",     // LAVD wrapper transitive include
        "scx/scheds/rust/scx_mitosis/src/bpf",  // mitosis wrapper transitive include
        "scx/scheds/rust/scx_cosmos/src/bpf",   // cosmos wrapper transitive include
        "scx/scheds/rust/scx_tickless/src/bpf", // tickless wrapper transitive include
        // scx submodule headers used by ALL schedulers via include_paths above
        "scx/scheds/include",
        "scx/scheds/vmlinux",
    ];
    for d in scx_rerun_dirs {
        println!("cargo:rerun-if-changed={}", root_dir.join(d).display());
    }
    println!("cargo:rerun-if-env-changed=SCX_SIM_COVERAGE");
}
