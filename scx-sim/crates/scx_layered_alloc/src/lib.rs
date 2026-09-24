//! scx_layered's **real** CPU allocator, compiled into scxsim.
//!
//! # Why this crate exists
//!
//! Tier 3 of scx_layered support means modelling the userspace control loop
//! that continuously re-allocates CPUs between layers. That allocation policy
//! is not incidental glue — it *is* a large part of what scx_layered does, and
//! it lives in `scx_layered/src/alloc.rs` (a ~2500-line water-fill allocator).
//!
//! Re-implementing it in scxsim would be a textbook fake approximation: same
//! interface, plausible outputs, silently divergent on the corner cases that
//! matter. `scx-sim/CLAUDE.md` forbids exactly that. So instead the upstream
//! source is compiled in verbatim as [`crate::layered_alloc_upstream`], the
//! same way `schedulers/layered/wrapper.c` `#include`s `main.bpf.c` rather
//! than reproducing it.
//!
//! Two things make this practical, both verified before committing to the
//! approach:
//!
//! * `alloc.rs` is self-contained. Its only crate-local dependency is
//!   [`largest_remainder`], and it references no `Topology`, no libbpf, no
//!   filesystem, and no `unsafe`. Its public entry point
//!   [`unified_alloc`](layered_alloc_upstream::unified_alloc) is a pure
//!   function over `Vec<usize>` and plain structs.
//! * It ships ~80 of its own unit tests, which are compiled and run here too,
//!   as this crate's unit tests. Upstream's allocator test suite therefore
//!   becomes part of scxsim's, and an upstream change that breaks its own
//!   invariants fails our build.
//!
//! # Why a separate crate
//!
//! An included file is parsed under the edition of the crate that includes it,
//! so whatever crate compiles `alloc.rs` must be on upstream scx_layered's
//! edition — 2024, and `alloc.rs` uses let chains, which do not parse under
//! 2021. This used to be a module of scx_simulator; giving the one upstream
//! file its own crate lets it follow upstream's edition while scx_simulator
//! stays on 2021. scx_simulator re-exports [`layered_alloc_upstream`] at its
//! crate root, so nothing that uses it had to change.
//!
//! # The one vendored piece
//!
//! `alloc.rs` does `use crate::largest_remainder;`, and that helper lives in
//! `scx_layered/src/lib.rs` — a 3533-line module that pulls in the generated
//! BPF skeleton and cannot be included here. It is a ~30-line pure function,
//! reproduced below verbatim with its provenance recorded.
//!
//! Because a silent copy is a copy that drifts, scx_simulator's
//! `tests/layered_alloc.rs` re-reads the upstream `lib.rs` at test time and
//! asserts our copy is still token-identical to it. If upstream edits
//! `largest_remainder`, that test fails rather than the two quietly
//! disagreeing.

#![forbid(unsafe_code)]

/// Distribute `total` across `quotas` by the largest-remainder method.
///
/// VENDORED VERBATIM from `scx/scheds/rust/scx_layered/src/lib.rs`
/// (`pub fn largest_remainder`). Do not edit: scx_simulator's
/// `tests/layered_alloc.rs` asserts this body still matches upstream
/// token-for-token, so an edit here registers as drift. If upstream changes,
/// re-copy rather than patch.
///
/// It lives here instead of being `include!`d because upstream's `lib.rs`
/// also contains the BPF skeleton bindings, which scxsim cannot build.
pub fn largest_remainder(total: usize, quotas: &[f64]) -> Vec<usize> {
    if quotas.is_empty() {
        return vec![];
    }

    let sum: f64 = quotas.iter().sum();
    if sum == 0.0 {
        // No demand — distribute nothing.
        return vec![0; quotas.len()];
    }

    // Scale quotas so they sum to `total`.
    let scaled: Vec<f64> = quotas.iter().map(|q| q / sum * total as f64).collect();

    // Floor each scaled value.
    let floors: Vec<usize> = scaled.iter().map(|s| *s as usize).collect();
    let floor_sum: usize = floors.iter().sum();
    let mut remainder = total.saturating_sub(floor_sum);

    // Sort indices by descending fractional part.
    let mut indices: Vec<usize> = (0..quotas.len()).collect();
    indices.sort_by(|&a, &b| {
        let frac_a = scaled[a] - floors[a] as f64;
        let frac_b = scaled[b] - floors[b] as f64;
        frac_b
            .partial_cmp(&frac_a)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut result = floors;
    for &i in &indices {
        if remainder == 0 {
            break;
        }
        result[i] += 1;
        remainder -= 1;
    }

    result
}

// The upstream allocator itself: `pub mod layered_alloc_upstream`, declared by
// a one-line `#[path = "<scx_root>/.../alloc.rs"]` wrapper that build.rs
// generates into OUT_DIR, so the path follows `SCX_ROOT` like every other scx
// source (see `scxsim_build::emit_upstream_module` for why it is generated).
include!(concat!(env!("OUT_DIR"), "/layered_alloc_upstream_mod.rs"));
