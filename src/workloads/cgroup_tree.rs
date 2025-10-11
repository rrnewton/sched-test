//! CGroup tree workload implementation.

use cgroups_rs::fs::{Resources, MaxValue};
use quickcheck::{Arbitrary, Gen};

/// System resource constraints used for generating realistic cgroup configurations.
#[derive(Debug, Clone, Copy)]
pub struct SystemConstraints {
    /// Number of CPUs available on the system
    pub num_cpus: usize,
    /// Total memory in bytes
    pub total_memory_bytes: u64,
}

impl SystemConstraints {
    /// Detect system constraints from the current machine
    pub fn detect() -> Self {
        let num_cpus = num_cpus::get();

        // Read total memory from /proc/meminfo
        let total_memory_bytes = Self::read_total_memory().unwrap_or(16 * 1024 * 1024 * 1024); // Default to 16GB

        Self {
            num_cpus,
            total_memory_bytes,
        }
    }

    /// Read total memory from /proc/meminfo
    fn read_total_memory() -> Option<u64> {
        let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
        for line in meminfo.lines() {
            if line.starts_with("MemTotal:") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    let kb = parts[1].parse::<u64>().ok()?;
                    return Some(kb * 1024); // Convert KB to bytes
                }
            }
        }
        None
    }
}

/// A node in a cgroup tree hierarchy.
#[derive(Debug, Clone)]
pub struct CGroupTreeNode {
    /// The resource limits for this cgroup
    pub resources: RandResources,
    /// Child cgroups under this node
    pub children: Vec<CGroupTreeNode>,
}

impl CGroupTreeNode {
    /// Generate a random cgroup tree with the given constraints and maximum depth.
    ///
    /// # Arguments
    /// * `g` - QuickCheck's random number generator
    /// * `constraints` - System constraints (CPU count, memory size) to respect
    /// * `max_depth` - Maximum depth of the tree (0 means just a single node)
    /// * `max_children` - Maximum number of children per node
    ///
    /// # Example
    /// ```rust,ignore
    /// let constraints = SystemConstraints::detect();
    /// let tree = CGroupTreeNode::arbitrary_tree(&mut gen, &constraints, 3, 4);
    /// ```
    pub fn arbitrary_tree(
        g: &mut Gen,
        constraints: &SystemConstraints,
        max_depth: usize,
        max_children: usize,
    ) -> Self {
        let resources = RandResources::arbitrary_with_constraints(g, constraints);

        let children = if max_depth == 0 {
            Vec::new()
        } else {
            // Randomly decide how many children (0 to max_children)
            let num_children = if max_children > 0 {
                let n: usize = usize::arbitrary(g);
                n % (max_children + 1)
            } else {
                0
            };

            // Recursively generate children with reduced depth
            (0..num_children)
                .map(|_| Self::arbitrary_tree(g, constraints, max_depth - 1, max_children))
                .collect()
        };

        Self { resources, children }
    }

    /// Count the total number of nodes in this tree.
    pub fn node_count(&self) -> usize {
        1 + self.children.iter().map(|child| child.node_count()).sum::<usize>()
    }

    /// Get the maximum depth of this tree.
    pub fn max_depth(&self) -> usize {
        if self.children.is_empty() {
            0
        } else {
            1 + self.children.iter().map(|child| child.max_depth()).max().unwrap_or(0)
        }
    }
}

/// A workload that operates on cgroup trees.
#[derive(Default)]
pub struct CGroupTree {
    // TODO: Add fields as needed for cgroup tree operations
}

impl CGroupTree {
    /// Create a new CGroupTree workload instance.
    pub fn new() -> Self {
        Self::default()
    }
}

/// Newtype wrapper around Resources to implement Arbitrary.
///
/// This pattern allows integration with quickcheck macros while also supporting
/// explicit constraint control when needed.
#[derive(Debug, Clone)]
pub struct RandResources(pub Resources);

impl RandResources {
    /// Generate random resources with explicit system constraints.
    ///
    /// This is the recommended way to generate resources with realistic bounds
    /// that respect the actual capabilities of a specific machine.
    ///
    /// # Arguments
    /// * `g` - QuickCheck's random number generator
    /// * `constraints` - System constraints (CPU count, memory size) to respect
    ///
    /// # Example
    /// ```rust,ignore
    /// let constraints = SystemConstraints {
    ///     num_cpus: 8,
    ///     total_memory_bytes: 16 * 1024 * 1024 * 1024,
    /// };
    /// let resources = RandResources::arbitrary_with_constraints(&mut gen, &constraints);
    /// ```
    pub fn arbitrary_with_constraints(g: &mut Gen, constraints: &SystemConstraints) -> Self {
        Self::arbitrary_internal(g, Some(constraints))
    }

    /// Internal generator that optionally uses constraints
    fn arbitrary_internal(g: &mut Gen, constraints: Option<&SystemConstraints>) -> Self {
        // Detect constraints if not provided
        let detected_constraints;
        let constraints = match constraints {
            Some(c) => c,
            None => {
                detected_constraints = SystemConstraints::detect();
                &detected_constraints
            }
        };

        let max_cpu = constraints.num_cpus.saturating_sub(1).max(0);
        let max_memory = constraints.total_memory_bytes;

        let mut resources = Resources::default();

        // Randomly set CPU resources
        if bool::arbitrary(g) {
            // CPU shares (typically 1-1024, with 100 being default)
            resources.cpu.shares = if bool::arbitrary(g) {
                let shares: u16 = u16::arbitrary(g);
                Some(std::cmp::max(shares % 1024, 1) as u64) // 1-1024
            } else {
                None
            };

            // CPU quota and period should be set together
            if bool::arbitrary(g) {
                let period: u32 = u32::arbitrary(g);
                let period = ((period % 999000) + 1000) as u64; // 1000-1000000 microseconds
                let quota_factor: u32 = u32::arbitrary(g);
                let quota = ((quota_factor % period as u32) + 1000) as i64; // 1000 to period
                resources.cpu.quota = Some(quota);
                resources.cpu.period = Some(period);
            }

            // CPUSET - specific CPUs (simple format like "0", "0-2", "0,2,4")
            // Now respects actual CPU count on the system!
            if bool::arbitrary(g) && max_cpu > 0 {
                let cpu_type: u8 = u8::arbitrary(g);
                if cpu_type % 2 == 0 {
                    // Single CPU
                    let cpu_num: u64 = u64::arbitrary(g);
                    resources.cpu.cpus = Some(format!("{}", cpu_num % (max_cpu as u64 + 1)));
                } else {
                    // CPU range
                    let start: u64 = u64::arbitrary(g);
                    let len: u64 = u64::arbitrary(g);
                    let start = start % (max_cpu as u64 + 1);
                    let end = std::cmp::min(start + (len % 4) + 1, max_cpu as u64);
                    resources.cpu.cpus = Some(format!("{}-{}", start, end));
                }
            }
        }

        // Randomly set Memory resources
        // Now respects actual system memory!
        if bool::arbitrary(g) {
            // Memory hard limit (avoid very small values, min 1MB)
            // Cap at system memory
            resources.memory.memory_hard_limit = if bool::arbitrary(g) {
                let mem_bytes: u64 = u64::arbitrary(g);
                let max_limit = max_memory.min(100 * 1024 * 1024 * 1024); // Cap at 100GB for sanity
                let limit = ((mem_bytes % max_limit).max(1024 * 1024)) as i64; // Min 1MB
                Some(limit)
            } else {
                None
            };

            // Memory soft limit (should be <= hard limit if both are set)
            resources.memory.memory_soft_limit = if bool::arbitrary(g) {
                let limit = resources.memory.memory_hard_limit
                    .unwrap_or(max_memory.min(1024 * 1024 * 1024) as i64);
                let soft_factor: u64 = u64::arbitrary(g);
                let soft_limit = (soft_factor as i64 % limit).max(1024 * 1024);
                Some(soft_limit)
            } else {
                None
            };

            // Swappiness (0-100)
            resources.memory.swappiness = if bool::arbitrary(g) {
                let swappiness: u8 = u8::arbitrary(g);
                Some((swappiness % 101) as u64) // 0-100
            } else {
                None
            };

            // Memory swap limit
            resources.memory.memory_swap_limit = if bool::arbitrary(g) {
                let swap_bytes: u64 = u64::arbitrary(g);
                let max_swap = max_memory * 2; // Swap can be up to 2x system memory
                let limit = ((swap_bytes % max_swap).max(1024 * 1024)) as i64;
                Some(limit)
            } else {
                None
            };
        }

        // Randomly set PID resources
        if bool::arbitrary(g) {
            resources.pid.maximum_number_of_processes = if bool::arbitrary(g) {
                let max_procs: u16 = u16::arbitrary(g);
                Some(MaxValue::Value(std::cmp::max(max_procs % 10000, 1) as i64))
            } else {
                None
            };
        }

        // Randomly set Block I/O resources
        if bool::arbitrary(g) {
            // I/O weight (typically 10-1000, default 100)
            resources.blkio.weight = if bool::arbitrary(g) {
                let weight: u16 = u16::arbitrary(g);
                Some(std::cmp::max((weight % 991) + 10, 10)) // 10-1000
            } else {
                None
            };

            resources.blkio.leaf_weight = if bool::arbitrary(g) {
                let weight: u16 = u16::arbitrary(g);
                Some(std::cmp::max((weight % 991) + 10, 10)) // 10-1000
            } else {
                None
            };
        }

        // Randomly set Network resources
        if bool::arbitrary(g) {
            resources.network.class_id = if bool::arbitrary(g) {
                let class_id: u16 = u16::arbitrary(g);
                Some((class_id % 65534 + 1) as u64) // 1-65535
            } else {
                None
            };
        }

        RandResources(resources)
    }
}

// Implement Arbitrary trait for backwards compatibility and use with quickcheck macros
// This uses default system constraints
impl Arbitrary for RandResources {
    fn arbitrary(g: &mut Gen) -> Self {
        Self::arbitrary_internal(g, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quickcheck::{quickcheck, TestResult};

    #[test]
    fn test_empty() {
        let _cgroup_tree = CGroupTree::new();
        // Empty test to verify build system integration
    }

    #[test]
    fn test_random_resources_generation() {
        fn prop_generates_resources(seed: u64) -> TestResult {
            let mut gen = quickcheck::Gen::new(seed as usize % 100);
            let rand_resources = RandResources::arbitrary(&mut gen);

            // Print the generated resources for inspection
            println!("Generated Resources: {:?}", rand_resources);

            TestResult::passed()
        }

        quickcheck(prop_generates_resources as fn(u64) -> TestResult);
    }

    #[test]
    fn test_random_resources_with_constraints() {
        fn prop_respects_constraints(seed: u64) -> TestResult {
            let constraints = SystemConstraints {
                num_cpus: 4,
                total_memory_bytes: 8 * 1024 * 1024 * 1024,
            };

            let mut gen = quickcheck::Gen::new(seed as usize % 100);
            let rand_resources = RandResources::arbitrary_with_constraints(&mut gen, &constraints);

            // Validate CPU constraints
            if let Some(ref cpus) = rand_resources.0.cpu.cpus {
                // Parse the cpuset string and check bounds
                println!("Generated cpuset: {}", cpus);
                // Basic validation - cpuset should not reference CPU >= 4
                for part in cpus.split(',') {
                    if let Some((start, end)) = part.split_once('-') {
                        let start_cpu: usize = start.parse().unwrap_or(0);
                        let end_cpu: usize = end.parse().unwrap_or(0);
                        if start_cpu >= constraints.num_cpus || end_cpu >= constraints.num_cpus {
                            return TestResult::failed();
                        }
                    } else if let Ok(cpu) = part.parse::<usize>() {
                        if cpu >= constraints.num_cpus {
                            return TestResult::failed();
                        }
                    }
                }
            }

            // Validate memory constraints
            if let Some(mem_limit) = rand_resources.0.memory.memory_hard_limit {
                if mem_limit < 0 || mem_limit as u64 > constraints.total_memory_bytes {
                    println!("Memory limit out of bounds: {} > {}", mem_limit, constraints.total_memory_bytes);
                    return TestResult::failed();
                }
            }

            TestResult::passed()
        }

        quickcheck(prop_respects_constraints as fn(u64) -> TestResult);
    }

    #[test]
    fn test_system_constraints_detection() {
        let constraints = SystemConstraints::detect();
        println!("Detected {} CPUs", constraints.num_cpus);
        println!("Detected {} bytes of memory ({} GB)",
                 constraints.total_memory_bytes,
                 constraints.total_memory_bytes / (1024 * 1024 * 1024));

        assert!(constraints.num_cpus > 0, "Should detect at least 1 CPU");
        assert!(constraints.total_memory_bytes > 0, "Should detect some memory");
    }

    #[test]
    fn test_example_usage_patterns() {
        let mut gen = quickcheck::Gen::new(42);

        // Pattern 1: Use default Arbitrary (automatically detects system constraints)
        let _resources1 = RandResources::arbitrary(&mut gen);

        // Pattern 2: Explicitly use system constraints
        let constraints = SystemConstraints::detect();
        let _resources2 = RandResources::arbitrary_with_constraints(&mut gen, &constraints);

        // Pattern 3: Use custom constraints (e.g., for testing on different machine configs)
        let small_machine = SystemConstraints {
            num_cpus: 2,
            total_memory_bytes: 4 * 1024 * 1024 * 1024, // 4GB
        };
        let _resources3 = RandResources::arbitrary_with_constraints(&mut gen, &small_machine);

        // Pattern 4: Use with quickcheck property testing
        // (See test_random_resources_with_constraints for an example)

        println!("All usage patterns work!");
    }

    #[test]
    fn test_cgroup_tree_generation() {
        let mut gen = quickcheck::Gen::new(456);  // Changed seed for more variety
        let constraints = SystemConstraints {
            num_cpus: 8,
            total_memory_bytes: 16 * 1024 * 1024 * 1024, // 16GB
        };

        let max_depth = 5;
        let tree = CGroupTreeNode::arbitrary_tree(&mut gen, &constraints, max_depth, 3);

        println!("Generated tree with {} nodes", tree.node_count());
        println!("Tree depth: {}", tree.max_depth());

        // Print a simple visualization
        fn print_tree(node: &CGroupTreeNode, prefix: &str, is_last: bool) {
            let connector = if is_last { "└─" } else { "├─" };
            print!("{}{} ", prefix, connector);

            // Print some key resource info
            let mut info = Vec::new();
            if let Some(ref cpus) = node.resources.0.cpu.cpus {
                info.push(format!("cpus:{}", cpus));
            }
            if let Some(shares) = node.resources.0.cpu.shares {
                info.push(format!("shares:{}", shares));
            }
            if let Some(mem) = node.resources.0.memory.memory_hard_limit {
                let mb = mem / (1024 * 1024);
                info.push(format!("mem:{}MB", mb));
            }

            println!("Node [{}]", if info.is_empty() { "no limits".to_string() } else { info.join(", ") });

            let child_prefix = format!("{}{}", prefix, if is_last { "   " } else { "│  " });
            for (i, child) in node.children.iter().enumerate() {
                let is_last_child = i == node.children.len() - 1;
                print_tree(child, &child_prefix, is_last_child);
            }
        }

        println!("\nTree structure:");
        print_tree(&tree, "", true);

        assert!(tree.max_depth() <= max_depth, "Tree should not exceed max depth");
        assert!(tree.node_count() >= 1, "Tree should have at least the root node");
    }

    #[test]
    fn test_cgroup_tree_properties() {
        fn prop_tree_respects_constraints(seed: u64) -> TestResult {
            let mut gen = quickcheck::Gen::new((seed as usize % 100) + 1);
            let constraints = SystemConstraints {
                num_cpus: 4,
                total_memory_bytes: 8 * 1024 * 1024 * 1024,
            };

            let max_depth = 2;
            let max_children = 3;

            let tree = CGroupTreeNode::arbitrary_tree(&mut gen, &constraints, max_depth, max_children);

            // Property 1: Tree depth should not exceed max_depth
            if tree.max_depth() > max_depth {
                return TestResult::failed();
            }

            // Property 2: Tree should have at least one node (the root)
            if tree.node_count() < 1 {
                return TestResult::failed();
            }

            // Property 3: No node should have more than max_children
            fn check_children_count(node: &CGroupTreeNode, max: usize) -> bool {
                if node.children.len() > max {
                    return false;
                }
                node.children.iter().all(|child| check_children_count(child, max))
            }

            if !check_children_count(&tree, max_children) {
                return TestResult::failed();
            }

            TestResult::passed()
        }

        quickcheck(prop_tree_respects_constraints as fn(u64) -> TestResult);
    }
}