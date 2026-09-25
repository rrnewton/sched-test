//! Emit the host-export link args for this package's own `ingest` binaries.
//!
//! A scheduler `.so` resolves kfunc / SDT / arena symbols from the *loading
//! binary's* dynamic symbol table, and `scx_simulator` refuses to load one into
//! a binary that does not export them all (`LoadError::HostSymbolsNotExported`).
//! `scxsim_build::emit_host_link_args` puts them there, but
//! `cargo:rustc-link-arg` reaches only the calling package's own binaries,
//! tests, examples and benches: it is **NOT transitive**. So this covers this
//! crate's `tests/sched_basic_proportional.rs`, which loads a scheduler and runs
//! a lowered scenario, and nothing downstream: a consumer that enables `ingest`
//! and loads a scheduler must call `emit_host_link_args()` from its own build
//! script.
//!
//! Documented in `scx-sim/ai_docs/ktstr_scxsim_embed_contract.md` §2; proved by
//! `crates/embed_harness`, with `crates/embed_unexported` as the negative
//! control.
//!
//! Emitted only when `ingest` is on, the only configuration in which this
//! package's binaries load a scheduler (the one test that does requires it).
//! `ingest` is also what enables the `scxsim-build` build-dependency, so a
//! default build compiles this script without it.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    #[cfg(feature = "ingest")]
    scxsim_build::emit_host_link_args();
}
