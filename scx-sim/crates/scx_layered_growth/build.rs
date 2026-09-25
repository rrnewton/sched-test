//! Resolve scx_layered's `layer_core_growth.rs` from the ACTIVE scx tree (the
//! `SCX_ROOT` override, else this crate's own `vendor/scx`: a symlink into the
//! scx submodule in a sched-test checkout, the file itself in a published crate)
//! and emit the wrapper `src/lib.rs` includes. This crate compiles that upstream
//! policy file verbatim; see `scxsim_build::emit_upstream_module` for why the
//! `#[path]` is generated rather than written as a literal.

fn main() {
    scxsim_build::emit_bundled_upstream_module(
        "scheds/rust/scx_layered/src/layer_core_growth.rs",
        "layer_core_growth",
    );
}
