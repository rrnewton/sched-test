//! Resolve scx_layered's `alloc.rs` from the ACTIVE scx tree (the `SCX_ROOT`
//! override or the bundled submodule) and emit the wrapper `src/lib.rs`
//! includes. See `scxsim_build::emit_upstream_module`.

use std::env;
use std::path::PathBuf;

fn main() {
    let manifest_dir: PathBuf = env::var("CARGO_MANIFEST_DIR").unwrap().into();
    // Workspace root is two levels up from crates/scx_layered_alloc; the repo
    // root (which holds the scx submodule) is one above that.
    let root_dir = manifest_dir.join("../../..").canonicalize().unwrap();
    let scx_root = scxsim_build::resolve_scx_root(&root_dir.join("scx"));
    scxsim_build::emit_upstream_module(
        &scx_root,
        "scheds/rust/scx_layered/src/alloc.rs",
        "layered_alloc_upstream",
    );
}
