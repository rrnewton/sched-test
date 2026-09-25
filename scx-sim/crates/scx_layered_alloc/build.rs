//! Resolve scx_layered's `alloc.rs` from the ACTIVE scx tree (the `SCX_ROOT`
//! override, else this crate's own `vendor/scx`: a symlink into the scx
//! submodule in a sched-test checkout, the file itself in a published crate) and
//! emit the wrapper `src/lib.rs` includes. See
//! `scxsim_build::emit_upstream_module`.

fn main() {
    scxsim_build::emit_bundled_upstream_module(
        "scheds/rust/scx_layered/src/alloc.rs",
        "layered_alloc_upstream",
    );
}
