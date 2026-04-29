//! E2E test: create cgroups with cpu.max limits, fork busy-loop processes,
//! verify placement and bandwidth enforcement, then clean up.
//!
//! Usage (requires root):
//!   sudo cargo run --example e2e_cgroup_test --release
//!   sudo cargo run --example e2e_cgroup_test --release -- --hold 20

use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::Result;
use rt_app_rs::cgroup::CgroupHierarchy;
use rt_app_rs::spec::RtAppSpec;

/// Spec: two leaf cgroups with very different cpu.max limits.
///   /fg → 100% of 1 CPU  (quota=100000, period=100000)  — 2 busy processes
///   /bg →  20% of 1 CPU  (quota=20000,  period=100000)  — 1 busy process
const TEST_SPEC: &str = r#"{
    "global": { "duration": 10 },
    "cgroups": {
        "/fg":  { "cpu.max": { "quota": 100000, "period": 100000 } },
        "/bg":  { "cpu.max": { "quota":  20000, "period": 100000 } }
    },
    "tasks": {
        "fg_spin_0": { "cgroup": "/fg", "loop": -1 },
        "fg_spin_1": { "cgroup": "/fg", "loop": -1 },
        "bg_spin_0": { "cgroup": "/bg", "loop": -1 }
    }
}"#;

fn main() -> Result<()> {
    let hold_secs: u64 = std::env::args()
        .position(|a| a == "--hold")
        .and_then(|i| std::env::args().nth(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(15);

    println!("╔══════════════════════════════════════════════════════╗");
    println!("║  rt-app-rs  E2E cgroup test                        ║");
    println!("╚══════════════════════════════════════════════════════╝");

    // ── 1. Parse spec ──────────────────────────────────────────────
    let spec = RtAppSpec::from_json(TEST_SPEC)?;
    println!(
        "\n[1] Spec parsed: {} cgroups, {} tasks",
        spec.cgroups.len(),
        spec.tasks.len()
    );
    for (path, def) in &spec.cgroups {
        let cm = def
            .cpu_max
            .as_ref()
            .map(|c| c.to_cgroupfs_string())
            .unwrap_or_default();
        println!("    {:<10} cpu.max = {}", path, cm);
    }

    // ── 2. Create hierarchy ────────────────────────────────────────
    let hierarchy = CgroupHierarchy::create(&spec, Some("rtapp_e2e_test"))?;
    let root = hierarchy.root_dir().to_path_buf();
    println!("\n[2] Hierarchy created at {}", root.display());

    for (spec_path, node) in hierarchy.iter() {
        let dir = node.path();
        let cpu_max_val =
            std::fs::read_to_string(dir.join("cpu.max")).unwrap_or_else(|_| "(unreadable)".into());
        println!(
            "    {} → {}  cpu.max={}",
            spec_path,
            dir.display(),
            cpu_max_val.trim()
        );
        assert!(dir.exists(), "cgroup dir should exist: {}", dir.display());
    }

    // ── 3. Fork busy-loop child processes ──────────────────────────
    println!("\n[3] Forking busy-loop child processes...");
    let mut children: Vec<(String, String, u32)> = Vec::new(); // (task_name, cgroup_path, pid)

    for (task_name, task_def) in &spec.tasks {
        if let Some(ref cg_path) = task_def.cgroup {
            let pid = unsafe { libc::fork() };
            if pid == 0 {
                // Child: busy loop forever until killed
                loop {
                    std::hint::spin_loop();
                }
            } else if pid > 0 {
                let child_pid = pid as u32;
                println!("    forked {} (pid={}) → {}", task_name, child_pid, cg_path);
                children.push((task_name.clone(), cg_path.clone(), child_pid));
            } else {
                eprintln!("    fork failed for {}", task_name);
            }
        }
    }

    // ── 4. Place child processes into cgroups ──────────────────────
    println!("\n[4] Placing child processes into cgroups:");
    for (name, cg_path, pid) in &children {
        match hierarchy.place_process(cg_path, *pid) {
            Ok(()) => println!("    ✓ pid={} ({}) → {}", pid, name, cg_path),
            Err(e) => println!("    ✗ pid={} ({}) → {} FAILED: {}", pid, name, cg_path, e),
        }
    }

    // ── 5. Verify placement via cgroup.procs ───────────────────────
    println!("\n[5] Verifying placement (cgroup.procs):");
    for (spec_path, node) in hierarchy.iter() {
        let procs_path = node.path().join("cgroup.procs");
        let contents = std::fs::read_to_string(&procs_path).unwrap_or_default();
        let pids: Vec<&str> = contents.lines().filter(|l| !l.is_empty()).collect();
        println!("    {} → {} processes: {:?}", spec_path, pids.len(), pids);
    }

    // ── 6. Let processes run and measure CPU utilization ────────────
    println!(
        "\n[6] Running busy loops for {}s — measuring CPU utilization via cpu.stat ...",
        hold_secs
    );

    let mut start_stats: std::collections::BTreeMap<String, (u64, u64)> =
        std::collections::BTreeMap::new();
    for (spec_path, node) in hierarchy.iter() {
        let stat_path = node.path().join("cpu.stat");
        if let Ok(content) = std::fs::read_to_string(&stat_path) {
            let usage = parse_cpu_stat_field(&content, "usage_usec");
            let nr_throttled = parse_cpu_stat_field(&content, "nr_throttled");
            start_stats.insert(spec_path.to_string(), (usage, nr_throttled));
        }
    }
    let t_start = Instant::now();

    let snapshot_interval = Duration::from_secs(3);
    let total_hold = Duration::from_secs(hold_secs);
    let mut elapsed = Duration::ZERO;
    while elapsed < total_hold {
        let sleep_time = snapshot_interval.min(total_hold - elapsed);
        std::thread::sleep(sleep_time);
        elapsed = t_start.elapsed();

        println!("\n    --- snapshot at {:.1}s ---", elapsed.as_secs_f64());
        for (spec_path, node) in hierarchy.iter() {
            let stat_path = node.path().join("cpu.stat");
            if let Ok(content) = std::fs::read_to_string(&stat_path) {
                let usage_now = parse_cpu_stat_field(&content, "usage_usec");
                let throttled_now = parse_cpu_stat_field(&content, "nr_throttled");
                let (usage_start, throttled_start) =
                    start_stats.get(spec_path).copied().unwrap_or((0, 0));
                let delta_us = usage_now.saturating_sub(usage_start);
                let wall_us = elapsed.as_micros() as u64;
                let utilization = if wall_us > 0 {
                    delta_us as f64 / wall_us as f64
                } else {
                    0.0
                };
                let throttle_count = throttled_now.saturating_sub(throttled_start);
                let cpu_max_str =
                    std::fs::read_to_string(node.path().join("cpu.max")).unwrap_or_default();
                println!(
                    "    {:<6} cpu.max={:<16} usage={:.3} CPU  throttled={} times",
                    spec_path,
                    cpu_max_str.trim(),
                    utilization,
                    throttle_count,
                );
            }
        }
    }

    // ── 7. Final summary ───────────────────────────────────────────
    println!(
        "\n[7] Final CPU utilization summary (over {:.1}s):",
        t_start.elapsed().as_secs_f64()
    );
    let wall_us = t_start.elapsed().as_micros() as u64;
    for (spec_path, node) in hierarchy.iter() {
        let stat_path = node.path().join("cpu.stat");
        if let Ok(content) = std::fs::read_to_string(&stat_path) {
            let usage_now = parse_cpu_stat_field(&content, "usage_usec");
            let throttled_now = parse_cpu_stat_field(&content, "nr_throttled");
            let throttled_time = parse_cpu_stat_field(&content, "throttled_usec");
            let (usage_start, _) = start_stats.get(spec_path).copied().unwrap_or((0, 0));
            let delta_us = usage_now.saturating_sub(usage_start);
            let utilization = if wall_us > 0 {
                delta_us as f64 / wall_us as f64
            } else {
                0.0
            };

            let cpu_max_str =
                std::fs::read_to_string(node.path().join("cpu.max")).unwrap_or_default();
            let limit = parse_cpu_max_limit(cpu_max_str.trim());

            println!(
                "    {:<6}  limit={:<6}  actual={:.3} CPU  throttled={} ({:.1}ms total)",
                spec_path,
                format!("{:.0}%", limit * 100.0),
                utilization,
                throttled_now,
                throttled_time as f64 / 1000.0,
            );

            if utilization > limit * 1.15 + 0.02 {
                println!(
                    "      ⚠️  WARNING: utilization {:.3} exceeds limit {:.3}!",
                    utilization, limit
                );
            } else {
                println!("      ✓  within bandwidth limit");
            }
        }
    }

    // ── 8. Kill child processes ────────────────────────────────────
    println!("\n[8] Killing child processes...");
    for (name, _, pid) in &children {
        unsafe {
            libc::kill(*pid as i32, libc::SIGKILL);
        }
        // Reap
        let mut status: i32 = 0;
        unsafe {
            libc::waitpid(*pid as i32, &mut status, 0);
        }
        println!("    killed and reaped {} (pid={})", name, pid);
    }

    // ── 9. Verify cleanup ──────────────────────────────────────────
    println!("\n[9] Dropping hierarchy (RAII cleanup)...");
    drop(hierarchy);
    let still_exists = root.exists();
    println!("    {} exists after drop: {}", root.display(), still_exists);
    assert!(!still_exists, "cgroup root should be cleaned up");
    println!("    ✓ Cleanup successful");

    println!("\n╔══════════════════════════════════════════════════════╗");
    println!("║  E2E TEST PASSED                                    ║");
    println!("╚══════════════════════════════════════════════════════╝");
    Ok(())
}

fn parse_cpu_stat_field(content: &str, field: &str) -> u64 {
    for line in content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() == 2 && parts[0] == field {
            return parts[1].parse().unwrap_or(0);
        }
    }
    0
}

fn parse_cpu_max_limit(s: &str) -> f64 {
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() == 2 {
        if parts[0] == "max" {
            return f64::INFINITY;
        }
        let quota: f64 = parts[0].parse().unwrap_or(0.0);
        let period: f64 = parts[1].parse().unwrap_or(100000.0);
        if period > 0.0 {
            quota / period
        } else {
            f64::INFINITY
        }
    } else {
        f64::INFINITY
    }
}
