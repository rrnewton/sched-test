//! scx_layered's periodic userspace CPU-allocation control loop.
//!
//! This module is deliberately limited to the production `Linear` growth
//! algorithm on a flat, non-SMT topology. The target calculation and shrink /
//! grow ordering mirror `scx_layered/src/main.rs`; CPU budgets come from the
//! real upstream `unified_alloc()`. Other growth algorithms require upstream's
//! `layer_core_growth.rs` plus its `Topology` / `CpuPool` dependencies and are
//! rejected by the public enable method rather than approximated here.

use crate::layered::{LayerGrowthAlgo, LayerKind, LayerSpec};
use crate::layered_alloc_upstream::{unified_alloc, LayerDemand};

const USAGE_HALF_LIFE_NS: f64 = 100_000_000.0;

/// Cumulative BPF runtime counters and current CPU masks sampled at one tick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayeredControlSnapshot {
    pub usages: Vec<[u64; 2]>,
    pub cpu_masks: Vec<Vec<bool>>,
}

/// Result of one userspace control-loop iteration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayeredControlUpdate {
    pub cpu_masks: Vec<Vec<bool>>,
    pub targets: Vec<usize>,
}

/// Stateful utilization EWMA and CPU allocation driver.
pub struct LayeredControl {
    period_ns: u64,
    nr_cpus: usize,
    specs: Vec<LayerSpec>,
    previous_usages: Vec<[u64; 2]>,
    layer_utils: Vec<[f64; 2]>,
}

impl LayeredControl {
    pub fn new(period_ns: u64, nr_cpus: usize, specs: Vec<LayerSpec>) -> Self {
        assert!(period_ns > 0, "layered control period must be positive");
        assert!(nr_cpus > 0, "layered control needs at least one CPU");
        for spec in &specs {
            if spec.kind == LayerKind::Open {
                continue;
            }
            assert_eq!(
                spec.growth_algo,
                LayerGrowthAlgo::Linear,
                "Tier-3 control currently supports only Linear growth; {:?} uses {:?}",
                spec.name,
                spec.growth_algo
            );
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
        }

        let nr_layers = specs.len();
        Self {
            period_ns,
            nr_cpus,
            specs,
            previous_usages: vec![[0; 2]; nr_layers],
            layer_utils: vec![[0.0; 2]; nr_layers],
        }
    }

    pub fn period_ns(&self) -> u64 {
        self.period_ns
    }

    /// Run one production-shaped control iteration.
    pub fn step(&mut self, snapshot: LayeredControlSnapshot) -> LayeredControlUpdate {
        assert_eq!(snapshot.usages.len(), self.specs.len());
        assert_eq!(snapshot.cpu_masks.len(), self.specs.len());
        assert!(snapshot.cpu_masks.iter().all(|m| m.len() == self.nr_cpus));

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

        let demands: Vec<LayerDemand> = self
            .specs
            .iter()
            .zip(&dampened)
            .map(|(spec, &(target, _))| LayerDemand {
                raw_pinned: vec![0],
                raw_unpinned: if spec.kind == LayerKind::Open {
                    0
                } else {
                    target
                },
                weight: spec.weight as usize,
                spread: false,
            })
            .collect();
        let node_groups = vec![vec![vec![0]]; self.specs.len()];
        let allocations = unified_alloc(self.nr_cpus, &[self.nr_cpus], &demands, &node_groups);
        let targets: Vec<usize> = allocations.iter().map(|a| a.total()).collect();
        let cpu_masks = self.apply_linear_targets(snapshot.cpu_masks, &targets);

        LayeredControlUpdate { cpu_masks, targets }
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

    /// Mirror production's non-StickyDynamic flat-topology shrink/grow loops.
    fn apply_linear_targets(&self, mut masks: Vec<Vec<bool>>, targets: &[usize]) -> Vec<Vec<bool>> {
        let mut available = vec![true; self.nr_cpus];
        for (spec, mask) in self.specs.iter().zip(&masks) {
            if spec.kind != LayerKind::Open {
                for (cpu, &set) in mask.iter().enumerate() {
                    if set {
                        assert!(
                            available[cpu],
                            "overlapping non-open CPU allocation is unsupported"
                        );
                        available[cpu] = false;
                    }
                }
            }
        }

        let mut ascending: Vec<(usize, usize)> = targets.iter().copied().enumerate().collect();
        ascending.sort_by_key(|entry| entry.1);

        for &(idx, target) in ascending.iter().rev() {
            if self.specs[idx].kind == LayerKind::Open {
                continue;
            }
            let order = self.linear_order(idx);
            while masks[idx].iter().filter(|&&set| set).count() > target {
                let cpu = order
                    .iter()
                    .rev()
                    .copied()
                    .find(|&cpu| masks[idx][cpu])
                    .expect("layer target below current but no owned CPU found");
                masks[idx][cpu] = false;
                available[cpu] = true;
            }
        }

        for &(idx, target) in &ascending {
            if self.specs[idx].kind == LayerKind::Open {
                continue;
            }
            let order = self.linear_order(idx);
            while masks[idx].iter().filter(|&&set| set).count() < target {
                let cpu = order
                    .iter()
                    .copied()
                    .find(|&cpu| available[cpu])
                    .expect("allocator target exceeds available CPUs");
                masks[idx][cpu] = true;
                available[cpu] = false;
            }
        }

        for (idx, spec) in self.specs.iter().enumerate() {
            if spec.kind == LayerKind::Open {
                masks[idx].clone_from(&available);
            }
        }
        masks
    }

    fn linear_order(&self, layer_idx: usize) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.nr_cpus).collect();
        let chunk = self.nr_cpus.div_ceil(self.specs.len());
        order.rotate_right((chunk * layer_idx).min(self.nr_cpus));
        order
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
        let mut control = LayeredControl::new(100_000_000, 4, specs());
        let update = control.step(LayeredControlSnapshot {
            // Two CPU-seconds/sec of busy work; no idle work. The 100ms EWMA
            // starts at zero, so the first 100ms sample contributes one CPU.
            usages: vec![[400_000_000, 0], [0, 0]],
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
    #[should_panic(expected = "supports only Linear growth")]
    fn unsupported_growth_is_rejected_instead_of_approximated() {
        let mut spec = LayerSpec::new("reverse", LayerKind::Grouped).with_util_range(0.8, 0.9);
        spec.growth_algo = LayerGrowthAlgo::Reverse;
        LayeredControl::new(100_000_000, 4, vec![spec]);
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
