use std::env;
use std::path::PathBuf;
use std::process::Command;

use scxsim_build::{
    build_schedulers, cgroup_bw_new_api, resolve_scx_root, scx_include_paths,
    standalone_definitions, EXPORTED_SYMS,
};

fn main() {
    let manifest_dir: PathBuf = env::var("CARGO_MANIFEST_DIR").unwrap().into();
    // Workspace root is two levels up from crates/scx_simulator
    let workspace_dir = manifest_dir.join("../..").canonicalize().unwrap();
    // Repo root is one level up from the workspace (scx-sim/)
    let root_dir = workspace_dir.join("..").canonicalize().unwrap();
    let out_dir: PathBuf = env::var("OUT_DIR").unwrap().into();

    // scx source root: the SCX_ROOT override (so a crate embedding scx_simulator
    // can supply its own scx sources -- the published crate carries no submodule)
    // or the bundled submodule at <repo-root>/scx. Resolution + validity check
    // live in scxsim_build::resolve_scx_root, shared with the embed harness, so
    // every scheduler's scx sources (headers, BPF source, the scx/lib bodies lavd
    // compiles in) follow one override path.
    let scx_root = resolve_scx_root(&root_dir.join("scx"));

    let coverage = env::var("SCX_SIM_COVERAGE").as_deref() == Ok("1");

    // C substrate vendored into this crate so `cargo package` ships it
    // (previously at scx-sim/csrc and repo-root lib/scxtest, outside the crate).
    let csrc_dir = manifest_dir.join("csrc");
    let scxtest_dir = manifest_dir.join("scxtest");

    // libbpf headers (libbpf-sys exports its include dir as DEP_BPF_INCLUDE).
    let bpf_include: PathBuf = env::var("DEP_BPF_INCLUDE")
        .expect("libbpf-sys include must be available")
        .into();
    // Crate-local C dirs first, then the scx-derived -I set (shared with
    // embedders via scxsim_build::scx_include_paths, one source of truth for the
    // order). -I resolution is first-match, so this reproduces the standalone
    // build's historical -I sequence exactly -- keep crate-local dirs ahead of
    // the scx trees and preserve the order (the .so build is sensitive to it).
    // None vmlinux override: the standalone build uses the vendored,
    // scx-versioned vmlinux (an embedder passes Some(kernel_vmlinux_dir)).
    let include_paths: Vec<PathBuf> = [csrc_dir.clone(), scxtest_dir.clone()]
        .into_iter()
        .chain(scx_include_paths(&scx_root, &bpf_include, None))
        .collect();

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
        scxtest_dir.join("scx_test.c"),
        scxtest_dir.join("scx_test_map.c"),
        scxtest_dir.join("scx_test_cpumask.c"),
    ]);
    configure_build(&mut scxtest);
    scxtest.compile("scxtest");

    // Build the task_struct accessor library
    let mut sim_task = cc::Build::new();
    sim_task.file(csrc_dir.join("sim_task.c"));
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
    sim_sdt.file(csrc_dir.join("sim_sdt_stubs.c"));
    sim_sdt.file(csrc_dir.join("sim_arena.c"));
    configure_build(&mut sim_sdt);
    sim_sdt.compile("sim_sdt_stubs");

    // Build the cgroup CSS iterator support.
    // Provides sim_css_next() and related functions for bpf_for_each(css, ...).
    let mut sim_cgroup = cc::Build::new();
    sim_cgroup.file(csrc_dir.join("sim_cgroup.c"));
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
    sim_atq.file(csrc_dir.join("sim_atq.c"));
    configure_build(&mut sim_atq);
    sim_atq.compile("sim_atq");

    // ---------------------------------------------------------------
    // Shared libraries (.so) for schedulers — compiled here via clang.
    // Ported from `make -C schedulers` so the .so build is self-contained
    // and uses the same toolchain as the cc::Build static libs above. The
    // Makefile is retained only for the optional `make e9` post-processing,
    // which instruments the .so files produced here.
    // ---------------------------------------------------------------

    let scheduler_dir = if coverage {
        out_dir.join("schedulers_cov")
    } else {
        out_dir.join("schedulers")
    };
    std::fs::create_dir_all(&scheduler_dir).expect("create scheduler output dir");

    // SCX cgroup_bw API flag-day probe (shared with the embed harness): the NEW
    // function signatures are gated on `struct scx_task_cgroup_bw` in
    // scheds/include/lib/cgroup.h, matching schedulers/Makefile's grep.
    let cgroup_bw_new_api = cgroup_bw_new_api(&scx_root);

    // The standalone scheduler set as owned definitions; an embedder drives the
    // same build_schedulers with its own definitions (one build path, two providers).
    let defs = standalone_definitions();
    build_schedulers(
        &workspace_dir.join("schedulers"),
        &defs,
        &scheduler_dir,
        &csrc_dir,
        &scxtest_dir,
        &include_paths,
        &scx_root,
        &compiler,
        coverage,
        cgroup_bw_new_api,
    );

    println!("cargo:rerun-if-env-changed=SCXSIM_PHASE2_REAL_CGROUP_BW");

    // ---------------------------------------------------------------
    // Linker flags for the main binary
    // ---------------------------------------------------------------

    // The kfunc/SDT/arena/atq/e9 symbols the dlopen'd `.so` resolve from this
    // binary at load time live in scxsim_build::EXPORTED_SYMS (the single source
    // of truth an embedder re-emits; see that const for the grouped rationale).
    // Emitted three ways below: -rdynamic + per-symbol --undefined on this
    // binary, plus SCXSIM_EXPORTED_SYMS for tests/symbol_export.rs.
    // Export all symbols so `.so` files can resolve kfuncs and scxtest funcs.
    println!("cargo:rustc-link-arg=-rdynamic");
    for sym in EXPORTED_SYMS {
        println!("cargo:rustc-link-arg=-Wl,--undefined={sym}");
    }
    println!(
        "cargo:rustc-env=SCXSIM_EXPORTED_SYMS={}",
        EXPORTED_SYMS.join(",")
    );

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
    // The scx watch list below is GENERATED from the manifest (each scheduler's
    // scx_bpf_dir subtree + the shared header/lib trees), so a NEW scheduler's
    // subtree is covered automatically -- no manual edit to a hardcoded list.
    // Vendored C substrate (lives in this crate).
    println!("cargo:rerun-if-changed={}", csrc_dir.display());
    println!("cargo:rerun-if-changed={}", scxtest_dir.display());

    let rerun_dirs: &[&str] = &[
        // scx-sim local source — Makefile + wrapper.c per scheduler
        "schedulers",
    ];
    for d in rerun_dirs {
        println!("cargo:rerun-if-changed={}", workspace_dir.join(d).display());
    }
    // Shared scx trees every scheduler pulls in, plus each scheduler's own scx
    // BPF subtree (generated from the manifest's scx_bpf_dir flag).
    let mut scx_rerun_dirs: Vec<PathBuf> = vec![
        scx_root.join("lib"),            // ravg.bpf.c, cgroup_bw.bpf.c, ...
        scx_root.join("scheds/include"), // headers used by ALL schedulers
        scx_root.join("scheds/vmlinux"),
    ];
    for m in &defs {
        if m.scx_bpf_dir {
            scx_rerun_dirs.push(scx_root.join(format!("scheds/rust/scx_{}/src/bpf", m.name)));
        }
    }
    for d in &scx_rerun_dirs {
        println!("cargo:rerun-if-changed={}", d.display());
    }
    println!("cargo:rerun-if-env-changed=SCX_SIM_COVERAGE");
}
