//! Convert a production cgroup hierarchy JSON to an rt-app-rs spec with synthetic workloads.
//!
//! Reads a `data/cgroup_hierarchies/pattern_NNNN_*.json` file and generates a
//! valid rt-app-rs JSON spec where:
//! - The cgroup hierarchy is faithfully reproduced with `cpu.max` limits
//! - Each leaf cgroup gets synthetic workload threads with phases scaled to
//!   its bandwidth quota
//! - The output is ready to run with `demo_cgroup`
//!
//! Usage:
//!   cargo run --bin cgroup2rtapp -- pattern_0008.json -o output.json
//!   cargo run --bin cgroup2rtapp -- pattern_0008.json --duration 60 --seed 42

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde_json::json;

use rt_app_rs::spec::{CgroupDef, ProductionNode, ProductionPattern, RtAppSpec, TaskDef};

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(
    name = "cgroup2rtapp",
    about = "Convert production cgroup hierarchy JSON to rt-app-rs spec with synthetic workloads"
)]
struct Cli {
    /// Input production cgroup hierarchy JSON file
    input: PathBuf,

    /// Output file (default: stdout)
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Workload duration in seconds
    #[arg(short, long, default_value_t = 30)]
    duration: u64,

    /// Random seed for workload generation (0 = random)
    #[arg(short, long, default_value_t = 0)]
    seed: u64,

    /// Maximum threads per leaf cgroup
    #[arg(long, default_value_t = 4)]
    max_threads_per_leaf: usize,

    /// Minimum threads per leaf cgroup
    #[arg(long, default_value_t = 1)]
    min_threads_per_leaf: usize,

    /// Target aggregate CPU utilization within each leaf (0.0-1.0)
    #[arg(long, default_value_t = 0.6)]
    utilization: f64,

    /// Also emit interior (non-leaf) cgroups in the output
    /// (leaves-only is more compact but misses intermediate cpu.max limits)
    #[arg(long, default_value_t = true)]
    include_interior: bool,

    /// Pretty-print the JSON output
    #[arg(long, default_value_t = true)]
    pretty: bool,
}

// ---------------------------------------------------------------------------
// Workload generation
// ---------------------------------------------------------------------------

/// A workload profile template.
#[derive(Debug, Clone, Copy)]
enum WorkloadProfile {
    /// Latency-sensitive: short bursts, long sleeps (e.g. web serving)
    LatencySensitive,
    /// Batch: long compute, short pauses (e.g. data processing)
    Batch,
    /// Periodic: medium work, medium sleep (e.g. monitoring, heartbeats)
    Periodic,
}

impl WorkloadProfile {
    /// Pick a random profile.
    fn random(rng: &mut StdRng) -> Self {
        match rng.random_range(0..3) {
            0 => Self::LatencySensitive,
            1 => Self::Batch,
            _ => Self::Periodic,
        }
    }

    /// Base run/sleep times in microseconds (before scaling).
    fn base_phases(&self) -> (u64, u64) {
        match self {
            Self::LatencySensitive => (200, 1800), // 10% duty cycle base
            Self::Batch => (1500, 500),            // 75% duty cycle base
            Self::Periodic => (500, 1500),         // 25% duty cycle base
        }
    }

    /// Name prefix for tasks with this profile.
    fn prefix(&self) -> &'static str {
        match self {
            Self::LatencySensitive => "fg",
            Self::Batch => "bg",
            Self::Periodic => "mon",
        }
    }
}

/// Information about a leaf cgroup for workload generation.
struct LeafInfo {
    /// Spec path (e.g. "/n0/n0_c0/n0_c0_c0")
    spec_path: String,
    /// Effective CPU count (quota / period), or None for unlimited
    effective_cpus: Option<f64>,
    /// Short name for task naming (last path component)
    #[allow(dead_code)]
    short_name: String,
}

/// Collect leaf cgroups from the production tree.
fn collect_leaves(node: &ProductionNode, parent_path: &str, leaves: &mut Vec<LeafInfo>) {
    let path = if parent_path.is_empty() {
        format!("/{}", node.name)
    } else {
        format!("{}/{}", parent_path, node.name)
    };

    if node.children.is_empty() {
        let effective_cpus = node.cpu_max.as_ref().and_then(|cm| {
            if cm.quota == "max" {
                None
            } else {
                let quota = cm.quota.parse::<f64>().ok()?;
                let period = cm.period.parse::<f64>().ok()?;
                Some(quota / period)
            }
        });

        leaves.push(LeafInfo {
            spec_path: path,
            effective_cpus,
            short_name: node.name.clone(),
        });
    } else {
        for child in &node.children {
            collect_leaves(child, &path, leaves);
        }
    }
}

/// Generate synthetic tasks for a leaf cgroup.
fn generate_tasks_for_leaf(
    leaf: &LeafInfo,
    leaf_idx: usize,
    rng: &mut StdRng,
    cli: &Cli,
) -> Vec<(String, TaskDef)> {
    let profile = WorkloadProfile::random(rng);
    let (base_run, base_sleep) = profile.base_phases();

    // How many threads? Scale by effective CPU count if known.
    let thread_count = match leaf.effective_cpus {
        Some(cpus) if cpus >= 2.0 => {
            // More CPUs → more threads, but capped
            let n = (cpus as usize).min(cli.max_threads_per_leaf);
            n.max(cli.min_threads_per_leaf)
        }
        Some(cpus) if cpus < 1.0 => {
            // Fractional CPU → just 1 thread
            cli.min_threads_per_leaf
        }
        _ => {
            // 1 CPU or unlimited → random 1-max
            rng.random_range(cli.min_threads_per_leaf..=cli.max_threads_per_leaf)
        }
    };

    // Scale run time to achieve target utilization within the cgroup's quota.
    // If the cgroup has N effective CPUs and we spawn T threads, each thread
    // should use approximately (N * utilization / T) of one CPU.
    let per_thread_target = match leaf.effective_cpus {
        Some(cpus) => (cpus * cli.utilization / thread_count as f64).min(1.0),
        None => cli.utilization / thread_count as f64,
    };

    // Adjust run/sleep to hit the per-thread target duty cycle.
    // duty_cycle = run / (run + sleep)
    // We keep the total period (run+sleep) close to the base, just adjust the ratio.
    let total_period = base_run + base_sleep;
    let run_us = ((total_period as f64) * per_thread_target.max(0.01)) as u64;
    let run_us = run_us.max(10); // minimum 10µs
    let sleep_us = total_period.saturating_sub(run_us).max(10);

    // Add some jitter (±20%)
    let jitter = |rng: &mut StdRng, val: u64| -> u64 {
        let factor = rng.random_range(0.8..1.2_f64);
        (val as f64 * factor).max(10.0) as u64
    };

    let mut tasks = Vec::new();
    for t in 0..thread_count {
        let name = format!("{}_{}_t{}", profile.prefix(), leaf_idx, t);
        let run = jitter(rng, run_us);
        let sleep = jitter(rng, sleep_us);

        let mut extra = serde_json::Map::new();
        extra.insert("loop".to_string(), json!(-1));

        // Build phases
        let mut phases = serde_json::Map::new();
        phases.insert("work".to_string(), json!({ "loop": 1, "run": run }));
        phases.insert("idle".to_string(), json!({ "loop": 1, "sleep": sleep }));
        extra.insert("phases".to_string(), serde_json::Value::Object(phases));

        tasks.push((
            name,
            TaskDef {
                cgroup: Some(leaf.spec_path.clone()),
                extra,
            },
        ));
    }

    tasks
}

// ---------------------------------------------------------------------------
// Main conversion
// ---------------------------------------------------------------------------

fn convert(cli: &Cli) -> Result<String> {
    // Load production pattern
    let pattern = ProductionPattern::load(&cli.input)
        .with_context(|| format!("loading {}", cli.input.display()))?;

    eprintln!(
        "Loaded pattern {} ({} hosts, {} in hierarchy)",
        pattern.pattern_id,
        pattern.host_count,
        count_nodes(&pattern.hierarchy),
    );

    // Build cgroup definitions
    let cgroup_defs = pattern.to_cgroup_defs();
    eprintln!("  {} cgroup paths (flat)", cgroup_defs.len());

    // Collect leaf cgroups
    let mut leaves = Vec::new();
    collect_leaves(&pattern.hierarchy, "", &mut leaves);
    eprintln!("  {} leaf cgroups", leaves.len());

    // Set up RNG
    let seed = if cli.seed == 0 {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        eprintln!("  Using random seed: {}", ts);
        ts
    } else {
        eprintln!("  Using fixed seed: {}", cli.seed);
        cli.seed
    };
    let mut rng = StdRng::seed_from_u64(seed);

    // Generate tasks for each leaf
    let mut tasks = BTreeMap::new();
    for (leaf_idx, leaf) in leaves.iter().enumerate() {
        let leaf_tasks = generate_tasks_for_leaf(leaf, leaf_idx, &mut rng, cli);
        let cpu_str = match leaf.effective_cpus {
            Some(c) => format!("{:.1} CPUs", c),
            None => "unlimited".to_string(),
        };
        eprintln!(
            "  Leaf {:>3}: {} — {} ({} threads)",
            leaf_idx,
            leaf.spec_path,
            cpu_str,
            leaf_tasks.len(),
        );
        for (name, def) in leaf_tasks {
            tasks.insert(name, def);
        }
    }

    eprintln!("  Total tasks: {}", tasks.len());

    // Choose which cgroup entries to include
    let final_cgroups: BTreeMap<String, CgroupDef> = if cli.include_interior {
        cgroup_defs
    } else {
        // Only leaves
        let leaf_paths: std::collections::HashSet<&str> =
            leaves.iter().map(|l| l.spec_path.as_str()).collect();
        cgroup_defs
            .into_iter()
            .filter(|(path, _)| leaf_paths.contains(path.as_str()))
            .collect()
    };

    // Assemble the spec
    let global = json!({
        "default_policy": "SCHED_OTHER",
        "duration": cli.duration,
        "calibration": "CPU0",
        "log_basename": format!("cgroup_{}", pattern.pattern_id),
        "log_size": 100,
        "logdir": "./"
    });

    let spec = RtAppSpec {
        global: Some(global),
        cgroups: final_cgroups,
        tasks,
    };

    // Validate
    spec.validate()
        .context("generated spec failed validation")?;

    // Serialize
    let json_str = if cli.pretty {
        serde_json::to_string_pretty(&spec).context("serializing spec")?
    } else {
        serde_json::to_string(&spec).context("serializing spec")?
    };

    Ok(json_str)
}

fn count_nodes(node: &ProductionNode) -> usize {
    1 + node.children.iter().map(count_nodes).sum::<usize>()
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let json = convert(&cli)?;

    match &cli.output {
        Some(path) => {
            std::fs::write(path, &json).with_context(|| format!("writing {}", path.display()))?;
            eprintln!("Wrote {}", path.display());
        }
        None => {
            println!("{}", json);
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_cli(input: &str) -> Cli {
        Cli {
            input: PathBuf::from(input),
            output: None,
            duration: 30,
            seed: 42,
            max_threads_per_leaf: 4,
            min_threads_per_leaf: 1,
            utilization: 0.6,
            include_interior: true,
            pretty: false,
        }
    }

    #[test]
    fn test_collect_leaves_simple() {
        let node = ProductionNode {
            name: "n0".to_string(),
            cpu_max: None,
            children: vec![
                ProductionNode {
                    name: "n0_c0".to_string(),
                    cpu_max: Some(rt_app_rs::spec::ProductionCpuMax {
                        quota: "200000".to_string(),
                        period: "100000".to_string(),
                    }),
                    children: vec![],
                },
                ProductionNode {
                    name: "n0_c1".to_string(),
                    cpu_max: Some(rt_app_rs::spec::ProductionCpuMax {
                        quota: "max".to_string(),
                        period: "100000".to_string(),
                    }),
                    children: vec![],
                },
            ],
        };

        let mut leaves = Vec::new();
        collect_leaves(&node, "", &mut leaves);
        assert_eq!(leaves.len(), 2);
        assert_eq!(leaves[0].spec_path, "/n0/n0_c0");
        assert_eq!(leaves[0].effective_cpus, Some(2.0));
        assert_eq!(leaves[1].spec_path, "/n0/n0_c1");
        assert_eq!(leaves[1].effective_cpus, None); // "max"
    }

    #[test]
    fn test_generate_tasks_respects_cpu_count() {
        let cli = make_cli("dummy");
        let mut rng = StdRng::seed_from_u64(42);

        // 1-CPU leaf → 1 thread
        let leaf = LeafInfo {
            spec_path: "/test".to_string(),
            effective_cpus: Some(0.5),
            short_name: "test".to_string(),
        };
        let tasks = generate_tasks_for_leaf(&leaf, 0, &mut rng, &cli);
        assert_eq!(tasks.len(), 1);

        // 10-CPU leaf → capped at max_threads_per_leaf (4)
        let leaf = LeafInfo {
            spec_path: "/big".to_string(),
            effective_cpus: Some(10.0),
            short_name: "big".to_string(),
        };
        let tasks = generate_tasks_for_leaf(&leaf, 1, &mut rng, &cli);
        assert_eq!(tasks.len(), 4);
    }

    #[test]
    fn test_generated_spec_validates() {
        // Build a minimal production pattern inline
        let pattern_json = r#"{
            "pattern_id": "test",
            "host_count": 1,
            "hosts": ["host_001"],
            "hierarchy": {
                "name": "n0",
                "cpu_max": null,
                "children": [
                    {
                        "name": "n0_c0",
                        "cpu_max": { "quota": "max", "period": "100000" },
                        "children": [
                            {
                                "name": "n0_c0_c0",
                                "cpu_max": { "quota": "200000", "period": "100000" }
                            }
                        ]
                    },
                    {
                        "name": "n0_c1",
                        "cpu_max": { "quota": "max", "period": "100000" },
                        "children": [
                            {
                                "name": "n0_c1_c8",
                                "cpu_max": { "quota": "100000", "period": "100000" }
                            },
                            {
                                "name": "n0_c1_c9",
                                "cpu_max": { "quota": "200000", "period": "100000" }
                            }
                        ]
                    }
                ]
            }
        }"#;

        // Write to temp file
        let dir = tempfile::tempdir().unwrap();
        let input_path = dir.path().join("test_pattern.json");
        std::fs::write(&input_path, pattern_json).unwrap();

        let cli = Cli {
            input: input_path,
            output: None,
            duration: 10,
            seed: 42,
            max_threads_per_leaf: 2,
            min_threads_per_leaf: 1,
            utilization: 0.5,
            include_interior: true,
            pretty: false,
        };

        let json_output = convert(&cli).unwrap();

        // Parse back as RtAppSpec
        let spec = RtAppSpec::from_json(&json_output).unwrap();
        assert!(spec.has_cgroups());
        assert!(!spec.tasks.is_empty());

        // Every task should reference a valid cgroup
        for task in spec.tasks.values() {
            assert!(task.cgroup.is_some());
            let cg = task.cgroup.as_ref().unwrap();
            assert!(
                spec.cgroups.contains_key(cg),
                "task references undefined cgroup: {}",
                cg
            );
        }
    }
}
