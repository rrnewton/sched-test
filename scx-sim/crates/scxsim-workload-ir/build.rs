//! Re-emit the kfunc-export link contract for the `ingest` feature.
//!
//! A scheduler `.so` resolves kfunc / SDT / arena symbols from the *loading
//! binary's* dynamic symbol table. `scx_simulator`'s build script forces those
//! symbols into its own binaries with `-rdynamic` + per-symbol
//! `-Wl,--undefined`, but `cargo:rustc-link-arg` is **NOT transitive**: a
//! downstream crate that loads a `.so` must re-emit them or the load fails at
//! `RTLD_NOW` with `undefined symbol: sim_arena_offset`.
//!
//! That is documented in `scx-sim/ai_docs/ktstr_scxsim_embed_contract.md` §2 and
//! proved by `crates/embed_harness`. This crate hits it for real in
//! `tests/sched_basic_proportional.rs`, which loads a scheduler and runs a
//! lowered scenario.
//!
//! Emitted only when `ingest` is on, so a consumer taking just the IR and the
//! lowering — ktstr's DSL PR, which stays scx-sim-free — links normally.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // Cargo sets CARGO_FEATURE_<NAME> for each enabled feature.
    if std::env::var_os("CARGO_FEATURE_INGEST").is_none() {
        return;
    }
    println!("cargo:rustc-link-arg=-rdynamic");
    for sym in scxsim_build::EXPORTED_SYMS {
        println!("cargo:rustc-link-arg=-Wl,--undefined={sym}");
    }
}
