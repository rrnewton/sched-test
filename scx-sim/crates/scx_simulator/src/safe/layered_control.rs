//! scx_layered's periodic userspace CPU-allocation control loop.
//!
//! Target calculation and shrink/grow ordering mirror `main.rs`; CPU budgets
//! come from the real upstream `unified_alloc()`, while core and node ordering
//! execute the real upstream `layer_core_growth.rs` through the
//! `scx_layered_growth` topology adapter.

use crate::layered::{LayerGrowthAlgo, LayerKind, LayerSpec};
use crate::layered_alloc_upstream::{unified_alloc, LayerDemand};
use scx_layered_growth::layer_core_growth;
use scx_layered_growth::{algorithm_from_bpf, CpuPool, LayerSpec as GrowthSpec, Topology};

const USAGE_HALF_LIFE_NS: f64 = 100_000_000.0;

/// Cumulative BPF runtime counters and current CPU masks sampled at one tick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayeredControlSnapshot {
    pub usages: Vec<[u64; 2]>,
    pub node_usages: Vec<Vec<u64>>,
    pub node_pinned_usages: Vec<Vec<u64>>,
    pub cpu_masks: Vec<Vec<bool>>,
}

/// Result of one userspace control-loop iteration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayeredControlUpdate {
    pub cpu_masks: Vec<Vec<bool>>,
    pub targets: Vec<usize>,
    pub growth_denied: Vec<Vec<bool>>,
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
    llc_nodes: Vec<usize>,
    core_orders: Vec<Vec<Vec<usize>>>,
    node_orders: Vec<Vec<usize>>,
    node_groups: Vec<Vec<Vec<usize>>>,
    growth_denied: Vec<Vec<bool>>,
    growth_denied_counts: Vec<Vec<u64>>,
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
        let core_nodes = topology
            .all_cores
            .values()
            .map(|core| core.node_id)
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
            llc_nodes,
            core_orders,
            node_orders,
            node_groups,
            growth_denied: vec![vec![false; nr_nodes]; nr_layers],
            growth_denied_counts: vec![vec![0; nr_nodes]; nr_layers],
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
                    (current - (current - target).div_ceil(2), min)
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

        LayeredControlUpdate {
            cpu_masks,
            targets,
            growth_denied: self.growth_denied.clone(),
        }
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
                    if available[core] {
                        for &cpu in &self.core_cpus[core] {
                            masks[idx][cpu] = true;
                        }
                        available[core] = false;
                        to_grow = to_grow.saturating_sub(self.threads_per_core);
                    }
                }
                assert_eq!(to_grow, 0, "allocator target exceeds available cores");
            }
        }

        for (idx, spec) in self.specs.iter().enumerate() {
            if spec.kind == LayerKind::Open {
                masks[idx].fill(false);
                for (core, &is_available) in available.iter().enumerate() {
                    if is_available {
                        for &cpu in &self.core_cpus[core] {
                            masks[idx][cpu] = true;
                        }
                    }
                }
            }
        }
        masks
    }

    pub fn growth_denied(&self, layer: usize, node: usize) -> bool {
        self.growth_denied[layer][node]
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

    /// Guard the source formula specialized by `linear_order()`. Compiling
    /// all of upstream `layer_core_growth.rs` also requires its production
    /// Topology/CpuPool types and remains the next Tier-3 increment.
    #[test]
    fn flat_linear_order_formula_has_not_drifted_upstream() {
        let upstream_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../scx/scheds/rust/scx_layered/src/layer_core_growth.rs");
        let upstream = std::fs::read_to_string(&upstream_path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", upstream_path.display()));
        let expected = normalize(
            r#"
            fn rotate_node_layer_offset(&self, vec: &mut [usize]) {
                if vec.is_empty() {
                    return;
                }
                let num_cores = vec.len();
                let chunk = num_cores.div_ceil(self.layer_specs.len());
                vec.rotate_right((chunk * self.layer_idx).min(num_cores));
            }
            "#,
        );
        let normalized = normalize(&upstream);
        assert!(
            normalized.contains(&expected),
            "upstream Linear rotation changed; link/update layer_core_growth instead of silently drifting"
        );
        assert!(
            normalized.contains(&normalize(
                "LayerGrowthAlgo::Linear => generator.grow_linear()"
            )),
            "upstream Linear no longer uses grow_linear"
        );
        assert!(
            normalized.contains(&normalize("self.rotate_node_layer_offset(&mut order);")),
            "upstream grow_linear no longer uses the guarded rotation"
        );
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
