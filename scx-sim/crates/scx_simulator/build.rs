use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

use scxsim_build::{
    build_schedulers, bundled_scx_root, cgroup_bw_new_api, emit_host_link_args, resolve_scx_root,
    standalone_definitions, KernelConfig, SchedulerDefinition, SimBuildInputs,
};

fn main() {
    let manifest_dir: PathBuf = env::var("CARGO_MANIFEST_DIR").unwrap().into();
    let out_dir: PathBuf = env::var("OUT_DIR").unwrap().into();

    // scx source root: the SCX_ROOT override (so a crate embedding scx_simulator
    // can supply its own scx sources), else the scx subset this crate bundles
    // under vendor/scx -- symlinks into the scx submodule in a sched-test
    // checkout, real files in a published crate (see
    // scxsim_build::bundled_scx_root). Resolution + validity check live in
    // scxsim_build, shared with every build script that compiles scx sources, so
    // every scheduler's scx sources (headers, BPF source, the scx/lib bodies lavd
    // compiles in) follow one override path.
    let scx_root = resolve_scx_root(|| {
        bundled_scx_root(
            &manifest_dir.join("vendor/scx"),
            Path::new("scheds/include"),
        )
    });

    let coverage = env::var("SCX_SIM_COVERAGE").as_deref() == Ok("1");

    // Everything the C side is compiled from. csrc/scxtest are the C substrate
    // vendored into this crate so `cargo package` ships it (previously at
    // scx-sim/csrc and repo-root lib/scxtest, outside the crate); the libbpf
    // headers come from libbpf-sys, which exports its include dir as
    // DEP_BPF_INCLUDE. Published to direct dependents via `links = "scxsim"`, so
    // an embedder compiles its own scheduler `.so` against exactly this
    // substrate -- see SimBuildInputs.
    let inputs = SimBuildInputs {
        csrc: manifest_dir.join("csrc"),
        scxtest: manifest_dir.join("scxtest"),
        scx_root,
        bpf_include: env::var("DEP_BPF_INCLUDE")
            .expect("libbpf-sys include must be available")
            .into(),
        schedulers: in_crate(&manifest_dir, "schedulers"),
    };
    inputs.emit_metadata();
    // Crate-local C dirs first, then the scx-derived -I set (one source of truth
    // for the order, shared with embedders). -I resolution is first-match, so
    // this reproduces the standalone build's historical -I sequence exactly --
    // keep crate-local dirs ahead of the scx trees and preserve the order (the
    // .so build is sensitive to it). None vmlinux override: the standalone build
    // uses the vendored, scx-versioned vmlinux (an embedder passes
    // Some(kernel_vmlinux_dir)).
    let include_paths = inputs.include_paths(None);

    // Common compiler: BPF scheduler code compiled as userspace C has
    // inherently unused parameters (fixed BPF ops signatures) and unknown
    // attributes (preserve_access_index from vmlinux.h).
    let compiler = env::var("BPF_CLANG").unwrap_or_else(|_| "clang".into());

    // Standalone uses the kernel-config defaults baked into sim_kconfig_defaults.h
    // (no overrides). An embedder constructs a non-default KernelConfig to compile
    // its .so files against the kernel under test; the host static libs below take
    // none, because every kernel-config value lives in the .so. Default => empty
    // cflag_defines => byte-identical .so.
    let kernel_config = KernelConfig::default();

    build_static_libs(&inputs, &include_paths, &compiler, coverage);

    // ---------------------------------------------------------------
    // Shared libraries (.so) for schedulers — compiled here via clang.
    // Ported from `make -C schedulers` so the .so build is self-contained
    // and uses the same toolchain as the cc::Build static libs. The
    // Makefile is retained only for the optional `make e9` post-processing,
    // which instruments the .so files produced here.
    // ---------------------------------------------------------------

    let scheduler_dir = if coverage {
        out_dir.join("schedulers_cov")
    } else {
        out_dir.join("schedulers")
    };
    std::fs::create_dir_all(&scheduler_dir).expect("create scheduler output dir");

    // The standalone scheduler set as owned definitions; an embedder drives the
    // same build_schedulers with its own definitions (one build path, two providers).
    let defs = standalone_definitions();
    build_schedulers(
        &inputs.schedulers,
        &defs,
        &scheduler_dir,
        &inputs.csrc,
        &inputs.scxtest,
        &include_paths,
        &inputs.scx_root,
        &compiler,
        coverage,
        // SCX cgroup_bw API flag-day probe (shared with embedders): the NEW
        // function signatures are gated on `struct scx_task_cgroup_bw` in
        // scheds/include/lib/cgroup.h, matching schedulers/Makefile's grep.
        cgroup_bw_new_api(&inputs.scx_root),
        &kernel_config,
    );

    println!("cargo:rerun-if-env-changed=SCXSIM_PHASE2_REAL_CGROUP_BW");

    // ---------------------------------------------------------------
    // Linker flags for the main binary
    // ---------------------------------------------------------------

    // The kfunc/SDT/arena/atq/e9 symbols the dlopen'd `.so` resolve from this
    // binary at load time live in scxsim_build::HOST_EXPORTS (see that const for
    // the grouped rationale): -rdynamic + per-symbol --undefined on this
    // package's binaries, through the same helper every embedder calls from its
    // own build script, since link args do not propagate.
    emit_host_link_args();

    if coverage {
        link_profile_runtime(&compiler);
    }

    // Expose scheduler .so directory to Rust code
    println!(
        "cargo:rustc-env=SCHEDULER_SO_DIR={}",
        scheduler_dir.display()
    );

    // Expose bpftrace script path for --bpf-trace option
    println!(
        "cargo:rustc-env=BPFTRACE_SCRIPT={}",
        in_crate(&manifest_dir, "scripts/trace_scx_ops.bt").display()
    );

    emit_rerun_triggers(&manifest_dir, &inputs, &defs);
}

/// A path this crate carries under `manifest_dir`, canonicalized. In a
/// sched-test checkout `schedulers/` and `scripts/trace_scx_ops.bt` are symlinks
/// into the scx-sim workspace (`cargo package` materializes them), so
/// canonicalizing keeps the compile paths -- and the `.so` bytes -- exactly what
/// they were when this script read the workspace copies directly.
fn in_crate(manifest_dir: &Path, rel: &str) -> PathBuf {
    let p = manifest_dir.join(rel);
    p.canonicalize()
        .unwrap_or_else(|e| panic!("{} does not resolve: {e}", p.display()))
}

/// The static libraries linked into this crate (and so into every binary that
/// links it), compiled with the same `-I` set and compiler as the scheduler
/// `.so` files.
fn build_static_libs(
    inputs: &SimBuildInputs,
    include_paths: &[PathBuf],
    compiler: &str,
    coverage: bool,
) {
    let csrc_dir = &inputs.csrc;
    let scxtest_dir = &inputs.scxtest;

    // DRY helper: apply common config to a cc::Build
    let configure_build = |build: &mut cc::Build| {
        build
            .compiler(compiler)
            .define("SCX_BPF_UNITTEST", None)
            .includes(include_paths)
            .flag("-Wno-unused-parameter")
            .flag("-Wno-unknown-attributes");
        if coverage {
            build
                .flag("-fprofile-instr-generate")
                .flag("-fcoverage-mapping");
        }
    };

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

    // Build the task_struct accessor library.
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
}

/// Link the clang profile runtime, which provides the `__llvm_profile_*`
/// symbols the coverage-instrumented `.so` files need.
fn link_profile_runtime(compiler: &str) {
    let rt_dir_output = Command::new(compiler)
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
    let profile_lib = if Path::new(&format!("{rt_dir}/libclang_rt.profile.a")).exists() {
        "clang_rt.profile"
    } else {
        "clang_rt.profile-x86_64"
    };
    println!("cargo:rustc-link-search=native={rt_dir}");
    println!("cargo:rustc-link-lib=static={profile_lib}");
}

/// Rebuild triggers.
///
/// The list MUST cover every directory whose contents the scheduler `.so`
/// build depends on. If a directory is omitted, swapping the scx submodule
/// SHA can silently leave a STALE `.so` in place: cargo's incremental
/// logic skips the build script, the Makefile is never re-invoked, and
/// the cached `.so` from the previous SHA is reused unchanged.
///
/// Worked example (the bug this list exists to prevent): pre-fix, only
/// `scx_tickless/src/bpf` and `scx_cosmos/src/bpf` were watched. Swapping
/// the scx submodule between two SHAs that differed only in
/// `scheds/rust/scx_lavd/src/bpf/` produced a 0.06s no-op `cargo build`
/// and the stale `libscx_lavd.so` from the prior SHA was reused. The
/// workaround was `touch crates/scx_simulator/build.rs`. See:
/// - tg `official-scheduler-rebuild-action-replace-touch-build-rs-hack`
/// - experiments/lavd_cpubw_stalls_202604/CPU_BW_STALL_BUG_REPRODUCER_REPORT.md
///
/// The scx watch list is GENERATED from the manifest by
/// `SimBuildInputs::rerun_paths` (each scheduler's scx_bpf_dir subtree + the
/// shared header/lib trees, one list shared with embedders), so a NEW
/// scheduler's subtree is covered automatically -- no manual edit to a
/// hardcoded list.
fn emit_rerun_triggers(manifest_dir: &Path, inputs: &SimBuildInputs, defs: &[SchedulerDefinition]) {
    // Beyond the shared list: the scx-sim local scheduler sources (Makefile +
    // wrapper.c per scheduler), and the vendored scx subset itself, so
    // re-pointing one of its symlinks re-runs this script too.
    for d in [inputs.schedulers.clone(), manifest_dir.join("vendor")]
        .into_iter()
        .chain(inputs.rerun_paths(defs))
    {
        println!("cargo:rerun-if-changed={}", d.display());
    }
    println!("cargo:rerun-if-env-changed=SCX_SIM_COVERAGE");
}
