//! Build script for the ktstr scenario replay harness.
//!
//! The same job as `embed_harness`'s: emit the host-export link args and build
//! `libscx_simple.so` the way an external embedder (cargo-ktstr) would --
//! through scx_simulator's `links = "scxsim"` metadata
//! (`scxsim_build::SimBuildInputs`), never through a workspace path. Kept as
//! its own copy because it IS the contract an embedder must satisfy
//! independently; the shared logic lives in scxsim_build.

use std::env;
use std::path::PathBuf;

use scxsim_build::{emit_host_link_args, KernelConfig, SchedulerDefinition, SimBuildInputs};

fn main() {
    // (1) Put the host exports (`HOST_EXPORTS`: the map, kfunc, SDT, arena and
    // ATQ symbols scx_simulator's C static libs define and link into this
    // binary) in this binary's dynamic symbol table, so a dlopen'd `.so` binds
    // the simulator's definitions. `cargo:rustc-link-arg` is NON-TRANSITIVE --
    // scx_simulator's own build.rs emits it for ITS binaries only -- so every
    // package whose binaries load a `.so` must call this itself; without it the
    // loader refuses with `LoadError::HostSymbolsNotExported` (`embed_unexported`
    // is that negative control). This call IS the contract under test.
    emit_host_link_args();

    // (2) scx_simulator's C substrate, scx tree and libbpf headers, as it was
    // built. Cargo passes them to this build script because scx_simulator is a
    // NORMAL dependency of this crate.
    let inputs = SimBuildInputs::from_dep_env();

    // An embedder builds the definition FROM SCRATCH (not standalone_definitions),
    // via the canonical fluent chain (new + with_* — the expression a scheduler
    // DSL emits). `simple` is the one scheduler that overrides new()'s defaults:
    // its source is local (no scx BPF dir) and needs no const stripping.
    let simple_def = SchedulerDefinition::new("simple")
        .with_strip_const(false)
        .with_scx_bpf_dir(false);

    // KernelConfig::default() keeps the standalone defaults.
    let out_dir: PathBuf = env::var("OUT_DIR").unwrap().into();
    let so_dir = inputs.build_bundled(
        std::slice::from_ref(&simple_def),
        &out_dir,
        &KernelConfig::default(),
    );
    println!("cargo:rustc-env=REPLAY_SO_DIR={}", so_dir.display());
}
