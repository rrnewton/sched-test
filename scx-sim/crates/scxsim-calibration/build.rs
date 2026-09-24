//! Emit the host-export link args for this package's own `sim` binaries.
//!
//! A scheduler `.so` resolves kfunc / SDT / arena symbols from the *loading
//! binary's* dynamic symbol table, and `scx_simulator` refuses to load one into
//! a binary that does not export them all (`LoadError::HostSymbolsNotExported`).
//! `scxsim_build::emit_host_link_args` puts them there, but
//! `cargo:rustc-link-arg` reaches only the calling package's own binaries,
//! tests, examples and benches: it is **NOT transitive**, so this covers this
//! crate's calibration test and nothing downstream.
//!
//! Documented in `scx-sim/ai_docs/ktstr_scxsim_embed_contract.md` §2; the same
//! call is in `scxsim-workload-ir/build.rs`.
//!
//! Emitted only when `sim` is on, the only configuration in which this
//! package's binaries load a scheduler (the one test that does requires it).

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // Cargo sets CARGO_FEATURE_<NAME> for each enabled feature.
    if std::env::var_os("CARGO_FEATURE_SIM").is_none() {
        return;
    }
    scxsim_build::emit_host_link_args();
}
