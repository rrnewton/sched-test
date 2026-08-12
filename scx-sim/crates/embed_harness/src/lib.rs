//! Test fixture proving the embedder side of the dlopen kfunc-export link
//! contract -- the non-transitivity of the `EXPORTED_SYMS` link args.
//!
//! This crate has no runtime code. Its `build.rs` plays the role of an external
//! embedder's build script: it re-emits the `-rdynamic` / `-Wl,--undefined`
//! link args from [`scxsim_build::EXPORTED_SYMS`] (those args are NOT inherited
//! across the `scx_simulator` crate boundary) and compiles `libscx_simple.so`
//! via [`scxsim_build::build_schedulers`]. `tests/embed.rs` then loads that `.so`
//! through `scx_simulator` (which dlopens with RTLD_NOW, resolving every symbol
//! eagerly at load) and runs a real simulation; a missing symbol fails the load,
//! so a normal completion proves the re-emitted link args worked.
