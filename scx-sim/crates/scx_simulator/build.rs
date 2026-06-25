use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir: PathBuf = env::var("CARGO_MANIFEST_DIR").unwrap().into();
    // Workspace root is two levels up from crates/scx_simulator
    let workspace_dir = manifest_dir.join("../..").canonicalize().unwrap();
    // Repo root is one level up from the workspace (scx-sim/)
    let root_dir = workspace_dir.join("..").canonicalize().unwrap();
    let out_dir: PathBuf = env::var("OUT_DIR").unwrap().into();

    // scx source root. Defaults to the bundled submodule at <repo-root>/scx;
    // SCX_ROOT overrides it so a crate embedding scx_simulator can supply its
    // own scx sources (the published crate does not carry the submodule). When
    // set, the path must exist and look like an scx checkout (have scheds/include)
    // — fail loud rather than silently miscompile against a partial tree. Every
    // scheduler's scx sources (headers, BPF source, and the scx/lib bodies lavd
    // compiles in) derive from scx_root, so all schedulers follow the override.
    let scx_root = match env::var("SCX_ROOT") {
        Ok(v) => {
            let canon = PathBuf::from(&v)
                .canonicalize()
                .unwrap_or_else(|e| panic!("SCX_ROOT={v} is not accessible: {e}"));
            assert!(
                canon.join("scheds/include").is_dir(),
                "SCX_ROOT={v} does not look like an scx checkout (missing scheds/include)"
            );
            canon
        }
        Err(_) => root_dir.join("scx"),
    };
    println!("cargo:rerun-if-env-changed=SCX_ROOT");

    let coverage = env::var("SCX_SIM_COVERAGE").as_deref() == Ok("1");

    // C substrate vendored into this crate so `cargo package` ships it
    // (previously at scx-sim/csrc and repo-root lib/scxtest, outside the crate).
    let csrc_dir = manifest_dir.join("csrc");
    let scxtest_dir = manifest_dir.join("scxtest");

    let include_paths: Vec<PathBuf> = vec![
        // Our own C source directory (vendored into this crate)
        csrc_dir.clone(),
        // scxtest unit-test infrastructure (vendored into this crate)
        scxtest_dir.clone(),
        // Scheduler include paths
        scx_root.join("scheds/include"),
        scx_root.join("scheds/include/lib"),
        scx_root.join("scheds/vmlinux"),
        scx_root.join("scheds/vmlinux/arch/x86"),
        scx_root.join("scheds/include/bpf-compat"),
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

    // SCX cgroup_bw API flag-day probe: the NEW function signatures are gated
    // on the presence of `struct scx_task_cgroup_bw` in
    // scheds/include/lib/cgroup.h (mirrors schedulers/Makefile SCX_CGROUP_BW_API).
    let cgroup_bw_new_api = std::fs::read_to_string(scx_root.join("scheds/include/lib/cgroup.h"))
        .map(|s| {
            // Matches schedulers/Makefile's `grep '^struct scx_task_cgroup_bw'`
            // exactly (line-anchored, no leading-whitespace tolerance) so this
            // probe and the still-live Makefile grep used by `make e9` agree.
            s.lines()
                .any(|l| l.starts_with("struct scx_task_cgroup_bw"))
        })
        .unwrap_or(false);

    build_schedulers(
        &workspace_dir.join("schedulers"),
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

    // Symbols DEFINED in the static C libs compiled into the main binary that
    // the dlopen'd scheduler `.so` files resolve at load time. Rust does not
    // reference them, so without `--undefined` the linker drops them and a
    // `.so` SIGSEGVs at its first kfunc call; `-rdynamic` puts them in the
    // binary's dynamic symbol table so dlopen can find them.
    //
    // Grouped rationale:
    // - scx_test_map_*: scheduler code calls them via the bpf_map_lookup_elem /
    //   bpf_map_delete_elem macros (lavd routes bpf_map_delete_elem to
    //   scx_test_map_delete_elem for cbw_cgrp_map); clear_all resets the
    //   thread-local map registry between runs.
    // - scx_task_*/scx_arena_subprog_init: per-task SDT storage. The `.so`
    //   files omit sim_sdt_stubs.c and resolve these from the binary so there
    //   is ONE SDT hash table (sim_sdt_reset works for deterministic re-runs).
    // - e9_preempt_yield/E9_SHARED_RBC: resolved by e9patch-instrumented `.so`
    //   variants. (a later change will feature-gate the e9 path; when it does, drop these
    //   two here — consumers read the emitted list, so nothing else changes.)
    // - sim_arena_*: arena allocator shared by sim_sdt_stubs (binary) and
    //   sim_bpf_stubs (.so).
    // - scx_atq_create_internal: one `--undefined` forces the whole sim_atq.o
    //   TU (all scx_atq_* follow); cgroup_bw.bpf.c in the `.so` consumes them.
    //
    // Emitted ALSO as `SCXSIM_EXPORTED_SYMS` (single source of truth): tests/
    // symbol_export.rs asserts each resolves in the process image, and a
    // downstream binary embedding scx_simulator re-emits the same set for its
    // own test binaries — no hand-copied list to drift.
    const EXPORTED_SYMS: &[&str] = &[
        "scx_test_map_lookup_elem",
        "scx_test_map_delete_elem",
        "scx_test_map_clear_all",
        "scx_task_init",
        "scx_task_alloc",
        "scx_task_data",
        "scx_task_free",
        "scx_arena_subprog_init",
        "e9_preempt_yield",
        "E9_SHARED_RBC",
        "sim_arena_buf",
        "sim_arena_offset",
        "scx_atq_create_internal",
    ];

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
    // When a NEW scheduler is added under `scx-sim/schedulers/`, audit its
    // `wrapper.c` for `#include "../../scx/...` lines and any
    // `*_BPF_DIR := $(ROOT_DIR)/...` in its `config.mk`, and add the
    // corresponding directory to this list. Today's wrappers transitively
    // depend on the dirs below.
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
    let scx_rerun_dirs: &[&str] = &[
        // scx source-root subtrees (paths relative to SCX_ROOT) pulled in by
        // wrapper.c per-scheduler #includes and by the include_paths above.
        // Watching each subtree forces a rebuild whenever the scx source is
        // swapped to a revision that touched those files.
        "lib",                              // ravg.bpf.c, cgroup_bw.bpf.c, ...
        "scheds/rust/scx_lavd/src/bpf",     // LAVD wrapper transitive include
        "scheds/rust/scx_mitosis/src/bpf",  // mitosis wrapper transitive include
        "scheds/rust/scx_cosmos/src/bpf",   // cosmos wrapper transitive include
        "scheds/rust/scx_tickless/src/bpf", // tickless wrapper transitive include
        // scx headers used by ALL schedulers via include_paths above
        "scheds/include",
        "scheds/vmlinux",
    ];
    for d in scx_rerun_dirs {
        println!("cargo:rerun-if-changed={}", scx_root.join(d).display());
    }
    println!("cargo:rerun-if-env-changed=SCX_SIM_COVERAGE");
}

/// Compile every scheduler `.so` from its `wrapper.c` plus the shared sim C
/// translation units, replicating `schedulers/Makefile` exactly. Each subdir
/// of `schedulers_src` that contains a `wrapper.c` is discovered and built into
/// `libscx_<name>.so` under `out`.
///
/// TU split (must match the Makefile and the static-lib cc::Build above):
/// - FULL-CFLAGS TUs (`wrapper.c`, `sim_bpf_stubs.c`, `overrides.c`): base
///   flags + coverage + cgroup_bw API + the cgroup_bw compile-in + every include + per-scheduler
///   extras (`-Dconst=` and the scx BPF include for all but `simple`).
/// - SPECIAL TUs (`sim_sigfpe.c`, `sim_rbc_trampoline.c`,
///   `sim_deterministic_mem.c`): CFLAGS_BASE ONLY — no includes, no coverage,
///   no defines. Coverage instrumentation or vmlinux.h here corrupts the x86
///   SIGFPE decoder, the RBC trampoline layout, and the branchless mem ops.
/// - `sim_sdt_stubs.c` is deliberately NOT linked into the `.so` (the single
///   SDT table lives in the main binary).
///
/// `scx_root` is the scx source tree (default = the bundled submodule, or an
/// SCX_ROOT override). All scheduler scx sources — including lavd's compiled-in
/// scx/lib bodies (ravg.bpf.c, cgroup_bw.bpf.c), resolved via -I<scx_root>/lib —
/// derive from scx_root, so every scheduler follows the override.
#[allow(clippy::too_many_arguments)]
fn build_schedulers(
    schedulers_src: &Path,
    out: &Path,
    csrc_dir: &Path,
    scxtest_dir: &Path,
    include_paths: &[PathBuf],
    scx_root: &Path,
    compiler: &str,
    coverage: bool,
    cgroup_bw_new_api: bool,
) {
    // CFLAGS_BASE — applied to every scheduler TU (mirrors Makefile CFLAGS_BASE).
    let cflags_base: &[&str] = &[
        "-fPIC",
        "-DSCX_BPF_UNITTEST",
        "-g",
        "-O2",
        "-Wno-unused-parameter",
        "-Wno-unknown-attributes",
        "-Wno-implicit-function-declaration",
    ];

    // Discover schedulers: subdirs of schedulers_src that contain wrapper.c.
    let mut names: Vec<String> = std::fs::read_dir(schedulers_src)
        .expect("read schedulers dir")
        .flatten()
        .filter(|e| e.path().join("wrapper.c").is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    assert!(
        !names.is_empty(),
        "no schedulers (subdirs with wrapper.c) under {}",
        schedulers_src.display()
    );

    // -I list shared by the full-CFLAGS TUs: the crate include set + <scx_root>/lib,
    // where lavd's compiled-in scx library bodies (ravg.bpf.c, cgroup_bw.bpf.c)
    // resolve so they follow SCX_ROOT. (The former -I schedulers anchor existed
    // only to resolve the wrappers' "../../scx/lib/*.bpf.c" relative includes,
    // which are now rehomed to plain names found via <scx_root>/lib.)
    let scx_lib = scx_root.join("lib");
    let base_includes: Vec<&Path> = include_paths
        .iter()
        .map(PathBuf::as_path)
        .chain(std::iter::once(scx_lib.as_path()))
        .collect();

    for name in &names {
        let sched_dir = schedulers_src.join(name);

        // `simple` has no config.mk: `const` stays intact and it pulls no scx
        // BPF include (its scheduler source is local). Every other scheduler
        // strips `const` (BPF const-volatile globals must be writable) and adds
        // its scheds/rust/scx_<name>/src/bpf dir; lavd/cosmos additionally
        // include their own scheduler dir (a generated/patched source lives
        // there).
        let strip_const = name != "simple";
        let mut extra_includes: Vec<PathBuf> = Vec::new();
        if name != "simple" {
            extra_includes.push(scx_root.join(format!("scheds/rust/scx_{name}/src/bpf")));
            if name == "lavd" || name == "cosmos" {
                extra_includes.push(sched_dir.clone());
            }
        }

        // cosmos: regenerate the div-by-zero-guarded copy of main.bpf.c. BPF
        // integer division by zero yields 0; native C raises SIGFPE. The sed
        // transform in cosmos/config.mk guards the one divide that can see a
        // zero divisor. Regenerated from the upstream source on every build so
        // a stale checked-in copy cannot drift from the active scx SHA.
        if name == "cosmos" {
            let src = scx_root.join("scheds/rust/scx_cosmos/src/bpf/main.bpf.c");
            let content = std::fs::read_to_string(&src)
                .unwrap_or_else(|e| panic!("read {}: {e}", src.display()));
            let patched = content.replace(
                "new_freq = (100 * NSEC_PER_MSEC) / interval;",
                "new_freq = interval ? (100 * NSEC_PER_MSEC) / interval : 0;",
            );
            std::fs::write(sched_dir.join("cosmos_main_patched.c"), patched)
                .expect("write cosmos_main_patched.c");
        }

        let mut objs: Vec<PathBuf> = Vec::new();

        // Full-CFLAGS TUs.
        let full_srcs = [
            sched_dir.join("wrapper.c"),
            csrc_dir.join("sim_dsq_iter_glue.c"),
            csrc_dir.join("sim_bpf_stubs.c"),
            scxtest_dir.join("overrides.c"),
        ];
        for src in &full_srcs {
            let obj = out.join(format!("{name}_{}.o", file_stem(src)));
            let mut cmd = Command::new(compiler);
            cmd.args(cflags_base);
            if coverage {
                cmd.args(["-fprofile-instr-generate", "-fcoverage-mapping"]);
            }
            if cgroup_bw_new_api {
                cmd.arg("-DSCX_CGROUP_BW_NEW_API=1");
            }
            cmd.arg("-DSCXSIM_PHASE2_REAL_CGROUP_BW=1");
            if strip_const {
                cmd.arg("-Dconst=");
            }
            for inc in base_includes
                .iter()
                .copied()
                .chain(extra_includes.iter().map(PathBuf::as_path))
            {
                cmd.arg("-I").arg(inc);
            }
            cmd.arg("-c").arg("-o").arg(&obj).arg(src);
            run(cmd, &format!("compile {} for {name}", src.display()));
            objs.push(obj);
        }

        // Special TUs: CFLAGS_BASE only.
        for tu in [
            "sim_sigfpe.c",
            "sim_rbc_trampoline.c",
            "sim_deterministic_mem.c",
        ] {
            let src = csrc_dir.join(tu);
            let obj = out.join(format!("{name}_{}.o", file_stem(&src)));
            let mut cmd = Command::new(compiler);
            cmd.args(cflags_base);
            cmd.arg("-c").arg("-o").arg(&obj).arg(&src);
            run(cmd, &format!("compile {tu} for {name}"));
            objs.push(obj);
        }

        // Link the .so. `-Wl,--init=e9_so_init` gives the otherwise
        // freestanding (`-nostdlib`) library a DT_INIT so e9tool's loader
        // runs it; the coverage build swaps `-nostdlib` for the profile
        // runtime instead.
        let so = out.join(format!("libscx_{name}.so"));
        let mut link = Command::new(compiler);
        link.arg("-shared");
        if coverage {
            link.arg("-fprofile-instr-generate");
        } else {
            link.arg("-nostdlib");
        }
        link.arg("-Wl,--init=e9_so_init").arg("-o").arg(&so);
        for obj in &objs {
            link.arg(obj);
        }
        run(link, &format!("link libscx_{name}.so"));
    }
}

/// File stem of a C source as a `&str` (e.g. `sim_bpf_stubs.c` → `sim_bpf_stubs`).
fn file_stem(p: &Path) -> &str {
    p.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_else(|| panic!("source path has no UTF-8 stem: {}", p.display()))
}

/// Run a compile/link command, panicking with `desc` on spawn failure or a
/// non-zero exit (a failed scheduler build must fail the cargo build loudly).
fn run(mut cmd: Command, desc: &str) {
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("spawn failed ({desc}): {e}"));
    assert!(status.success(), "{desc} failed: {status}");
}
