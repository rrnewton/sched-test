//! Re-emit the kfunc-export link contract for the `sim` feature.
//!
//! A scheduler `.so` resolves kfunc / SDT / arena symbols from the *loading
//! binary's* dynamic symbol table. `scx_simulator`'s build script forces those
//! symbols into its own binaries, but `cargo:rustc-link-arg` is **NOT
//! transitive**: a downstream crate that loads a `.so` must re-emit them or the
//! load fails at `RTLD_NOW` with `undefined symbol: sim_arena_offset`.
//!
//! Documented in `scx-sim/ai_docs/ktstr_scxsim_embed_contract.md` §2; the same
//! re-emission is in `scxsim-workload-ir/build.rs`, which hit it for real.
//!
//! Emitted only when `sim` is on, so a consumer taking just the metric registry
//! and the verdict rule links normally.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // Cargo sets CARGO_FEATURE_<NAME> for each enabled feature.
    if std::env::var_os("CARGO_FEATURE_SIM").is_none() {
        return;
    }
    println!("cargo:rustc-link-arg=-rdynamic");
    for sym in scxsim_build::EXPORTED_SYMS {
        println!("cargo:rustc-link-arg=-Wl,--undefined={sym}");
    }
}
