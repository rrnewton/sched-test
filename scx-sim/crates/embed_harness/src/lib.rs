//! Test fixture proving the embedder side of the dlopen host-export link
//! contract: cargo does not pass link arguments on to dependents, so a
//! downstream binary that loads a scheduler must emit them itself.
//!
//! This crate has no runtime code. Its `build.rs` plays the role of an external
//! embedder's build script: it calls [`scxsim_build::emit_host_link_args`]
//! (`-rdynamic`, plus one `-Wl,--undefined` per entry of
//! [`scxsim_build::HOST_EXPORTS`]; scx_simulator's own build script emits them
//! for its own binaries only) and compiles `libscx_simple.so` via
//! [`scxsim_build::SimBuildInputs::build_bundled`]. `tests/embed.rs` then loads
//! that `.so` through `scx_simulator` and runs a real simulation. The loader
//! refuses to `dlopen` into a binary that does not export every host export
//! (`LoadError::HostSymbolsNotExported`), so a normal completion proves the
//! emitted link args reached this binary. `embed_unexported` is the negative
//! control: the same dependency with the build-script call left out.
