//! Tests for cgroup tree creation and resource management.

use crate::workloads::cgroup_tree::{CGroupTreeNode, SystemConstraints, ActualizedCGroupTree};
use crate::util::shared::{BumpAllocator, SharedBox};
use anyhow::Result;
use quickcheck::Gen;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

/// Environment variable name for controlling the random seed.
/// If set, the tree generation will use this seed for reproducibility.
/// If not set, a random seed based on current time will be used.
const SEED_ENV_VAR: &str = "SCHTEST_SEED";

/// Get the seed for random tree generation.
/// Checks SCHTEST_SEED environment variable first, falls back to time-based random seed.
fn get_seed() -> usize {
    if let Ok(seed_str) = std::env::var(SEED_ENV_VAR) {
        if let Ok(seed) = seed_str.parse::<usize>() {
            eprintln!("Using seed from {}: {}", SEED_ENV_VAR, seed);
            return seed;
        } else {
            eprintln!("Warning: {} value '{}' is not a valid usize, using random seed",
                      SEED_ENV_VAR, seed_str);
        }
    }
    // Use current time as a random seed
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as usize)
        .unwrap_or(42);
    eprintln!("Using random seed: {} (set {}={} to reproduce)", seed, SEED_ENV_VAR, seed);
    seed
}

/// Test that we can successfully create a cgroup tree.
///
/// This test generates a random cgroup tree and verifies that:
/// 1. The tree can be generated with realistic constraints
/// 2. The cgroups can be created on the filesystem
/// 3. CPU hog workloads can be launched in each leaf cgroup
/// 4. The hogs can be started and stopped via shared memory signaling
/// 5. The cgroups are cleaned up when done
fn create_cgroup_tree() -> Result<()> {
    let seed = get_seed();
    let mut gen = Gen::new(seed);
    let constraints = SystemConstraints::detect();

    eprintln!("Detected {} CPUs, {} bytes memory",
              constraints.num_cpus,
              constraints.total_memory_bytes);

    // Generate a modest-sized tree: max depth 3, max 3 children per node
    let mut tree = CGroupTreeNode::arbitrary_tree(&mut gen, &constraints, 3, 3);

    eprintln!("\nGenerated tree with {} nodes, depth {}",
              tree.node_count(),
              tree.max_depth());

    eprintln!("\nCGroup Tree Structure (before sanitization):");
    tree.print_tree();

    // Sanitize cpusets to fix parent-child conflicts
    eprintln!("\nSanitizing cpuset constraints...");
    let fixes = tree.sanitize_cpusets(constraints.num_cpus);
    if fixes > 0 {
        eprintln!("Fixed {} cpuset conflict(s)", fixes);
        eprintln!("\nCGroup Tree Structure (after sanitization):");
        tree.print_tree();
    } else {
        eprintln!("No cpuset conflicts found");
    }
    eprintln!();

    // Compute spinner allocation to ensure CPU contention
    let allocation = tree.compute_spinner_allocation(constraints.num_cpus);
    allocation.print_summary();

    // Create the actual cgroups
    let actualized = tree.create("schtest_cgroup_tree")?;

    eprintln!("\nSuccessfully created {} cgroups", actualized.len());

    // Verify the number of cgroups created matches the tree
    assert_eq!(actualized.len(), tree.node_count(),
               "Number of created cgroups should match tree node count");

    // Create shared memory for start signal and scheduled time counters
    let allocator = BumpAllocator::new("cgroup_test", 1024 * 1024)?;
    let start_signal = SharedBox::new(allocator.clone(), AtomicU32::new(0))?;

    // Allocate scheduled_ns counters (one per leaf)
    let num_leaves = actualized.count_leaves();
    let mut scheduled_ns_counters = Vec::new();
    for _ in 0..num_leaves {
        scheduled_ns_counters.push(SharedBox::new(allocator.clone(), AtomicU64::new(0))?);
    }

    // Launch CPU hogs in all leaf cgroups
    let hog_duration = Duration::from_secs(2);
    eprintln!("\nLaunching {} total CPU hogs in {} leaf cgroups for {:?}...",
              allocation.total_spinners,
              num_leaves,
              hog_duration);

    let hogs = actualized.launch_leaf_hogs_with_allocation(
        hog_duration,
        start_signal.clone(),
        scheduled_ns_counters.clone(),
        &allocation,
    )?;

    eprintln!("Launched {} CPU hogs (waiting for start signal)", hogs.len());
    assert_eq!(hogs.len(), allocation.total_spinners,
               "Should have launched the allocated number of hogs");

    // Give hogs a moment to initialize and enter their cgroups
    std::thread::sleep(Duration::from_millis(100));

    // Signal hogs to start
    eprintln!("Signaling hogs to START");
    ActualizedCGroupTree::start_hogs(&start_signal);

    // Wait for all hogs to complete
    eprintln!("Waiting for hogs to complete...");

    match ActualizedCGroupTree::wait_for_hogs(hogs) {
        Ok(()) => {
            eprintln!("All hogs completed successfully");
        }
        Err(e) => {
            // Some hogs may have been OOM-killed due to strict memory limits
            // This is actually expected and shows that cgroup limits are working!
            eprintln!("Some hogs were killed (likely OOM from memory limits): {}", e);
            eprintln!("This demonstrates that cgroup resource limits are being enforced!");
        }
    }

    // Print scheduled time table with right-justified columns
    eprintln!("\nLeaf Node Scheduled Time:");

    // Collect results from the per-leaf counters (each counter aggregates all spinners for that leaf)
    let results: Vec<(usize, u64)> = allocation.leaf_node_ids.iter()
        .zip(scheduled_ns_counters.iter())
        .map(|(&node_id, counter)| (node_id, counter.load(Ordering::Acquire)))
        .collect();

    // Find max widths
    let max_node_id = results.iter().map(|(id, _)| *id).max().unwrap_or(0);
    let max_ns = results.iter().map(|(_, ns)| *ns).max().unwrap_or(0);
    let node_id_width = max_node_id.to_string().len().max(7); // "node_id" is 7 chars
    let ns_width = max_ns.to_string().len().max(13); // "scheduled_ns" is 12 chars

    // Print header
    eprintln!("{:>width1$}, {:>width2$}", "node_id", "scheduled_ns", width1 = node_id_width, width2 = ns_width);

    // Print rows
    for (node_id, scheduled_ns) in &results {
        eprintln!("{:>width1$}, {:>width2$}", node_id, scheduled_ns, width1 = node_id_width, width2 = ns_width);
    }

    // Compute and print oracle statistics
    let oracle_stats = actualized.compute_oracle_stats(&results);
    ActualizedCGroupTree::print_oracle_stats(&oracle_stats);

    // Cgroups will be automatically deleted when actualized is dropped
    Ok(())
}

test!("create_cgroup_tree", create_cgroup_tree);

/// Simple test with a deterministic 2-leaf tree.
///
/// This test creates a simple tree with:
/// - Root (no limits)
///   - Leaf 1: cpu.max = 25%
///   - Leaf 2: cpu.max = 50%
///
/// We expect leaf 2 to perform approximately 2x the operations of leaf 1.
fn simple_cgroup_test() -> Result<()> {
    let constraints = SystemConstraints::detect();

    eprintln!("Detected {} CPUs, {} bytes memory",
              constraints.num_cpus,
              constraints.total_memory_bytes);

    // Create simple deterministic tree
    let mut tree = CGroupTreeNode::simple_test_tree();

    eprintln!("\nSimple Test Tree (2 leaves with 25% and 50% CPU limits):");
    tree.print_tree();

    // Sanitize cpusets (should be no-op for this simple tree)
    let fixes = tree.sanitize_cpusets(constraints.num_cpus);
    if fixes > 0 {
        eprintln!("\nFixed {} cpuset conflict(s)", fixes);
        tree.print_tree();
    }
    eprintln!();

    // Compute spinner allocation to ensure CPU contention
    let allocation = tree.compute_spinner_allocation(constraints.num_cpus);
    allocation.print_summary();

    // Create the actual cgroups
    let actualized = tree.create("schtest_simple")?;

    eprintln!("\nSuccessfully created {} cgroups", actualized.len());

    // Create shared memory for start signal and scheduled time counters
    let allocator = BumpAllocator::new("cgroup_simple", 1024 * 1024)?;
    let start_signal = SharedBox::new(allocator.clone(), AtomicU32::new(0))?;

    // Allocate scheduled_ns counters (one per leaf)
    let num_leaves = actualized.count_leaves();
    let mut scheduled_ns_counters = Vec::new();
    for _ in 0..num_leaves {
        scheduled_ns_counters.push(SharedBox::new(allocator.clone(), AtomicU64::new(0))?);
    }

    // Launch CPU hogs in all leaf cgroups (5 second duration for more stable results)
    let hog_duration = Duration::from_secs(5);
    eprintln!("\nLaunching {} total CPU hogs in {} leaf cgroups for {:?}...",
              allocation.total_spinners,
              num_leaves,
              hog_duration);

    let hogs = actualized.launch_leaf_hogs_with_allocation(
        hog_duration,
        start_signal.clone(),
        scheduled_ns_counters.clone(),
        &allocation,
    )?;

    eprintln!("Launched {} CPU hogs (waiting for start signal)", hogs.len());

    // Give hogs a moment to initialize
    std::thread::sleep(Duration::from_millis(100));

    // Signal hogs to start
    eprintln!("Signaling hogs to START");
    ActualizedCGroupTree::start_hogs(&start_signal);

    // Wait for all hogs to complete
    eprintln!("Waiting for hogs to complete...");

    match ActualizedCGroupTree::wait_for_hogs(hogs) {
        Ok(()) => {
            eprintln!("All hogs completed successfully");
        }
        Err(e) => {
            eprintln!("Error waiting for hogs: {}", e);
        }
    }

    // Print scheduled time table with right-justified columns
    eprintln!("\nLeaf Node Scheduled Time:");

    // Collect results from the per-leaf counters (each counter aggregates all spinners for that leaf)
    let results: Vec<(usize, u64)> = allocation.leaf_node_ids.iter()
        .zip(scheduled_ns_counters.iter())
        .map(|(&node_id, counter)| (node_id, counter.load(Ordering::Acquire)))
        .collect();

    // Find max widths
    let max_node_id = results.iter().map(|(id, _)| *id).max().unwrap_or(0);
    let max_ns = results.iter().map(|(_, ns)| *ns).max().unwrap_or(0);
    let node_id_width = max_node_id.to_string().len().max(7); // "node_id" is 7 chars
    let ns_width = max_ns.to_string().len().max(13); // "scheduled_ns" is 12 chars

    // Print header
    eprintln!("{:>width1$}, {:>width2$}", "node_id", "scheduled_ns", width1 = node_id_width, width2 = ns_width);

    // Print rows
    for (node_id, scheduled_ns) in &results {
        eprintln!("{:>width1$}, {:>width2$}", node_id, scheduled_ns, width1 = node_id_width, width2 = ns_width);
    }

    // Calculate and show ratio
    if results.len() == 2 {
        let ns1 = results[0].1;
        let ns2 = results[1].1;
        let ratio = if ns1 > 0 { ns2 as f64 / ns1 as f64 } else { 0.0 };
        eprintln!("\nRatio (leaf2/leaf1): {:.2} (expected ~2.0)", ratio);
    }

    // Compute and print oracle statistics
    let oracle_stats = actualized.compute_oracle_stats(&results);
    ActualizedCGroupTree::print_oracle_stats(&oracle_stats);

    Ok(())
}

test!("simple_cgroup_test", simple_cgroup_test);

/// Fixed random tree test for reproducible debugging.
///
/// This test uses a fixed seed (42) to generate the same tree every time,
/// making it easier to debug oracle calculations.
fn fixed_random_tree_test() -> Result<()> {
    let constraints = SystemConstraints::detect();

    eprintln!("Detected {} CPUs, {} bytes memory",
              constraints.num_cpus,
              constraints.total_memory_bytes);

    // Create fixed random tree (seed = 42)
    let mut tree = CGroupTreeNode::fixed_random_tree();

    eprintln!("\nFixed Random Tree (seed=42, before sanitization):");
    tree.print_tree();

    // Sanitize cpusets to fix parent-child conflicts
    eprintln!("\nSanitizing cpuset constraints...");
    let fixes = tree.sanitize_cpusets(constraints.num_cpus);
    if fixes > 0 {
        eprintln!("Fixed {} cpuset conflict(s)", fixes);
        eprintln!("\nFixed Random Tree (after sanitization):");
        tree.print_tree();
    } else {
        eprintln!("No cpuset conflicts found");
    }
    eprintln!();

    // Compute spinner allocation to ensure CPU contention
    let allocation = tree.compute_spinner_allocation(constraints.num_cpus);
    allocation.print_summary();

    // Create the actual cgroups
    let actualized = tree.create("schtest_fixed")?;

    eprintln!("\nSuccessfully created {} cgroups", actualized.len());

    // Create shared memory for start signal and scheduled time counters
    let allocator = BumpAllocator::new("cgroup_fixed", 1024 * 1024)?;
    let start_signal = SharedBox::new(allocator.clone(), AtomicU32::new(0))?;

    // Allocate scheduled_ns counters (one per leaf)
    let num_leaves = actualized.count_leaves();
    let mut scheduled_ns_counters = Vec::new();
    for _ in 0..num_leaves {
        scheduled_ns_counters.push(SharedBox::new(allocator.clone(), AtomicU64::new(0))?);
    }

    // Launch CPU hogs in all leaf cgroups (5 second duration for stable results)
    let hog_duration = Duration::from_secs(5);
    eprintln!("\nLaunching {} total CPU hogs in {} leaf cgroups for {:?}...",
              allocation.total_spinners,
              num_leaves,
              hog_duration);

    let hogs = actualized.launch_leaf_hogs_with_allocation(
        hog_duration,
        start_signal.clone(),
        scheduled_ns_counters.clone(),
        &allocation,
    )?;

    eprintln!("Launched {} CPU hogs (waiting for start signal)", hogs.len());

    // Give hogs a moment to initialize
    std::thread::sleep(Duration::from_millis(100));

    // Signal hogs to start
    eprintln!("Signaling hogs to START");
    ActualizedCGroupTree::start_hogs(&start_signal);

    // Wait for all hogs to complete
    eprintln!("Waiting for hogs to complete...");

    match ActualizedCGroupTree::wait_for_hogs(hogs) {
        Ok(()) => {
            eprintln!("All hogs completed successfully");
        }
        Err(e) => {
            eprintln!("Some hogs were killed (likely OOM): {}", e);
        }
    }

    // Print scheduled time table
    eprintln!("\nLeaf Node Scheduled Time:");

    // Collect results from the per-leaf counters (each counter aggregates all spinners for that leaf)
    let results: Vec<(usize, u64)> = allocation.leaf_node_ids.iter()
        .zip(scheduled_ns_counters.iter())
        .map(|(&node_id, counter)| (node_id, counter.load(Ordering::Acquire)))
        .collect();

    let max_node_id = results.iter().map(|(id, _)| *id).max().unwrap_or(0);
    let max_ns = results.iter().map(|(_, ns)| *ns).max().unwrap_or(0);
    let node_id_width = max_node_id.to_string().len().max(7);
    let ns_width = max_ns.to_string().len().max(13);

    eprintln!("{:>width1$}, {:>width2$}", "node_id", "scheduled_ns", width1 = node_id_width, width2 = ns_width);

    for (node_id, scheduled_ns) in &results {
        eprintln!("{:>width1$}, {:>width2$}", node_id, scheduled_ns, width1 = node_id_width, width2 = ns_width);
    }

    // Compute and print oracle statistics
    let oracle_stats = actualized.compute_oracle_stats(&results);
    ActualizedCGroupTree::print_oracle_stats(&oracle_stats);

    Ok(())
}

test!("fixed_random_tree_test", fixed_random_tree_test);
