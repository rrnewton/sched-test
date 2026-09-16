//! Resolve scx_layered's `layer_core_growth.rs` from the ACTIVE scx tree.
//!
//! This crate compiles that upstream policy file verbatim (see `src/lib.rs`).
//! It used to locate it with a `#[path = "../../../../scx/..."]` literal, which
//! is fixed at the bundled submodule: a `#[path]` attribute takes a string
//! literal and so cannot see `SCX_ROOT`. An embedder pointing `SCX_ROOT` at
//! their own scx checkout therefore got the scheduler `.so` built from their
//! tree and this growth policy from ours, in one binary, silently.
//!
//! So generate the literal instead: this emits a one-line
//! `#[path = "<abs>"] pub mod layer_core_growth;` wrapper into OUT_DIR, from
//! the same `scxsim_build::resolve_scx_root` every other scx source follows.
//! (`include!`ing the file straight into an inline `mod` is not an option —
//! upstream files open with `//!` module docs, and inner doc comments are
//! illegal in a macro expansion, E0753.) The upstream file stays an ordinary
//! file module: byte-identical to the pin, no source rewriting.

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let manifest_dir: PathBuf = env::var("CARGO_MANIFEST_DIR").unwrap().into();
    // Workspace root is two levels up from crates/scx_layered_growth; the repo
    // root (which holds the scx submodule) is one above that.
    let root_dir = manifest_dir.join("../../..").canonicalize().unwrap();
    let scx_root = scxsim_build::resolve_scx_root(&root_dir.join("scx"));

    let growth_rs = scx_root.join("scheds/rust/scx_layered/src/layer_core_growth.rs");
    assert!(
        growth_rs.is_file(),
        "scx_layered's layer_core_growth.rs not found at {} (is SCX_ROOT an scx checkout?)",
        growth_rs.display()
    );
    let out_dir: PathBuf = env::var("OUT_DIR").unwrap().into();
    fs::write(
        out_dir.join("layer_core_growth_mod.rs"),
        format!("#[path = {:?}]\npub mod layer_core_growth;\n", growth_rs),
    )
    .expect("write layer_core_growth_mod.rs");
    println!("cargo:rerun-if-changed={}", growth_rs.display());
}
