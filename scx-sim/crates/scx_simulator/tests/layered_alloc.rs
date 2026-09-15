//! Guards on scx_layered's real allocator being compiled into scxsim.
//!
//! `safe/layered_alloc_upstream` is `scx_layered/src/alloc.rs` compiled
//! verbatim, so that Tier-3 CPU reallocation runs layered's actual policy
//! instead of a re-implementation of it. Two things need guarding:
//!
//! 1. That it really is the upstream allocator and it really runs (the ~80
//!    upstream unit tests inside the module cover its internals; the test
//!    here covers the public entry point scxsim will drive).
//! 2. That the one helper we had to vendor has not drifted from upstream.

use scx_simulator::{unified_alloc, LayerDemand};

/// Repo root, derived from the crate manifest rather than hardcoded.
fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .expect("repo root")
}

/// Extract `fn <name>` through its closing brace, assuming rustfmt'd input
/// (the closing brace of a top-level fn is the first `\n}` after the start).
///
/// Anchored at line start so a mention of the name inside a doc comment is
/// not mistaken for the definition — the first version of this matched the
/// provenance comment in `layered_alloc.rs` and compared docs against code.
fn extract_fn(src: &str, name: &str) -> String {
    let needle = format!("\npub fn {name}");
    let start = src
        .find(&needle)
        .map(|i| i + 1)
        .unwrap_or_else(|| panic!("{name} definition not found"));
    let rest = &src[start..];
    let end = rest.find("\n}").expect("unterminated fn") + 2;
    rest[..end].to_string()
}

/// Collapse whitespace so pure reformatting is not reported as drift.
fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ---------------------------------------------------------------------------
// Drift guard
// ---------------------------------------------------------------------------

/// `alloc.rs` needs `crate::largest_remainder`, which lives in upstream's
/// `lib.rs` alongside the BPF skeleton bindings scxsim cannot build. It is
/// therefore the single piece of scx_layered we copy rather than compile.
///
/// A silent copy drifts, so compare it against upstream at test time. If this
/// fails after an scx submodule bump, re-copy the upstream body into
/// `safe/layered_alloc.rs` — do not patch one side.
#[test]
fn vendored_largest_remainder_matches_upstream() {
    let upstream_path = repo_root().join("scx/scheds/rust/scx_layered/src/lib.rs");
    let ours_path = repo_root().join("scx-sim/crates/scx_simulator/src/safe/layered_alloc.rs");

    let upstream = std::fs::read_to_string(&upstream_path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", upstream_path.display()));
    let ours = std::fs::read_to_string(&ours_path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", ours_path.display()));

    let upstream_fn = extract_fn(&upstream, "largest_remainder");
    let ours_fn = extract_fn(&ours, "largest_remainder");

    assert_eq!(
        normalize(&ours_fn),
        normalize(&upstream_fn),
        "the vendored copy of largest_remainder has drifted from\n  {}\nRe-copy \
         the upstream body into safe/layered_alloc.rs rather than patching \
         either side.",
        upstream_path.display()
    );
}

/// Sanity check that the drift guard can actually fail — otherwise a broken
/// extractor would make the guard vacuous.
#[test]
fn drift_guard_detects_a_difference() {
    // Leading newline: extract_fn anchors definitions at line start, and a
    // real source file always has one before a top-level fn.
    let a = "\npub fn largest_remainder(total: usize) -> usize {\n    total\n}";
    let b = "\npub fn largest_remainder(total: usize) -> usize {\n    total + 1\n}";
    assert_eq!(normalize(&extract_fn(a, "largest_remainder")), normalize(a));

    // And it must ignore a doc-comment mention rather than treating it as the
    // definition — the bug the first version of this extractor had.
    let doc_then_def = concat!(
        "/// see `pub fn largest_remainder` for details\n",
        "pub fn largest_remainder(total: usize) -> usize {\n    total\n}"
    );
    assert!(
        !extract_fn(doc_then_def, "largest_remainder").contains("see `"),
        "extractor matched a doc-comment mention instead of the definition"
    );
    assert_ne!(
        normalize(&extract_fn(a, "largest_remainder")),
        normalize(&extract_fn(b, "largest_remainder")),
        "the extractor collapses differing bodies to the same text — the \
         drift guard would never fire"
    );
}

// ---------------------------------------------------------------------------
// The allocator is real and reachable
// ---------------------------------------------------------------------------

/// `unified_alloc` is the entry point the Tier-3 control loop will call. Drive
/// it directly and assert the water-fill property that makes it worth linking:
/// a pool split between two layers of equal demand but unequal weight goes to
/// the heavier layer in proportion, not evenly.
#[test]
fn unified_alloc_splits_by_weight() {
    let demands = vec![
        LayerDemand {
            raw_pinned: vec![0],
            raw_unpinned: 8,
            weight: 300,
            spread: false,
        },
        LayerDemand {
            raw_pinned: vec![0],
            raw_unpinned: 8,
            weight: 100,
            spread: false,
        },
    ];
    let node_groups = vec![vec![vec![0usize]], vec![vec![0usize]]];
    let allocs = unified_alloc(8, &[8], &demands, &node_groups);

    assert_eq!(allocs.len(), 2);
    let heavy: usize = allocs[0].unpinned.iter().sum();
    let light: usize = allocs[1].unpinned.iter().sum();
    assert_eq!(
        heavy + light,
        8,
        "water-fill must distribute the whole pool, got {heavy} + {light}"
    );
    assert!(
        heavy > light,
        "the 3x-weight layer should get the larger share, got {heavy} vs {light}"
    );
}

/// Demand caps beat weight: a heavy layer that only wants 1 unit must not be
/// handed the pool. Guards against the loop later mistaking "weight" for
/// "entitlement" when it builds `LayerDemand`s from measured utilisation.
#[test]
fn unified_alloc_respects_demand_caps() {
    let demands = vec![
        LayerDemand {
            raw_pinned: vec![0],
            raw_unpinned: 1,
            weight: 1000,
            spread: false,
        },
        LayerDemand {
            raw_pinned: vec![0],
            raw_unpinned: 8,
            weight: 100,
            spread: false,
        },
    ];
    let node_groups = vec![vec![vec![0usize]], vec![vec![0usize]]];
    let allocs = unified_alloc(8, &[8], &demands, &node_groups);

    let capped: usize = allocs[0].unpinned.iter().sum();
    let rest: usize = allocs[1].unpinned.iter().sum();
    assert_eq!(capped, 1, "a layer demanding 1 unit must not exceed it");
    assert_eq!(rest, 7, "the excess must flow to the layer that can use it");
}
