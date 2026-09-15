//! Guards on scx_layered's real cross-NUMA gate policy being compiled into
//! scxsim.
//!
//! `safe/layered_xnuma.rs` carries `xnuma_check_active` and
//! `xnuma_compute_rates` copied token-for-token from upstream's `main.rs`.
//! They cannot be `#[path]`-included the way `alloc.rs` is (they sit in the
//! middle of a 5000-line file that pulls in the generated BPF skeleton), so
//! they are vendored — and a silent copy drifts. Three things need guarding:
//!
//! 1. that the vendored bodies still match upstream;
//! 2. that the two constants they depend on still match;
//! 3. that the drift guard can actually fail, so it is not vacuous.
//!
//! Plus a behavioural check that the policy is reachable and does what makes
//! it worth vendoring rather than approximating.

use scx_simulator::{xnuma_check_active, xnuma_compute_rates, DUTY_CYCLE_SCALE, XNUMA_RATE_DAMPEN};

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
/// Matches both `fn` and `pub fn`: upstream keeps these crate-private, and
/// the vendored copies are `pub` so scxsim can name them across modules. That
/// one-word difference is the only edit allowed, and it is why the signature
/// line is compared with the keyword stripped.
fn extract_fn(src: &str, name: &str) -> String {
    let start = ["\npub fn ", "\nfn "]
        .iter()
        .find_map(|kw| src.find(&format!("{kw}{name}")).map(|i| i + 1))
        .unwrap_or_else(|| panic!("{name} definition not found"));
    let rest = &src[start..];
    let end = rest.find("\n}").expect("unterminated fn") + 2;
    rest[..end].to_string()
}

/// Collapse whitespace, and erase the `pub` the vendored copies add, so that
/// neither reformatting nor the visibility change reads as drift.
fn normalize(s: &str) -> String {
    s.split_whitespace()
        .filter(|t| *t != "pub")
        .collect::<Vec<_>>()
        .join(" ")
}

fn upstream_main_rs() -> String {
    let p = repo_root().join("scx/scheds/rust/scx_layered/src/main.rs");
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

fn ours() -> String {
    let p = repo_root().join("scx-sim/crates/scx_simulator/src/safe/layered_xnuma.rs");
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

// ---------------------------------------------------------------------------
// Drift guards
// ---------------------------------------------------------------------------

/// The two policy functions must still be upstream's, token for token.
///
/// If this fails after an scx submodule bump, re-copy the upstream bodies
/// into `safe/layered_xnuma.rs` — do not patch one side. And read the diff
/// first: a change here changes when scx_layered is allowed to move work
/// across a socket, which is the behaviour `tests/numa_topology.rs` asserts.
#[test]
fn vendored_xnuma_policy_matches_upstream() {
    let upstream = upstream_main_rs();
    let ours = ours();
    for name in ["xnuma_check_active", "xnuma_compute_rates"] {
        assert_eq!(
            normalize(&extract_fn(&ours, name)),
            normalize(&extract_fn(&upstream, name)),
            "the vendored copy of {name} has drifted from upstream main.rs. \
             Re-copy the upstream body into safe/layered_xnuma.rs rather than \
             patching either side."
        );
    }
}

/// The two constants those functions read must match too. They are separate
/// `const` items, so the function-body comparison above does not cover them,
/// and `XNUMA_RATE_DAMPEN` in particular scales every rate the gate publishes.
#[test]
fn vendored_xnuma_constants_match_upstream() {
    let upstream = upstream_main_rs();
    for (name, ours) in [
        ("DUTY_CYCLE_SCALE", DUTY_CYCLE_SCALE),
        ("XNUMA_RATE_DAMPEN", XNUMA_RATE_DAMPEN),
    ] {
        let line = upstream
            .lines()
            .find(|l| l.trim_start().starts_with(&format!("const {name}: f64")))
            .unwrap_or_else(|| panic!("upstream no longer declares `const {name}: f64`"));
        let rhs = line
            .split('=')
            .nth(1)
            .and_then(|r| r.split(';').next())
            .expect("malformed const")
            .trim();
        // Evaluate the two forms upstream actually uses rather than eval'ing
        // arbitrary Rust: a literal, or `(1u64 << N) as f64`.
        let want: f64 = if let Some(shift) = rhs
            .strip_prefix("(1u64 <<")
            .and_then(|r| r.split(')').next())
            .and_then(|n| n.trim().parse::<u32>().ok())
        {
            (1u64 << shift) as f64
        } else {
            rhs.parse().unwrap_or_else(|_| {
                panic!("upstream `{name}` is now `{rhs}`, which this guard cannot evaluate")
            })
        };
        assert_eq!(
            ours, want,
            "vendored {name} is {ours} but upstream now says {rhs}"
        );
    }
}

/// The upstream defaults the scxsim `LayerSpec` adopts must still be the
/// upstream defaults.
///
/// These live in `config.rs`, not `main.rs`, and a change to either would
/// silently retune every multi-node simulation.
#[test]
fn xnuma_threshold_defaults_match_upstream_config() {
    let p = repo_root().join("scx/scheds/rust/scx_layered/src/config.rs");
    let src = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {p:?}: {e}"));
    for (fname, ours) in [
        (
            "default_xnuma_threshold",
            scx_simulator::DEFAULT_XNUMA_THRESHOLD,
        ),
        (
            "default_xnuma_threshold_delta",
            scx_simulator::DEFAULT_XNUMA_THRESHOLD_DELTA,
        ),
    ] {
        let body = extract_fn(&src, fname);
        let want = format!("({}, {})", ours.0, ours.1);
        assert!(
            normalize(&body).contains(&want),
            "scxsim's default for {fname} is {want}, but upstream's body is:\n{body}"
        );
    }
}

/// The drift guard must be able to fail — otherwise a broken extractor makes
/// every assertion above vacuous.
#[test]
fn drift_guard_detects_a_difference() {
    let a = "\nfn xnuma_check_active(n: usize) -> usize {\n    n\n}";
    let b = "\nfn xnuma_check_active(n: usize) -> usize {\n    n + 1\n}";
    assert_ne!(
        normalize(&extract_fn(a, "xnuma_check_active")),
        normalize(&extract_fn(b, "xnuma_check_active")),
        "the extractor collapses differing bodies to the same text"
    );
    // `pub` on our side must NOT read as drift, or the guard cries wolf on
    // every run and gets muted.
    let theirs = "\nfn xnuma_check_active(n: usize) -> usize {\n    n\n}";
    let mine = "\npub fn xnuma_check_active(n: usize) -> usize {\n    n\n}";
    assert_eq!(
        normalize(&extract_fn(theirs, "xnuma_check_active")),
        normalize(&extract_fn(mine, "xnuma_check_active"))
    );
    // A doc-comment mention must not be mistaken for the definition.
    let doc_then_def = concat!(
        "/// see `fn xnuma_check_active` for details\n",
        "fn xnuma_check_active(n: usize) -> usize {\n    n\n}"
    );
    assert!(
        !extract_fn(doc_then_def, "xnuma_check_active").contains("see `"),
        "extractor matched a doc-comment mention instead of the definition"
    );
}

// ---------------------------------------------------------------------------
// The policy is real and reachable
// ---------------------------------------------------------------------------

/// Hysteresis: activation needs all three of high load, high surplus and
/// growth denied; deactivation needs any one to lapse.
///
/// This is the property that makes the gate a gate rather than a switch, and
/// it is the one an approximation would most plausibly get wrong (by using a
/// single threshold).
#[test]
fn activation_requires_growth_denied_and_deactivation_does_not() {
    // Node 0 heavily loaded, node 1 empty, 4 CPUs each.
    let duty = [8.0, 0.0];
    let allocs = [4usize, 4];
    let threshold = (0.6, 0.7);
    let delta = (0.2, 0.3);

    // Growth was denied on node 0 → it opens as a migration source.
    let opened = xnuma_check_active(&duty, &allocs, threshold, delta, &[true, true], &[false; 2]);
    assert_eq!(opened, vec![true, false]);

    // Same load, but the allocator DID grow node 0 → it must not open, even
    // though the load and surplus conditions are unchanged.
    let closed = xnuma_check_active(
        &duty,
        &allocs,
        threshold,
        delta,
        &[false, false],
        &[false; 2],
    );
    assert_eq!(
        closed,
        vec![false, false],
        "growth_denied is a required conjunct for activation"
    );

    // And an already-open node closes as soon as growth succeeds.
    let reclosed = xnuma_check_active(
        &duty,
        &allocs,
        threshold,
        delta,
        &[false, false],
        &[true, false],
    );
    assert_eq!(reclosed, vec![false, false]);
}

/// Water-fill: the rate out of a surplus node is its surplus times the
/// destination's share of total deficit, halved by `XNUMA_RATE_DAMPEN` and
/// scaled by `DUTY_CYCLE_SCALE`.
#[test]
fn compute_rates_water_fills_from_surplus_to_deficit() {
    // 8 CPU-seconds of demand on node 0, none on node 1, 4 CPUs each. The
    // water line is 8/8 = 1.0 per CPU, so node 0 has 4 surplus and node 1 has
    // 4 deficit.
    let result = xnuma_compute_rates(&[8.0, 0.0], &[4, 4]);
    let expected = (4.0 * XNUMA_RATE_DAMPEN * DUTY_CYCLE_SCALE) as u64;
    assert_eq!(result.rates[0][1], expected);
    assert_eq!(result.rates[1][0], 0, "the deficit node is not a source");
    assert_eq!(result.rates[0][0], 0, "self-migration has no rate");
}

/// A balanced machine publishes no budget at all, which is what keeps a
/// healthy multi-node run from paying for pointless cross-socket traffic.
#[test]
fn a_balanced_machine_gets_no_migration_budget() {
    let result = xnuma_compute_rates(&[4.0, 4.0], &[4, 4]);
    assert_eq!(result.rates[0][1], 0);
    assert_eq!(result.rates[1][0], 0);
}
