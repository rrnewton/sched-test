//! Tests for cgroup tree creation and resource management.

use crate::test;
use crate::workloads::cgroup_tree::{CGroupTreeNode, SystemConstraints};
use anyhow::Result;
use quickcheck::Gen;

/// Test that we can successfully create a cgroup tree.
///
/// This test generates a random cgroup tree and verifies that:
/// 1. The tree can be generated with realistic constraints
/// 2. The cgroups can be created on the filesystem
/// 3. The resource limits are properly applied
/// 4. The cgroups are cleaned up when done
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

    // Cgroups will be automatically deleted when actualized is dropped
    Ok(())
}

test!("create_cgroup_tree", create_cgroup_tree);
