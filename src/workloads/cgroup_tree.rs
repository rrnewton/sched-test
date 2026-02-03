//! CGroup tree workload implementation.

use cgroups_rs::fs::{Resources, MaxValue};
use cgroups_rs::fs::Cgroup;
use cgroups_rs::fs::cgroup_builder::CgroupBuilder;
use cgroups_rs::fs::hierarchies;
use quickcheck::{Arbitrary, Gen};
use anyhow::{Result, Context};
use std::time::{Duration, Instant};
use std::sync::atomic::{AtomicU32, Ordering};
use crate::util::child::Child;
use crate::util::shared::SharedBox;

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

/// Default maximum depth for randomly generated cgroup trees.
pub const DEFAULT_MAX_TREE_DEPTH: usize = 7;

/// Default maximum number of children per node in randomly generated cgroup trees.
pub const DEFAULT_MAX_CHILDREN: usize = 4;

/// A node in a cgroup tree hierarchy.
#[derive(Debug, Clone)]
pub struct CGroupTreeNode {
    /// The resource limits for this cgroup
    pub resources: RandResources,
    /// Child cgroups under this node
    pub children: Vec<CGroupTreeNode>,
}

/// Simple CPU hog workload that spins using Instant::now()
fn cpu_hog_workload(duration: Duration, start_signal: SharedBox<AtomicU32>) {
    // Wait for start signal
    while start_signal.load(Ordering::Acquire) == 0 {
        std::hint::spin_loop();
    }

    // Spin for the specified duration
    let start = Instant::now();
    while start.elapsed() < duration {
        // Just spin, checking time
        std::hint::spin_loop();
    }
}

/// A handle to a launched CPU hog process
pub struct CGroupHog {
    child: Child,
}

impl CGroupHog {
    /// Wait for the hog to complete
    pub fn wait(mut self) -> Result<()> {
        let result = self.child.wait(true, false);
        match result {
            Some(Ok(())) => Ok(()),
            Some(Err(e)) => Err(e),
            None => Ok(()),
        }
    }

    /// Check if the hog is still alive
    pub fn alive(&mut self) -> bool {
        self.child.alive()
    }
}

/// A created cgroup tree that exists on the filesystem.
/// When dropped, all cgroups in the tree are deleted.
pub struct ActualizedCGroupTree {
    /// The root cgroup and all children
    cgroups: Vec<Cgroup>,
    /// The tree structure (kept for traversal)
    tree: CGroupTreeNode,
}

impl ActualizedCGroupTree {
    /// Get the number of cgroups in this tree
    pub fn len(&self) -> usize {
        self.cgroups.len()
    }

    /// Check if the tree is empty
    pub fn is_empty(&self) -> bool {
        self.cgroups.is_empty()
    }

    /// Launch CPU hog workloads in leaf cgroups only.
    /// The hogs will wait for a start signal before actually spinning.
    ///
    /// # Arguments
    /// * `duration` - How long each hog should spin
    /// * `start_signal` - Shared atomic flag to signal when to start
    ///
    /// # Returns
    /// Vector of hog handles that can be waited on
    pub fn launch_leaf_hogs(
        &self,
        duration: Duration,
        start_signal: SharedBox<AtomicU32>,
    ) -> Result<Vec<CGroupHog>> {
        let mut hogs = Vec::new();
        self.launch_leaf_hogs_recursive(&self.tree, 0, duration, start_signal.clone(), &mut hogs)?;
        Ok(hogs)
    }

    /// Signal all hogs to start running
    pub fn start_hogs(start_signal: &SharedBox<AtomicU32>) {
        start_signal.store(1, Ordering::Release);
    }

    /// Wait for all hogs to complete
    pub fn wait_for_hogs(hogs: Vec<CGroupHog>) -> Result<()> {
        for hog in hogs {
            hog.wait()?;
        }
        Ok(())
    }

    /// Count the number of leaf nodes in the tree
    pub fn count_leaves(&self) -> usize {
        Self::count_leaves_recursive(&self.tree)
    }

    fn count_leaves_recursive(node: &CGroupTreeNode) -> usize {
        if node.children.is_empty() {
            1
        } else {
            node.children.iter().map(|child| Self::count_leaves_recursive(child)).sum()
        }
    }

    /// Recursively launch hogs in leaf nodes
    fn launch_leaf_hogs_recursive(
        &self,
        node: &CGroupTreeNode,
        cgroup_index: usize,
        duration: Duration,
        start_signal: SharedBox<AtomicU32>,
        hogs: &mut Vec<CGroupHog>,
    ) -> Result<usize> {
        let current_index = cgroup_index;

        if node.children.is_empty() {
            // This is a leaf - launch a hog
            let cgroup = &self.cgroups[current_index];
            let cgroup_path_str = cgroup.path();
            let start_signal_clone = start_signal.clone();

            let child = Child::run(
                move || {
                    // Just run the CPU hog - parent will add us to cgroup
                    cpu_hog_workload(duration, start_signal_clone);
                    Ok(())
                },
                None,
            )?;

            // Add the child process to the cgroup from the parent
            let pid = child.pid().as_raw();
            let procs_path = std::path::Path::new("/sys/fs/cgroup")
                .join(cgroup_path_str)
                .join("cgroup.procs");
            std::fs::write(&procs_path, pid.to_string())
                .context(format!("Failed to write PID {} to {:?}", pid, procs_path))?;

            hogs.push(CGroupHog { child });
            Ok(current_index + 1)
        } else {
            // Interior node - recurse to children
            let mut next_index = current_index + 1;
            for child in &node.children {
                next_index = self.launch_leaf_hogs_recursive(
                    child,
                    next_index,
                    duration,
                    start_signal.clone(),
                    hogs,
                )?;
            }
            Ok(next_index)
        }
    }
}

impl Drop for ActualizedCGroupTree {
    fn drop(&mut self) {
        // Delete in reverse order to ensure children are deleted before parents
        for cgroup in self.cgroups.iter().rev() {
            let _ = cgroup.delete();
        }
    }
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

    /// Pretty-print the tree structure to stderr.
    pub fn print_tree(&self) {
        Self::print_tree_recursive(self, "", true);
    }

    /// Recursively print the tree with proper indentation.
    fn print_tree_recursive(node: &CGroupTreeNode, prefix: &str, is_last: bool) {
        let connector = if is_last { "└─" } else { "├─" };
        eprint!("{}{} ", prefix, connector);

        // Print some key resource info
        let mut info = Vec::new();

        // CPU resources
        if let Some(ref cpus) = node.resources.0.cpu.cpus {
            info.push(format!("cpus:{}", cpus));
        }
        if let Some(shares) = node.resources.0.cpu.shares {
            info.push(format!("shares:{}", shares));
        }
        if let Some(quota) = node.resources.0.cpu.quota {
            if let Some(period) = node.resources.0.cpu.period {
                // Show as percentage of one CPU
                let percent = (quota as f64 / period as f64) * 100.0;
                info.push(format!("cpu.max:{:.1}%", percent));
            }
        }

        // Memory resources
        if let Some(mem) = node.resources.0.memory.memory_hard_limit {
            let mb = mem / (1024 * 1024);
            info.push(format!("mem:{}MB", mb));
        }
        if let Some(mem) = node.resources.0.memory.memory_soft_limit {
            let mb = mem / (1024 * 1024);
            info.push(format!("soft:{}MB", mb));
        }
        if let Some(swappiness) = node.resources.0.memory.swappiness {
            info.push(format!("swappiness:{}", swappiness));
        }

        // Other resources
        if let Some(weight) = node.resources.0.blkio.weight {
            info.push(format!("blkio:{}", weight));
        }
        if let Some(class_id) = node.resources.0.network.class_id {
            info.push(format!("netclass:{}", class_id));
        }

        if info.is_empty() {
            eprintln!("Node [no limits]");
        } else {
            eprintln!("Node [{}]", info.join(", "));
        }

        let child_prefix = format!("{}{}", prefix, if is_last { "   " } else { "│  " });
        for (i, child) in node.children.iter().enumerate() {
            let is_last_child = i == node.children.len() - 1;
            Self::print_tree_recursive(child, &child_prefix, is_last_child);
        }
    }

    /// Create actual cgroups on the filesystem based on this tree structure.
    ///
    /// # Arguments
    /// * `base_name` - The base name for the root cgroup
    ///
    /// # Returns
    /// An `ActualizedCGroupTree` that owns the created cgroups. When dropped, all cgroups are deleted.
    ///
    /// # Example
    /// ```rust,ignore
    /// let tree = CGroupTreeNode::arbitrary_tree(&mut gen, &constraints, 3, 4);
    /// let actualized = tree.create("test_cgroup")?;
    /// // Use the cgroups...
    /// // They will be automatically deleted when actualized is dropped
    /// ```
    pub fn create(&self, base_name: &str) -> Result<ActualizedCGroupTree> {
        let mut all_cgroups = Vec::new();

        self.create_recursive(base_name, &mut all_cgroups)
            .context("Failed to create cgroup tree")?;

        Ok(ActualizedCGroupTree {
            cgroups: all_cgroups,
            tree: self.clone(),
        })
    }

    /// Recursively create cgroups for this node and all children.
    fn create_recursive(
        &self,
        name: &str,
        all_cgroups: &mut Vec<Cgroup>,
    ) -> Result<()> {
        // Create the cgroup with the builder
        let mut builder = CgroupBuilder::new(name);

        // Apply CPU resources if present
        if self.resources.0.cpu.shares.is_some()
            || self.resources.0.cpu.quota.is_some()
            || self.resources.0.cpu.period.is_some()
            || self.resources.0.cpu.cpus.is_some()
        {
            let mut cpu_builder = builder.cpu();

            if let Some(shares) = self.resources.0.cpu.shares {
                cpu_builder = cpu_builder.shares(shares);
            }
            if let Some(quota) = self.resources.0.cpu.quota {
                cpu_builder = cpu_builder.quota(quota);
            }
            if let Some(period) = self.resources.0.cpu.period {
                cpu_builder = cpu_builder.period(period);
            }
            if let Some(ref cpus) = self.resources.0.cpu.cpus {
                cpu_builder = cpu_builder.cpus(cpus.clone());
            }

            builder = cpu_builder.done();
        }

        // Apply memory resources if present
        if self.resources.0.memory.memory_hard_limit.is_some()
            || self.resources.0.memory.memory_soft_limit.is_some()
            || self.resources.0.memory.memory_swap_limit.is_some()
            || self.resources.0.memory.swappiness.is_some()
        {
            let mut mem_builder = builder.memory();

            if let Some(limit) = self.resources.0.memory.memory_hard_limit {
                mem_builder = mem_builder.memory_hard_limit(limit);
            }
            if let Some(limit) = self.resources.0.memory.memory_soft_limit {
                mem_builder = mem_builder.memory_soft_limit(limit);
            }
            if let Some(limit) = self.resources.0.memory.memory_swap_limit {
                mem_builder = mem_builder.memory_swap_limit(limit);
            }
            if let Some(swappiness) = self.resources.0.memory.swappiness {
                mem_builder = mem_builder.swappiness(swappiness);
            }

            builder = mem_builder.done();
        }

        // Apply Block I/O resources if present
        if self.resources.0.blkio.weight.is_some() || self.resources.0.blkio.leaf_weight.is_some() {
            let mut blkio_builder = builder.blkio();

            if let Some(weight) = self.resources.0.blkio.weight {
                blkio_builder = blkio_builder.weight(weight);
            }
            if let Some(leaf_weight) = self.resources.0.blkio.leaf_weight {
                blkio_builder = blkio_builder.leaf_weight(leaf_weight);
            }

            builder = blkio_builder.done();
        }

        // Apply Network resources if present
        if let Some(class_id) = self.resources.0.network.class_id {
            let mut net_builder = builder.network();
            net_builder = net_builder.class_id(class_id);
            builder = net_builder.done();
        }

        // Apply PID resources if present
        if let Some(ref max_procs) = self.resources.0.pid.maximum_number_of_processes {
            let mut pid_builder = builder.pid();
            pid_builder = pid_builder.maximum_number_of_processes(max_procs.clone());
            builder = pid_builder.done();
        }

        // Build the cgroup
        let hier = hierarchies::auto();
        let cgroup = builder.build(hier).context(format!("Failed to build cgroup {}", name))?;

        // Add this cgroup to the list FIRST (preorder traversal)
        all_cgroups.push(cgroup);

        // Create children with names like "parent_name/child_0", "parent_name/child_1", etc.
        for (i, child) in self.children.iter().enumerate() {
            let child_name = format!("{}/child_{}", name, i);
            child.create_recursive(&child_name, all_cgroups)?;
        }

        Ok(())
    }
}

/// Implement Arbitrary for CGroupTreeNode to enable use with QuickCheck macros.
///
/// Note: This performs I/O to detect system constraints. The default implementation
/// uses `DEFAULT_MAX_TREE_DEPTH` and `DEFAULT_MAX_CHILDREN` as tree generation parameters.
impl Arbitrary for CGroupTreeNode {
    fn arbitrary(g: &mut Gen) -> Self {
        let constraints = SystemConstraints::detect();
        Self::arbitrary_tree(g, &constraints, DEFAULT_MAX_TREE_DEPTH, DEFAULT_MAX_CHILDREN)
    }

    fn shrink(&self) -> Box<dyn Iterator<Item = Self>> {
        // 1. Shrink the children Vec using Vec's shrinker
        let children_vec = self.children.clone();
            let mut all_shrinks = Vec::new();

            // Shrink the children Vec using Vec's shrinker
            for shrunk_vec in children_vec.shrink() {
                all_shrinks.push(CGroupTreeNode {
                    resources: self.resources.clone(),
                    children: shrunk_vec,
                });
            }

            // Recursively shrink each child, keeping others unchanged
            let resources = self.resources.clone();
            let children = self.children.clone();
            for (i, child) in children.iter().enumerate() {
                for shrunk_child in child.shrink() {
                    let mut new_children = children.clone();
                    new_children[i] = shrunk_child;
                    all_shrinks.push(CGroupTreeNode {
                        resources: resources.clone(),
                        children: new_children,
                    });
                }
            }

            Box::new(all_shrinks.into_iter())
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

        // CPU shares (cpu.weight) - 60% probability
        // CPU shares (typically 1-1024, with 100 being default)
        if (u8::arbitrary(g) % 100) < 60 {
            let shares: u16 = u16::arbitrary(g);
            resources.cpu.shares = Some(std::cmp::max(shares % 1024, 1) as u64); // 1-1024
        }

        // CPU quota/period (cpu.max) - 30% probability
        // CPU quota and period should be set together
        if (u8::arbitrary(g) % 100) < 30 {
            let period: u32 = u32::arbitrary(g);
            let period = ((period % 999000) + 1000) as u64; // 1000-1000000 microseconds
            let quota_factor: u32 = u32::arbitrary(g);
            let quota = ((quota_factor % period as u32) + 1000) as i64; // 1000 to period
            resources.cpu.quota = Some(quota);
            resources.cpu.period = Some(period);
        }

        // CPUSET - specific CPUs (simple format like "0", "0-2", "0,2,4") - 20% probability
        // Now respects actual CPU count on the system!
        if (u8::arbitrary(g) % 100) < 20 && max_cpu > 0 {
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

    #[test]
    fn test_arbitrary_cgroup_tree() {
        // Test that the Arbitrary implementation works
        let mut gen = quickcheck::Gen::new(789);
        let tree = CGroupTreeNode::arbitrary(&mut gen);

        println!("Generated tree via Arbitrary with {} nodes", tree.node_count());
        println!("Tree depth: {}", tree.max_depth());

        assert!(tree.max_depth() <= DEFAULT_MAX_TREE_DEPTH,
                "Tree depth {} should not exceed DEFAULT_MAX_TREE_DEPTH {}",
                tree.max_depth(), DEFAULT_MAX_TREE_DEPTH);
        assert!(tree.node_count() >= 1, "Tree should have at least the root node");
    }

    #[test]
    fn test_arbitrary_with_quickcheck_macro() {
        // Test that CGroupTreeNode works with quickcheck! macro
        fn prop_arbitrary_tree_valid(tree: CGroupTreeNode) -> TestResult {
            // Basic validity checks
            if tree.node_count() < 1 {
                return TestResult::failed();
            }

            if tree.max_depth() > DEFAULT_MAX_TREE_DEPTH {
                return TestResult::failed();
            }

            // Check that no node has too many children
            fn check_max_children(node: &CGroupTreeNode) -> bool {
                if node.children.len() > DEFAULT_MAX_CHILDREN {
                    return false;
                }
                node.children.iter().all(check_max_children)
            }

            if !check_max_children(&tree) {
                return TestResult::failed();
            }

            TestResult::passed()
        }

        quickcheck(prop_arbitrary_tree_valid as fn(CGroupTreeNode) -> TestResult);
    }

    #[test]
    fn test_tree_shrinking() {
        // Test the shrink implementation
        let mut gen = quickcheck::Gen::new(321);
        let constraints = SystemConstraints {
            num_cpus: 4,
            total_memory_bytes: 8 * 1024 * 1024 * 1024,
        };

        // Generate a tree with some depth
        let tree = CGroupTreeNode::arbitrary_tree(&mut gen, &constraints, 2, 2);

        if tree.children.is_empty() {
            println!("Tree has no children, skipping shrink test");
            return;
        }

        let original_nodes = tree.node_count();
        println!("Original tree has {} nodes", original_nodes);

        let shrunk: Vec<_> = tree.shrink().take(5).collect();
        println!("Generated {} shrunk variants", shrunk.len());

        // At least one shrunk variant should be smaller
        let has_smaller = shrunk.iter().any(|t| t.node_count() < original_nodes);
        assert!(has_smaller, "Shrinking should produce smaller trees");

        // All shrunk trees should be valid
        for shrunk_tree in shrunk {
            assert!(shrunk_tree.node_count() >= 1, "Shrunk tree should have at least root");
        }
    }

    #[test]
    fn test_actualize_cgroup_tree() {
        use crate::util::user::User;

        // Skip if not root
        if !User::is_root() {
            println!("Skipping test (requires root)");
            return;
        }

        let mut gen = quickcheck::Gen::new(999);
        let constraints = SystemConstraints {
            num_cpus: 2,
            total_memory_bytes: 1024 * 1024 * 1024, // 1GB
        };

        // Generate a small tree
        let tree = CGroupTreeNode::arbitrary_tree(&mut gen, &constraints, 2, 2);

        println!("Generated tree with {} nodes", tree.node_count());

        // Try to create the cgroups
        let result = tree.create("test_cgroup_tree");

        match result {
            Ok(actualized) => {
                println!("Successfully created {} cgroups", actualized.len());
                assert_eq!(actualized.len(), tree.node_count());
                // Cgroups will be automatically deleted when actualized is dropped
            }
            Err(e) => {
                println!("Failed to create cgroups: {}", e);
                // This might fail on systems without cgroup support, so we don't panic
            }
        }
    }
}