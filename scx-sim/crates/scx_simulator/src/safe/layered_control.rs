//! scx_layered's periodic userspace CPU-allocation control loop.
//!
//! Target calculation and shrink/grow ordering mirror `main.rs`; CPU budgets
//! come from the real upstream `unified_alloc()`, while core and node ordering
//! execute the real upstream `layer_core_growth.rs` through the
//! `scx_layered_growth` topology adapter.

use crate::layered::{LayerGrowthAlgo, LayerKind, LayerSpec};
use crate::layered_alloc_upstream::{unified_alloc, LayerDemand};
use crate::layered_xnuma::{xnuma_check_active, xnuma_compute_rates};
use scx_layered_growth::layer_core_growth;
use scx_layered_growth::{algorithm_from_bpf, CpuPool, LayerSpec as GrowthSpec, Topology};

const USAGE_HALF_LIFE_NS: f64 = 100_000_000.0;

/// Cumulative BPF runtime counters and current CPU masks sampled at one tick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayeredControlSnapshot {
    pub usages: Vec<[u64; 2]>,
    pub node_usages: Vec<Vec<u64>>,
    pub node_pinned_usages: Vec<Vec<u64>>,
    /// Per-(layer, node) cumulative `cpu_ctx.layer_duty_sum`, the smoothed
    /// runnable time the BPF side accumulates in `layered_stopping()`. Unlike
    /// `node_usages` it counts queue wait as well as CPU time, so at
    /// saturation it exceeds utilisation — which is precisely the signal
    /// upstream's cross-NUMA gate rebalances on.
    pub node_duty_raw: Vec<Vec<u64>>,
    pub cpu_masks: Vec<Vec<bool>>,
}

/// Result of one userspace control-loop iteration.
#[derive(Clone, Debug, PartialEq)]
pub struct LayeredControlUpdate {
    pub cpu_masks: Vec<Vec<bool>>,
    pub targets: Vec<usize>,
    pub growth_denied: Vec<Vec<bool>>,
    /// `[layer][src][dst]` cross-NUMA migration budget, in `xnuma_gate()`'s
    /// units: `u64::MAX` = gating off, `0` = deny, else a token-bucket rate.
    pub xnuma_rates: Vec<Vec<Vec<u64>>>,
    /// `[layer][node]` — whether that node may act as a cross-NUMA migration
    /// source at all. `pick_idle_cpu()` and `try_consume_layer()` both check
    /// this before they check the budget.
    pub xnuma_mig_src: Vec<Vec<bool>>,
    /// `[layer][node]` EWMA duty sums, in CPU units, exposed so tests can
    /// assert on the input the gate decided from rather than only its output.
    pub node_duty_sums: Vec<Vec<f64>>,
}

/// Stateful utilization EWMA and CPU allocation driver.
pub struct LayeredControl {
    period_ns: u64,
    nr_cpus: usize,
    nr_nodes: usize,
    threads_per_core: usize,
    specs: Vec<LayerSpec>,
    previous_usages: Vec<[u64; 2]>,
    previous_node_usages: Vec<Vec<u64>>,
    previous_node_pinned_usages: Vec<Vec<u64>>,
    layer_utils: Vec<[f64; 2]>,
    layer_node_utils: Vec<Vec<f64>>,
    layer_node_pinned_utils: Vec<Vec<f64>>,
    core_cpus: Vec<Vec<usize>>,
    core_nodes: Vec<usize>,
    /// Per-core LLC id, the `llcs` half of upstream's `Layer::allowed_cpus`.
    core_llcs: Vec<usize>,
    llc_nodes: Vec<usize>,
    core_orders: Vec<Vec<Vec<usize>>>,
    node_orders: Vec<Vec<usize>>,
    node_groups: Vec<Vec<Vec<usize>>>,
    growth_denied: Vec<Vec<bool>>,
    growth_denied_counts: Vec<Vec<u64>>,
    previous_node_duty_raw: Vec<Vec<u64>>,
    layer_node_duty_sums: Vec<Vec<f64>>,
    xnuma_mig_src: Vec<Vec<bool>>,
}

impl LayeredControl {
    pub fn new(
        period_ns: u64,
        nr_cpus: usize,
        cpus_per_llc: usize,
        nr_nodes: usize,
        threads_per_core: usize,
        specs: Vec<LayerSpec>,
    ) -> Self {
        assert!(period_ns > 0, "layered control period must be positive");
        assert!(nr_cpus > 0, "layered control needs at least one CPU");
        assert!(threads_per_core > 0);
        for spec in &specs {
            if spec.kind == LayerKind::Open {
                continue;
            }
            assert!(
                spec.cpus.is_none(),
                "Tier-3 control cannot resize explicitly pinned layer {:?}",
                spec.name
            );
            let (low, high) = spec.util_range.unwrap_or_else(|| {
                panic!(
                    "Tier-3 control requires util_range for non-open layer {:?}",
                    spec.name
                )
            });
            assert!(
                low >= 0.0 && low < high,
                "invalid util range for {:?}",
                spec.name
            );
            if let Some((min, max)) = spec.cpus_range {
                assert!(
                    min <= max && max <= nr_cpus,
                    "invalid CPU range for {:?}",
                    spec.name
                );
            }
            assert!(
                !matches!(
                    spec.growth_algo,
                    LayerGrowthAlgo::CpuSetSpread
                        | LayerGrowthAlgo::CpuSetSpreadReverse
                        | LayerGrowthAlgo::CpuSetSpreadRandom
                ),
                "{:?} requires cgroup-cpuset topology, which scxsim does not model",
                spec.growth_algo
            );
        }

        let topology = Topology::simulated(nr_cpus, cpus_per_llc, nr_nodes, threads_per_core)
            .unwrap_or_else(|e| panic!("invalid layered control topology: {e:#}"));
        let nr_nodes = topology.nodes.len();
        let nr_llcs = nr_cpus.div_ceil(cpus_per_llc);
        assert!(
            nr_llcs == 1
                || specs
                    .iter()
                    .all(|s| s.growth_algo != LayerGrowthAlgo::StickyDynamic),
            "StickyDynamic on multiple LLCs requires production's runtime LLC-trading loop"
        );
        for spec in &specs {
            assert!(spec.nodes.iter().all(|&n| n < nr_nodes));
            assert!(spec.llcs.iter().all(|&l| l < nr_llcs));
        }
        let growth_specs: Vec<GrowthSpec> = specs
            .iter()
            .map(|spec| {
                GrowthSpec::new(
                    algorithm_from_bpf(spec.growth_algo as i32)
                        .expect("LayerGrowthAlgo ABI value is invalid"),
                )
                .with_nodes(spec.nodes.clone())
                .with_llcs(spec.llcs.clone())
            })
            .collect();
        let cpu_pool = CpuPool;
        let orders = scx_layered_growth::LayerGrowthAlgo::layer_core_orders(
            &cpu_pool,
            &growth_specs,
            &topology,
        )
        .unwrap_or_else(|e| panic!("upstream layer core ordering failed: {e:#}"));
        let core_orders = (0..specs.len()).map(|idx| orders[&idx].clone()).collect();
        let all_layer_nodes: Vec<&[usize]> = growth_specs
            .iter()
            .map(|spec| spec.nodes().as_slice())
            .collect();
        let node_orders = growth_specs
            .iter()
            .enumerate()
            .map(|(idx, spec)| {
                layer_core_growth::node_order(spec.nodes(), &topology, idx, &all_layer_nodes)
            })
            .collect();
        let node_groups = growth_specs
            .iter()
            .enumerate()
            .map(|(idx, spec)| {
                layer_core_growth::node_groups(
                    spec.nodes(),
                    &topology,
                    idx,
                    &all_layer_nodes,
                    &spec.kind.common().growth_algo,
                )
            })
            .collect();
        let core_cpus: Vec<Vec<usize>> = topology
            .all_cores
            .values()
            .map(|core| core.cpus.keys().copied().collect())
            .collect();
        let core_nodes: Vec<usize> = topology
            .all_cores
            .values()
            .map(|core| core.node_id)
            .collect();
        let core_llcs: Vec<usize> = topology
            .all_cores
            .values()
            .map(|core| core.llc_id)
            .collect();
        let llc_nodes = (0..nr_llcs)
            .map(|llc| {
                topology
                    .all_cores
                    .values()
                    .find(|core| core.llc_id == llc)
                    .expect("LLC without any core")
                    .node_id
            })
            .collect();
        let nr_layers = specs.len();
        Self {
            period_ns,
            nr_cpus,
            nr_nodes,
            threads_per_core,
            specs,
            previous_usages: vec![[0; 2]; nr_layers],
            previous_node_usages: vec![vec![0; nr_nodes]; nr_layers],
            previous_node_pinned_usages: vec![vec![0; nr_nodes]; nr_layers],
            layer_utils: vec![[0.0; 2]; nr_layers],
            layer_node_utils: vec![vec![0.0; nr_nodes]; nr_layers],
            layer_node_pinned_utils: vec![vec![0.0; nr_nodes]; nr_layers],
            core_cpus,
            core_nodes,
            core_llcs,
            llc_nodes,
            core_orders,
            node_orders,
            node_groups,
            growth_denied: vec![vec![false; nr_nodes]; nr_layers],
            growth_denied_counts: vec![vec![0; nr_nodes]; nr_layers],
            previous_node_duty_raw: vec![vec![0; nr_nodes]; nr_layers],
            layer_node_duty_sums: vec![vec![0.0; nr_nodes]; nr_layers],
            xnuma_mig_src: vec![vec![false; nr_nodes]; nr_layers],
        }
    }

    pub fn period_ns(&self) -> u64 {
        self.period_ns
    }

    /// Run one production-shaped control iteration.
    pub fn step(&mut self, snapshot: LayeredControlSnapshot) -> LayeredControlUpdate {
        assert_eq!(snapshot.usages.len(), self.specs.len());
        assert_eq!(snapshot.node_usages.len(), self.specs.len());
        assert_eq!(snapshot.node_pinned_usages.len(), self.specs.len());
        assert_eq!(snapshot.cpu_masks.len(), self.specs.len());
        assert!(snapshot.cpu_masks.iter().all(|m| m.len() == self.nr_cpus));
        assert!(snapshot
            .node_usages
            .iter()
            .all(|usage| usage.len() == self.nr_nodes));
        assert!(snapshot
            .node_pinned_usages
            .iter()
            .all(|usage| usage.len() == self.nr_nodes));

        let elapsed = self.period_ns as f64 / 1_000_000_000.0;
        let decay = 0.5f64.powf(self.period_ns as f64 / USAGE_HALF_LIFE_NS);
        for (idx, current) in snapshot.usages.iter().enumerate() {
            for (usage, &current_usage) in current.iter().enumerate() {
                let delta = current_usage.saturating_sub(self.previous_usages[idx][usage]);
                let instantaneous = delta as f64 / 1_000_000_000.0 / elapsed;
                self.layer_utils[idx][usage] =
                    self.layer_utils[idx][usage] * decay + instantaneous * (1.0 - decay);
            }
        }
        self.previous_usages = snapshot.usages;
        Self::update_node_utils(
            self.period_ns,
            &snapshot.node_usages,
            &mut self.previous_node_usages,
            &mut self.layer_node_utils,
        );
        Self::update_node_utils(
            self.period_ns,
            &snapshot.node_pinned_usages,
            &mut self.previous_node_pinned_usages,
            &mut self.layer_node_pinned_utils,
        );
        // `sched_stats.layer_node_duty_sums`, same shape as the two above:
        // upstream `main.rs` runs `metric_decay(compute_diff(cur, prev))` over
        // all three with the same `USAGE_DECAY`.
        Self::update_node_utils(
            self.period_ns,
            &snapshot.node_duty_raw,
            &mut self.previous_node_duty_raw,
            &mut self.layer_node_duty_sums,
        );

        let raw_targets: Vec<(usize, usize)> = self
            .specs
            .iter()
            .enumerate()
            .map(|(idx, spec)| self.raw_target(idx, spec, &snapshot.cpu_masks[idx]))
            .collect();
        let dampened: Vec<(usize, usize)> = raw_targets
            .iter()
            .enumerate()
            .map(|(idx, &(target, min))| {
                let current = snapshot.cpu_masks[idx].iter().filter(|&&set| set).count();
                if target < current {
                    // Mirrors main.rs::refresh_cpumasks(): shrink only halfway
                    // per cycle, but never below `min`. The `.max(min)` is
                    // upstream's and was missing here.
                    let dampened = current - (current - target).div_ceil(2);
                    (dampened.max(min), min)
                } else {
                    (target, min)
                }
            })
            .collect();

        let au = self.threads_per_core;
        let demands: Vec<LayerDemand> = self
            .specs
            .iter()
            .enumerate()
            .map(|(idx, spec)| {
                if spec.kind == LayerKind::Open {
                    return LayerDemand {
                        raw_pinned: vec![0; self.nr_nodes],
                        raw_unpinned: 0,
                        weight: spec.weight as usize,
                        spread: false,
                    };
                }
                let util_high = spec.util_range.expect("validated").1;
                let mut raw_pinned = vec![0; self.nr_nodes];
                for (node, pinned) in raw_pinned.iter_mut().enumerate() {
                    let pinned_util = self.layer_node_pinned_utils[idx][node];
                    if pinned_util >= 0.01 && self.node_allowed(spec, node) {
                        *pinned = ((pinned_util / util_high).ceil() as usize).div_ceil(au);
                    }
                }
                let target_units = dampened[idx].0.div_ceil(au);
                let pinned_units = raw_pinned.iter().sum::<usize>();
                LayerDemand {
                    raw_pinned,
                    raw_unpinned: target_units.saturating_sub(pinned_units),
                    weight: spec.weight as usize,
                    spread: matches!(
                        spec.growth_algo,
                        LayerGrowthAlgo::NodeSpread
                            | LayerGrowthAlgo::NodeSpreadReverse
                            | LayerGrowthAlgo::NodeSpreadRandom
                    ),
                }
            })
            .collect();
        let node_caps: Vec<usize> = (0..self.nr_nodes)
            .map(|node| self.core_nodes.iter().filter(|&&n| n == node).count())
            .collect();
        let allocations = unified_alloc(self.nr_cpus / au, &node_caps, &demands, &self.node_groups);
        let targets: Vec<usize> = allocations.iter().map(|a| a.total() * au).collect();
        let previous_node_cpus = self.node_cpu_counts(&snapshot.cpu_masks);
        let cpu_masks = self.apply_allocations(snapshot.cpu_masks, &allocations, &targets);
        let current_node_cpus = self.node_cpu_counts(&cpu_masks);

        for (idx, spec) in self.specs.iter().enumerate() {
            self.growth_denied[idx].fill(false);
            let Some((_, util_high)) = spec.util_range else {
                continue;
            };
            for node in 0..self.nr_nodes {
                let pinned_have = allocations[idx].pinned[node] * au;
                let unpinned_util = (self.layer_node_utils[idx][node]
                    - self.layer_node_pinned_utils[idx][node])
                    .max(0.0);
                let unpinned_cpus_needed = unpinned_util / util_high;
                let unpinned_cpus_have = current_node_cpus[idx][node].saturating_sub(pinned_have);
                let wanted = unpinned_cpus_needed > unpinned_cpus_have as f64;
                let got = current_node_cpus[idx][node] > previous_node_cpus[idx][node];
                if wanted && !got {
                    self.growth_denied[idx][node] = true;
                    self.growth_denied_counts[idx][node] += 1;
                }
            }
        }

        let (xnuma_rates, xnuma_mig_src) = self.refresh_xnuma(&current_node_cpus);

        LayeredControlUpdate {
            cpu_masks,
            targets,
            growth_denied: self.growth_denied.clone(),
            xnuma_rates,
            xnuma_mig_src,
            node_duty_sums: self.layer_node_duty_sums.clone(),
        }
    }

    /// Port of `main.rs::refresh_xnuma()`.
    ///
    /// Upstream writes straight into `skel.maps.bss_data.layers[l].node[s]`;
    /// here the two arrays are returned and the caller pushes them through
    /// `layered_set_xnuma` / `layered_set_xnuma_is_mig_src`. The branch
    /// structure is upstream's, and the policy itself is upstream's code —
    /// [`xnuma_check_active`] and [`xnuma_compute_rates`] are vendored
    /// token-identical (see [`crate::layered_xnuma`]).
    ///
    /// Three upstream behaviours that are easy to lose in a re-write, kept
    /// deliberately:
    ///
    /// * `nr_nodes <= 1` returns early and writes NOTHING, leaving the BSS
    ///   zeros in place. That is safe only because every `xnuma_gate()` call
    ///   short-circuits on `src_nid == dst_nid`, and on one node there is no
    ///   other pair.
    /// * The "gating off" branch (`threshold` both `<= 0.0`) writes
    ///   `is_mig_src = true` and `rate = u64::MAX` for EVERY pair, then
    ///   resets the *hysteresis* state to all-false — upstream's
    ///   `self.xnuma_mig_src[layer_idx].fill(false)` — so a later switch back
    ///   to gating starts from closed rather than from a stale open.
    /// * Rates are written BEFORE the flags, so a gate that activates this
    ///   iteration never sees a stale budget.
    #[allow(clippy::type_complexity)]
    fn refresh_xnuma(
        &mut self,
        current_node_cpus: &[Vec<usize>],
    ) -> (Vec<Vec<Vec<u64>>>, Vec<Vec<bool>>) {
        let nr_layers = self.specs.len();
        let nr_nodes = self.nr_nodes;
        let mut rates = vec![vec![vec![0u64; nr_nodes]; nr_nodes]; nr_layers];
        let mut mig_src = vec![vec![false; nr_nodes]; nr_layers];
        if nr_nodes <= 1 {
            return (rates, mig_src);
        }

        for layer_idx in 0..nr_layers {
            let threshold = self.specs[layer_idx].xnuma_threshold;
            let threshold_delta = self.specs[layer_idx].xnuma_threshold_delta;

            if threshold.0 <= 0.0 && threshold.1 <= 0.0 {
                // Off — all open, infinite budget.
                mig_src[layer_idx].fill(true);
                for row in rates[layer_idx].iter_mut() {
                    row.fill(u64::MAX);
                }
                self.xnuma_mig_src[layer_idx].fill(false);
                continue;
            }

            let is_mig_src = xnuma_check_active(
                &self.layer_node_duty_sums[layer_idx],
                &current_node_cpus[layer_idx],
                threshold,
                threshold_delta,
                &self.growth_denied[layer_idx],
                &self.xnuma_mig_src[layer_idx],
            );
            self.xnuma_mig_src[layer_idx] = is_mig_src.clone();

            let result = xnuma_compute_rates(
                &self.layer_node_duty_sums[layer_idx],
                &current_node_cpus[layer_idx],
            );
            for (row, computed) in rates[layer_idx].iter_mut().zip(result.rates.iter()) {
                row.copy_from_slice(&computed[..nr_nodes]);
            }
            mig_src[layer_idx].copy_from_slice(&is_mig_src[..nr_nodes]);
        }
        (rates, mig_src)
    }

    fn update_node_utils(
        period_ns: u64,
        current: &[Vec<u64>],
        previous: &mut [Vec<u64>],
        utils: &mut [Vec<f64>],
    ) {
        let elapsed = period_ns as f64 / 1_000_000_000.0;
        let decay = 0.5f64.powf(period_ns as f64 / USAGE_HALF_LIFE_NS);
        for layer in 0..current.len() {
            for node in 0..current[layer].len() {
                let delta = current[layer][node].saturating_sub(previous[layer][node]);
                let instantaneous = delta as f64 / 1_000_000_000.0 / elapsed;
                utils[layer][node] = utils[layer][node] * decay + instantaneous * (1.0 - decay);
            }
        }
        previous.clone_from_slice(current);
    }

    fn raw_target(&self, idx: usize, spec: &LayerSpec, current_mask: &[bool]) -> (usize, usize) {
        if spec.kind == LayerKind::Open {
            return (0, 0);
        }
        let (low_util, high_util) = spec.util_range.expect("validated in new");
        let mut util = self.layer_utils[idx][0];
        let current = current_mask.iter().filter(|&&set| set).count();
        if spec.util_includes_open_cputime || current == 0 {
            util += self.layer_utils[idx][1];
        }
        if util < 0.01 {
            util = 0.0;
        }
        let low = (util / high_util).ceil() as usize;
        let high = ((util / low_util).floor() as usize).max(low);
        let target = current.clamp(low, high);
        let (min, max) = spec.cpus_range.unwrap_or((0, self.nr_cpus));
        (target.clamp(min, max), min)
    }

    fn node_allowed(&self, spec: &LayerSpec, node: usize) -> bool {
        if spec.nodes.is_empty() && spec.llcs.is_empty() {
            return true;
        }
        spec.nodes.contains(&node) || spec.llcs.iter().any(|&llc| self.llc_nodes[llc] == node)
    }

    /// Upstream's `Layer::allowed_cpus` (`main.rs::Layer::new`, 1506-1610),
    /// asked one core at a time because allocation here happens in core units.
    ///
    /// A layer with neither `nodes` nor `llcs` is `allowed_cpus.set_all()`;
    /// otherwise the union of the named nodes' CPUs and the named LLCs' CPUs.
    /// Upstream intersects this into the grow candidates (`main.rs:3925`,
    /// `layer.allowed_cpus.and(node_span)`) and into the open-layer remainder
    /// (`main.rs:4073`, `available_cpus().and(&layer.allowed_cpus)`). Neither
    /// intersection was ported, so a node- or LLC-restricted layer could be
    /// grown onto CPUs its config forbids.
    fn core_allowed(&self, spec: &LayerSpec, core: usize) -> bool {
        if spec.nodes.is_empty() && spec.llcs.is_empty() {
            return true;
        }
        spec.nodes.contains(&self.core_nodes[core]) || spec.llcs.contains(&self.core_llcs[core])
    }

    fn node_cpu_counts(&self, masks: &[Vec<bool>]) -> Vec<Vec<usize>> {
        masks
            .iter()
            .map(|mask| {
                let mut counts = vec![0; self.nr_nodes];
                for (core, cpus) in self.core_cpus.iter().enumerate() {
                    let owned = cpus.iter().filter(|&&cpu| mask[cpu]).count();
                    assert!(
                        owned == 0 || owned == cpus.len(),
                        "layer allocation split SMT core {core}"
                    );
                    if owned != 0 {
                        counts[self.core_nodes[core]] += owned;
                    }
                }
                counts
            })
            .collect()
    }

    /// Mirror production's per-node, core-unit shrink/grow loops.
    fn apply_allocations(
        &self,
        mut masks: Vec<Vec<bool>>,
        allocations: &[crate::layered_alloc_upstream::LayerAlloc],
        targets: &[usize],
    ) -> Vec<Vec<bool>> {
        let mut available = vec![true; self.core_cpus.len()];
        for (spec, mask) in self.specs.iter().zip(&masks) {
            if spec.kind != LayerKind::Open {
                for (core, cpus) in self.core_cpus.iter().enumerate() {
                    let owned = cpus.iter().filter(|&&cpu| mask[cpu]).count();
                    assert!(owned == 0 || owned == cpus.len(), "partial SMT core");
                    if owned != 0 {
                        assert!(
                            available[core],
                            "overlapping non-open CPU allocation is unsupported"
                        );
                        available[core] = false;
                    }
                }
            }
        }

        let mut ascending: Vec<(usize, usize)> = targets.iter().copied().enumerate().collect();
        ascending.sort_by_key(|entry| entry.1);

        for &(idx, _target) in ascending.iter().rev() {
            if self.specs[idx].kind == LayerKind::Open {
                continue;
            }
            for node in 0..self.nr_nodes {
                let desired = allocations[idx].node_target(node) * self.threads_per_core;
                let current = self.core_orders[idx][node]
                    .iter()
                    .filter(|&&core| self.core_cpus[core].iter().all(|&cpu| masks[idx][cpu]))
                    .count()
                    * self.threads_per_core;
                let mut to_free = current.saturating_sub(desired);
                for &core in self.core_orders[idx][node].iter().rev() {
                    if to_free == 0 {
                        break;
                    }
                    if self.core_cpus[core].iter().all(|&cpu| masks[idx][cpu]) {
                        for &cpu in &self.core_cpus[core] {
                            masks[idx][cpu] = false;
                        }
                        available[core] = true;
                        to_free = to_free.saturating_sub(self.threads_per_core);
                    }
                }
            }
        }

        for &(idx, _target) in &ascending {
            if self.specs[idx].kind == LayerKind::Open {
                continue;
            }
            for &node in &self.node_orders[idx] {
                let desired = allocations[idx].node_target(node) * self.threads_per_core;
                let current = self.core_orders[idx][node]
                    .iter()
                    .filter(|&&core| self.core_cpus[core].iter().all(|&cpu| masks[idx][cpu]))
                    .count()
                    * self.threads_per_core;
                let mut to_grow = desired.saturating_sub(current);
                for &core in &self.core_orders[idx][node] {
                    if to_grow == 0 {
                        break;
                    }
                    // Upstream grows from `alloc_cpus(&node_allowed, ...)`
                    // where `node_allowed = layer.allowed_cpus.and(node_span)`
                    // (main.rs:3925). `core_order` alone is not that filter:
                    // it honours `nodes` but NOT `llcs`, so an LLC-restricted
                    // layer was reachable on any core of any node.
                    if !self.core_allowed(&self.specs[idx], core) {
                        continue;
                    }
                    if available[core] {
                        for &cpu in &self.core_cpus[core] {
                            masks[idx][cpu] = true;
                        }
                        available[core] = false;
                        to_grow = to_grow.saturating_sub(self.threads_per_core);
                    }
                }
                // A layer whose allowed cores are all taken simply does not
                // reach its target, exactly as upstream's `alloc_cpus`
                // returning `None` breaks its while loop (main.rs:3946-3952).
                // Only an UNRESTRICTED layer failing to grow indicates the
                // allocator asked for more than the machine has.
                if self.specs[idx].nodes.is_empty() && self.specs[idx].llcs.is_empty() {
                    assert_eq!(to_grow, 0, "allocator target exceeds available cores");
                }
            }
        }

        for (idx, spec) in self.specs.iter().enumerate() {
            if spec.kind == LayerKind::Open {
                masks[idx].fill(false);
                for (core, &is_available) in available.iter().enumerate() {
                    // main.rs:4073 — `available_cpus().and(&layer.allowed_cpus)`.
                    // The `available_cpus()` half was ported; the
                    // `allowed_cpus` half was not, so an open layer with an
                    // affinity took the whole free pool.
                    if is_available && self.core_allowed(spec, core) {
                        for &cpu in &self.core_cpus[core] {
                            masks[idx][cpu] = true;
                        }
                    }
                }
            }
        }
        masks
    }

    pub fn growth_denied_count(&self, layer: usize, node: usize) -> u64 {
        self.growth_denied_counts[layer][node]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layered::LayerMatch;

    fn normalize(source: &str) -> String {
        source.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn specs() -> Vec<LayerSpec> {
        vec![
            LayerSpec::new("busy", LayerKind::Grouped)
                .with_match(LayerMatch::CommPrefix("busy".into()))
                .with_util_range(0.8, 0.9),
            LayerSpec::new("idle", LayerKind::Grouped)
                .with_or(Vec::new())
                .with_util_range(0.8, 0.9),
        ]
    }

    #[test]
    fn measured_busy_layer_grows_and_idle_layer_shrinks() {
        let mut control = LayeredControl::new(100_000_000, 4, 4, 1, 1, specs());
        let update = control.step(LayeredControlSnapshot {
            // Two CPU-seconds/sec of busy work; no idle work. The 100ms EWMA
            // starts at zero, so the first 100ms sample contributes one CPU.
            usages: vec![[400_000_000, 0], [0, 0]],
            node_usages: vec![vec![400_000_000], vec![0]],
            node_pinned_usages: vec![vec![0], vec![0]],
            node_duty_raw: vec![vec![400_000_000], vec![0]],
            cpu_masks: vec![
                vec![true, true, false, false],
                vec![false, false, true, true],
            ],
        });
        assert!(update.targets[0] > update.targets[1]);
        assert!(update.cpu_masks[0].iter().filter(|&&v| v).count() > 2);
        assert!(update.cpu_masks[1].iter().filter(|&&v| v).count() < 2);
    }

    #[test]
    #[should_panic(expected = "requires cgroup-cpuset topology")]
    fn cpuset_growth_is_rejected_instead_of_approximated() {
        let mut spec = LayerSpec::new("reverse", LayerKind::Grouped).with_util_range(0.8, 0.9);
        spec.growth_algo = LayerGrowthAlgo::CpuSetSpread;
        LayeredControl::new(100_000_000, 4, 4, 1, 1, vec![spec]);
    }

    /// The loop cannot resize a layer whose CPU set userspace pinned
    /// explicitly, so it must refuse rather than quietly resize it anyway.
    #[test]
    #[should_panic(expected = "cannot resize explicitly pinned layer")]
    fn pinned_layer_growth_is_rejected_instead_of_silently_resized() {
        let mut spec = LayerSpec::new("pinned", LayerKind::Grouped).with_util_range(0.8, 0.9);
        spec.cpus = Some(vec![crate::CpuId(0), crate::CpuId(1)]);
        LayeredControl::new(100_000_000, 4, 4, 1, 1, vec![spec]);
    }

    /// `StickyDynamic` needs production's runtime LLC-trading loop, which only
    /// has anything to trade when there is more than one LLC. Single-LLC is
    /// therefore accepted and multi-LLC refused — assert both halves, since a
    /// blanket refusal would also satisfy the refusal half alone.
    #[test]
    fn sticky_dynamic_is_refused_on_multiple_llcs_and_allowed_on_one() {
        let spec = || {
            let mut s = LayerSpec::new("sticky", LayerKind::Grouped).with_util_range(0.8, 0.9);
            s.growth_algo = LayerGrowthAlgo::StickyDynamic;
            s
        };
        // One LLC covering all 4 CPUs: nothing to trade, so it is allowed.
        LayeredControl::new(100_000_000, 4, 4, 1, 1, vec![spec()]);

        // Two LLCs of 2: must refuse.
        let err = std::panic::catch_unwind(|| {
            LayeredControl::new(100_000_000, 4, 2, 1, 1, vec![spec()]);
        })
        .expect_err("StickyDynamic on 2 LLCs must be refused, not approximated");
        let msg = err
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| err.downcast_ref::<&str>().copied())
            .unwrap_or("");
        assert!(
            msg.contains("StickyDynamic on multiple LLCs"),
            "refused for the wrong reason: {msg}"
        );
    }

    /// The policy is compiled from this exact file. Guard the virtual cgroup
    /// boundary too: a future direct host filesystem read must fail here
    /// instead of making simulation results machine-dependent.
    #[test]
    fn upstream_growth_cannot_gain_unreviewed_host_sys_access() {
        let upstream_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../scx/scheds/rust/scx_layered/src/layer_core_growth.rs");
        let upstream = std::fs::read_to_string(&upstream_path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", upstream_path.display()));
        let sys_paths: Vec<&str> = upstream
            .lines()
            .filter(|line| line.contains("\"/sys/"))
            .collect();
        assert_eq!(
            sys_paths.len(),
            1,
            "upstream growth added or removed a host /sys path: {sys_paths:?}"
        );
        assert!(sys_paths[0].contains("WalkDir::new(\"/sys/fs/cgroup\")"));
        // Compare TRIMMED lines: the property being guarded is which `fs::`
        // calls upstream makes, not how they are indented. Pinning the
        // indentation makes the guard fail on a pure reformat, which trains
        // readers to "fix" it by pasting in whatever upstream now says —
        // exactly the reflex that would wave through a real new host read.
        let fs_calls: Vec<&str> = upstream
            .lines()
            .map(str::trim)
            .filter(|line| line.contains("fs::"))
            .collect();
        assert_eq!(
            fs_calls,
            ["if let Ok(content) = fs::read_to_string(entry.path()) {"],
            "upstream growth changed its filesystem access; review before updating this guard"
        );

        let adapter_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../scx_layered_growth/src/lib.rs");
        let adapter = std::fs::read_to_string(&adapter_path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", adapter_path.display()));
        assert!(adapter.contains("extern crate self as walkdir;"));
        assert!(adapter.contains("type IntoIter = std::iter::Empty<Self::Item>;"));
    }

    /// The target and dampening glue is specialized from `main.rs`, which
    /// cannot be linked independently of the production skeleton. Fail on
    /// upstream policy drift instead of silently retaining an old formula.
    #[test]
    fn flat_target_formulae_have_not_drifted_upstream() {
        let upstream_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../scx/scheds/rust/scx_layered/src/main.rs");
        let normalized = normalize(
            &std::fs::read_to_string(&upstream_path)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", upstream_path.display())),
        );
        for formula in [
            "const USAGE_HALF_LIFE_F64: f64 = USAGE_HALF_LIFE as f64 / 1_000_000_000.0;",
            "static ref USAGE_DECAY: f64 = 0.5f64.powf(1.0 / USAGE_HALF_LIFE_F64);",
            "let decay = decay_rate.powf(elapsed_f64); p * decay + c * (1.0 - decay)",
            "let low = (util / util_range.1).ceil() as usize;",
            "let high = ((util.max(peak_util) / util_range.0).floor() as usize).max(low);",
            "let target = layer.cpus.weight().clamp(low, high);",
            "let dampened = cur - (cur - target).div_ceil(2);",
            "ascending.sort_by_key(|a| a.1);",
        ] {
            assert!(
                normalized.contains(&normalize(formula)),
                "upstream control formula changed near `{formula}`; update/link the real policy"
            );
        }
    }
}
