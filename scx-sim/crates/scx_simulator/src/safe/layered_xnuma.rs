//! scx_layered's cross-NUMA migration gate, as upstream computes it.
//!
//! # Why this file exists
//!
//! `layers[l].node[src].xnuma[dst].rate` and `.xnuma_is_mig_src` are the two
//! fields that decide whether scx_layered may place or consume work across a
//! NUMA boundary. Both are written ONLY by userspace, every control-loop
//! iteration, from `Scheduler::refresh_xnuma()` in `scx_layered/src/main.rs`.
//! The BPF side reads them and reads nothing else:
//!
//! * `pick_idle_cpu()` skips its whole remote-node proximity walk unless
//!   `xnuma_is_mig_src(layer, src_nid)` (`main.bpf.c`, the `goto xnuma_done`
//!   guarded by that call);
//! * the remote-LLC consume loop in `try_consume_layer()` skips any LLC on
//!   another node unless the same flag AND `xnuma_gate()` pass;
//! * `xnuma_gate()` treats `rate == 0` as *deny* and `rate == u64::MAX` as
//!   *gating off, always allow*.
//!
//! Leaving those fields at their BSS zero therefore does not mean "no
//! policy": it means "cross-NUMA migration is permanently forbidden, in both
//! directions, on every layer". That is what scxsim did before this module
//! existed, and it is mb **sim-dox34** — with `nr_nodes > 1` every task was
//! born on node 0 (see [`crate::scenario::ForkPlacement`]) and could never
//! leave it, so `cpus_that_ran` came out at exactly `nr_cpus / nr_nodes`.
//!
//! # Vendored verbatim, with a drift guard
//!
//! The two functions that carry the policy — [`xnuma_check_active`] and
//! [`xnuma_compute_rates`] — are copied from upstream `main.rs` token for
//! token, the same way `layered_alloc.rs` copies `largest_remainder`. They
//! cannot be `#[path]`-included the way `alloc.rs` is: they live in the
//! middle of a 5000-line `main.rs` that pulls in the generated BPF skeleton.
//!
//! Because a silent copy drifts, `tests/layered_xnuma.rs` re-reads upstream's
//! `main.rs` at test time and asserts these bodies are still token-identical.
//! If upstream edits them, that test fails rather than the two quietly
//! disagreeing. Re-copy the upstream body; do not patch one side.
//!
//! What is NOT vendored here is `refresh_xnuma()` itself, because it is a
//! method on upstream's `Scheduler` that reaches into the libbpf skeleton.
//! Its *logic* is reproduced in
//! [`crate::layered_control::LayeredControl::step`], writing through
//! `layered_set_xnuma_*` instead of through `skel.maps.bss_data`; the
//! branch structure is annotated there against upstream line by line.

/// Fixed-point scale applied to duty cycles before they are stored in a
/// `struct xnuma_bucket`.
///
/// VENDORED VERBATIM from `scx/scheds/rust/scx_layered/src/main.rs`
/// (`const DUTY_CYCLE_SCALE`). `tests/layered_xnuma.rs` guards it.
pub const DUTY_CYCLE_SCALE: f64 = (1u64 << 20) as f64;

/// Fraction of a node's surplus that may migrate in one control cycle.
///
/// VENDORED VERBATIM from `scx/scheds/rust/scx_layered/src/main.rs`
/// (`const XNUMA_RATE_DAMPEN`). `tests/layered_xnuma.rs` guards it.
pub const XNUMA_RATE_DAMPEN: f64 = 0.5;

/// Result of xnuma water-fill computation for a single layer.
///
/// VENDORED VERBATIM from `scx/scheds/rust/scx_layered/src/main.rs`
/// (`struct XnumaRates`), with `pub` added so scxsim can name it across
/// module boundaries. Upstream keeps it crate-private.
pub struct XnumaRates {
    /// rates[src][dst]: migration rate in duty-cycle-scaled units.
    pub rates: Vec<Vec<u64>>,
}

/// Determine per-node migration source state with two-threshold hysteresis.
///
/// Each (layer, node) independently decides if it's a migration source.
/// Open (is_mig_src=true) requires all three:
///   1. load/alloc > threshold.1 (significant load)
///   2. surplus/alloc > delta.1 (significant imbalance)
///   3. growth_denied (allocation can't solve it)
///
/// Close (is_mig_src=false) when any one:
///   1. load/alloc < threshold.0 (load dropped)
///   2. surplus/alloc < delta.0 (imbalance resolved)
///   3. !growth_denied (growth succeeded)
///
/// VENDORED VERBATIM from `scx/scheds/rust/scx_layered/src/main.rs`
/// (`fn xnuma_check_active`), `pub` added. Do not edit:
/// `tests/layered_xnuma.rs` asserts this body still matches upstream
/// token-for-token, so an edit here registers as drift.
pub fn xnuma_check_active(
    duty_sums: &[f64],
    allocs: &[usize],
    threshold: (f64, f64),
    threshold_delta: (f64, f64),
    growth_denied: &[bool],
    currently_active: &[bool],
) -> Vec<bool> {
    let nr_nodes = duty_sums.len();
    let total_duty: f64 = duty_sums.iter().sum();
    let total_alloc: f64 = allocs.iter().map(|&a| a as f64).sum();
    let eq_ratio = if total_alloc > 0.0 {
        total_duty / total_alloc
    } else {
        0.0
    };

    let (thresh_lo, thresh_hi) = threshold;
    let (delta_lo, delta_hi) = threshold_delta;

    let mut result = vec![false; nr_nodes];
    for nid in 0..nr_nodes {
        let alloc = allocs[nid] as f64;
        if alloc <= 0.0 {
            if duty_sums[nid] > 0.0 && growth_denied[nid] {
                result[nid] = true;
            }
            continue;
        }

        let load_ratio = duty_sums[nid] / alloc;
        let surplus = duty_sums[nid] - eq_ratio * alloc;
        let surplus_ratio = surplus / alloc;

        let should_activate =
            load_ratio > thresh_hi && surplus_ratio > delta_hi && growth_denied[nid];
        let should_deactivate =
            load_ratio < thresh_lo || surplus_ratio < delta_lo || !growth_denied[nid];

        if should_activate {
            result[nid] = true;
        } else if should_deactivate {
            result[nid] = false;
        } else {
            result[nid] = currently_active[nid];
        }
    }
    result
}

/// Compute water-fill migration rates for a single layer.
///
/// Finds the equalization ratio (water line) across all nodes, then
/// computes per-(src, dst) migration rates proportional to each source's
/// surplus and each destination's share of total deficit.
///
/// VENDORED VERBATIM from `scx/scheds/rust/scx_layered/src/main.rs`
/// (`fn xnuma_compute_rates`), `pub` added. Do not edit:
/// `tests/layered_xnuma.rs` asserts this body still matches upstream
/// token-for-token, so an edit here registers as drift.
pub fn xnuma_compute_rates(duty_sums: &[f64], allocs: &[usize]) -> XnumaRates {
    let nr_nodes = duty_sums.len();
    let total_duty: f64 = duty_sums.iter().sum();
    let total_alloc: f64 = allocs.iter().map(|&a| a as f64).sum();

    if total_alloc <= 0.0 {
        return XnumaRates {
            rates: vec![vec![0u64; nr_nodes]; nr_nodes],
        };
    }

    let eq_ratio = total_duty / total_alloc;

    let mut surpluses = vec![0.0f64; nr_nodes];
    let mut deficits = vec![0.0f64; nr_nodes];
    for nid in 0..nr_nodes {
        let expected = eq_ratio * allocs[nid] as f64;
        let delta = duty_sums[nid] - expected;
        if delta > 0.0 {
            surpluses[nid] = delta;
        } else {
            deficits[nid] = -delta;
        }
    }

    let total_deficit: f64 = deficits.iter().sum();

    let mut rates = vec![vec![0u64; nr_nodes]; nr_nodes];
    for src in 0..nr_nodes {
        for dst in 0..nr_nodes {
            if src == dst || total_deficit <= 0.0 || surpluses[src] <= 0.0 {
                continue;
            }
            // Dampen: transfer half the surplus per cycle so convergence
            // is gradual rather than a single-step overcorrection.
            let migration = surpluses[src] * deficits[dst] / total_deficit * XNUMA_RATE_DAMPEN;
            rates[src][dst] = (migration * DUTY_CYCLE_SCALE) as u64;
        }
    }

    XnumaRates { rates }
}
