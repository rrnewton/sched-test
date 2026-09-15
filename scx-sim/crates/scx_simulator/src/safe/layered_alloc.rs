//! scx_layered's **real** CPU allocator, compiled into scxsim.
//!
//! # Why this file exists
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
//!   [`unified_alloc`] is a pure function over `Vec<usize>` and plain structs.
//! * It ships ~80 of its own unit tests, which are compiled and run here too.
//!   Upstream's allocator test suite therefore becomes part of scxsim's, and
//!   an upstream change that breaks its own invariants fails our build.
//!
//! # The one vendored piece
//!
//! `alloc.rs` does `use crate::largest_remainder;`, and that helper lives in
//! `scx_layered/src/lib.rs` — a 3533-line module that pulls in the generated
//! BPF skeleton and cannot be included here. It is a ~30-line pure function,
//! reproduced below verbatim with its provenance recorded.
//!
//! Because a silent copy is a copy that drifts, `tests/layered_alloc.rs`
//! re-reads the upstream `lib.rs` at test time and asserts our copy is still
//! token-identical to it. If upstream edits `largest_remainder`, that test
//! fails rather than the two quietly disagreeing.

/// Distribute `total` across `quotas` by the largest-remainder method.
///
/// VENDORED VERBATIM from `scx/scheds/rust/scx_layered/src/lib.rs`
/// (`pub fn largest_remainder`). Do not edit: `tests/layered_alloc.rs`
/// asserts this body still matches upstream token-for-token, so an edit here
/// registers as drift. If upstream changes, re-copy rather than patch.
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

// The upstream allocator itself is declared in `safe/mod.rs` as
// `#[path = "<scx>/scx_layered/src/alloc.rs"] pub mod layered_alloc_upstream;`
// and re-exported from the crate root. It has to be declared from a `mod.rs`
// so the `#[path]` resolves relative to `safe/` rather than to a
// `layered_alloc/` subdirectory that does not exist.
//
// It is a module rather than an `include!` because the upstream file opens
// with `//!` inner doc comments, which are only legal at the top of a module.
