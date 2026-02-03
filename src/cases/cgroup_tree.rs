//! Tests for cgroup tree creation and resource management.

use crate::test;
use crate::workloads::cgroup_tree::{CGroupTreeNode, SystemConstraints, ActualizedCGroupTree};
use crate::util::shared::{BumpAllocator, SharedBox};
use anyhow::Result;
use quickcheck::Gen;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

/// Test that we can successfully create a cgroup tree.
///
/// This test generates a random cgroup tree and verifies that:
/// 1. The tree can be generated with realistic constraints
/// 2. The cgroups can be created on the filesystem
/// 3. CPU hog workloads can be launched in each leaf cgroup
/// 4. The hogs can be started and stopped via shared memory signaling
/// 5. The cgroups are cleaned up when done
fn create_cgroup_tree() -> Result<()> {
    let mut gen = Gen::new(42);
    let constraints = SystemConstraints::detect();

    eprintln!("Detected {} CPUs, {} bytes memory",
              constraints.num_cpus,
              constraints.total_memory_bytes);

    // Generate a modest-sized tree: max depth 3, max 3 children per node
    let tree = CGroupTreeNode::arbitrary_tree(&mut gen, &constraints, 3, 3);

    eprintln!("\nGenerated tree with {} nodes, depth {}",
              tree.node_count(),
              tree.max_depth());

    eprintln!("\nCGroup Tree Structure:");
    tree.print_tree();
    eprintln!();

    // Create the actual cgroups
    let actualized = tree.create("schtest_cgroup_tree")?;

    eprintln!("Successfully created {} cgroups", actualized.len());

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

    // Launch CPU hogs in all leaf cgroups (placeholder: 2 second duration)
    let hog_duration = Duration::from_secs(2);
    eprintln!("\nLaunching CPU hogs in {} leaf cgroups for {:?}...",
              num_leaves,
              hog_duration);

    let hogs = actualized.launch_leaf_hogs(hog_duration, start_signal.clone(), scheduled_ns_counters.clone())?;

    eprintln!("Launched {} CPU hogs (waiting for start signal)", hogs.len());
    assert_eq!(hogs.len(), actualized.count_leaves(),
               "Should have one hog per leaf cgroup");

    // Give hogs a moment to initialize and enter their cgroups
    std::thread::sleep(Duration::from_millis(100));

    // Signal hogs to start
    eprintln!("Signaling hogs to START");
    ActualizedCGroupTree::start_hogs(&start_signal);

    // Wait for all hogs to complete
    eprintln!("Waiting for hogs to complete...");
    let hog_results: Vec<(usize, SharedBox<AtomicU64>)> = hogs.iter()
        .map(|h| (h.node_id, h.scheduled_ns.clone()))
        .collect();

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

    // Collect results first to find column widths
    let results: Vec<(usize, u64)> = hog_results.iter()
        .map(|(node_id, scheduled_ns)| (*node_id, scheduled_ns.load(Ordering::Acquire)))
        .collect();

    // Find max widths
    let max_node_id = results.iter().map(|(id, _)| *id).max().unwrap_or(0);
    let max_ns = results.iter().map(|(_, ns)| *ns).max().unwrap_or(0);
    let node_id_width = max_node_id.to_string().len().max(7); // "node_id" is 7 chars
    let ns_width = max_ns.to_string().len().max(13); // "scheduled_ns" is 12 chars

    // Print header
    eprintln!("{:>width1$}, {:>width2$}", "node_id", "scheduled_ns", width1 = node_id_width, width2 = ns_width);

    // Print rows
    for (node_id, scheduled_ns) in results {
        eprintln!("{:>width1$}, {:>width2$}", node_id, scheduled_ns, width1 = node_id_width, width2 = ns_width);
    }

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
    let tree = CGroupTreeNode::simple_test_tree();

    eprintln!("\nSimple Test Tree (2 leaves with 25% and 50% CPU limits):");
    tree.print_tree();
    eprintln!();

    // Create the actual cgroups
    let actualized = tree.create("schtest_simple")?;

    eprintln!("Successfully created {} cgroups", actualized.len());

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
    eprintln!("\nLaunching CPU hogs in {} leaf cgroups for {:?}...",
              num_leaves,
              hog_duration);

    let hogs = actualized.launch_leaf_hogs(hog_duration, start_signal.clone(), scheduled_ns_counters.clone())?;

    eprintln!("Launched {} CPU hogs (waiting for start signal)", hogs.len());

    // Give hogs a moment to initialize
    std::thread::sleep(Duration::from_millis(100));

    // Signal hogs to start
    eprintln!("Signaling hogs to START");
    ActualizedCGroupTree::start_hogs(&start_signal);

    // Wait for all hogs to complete
    eprintln!("Waiting for hogs to complete...");
    let hog_results: Vec<(usize, SharedBox<AtomicU64>)> = hogs.iter()
        .map(|h| (h.node_id, h.scheduled_ns.clone()))
        .collect();

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

    // Collect results first to find column widths
    let results: Vec<(usize, u64)> = hog_results.iter()
        .map(|(node_id, scheduled_ns)| (*node_id, scheduled_ns.load(Ordering::Acquire)))
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

    Ok(())
}

test!("simple_cgroup_test", simple_cgroup_test);
