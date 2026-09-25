//! Negative control for `embed_harness`, with no runtime code. This package has
//! no build script, so its test binary is linked without
//! `scxsim_build::emit_host_link_args()`; `tests/refused.rs` checks that
//! `scx_simulator` refuses to load a scheduler into it.
