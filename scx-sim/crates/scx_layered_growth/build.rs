//! Resolve scx_layered's `layer_core_growth.rs` from the ACTIVE scx tree (the
//! `SCX_ROOT` override or the bundled submodule) and emit the wrapper
//! `src/lib.rs` includes. This crate compiles that upstream policy file
//! verbatim; see `scxsim_build::emit_upstream_module` for why the `#[path]` is
//! generated rather than written as a literal.

use std::env;
use std::path::PathBuf;

fn main() {
    let manifest_dir: PathBuf = env::var("CARGO_MANIFEST_DIR").unwrap().into();
    // Workspace root is two levels up from crates/scx_layered_growth; the repo
    // root (which holds the scx submodule) is one above that.
    let root_dir = manifest_dir.join("../../..").canonicalize().unwrap();
    let scx_root = scxsim_build::resolve_scx_root(&root_dir.join("scx"));
    scxsim_build::emit_upstream_module(
        &scx_root,
        "scheds/rust/scx_layered/src/layer_core_growth.rs",
        "layer_core_growth",
    );
}
