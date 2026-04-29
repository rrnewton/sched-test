//! Demo: create a cgroup hierarchy from a JSON spec and place the current process.
//!
//! Usage (requires root):
//!   sudo cargo run --example demo_cgroup
//!   sudo cargo run --example demo_cgroup -- path/to/spec.json

use std::path::Path;

use anyhow::Result;
use rt_app_rs::cgroup::CgroupHierarchy;
use rt_app_rs::spec::RtAppSpec;

const DEFAULT_SPEC: &str = r#"{
    "global": {
        "duration": 5,
        "default_policy": "SCHED_OTHER",
        "calibration": "CPU0"
    },
    "cgroups": {
        "/app":     { "cpu.max": { "quota": "max", "period": 100000 } },
        "/app/fg":  { "cpu.max": { "quota": 3600000, "period": 100000 } },
        "/app/bg":  { "cpu.max": { "quota": 200000, "period": 100000 } },
        "/sys":     { "cpu.max": { "quota": "max", "period": 100000 } },
        "/sys/mon": { "cpu.max": { "quota": 100000, "period": 100000 } }
    },
    "tasks": {
        "fg_worker":   { "cgroup": "/app/fg",  "loop": -1 },
        "bg_worker_0": { "cgroup": "/app/bg",  "loop": -1 },
        "bg_worker_1": { "cgroup": "/app/bg",  "loop": -1 },
        "monitor":     { "cgroup": "/sys/mon", "loop": -1 }
    }
}"#;

fn main() -> Result<()> {
    let spec = match std::env::args().nth(1) {
        Some(path) => {
            println!("Loading spec from: {}", path);
            RtAppSpec::load(Path::new(&path))?
        }
        None => {
            println!("Using built-in demo spec");
            RtAppSpec::from_json(DEFAULT_SPEC)?
        }
    };

    println!("\n=== Spec Summary ===");
    println!("  Cgroups: {}", spec.cgroups.len());
    println!("  Tasks:   {}", spec.tasks.len());
    println!(
        "  Has cgroup assignments: {}",
        spec.tasks.values().any(|t| t.cgroup.is_some())
    );

    println!("\n=== Cgroup Definitions ===");
    for (path, def) in &spec.cgroups {
        let max_str = match &def.cpu_max {
            Some(cm) => cm.to_cgroupfs_string(),
            None => "(none)".to_string(),
        };
        println!("  {:<30} cpu.max = {}", path, max_str);
    }

    println!("\n=== Task → Cgroup Assignments ===");
    for (name, task) in &spec.tasks {
        let cg = task.cgroup.as_deref().unwrap_or("(none)");
        println!("  {:<20} → {}", name, cg);
    }

    // Create the hierarchy
    println!("\n=== Creating cgroup hierarchy ===");
    let hierarchy = CgroupHierarchy::create(&spec, Some("rtapp_demo"))?;
    println!("  Root: {}", hierarchy.root_dir().display());
    println!("  Nodes created: {}", hierarchy.len());

    // Verify by reading back
    println!("\n=== Verifying cpu.max values ===");
    for (path, _node) in hierarchy.iter() {
        match hierarchy.read_cpu_max(path) {
            Ok(cm) => println!("  {:<30} cpu.max = {}", path, cm),
            Err(e) => println!("  {:<30} ERROR: {}", path, e),
        }
    }

    // Place current process into the first leaf cgroup as a demo
    if !hierarchy.is_empty() {
        let pid = std::process::id();
        // Pick a leaf cgroup (one with no children — its path is not a prefix of any other)
        let all_paths: Vec<&str> = hierarchy.iter().map(|(p, _)| p).collect();
        let leaf_path = all_paths
            .iter()
            .find(|&&p| {
                !all_paths.iter().any(|&other| {
                    other != p && other.starts_with(p) && other[p.len()..].starts_with('/')
                })
            })
            .copied()
            .unwrap_or(all_paths[0]);

        println!(
            "\n=== Placing current process (pid={}) into {} ===",
            pid, leaf_path
        );
        hierarchy.place_process(leaf_path, pid)?;

        // Read back to verify
        let procs_content = std::fs::read_to_string(
            hierarchy
                .get(leaf_path)
                .unwrap()
                .path()
                .join("cgroup.procs"),
        )?;
        println!("  cgroup.procs contains: {:?}", procs_content.trim());
    } else {
        println!("\n=== No cgroups defined, skipping process placement ===");
    }

    println!("\n=== Cleanup (on drop) ===");
    let root = hierarchy.root_dir().to_path_buf();
    drop(hierarchy);
    println!("  Root dir exists after drop: {}", root.exists());

    println!("\nDone!");
    Ok(())
}
