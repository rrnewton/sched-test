//! Embedder build script: re-emit the non-transitive kfunc-export link args and
//! build `libscx_simple.so` the way an external embedder (cargo-ktstr) would.
//!
//! This is deliberately the SAME code an external embedder writes; it exists to
//! prove that reading `scxsim_build::EXPORTED_SYMS` + `build_schedulers` from a
//! DOWNSTREAM crate is sufficient, with no `links`/`DEP_` channel.

use std::env;
use std::path::PathBuf;

use scxsim_build::{
    build_schedulers, cgroup_bw_new_api, resolve_scx_root, scx_include_paths, SchedulerDefinition,
    EXPORTED_SYMS,
};

fn main() {
    // (1) Re-emit the link args that put the kfunc/SDT/arena symbols (defined in
    // scx_simulator's C static libs, linked into this binary via the dev-dep) in
    // this binary's dynamic symbol table so a dlopen'd `.so` resolves them. These
    // `cargo:rustc-link-arg` directives are NON-TRANSITIVE -- scx_simulator's own
    // build.rs emits them for ITS binaries only, so every downstream embedder
    // must re-emit them. This copy IS the contract under test.
    println!("cargo:rustc-link-arg=-rdynamic");
    for sym in EXPORTED_SYMS {
        println!("cargo:rustc-link-arg=-Wl,--undefined={sym}");
    }

    let manifest_dir: PathBuf = env::var("CARGO_MANIFEST_DIR").unwrap().into();
    let out_dir: PathBuf = env::var("OUT_DIR").unwrap().into();
    // Workspace root is two levels up from crates/embed_harness; repo root one
    // level above that (where the bundled scx submodule lives).
    let workspace_dir = manifest_dir.join("../..").canonicalize().unwrap();
    let repo_root = workspace_dir.join("..").canonicalize().unwrap();

    // scx source root: same resolution + validity check as the standalone build,
    // via the shared helper (SCX_ROOT override or the bundled submodule). Required
    // even though `simple` compiles no scx source -- its wrapper.c includes
    // <scx/common.bpf.h> + vmlinux.h from these -I dirs.
    let scx_root = resolve_scx_root(&repo_root.join("scx"));

    // libbpf-sys exports its header dir as DEP_BPF_INCLUDE to this build script
    // because libbpf-sys is a DIRECT NORMAL dependency of this crate (cargo
    // passes a links-crate's metadata to a build script only for normal deps).
    let bpf_include: PathBuf = env::var("DEP_BPF_INCLUDE")
        .expect("libbpf-sys include must be available")
        .into();

    // Reuse scx_simulator's vendored C substrate by path (no copy): the shared
    // sim TUs and the scxtest support headers `simple` compiles against.
    let scx_sim_crate = workspace_dir.join("crates/scx_simulator");
    let csrc_dir = scx_sim_crate.join("csrc");
    let scxtest_dir = scx_sim_crate.join("scxtest");

    // Same include order as the standalone build, via the shared helper. None
    // vmlinux override -- this in-repo proof uses the vendored vmlinux; a real
    // embedder driving a booted kernel passes Some(its kernel-derived vmlinux).
    let include_paths: Vec<PathBuf> = [csrc_dir.clone(), scxtest_dir.clone()]
        .into_iter()
        .chain(scx_include_paths(&scx_root, &bpf_include, None))
        .collect();

    // build_schedulers discovers ALL <name>/wrapper.c subdirs under its
    // schedulers_src arg; to build ONLY `simple` (self-contained -- no scx BPF
    // subtree, empty rodata), point it at an OUT_DIR-private dir holding just a
    // symlink to the canonical schedulers/simple (no checked-in copy to drift).
    let canonical_simple = workspace_dir.join("schedulers/simple");
    let harness_scheds = out_dir.join("schedulers_src");
    let simple_link = harness_scheds.join("simple");
    std::fs::create_dir_all(&harness_scheds).expect("create harness schedulers_src dir");
    // Clear any prior symlink; ignore only a genuine "not there yet", and fail
    // loud on anything else (e.g. a stale directory) rather than letting the
    // symlink below panic with a misleading EEXIST.
    match std::fs::remove_file(&simple_link) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => panic!("clear stale {}: {e}", simple_link.display()),
    }
    std::os::unix::fs::symlink(&canonical_simple, &simple_link)
        .expect("symlink schedulers/simple into the harness schedulers_src dir");

    let compiler = env::var("BPF_CLANG").unwrap_or_else(|_| "clang".into());
    // Same cgroup_bw API probe as the standalone build, via the shared helper
    // (harmless for simple, which uses no cgroup_bw; kept so the build path
    // matches standalone exactly).
    let cgroup_bw_new_api = cgroup_bw_new_api(&scx_root);

    let so_dir = out_dir.join("schedulers");
    std::fs::create_dir_all(&so_dir).expect("create harness .so output dir");

    // An embedder builds the definition FROM SCRATCH (not standalone_definitions).
    // `simple` is the one scheduler that overrides new()'s defaults: its source
    // is local (no scx BPF dir) and needs no const stripping.
    let mut simple_def = SchedulerDefinition::new("simple");
    simple_def.strip_const = false;
    simple_def.scx_bpf_dir = false;

    build_schedulers(
        &harness_scheds,
        std::slice::from_ref(&simple_def),
        &so_dir,
        &csrc_dir,
        &scxtest_dir,
        &include_paths,
        &scx_root,
        &compiler,
        false, // coverage
        cgroup_bw_new_api,
    );

    println!("cargo:rustc-env=HARNESS_SO_DIR={}", so_dir.display());
    println!("cargo:rerun-if-changed={}", canonical_simple.display());
    println!("cargo:rerun-if-changed={}", csrc_dir.display());
    println!("cargo:rerun-if-changed={}", scxtest_dir.display());
    // simple's wrapper.c pulls scx headers (<scx/common.bpf.h>, vmlinux.h) from
    // these scx subtrees, so an scx SHA swap must rebuild the harness .so too.
    println!(
        "cargo:rerun-if-changed={}",
        scx_root.join("scheds/include").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        scx_root.join("scheds/vmlinux").display()
    );
}
