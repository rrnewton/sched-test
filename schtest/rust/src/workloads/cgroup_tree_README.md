# CGroup Tree Module

This module provides tools for generating and working with cgroup (control group) hierarchies, with support for random generation via QuickCheck.

## Overview

The module solves a key challenge: **how to generate random resource limits that respect actual system constraints**.

When generating random cgroup configurations for testing, we need to ensure:
- CPUSET pinning doesn't reference CPUs that don't exist on the machine
- Memory limits don't exceed available system memory
- All resource limits are within valid ranges

## Core Types

### `SystemConstraints`

Encapsulates machine-specific resource limits:

```rust
pub struct SystemConstraints {
    pub num_cpus: usize,           // Number of CPUs available
    pub total_memory_bytes: u64,   // Total system memory
}
```

**Auto-detection:**
```rust
let constraints = SystemConstraints::detect();  // Reads from /proc/meminfo and num_cpus crate
```

**Manual construction (for testing):**
```rust
let constraints = SystemConstraints {
    num_cpus: 4,
    total_memory_bytes: 8 * 1024 * 1024 * 1024,  // 8GB
};
```

### `RandResources`

Wrapper around `cgroups_rs::Resources` that provides parameterized random generation:

```rust
pub struct RandResources(pub Resources);
```

**Generates random values for:**
- CPU shares (1-1024)
- CPU quotas and periods
- CPUSET pinning (respects actual CPU count)
- Memory hard/soft limits (respects available memory)
- Swap limits
- Swappiness (0-100)
- PID limits
- Block I/O weights
- Network class IDs

**Usage:**
```rust
// Method 1: Explicit constraints (recommended)
let resources = RandResources::arbitrary_with_constraints(&mut gen, &constraints);

// Method 2: Auto-detect constraints
use quickcheck::Arbitrary;
let resources = RandResources::arbitrary(&mut gen);
```

### `CGroupTreeNode`

Represents a node in a cgroup hierarchy:

```rust
pub struct CGroupTreeNode {
    pub resources: RandResources,      // Resource limits for this cgroup
    pub children: Vec<CGroupTreeNode>, // Child cgroups
}
```

**Generate random trees:**
```rust
let tree = CGroupTreeNode::arbitrary_tree(
    &mut gen,
    &constraints,
    max_depth: 3,      // Maximum tree depth
    max_children: 4,   // Maximum children per node
);
```

**Utility methods:**
```rust
let total_nodes = tree.node_count();  // Count all nodes in tree
let depth = tree.max_depth();         // Get maximum depth
```

## Examples

### Example 1: Generate Single Resource Limit

```rust
use quickcheck::Gen;

fn generate_resource_limit() {
    let mut gen = Gen::new(100);

    // Detect system constraints
    let constraints = SystemConstraints::detect();

    // Generate random resources that respect constraints
    let resources = RandResources::arbitrary_with_constraints(&mut gen, &constraints);

    // Use resources.0 to access the underlying cgroups_rs::Resources
    if let Some(ref cpus) = resources.0.cpu.cpus {
        println!("CPUSET: {}", cpus);
    }
}
```

### Example 2: Generate CGroup Hierarchy

```rust
fn generate_cgroup_tree() {
    let mut gen = Gen::new(42);
    let constraints = SystemConstraints {
        num_cpus: 8,
        total_memory_bytes: 16 * 1024 * 1024 * 1024,
    };

    // Generate a tree up to 3 levels deep, max 4 children per node
    let tree = CGroupTreeNode::arbitrary_tree(&mut gen, &constraints, 3, 4);

    println!("Generated {} nodes", tree.node_count());
    println!("Tree depth: {}", tree.max_depth());
}
```

### Example 3: Property-Based Testing

```rust
#[test]
fn test_resources_respect_constraints() {
    use quickcheck::{quickcheck, TestResult};

    fn prop_valid_cpuset(seed: u64) -> TestResult {
        let constraints = SystemConstraints {
            num_cpus: 4,
            total_memory_bytes: 8 * 1024 * 1024 * 1024,
        };

        let mut gen = Gen::new(seed as usize % 100);
        let resources = RandResources::arbitrary_with_constraints(&mut gen, &constraints);

        // Validate CPUSET doesn't reference CPU >= 4
        if let Some(ref cpus) = resources.0.cpu.cpus {
            for part in cpus.split(',') {
                if let Some((start, end)) = part.split_once('-') {
                    let end_cpu: usize = end.parse().unwrap_or(0);
                    if end_cpu >= constraints.num_cpus {
                        return TestResult::failed();
                    }
                } else if let Ok(cpu) = part.parse::<usize>() {
                    if cpu >= constraints.num_cpus {
                        return TestResult::failed();
                    }
                }
            }
        }

        TestResult::passed()
    }

    quickcheck(prop_valid_cpuset as fn(u64) -> TestResult);
}
```

## The Parameterization Pattern

QuickCheck's `Arbitrary` trait doesn't support parameters:

```rust
trait Arbitrary {
    fn arbitrary(g: &mut Gen) -> Self;
}
```

We work around this by providing both:
1. A parameterized method `arbitrary_with_constraints()`
2. A default `Arbitrary` implementation that auto-detects constraints

**Implementation pattern:**
```rust
impl RandResources {
    // Public parameterized API
    pub fn arbitrary_with_constraints(g: &mut Gen, constraints: &SystemConstraints) -> Self {
        Self::arbitrary_internal(g, Some(constraints))
    }

    // Internal implementation with optional constraints
    fn arbitrary_internal(g: &mut Gen, constraints: Option<&SystemConstraints>) -> Self {
        // Only detect constraints when not provided (lazy detection)
        let detected_constraints;
        let constraints = match constraints {
            Some(c) => c,
            None => {
                detected_constraints = SystemConstraints::detect();
                &detected_constraints
            }
        };
        // Generate values bounded by constraints...
    }
}

// Standard Arbitrary for compatibility with quickcheck macros
// Note: This performs I/O to detect system constraints - use sparingly!
impl Arbitrary for RandResources {
    fn arbitrary(g: &mut Gen) -> Self {
        Self::arbitrary_internal(g, None)
    }
}
```

**Design note:** We intentionally avoid implementing `Default` for `SystemConstraints` because `Default::default()` should be pure and cheap, but constraint detection performs I/O (reads `/proc/meminfo`). Instead, call `SystemConstraints::detect()` explicitly when needed.

This gives us:
- ✅ Explicit parameterization when needed
- ✅ Automatic constraint detection by default
- ✅ Compatibility with QuickCheck ecosystem
- ✅ Type-safe constraint passing

## Example Output

On a 64-core, 125GB machine:

```
Detected 64 CPUs
Detected 134225903616 bytes of memory (125 GB)

Generated tree with 24 nodes
Tree depth: 3

Tree structure:
└─ Node [cpus:2-6, shares:616, mem:374MB]
   ├─ Node [shares:1, mem:16383MB]
   │  └─ Node [cpus:0-3, mem:8192MB]
   └─ Node [cpus:10-12, shares:450]
      ├─ Node [mem:4096MB, swappiness:50]
      └─ Node [cpus:5]
```

## Testing

Run the test suite:

```bash
cargo test cgroup_tree
```

Run specific test with output:

```bash
cargo test test_cgroup_tree_generation -- --nocapture
```

## Future Extensions

Possible enhancements:

1. **More constraint types:**
   - NUMA node information
   - Specific block device limits
   - Network interface limits

2. **Tree validation:**
   - Ensure parent resources >= sum of children
   - Validate resource hierarchies make sense

3. **Tree mutations:**
   - Add/remove nodes
   - Modify resources while maintaining invariants

4. **Serialization:**
   - Save/load tree configurations
   - Export to cgroup filesystem commands

5. **Integration:**
   - Actually create cgroups from tree
   - Measure scheduler behavior with different trees

## See Also

- `docs/parameterized_arbitrary.md` - Detailed explanation of the parameterization approach
- `docs/IMPLEMENTATION_SUMMARY.md` - Implementation summary and results
- [cgroups-rs documentation](https://docs.rs/cgroups-rs/) - The underlying cgroup library
- [QuickCheck documentation](https://docs.rs/quickcheck/) - Property-based testing framework
