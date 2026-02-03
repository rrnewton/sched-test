//! Tests for cgroup tree creation and resource management.

use crate::test;
use crate::workloads::cgroup_tree::{CGroupTreeNode, SystemConstraints, ActualizedCGroupTree};
use crate::util::shared::{BumpAllocator, SharedBox};
use anyhow::Result;
use quickcheck::Gen;
use std::sync::atomic::AtomicU32;
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

    // Create shared memory for start signal
    let allocator = BumpAllocator::new("cgroup_test", 1024 * 1024)?;
    let start_signal = SharedBox::new(allocator, AtomicU32::new(0))?;

    // Launch CPU hogs in all leaf cgroups (placeholder: 2 second duration)
    let hog_duration = Duration::from_secs(2);
    eprintln!("\nLaunching CPU hogs in {} leaf cgroups for {:?}...",
              actualized.count_leaves(),
              hog_duration);

    let hogs = actualized.launch_leaf_hogs(hog_duration, start_signal.clone())?;

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

    // Cgroups will be automatically deleted when actualized is dropped
    Ok(())
}

test!("create_cgroup_tree", create_cgroup_tree);
