# Design Options for Running Workloads in Cgroup Trees

## Context

We have randomly generated hierarchical cgroup trees with resource limits. We need to run workloads inside these cgroups to test scheduler behavior and verify that resource limits are respected.

## Key Design Questions

1. **Where to place workloads?** (Leaves only vs. all nodes)
2. **What type of workloads?** (CPU, memory, I/O, mixed)
3. **How to manage processes?** (Fork model, integration with existing infrastructure)
4. **How to measure?** (Resource usage, limit enforcement)

---

## Option 1: Leaf-Only Workloads (Simplest)

**Concept**: Only run workloads at leaf nodes. Interior nodes naturally aggregate the resource usage of their children.

### Implementation

```rust
enum WorkloadType {
    CpuHog,      // Spin CPU at 100%
    MemoryHog,   // Allocate and touch memory
    Mixed,       // Both CPU and memory
}

struct WorkloadConfig {
    duration: Duration,
    workload_type: WorkloadType,
}

impl ActualizedCGroupTree {
    fn run_leaf_workloads(&self, config: WorkloadConfig) -> Result<()> {
        // For each leaf cgroup:
        // 1. Fork a process
        // 2. Add process PID to cgroup.procs
        // 3. Run workload (CPU spin, memory allocation, etc.)
        // 4. Wait for duration
        // 5. Collect stats
    }
}
```

### Pros
- ✅ Simple to implement
- ✅ Mirrors real-world usage (containers run at leaves)
- ✅ No process coordination complexity
- ✅ Clear resource attribution

### Cons
- ❌ Doesn't directly test interior node limits
- ❌ May not stress complex hierarchies

### Example Use Case
Testing if leaf containers respect their individual CPU/memory limits.

---

## Option 2: All-Nodes Workloads (Comprehensive)

**Concept**: Run workloads at every node (leaves + interior nodes).

### Implementation

```rust
impl ActualizedCGroupTree {
    fn run_all_nodes_workloads(&self, config: WorkloadConfig) -> Result<()> {
        // For each cgroup (DFS or BFS):
        // 1. Fork a process for this node
        // 2. Add to cgroup.procs
        // 3. Run workload
        // 4. Recursively handle children (if any)
    }
}
```

### Pros
- ✅ Tests all cgroup limits directly
- ✅ More comprehensive stress testing
- ✅ Tests limit inheritance and aggregation

### Cons
- ❌ More complex - many processes competing
- ❌ Less realistic (doesn't mirror typical usage)
- ❌ Harder to attribute resource usage
- ❌ Interior node + children processes may interfere

### Example Use Case
Testing complex limit interactions and ensuring parent limits constrain children.

---

## Option 3: Configurable Placement Strategy

**Concept**: Allow flexible configuration of where workloads run.

### Implementation

```rust
enum PlacementStrategy {
    LeavesOnly,
    AllNodes,
    Random(f64),           // Probability of placing workload at each node
    Depth(usize),          // Only at specific depth
    Custom(Box<dyn Fn(&CGroupTreeNode, usize) -> bool>), // User-defined
}

struct CGroupWorkloadRunner {
    tree: ActualizedCGroupTree,
    placement: PlacementStrategy,
    workload_configs: Vec<WorkloadConfig>,
}

impl CGroupWorkloadRunner {
    fn run(&mut self) -> Result<WorkloadStats> {
        // Traverse tree, apply placement strategy
        // Fork processes where strategy returns true
        // Run workloads, collect stats
    }
}
```

### Pros
- ✅ Maximum flexibility
- ✅ Can test different scenarios
- ✅ Can randomize for property-based testing

### Cons
- ❌ More complex API
- ❌ Harder to reason about

---

## Option 4: Workload Types Per Node (Fine-Grained)

**Concept**: Different workload types at different nodes based on resource limits.

### Implementation

```rust
impl CGroupTreeNode {
    fn suggested_workload(&self) -> WorkloadType {
        // If node has CPU limits -> CpuHog
        // If node has memory limits -> MemoryHog
        // If both -> Mixed
        // Otherwise -> Idle
        match (&self.resources.0.cpu.shares, &self.resources.0.memory.memory_hard_limit) {
            (Some(_), Some(_)) => WorkloadType::Mixed,
            (Some(_), None) => WorkloadType::CpuHog,
            (None, Some(_)) => WorkloadType::MemoryHog,
            _ => WorkloadType::Idle,
        }
    }
}
```

### Pros
- ✅ Intelligent workload selection
- ✅ Tests specific limits being set
- ✅ More realistic scenarios

### Cons
- ❌ May not test cross-resource interactions

---

## Workload Types Design

### CPU Hog
```rust
fn cpu_hog_workload(duration: Duration) {
    let spinner = Spinner::default();
    spinner.spin(duration);
}
```

### Memory Hog
```rust
fn memory_hog_workload(size_mb: usize, duration: Duration) {
    let mut allocations = Vec::new();
    let size_bytes = size_mb * 1024 * 1024;

    // Allocate memory in chunks
    let chunk_size = 1024 * 1024; // 1MB chunks
    for _ in 0..(size_bytes / chunk_size) {
        let mut chunk = vec![0u8; chunk_size];
        // Touch the memory to ensure it's actually allocated
        for byte in chunk.iter_mut() {
            *byte = rand::random();
        }
        allocations.push(chunk);
    }

    // Hold for duration
    std::thread::sleep(duration);
}
```

### I/O Hog
```rust
fn io_hog_workload(size_mb: usize, duration: Duration) {
    use std::fs::File;
    use std::io::Write;

    let start = Instant::now();
    while start.elapsed() < duration {
        let mut file = File::create("/tmp/io_test")?;
        let data = vec![0u8; size_mb * 1024 * 1024];
        file.write_all(&data)?;
        file.sync_all()?;
    }
}
```

### Mixed Workload
```rust
fn mixed_workload(cpu_percent: f64, mem_mb: usize, duration: Duration) {
    std::thread::scope(|s| {
        // CPU thread
        s.spawn(|| cpu_hog_workload(duration));

        // Memory thread
        s.spawn(|| memory_hog_workload(mem_mb, duration));
    });
}
```

---

## Integration with Existing Infrastructure

### Option A: Extend Process to Accept Existing Cgroup

```rust
impl Process {
    // New method that uses an existing cgroup instead of creating one
    pub fn create_in_cgroup<F>(
        ctx: &Context,
        cgroup: &Cgroup,  // Pre-created cgroup from our tree
        func: F,
        spec: Option<Spec>,
    ) -> Result<Self> {
        // Same as create() but skip Cgroup::create()
        // Use provided cgroup instead
    }
}
```

**Pros**: Integrates with existing Context/Process infrastructure
**Cons**: Need to modify Process API

### Option B: Direct Fork + cgroup.procs Write

```rust
impl ActualizedCGroupTree {
    fn spawn_workload_in_cgroup(
        &self,
        cgroup: &Cgroup,
        workload: impl Fn() + Send + 'static,
    ) -> Result<Child> {
        let child = Child::run(move || {
            workload();
            Ok(())
        }, None)?;

        // Write PID to cgroup.procs
        let procs_path = cgroup.path().join("cgroup.procs");
        std::fs::write(procs_path, child.pid().to_string())?;

        Ok(child)
    }
}
```

**Pros**: Simple, direct control
**Cons**: Bypasses existing infrastructure

### Option C: Hybrid - Use cgroups_rs Controllers

```rust
use cgroups_rs::CgroupPid;

impl ActualizedCGroupTree {
    fn add_process_to_cgroup(&self, cgroup: &Cgroup, pid: u64) -> Result<()> {
        // Use cgroups_rs API
        let cgroupv2 = cgroup.v2();
        cgroupv2.add_task(CgroupPid::from(pid))?;
        Ok(())
    }
}
```

**Pros**: Uses library API, handles v1/v2 differences
**Cons**: Need to verify cgroups_rs API

---

## Measurement & Verification

### What to Measure

1. **CPU Usage per Cgroup**
   - Read `cpu.stat` from each cgroup
   - Compare to limits (shares, quota)

2. **Memory Usage per Cgroup**
   - Read `memory.current`, `memory.peak`
   - Compare to `memory.max` limits

3. **Process Stats**
   - Per-process CPU time
   - Per-process memory usage
   - Scheduler statistics

### Verification Strategy

```rust
struct WorkloadStats {
    cgroup_path: PathBuf,
    cpu_usage_usec: u64,
    memory_usage_bytes: u64,
    cpu_limit_exceeded: bool,
    memory_limit_exceeded: bool,
}

impl ActualizedCGroupTree {
    fn collect_stats(&self) -> Result<Vec<WorkloadStats>> {
        // For each cgroup:
        // 1. Read cpu.stat
        // 2. Read memory.current
        // 3. Compare to configured limits
        // 4. Return statistics
    }

    fn verify_limits_respected(&self, stats: &[WorkloadStats]) -> Result<()> {
        for stat in stats {
            if stat.cpu_limit_exceeded {
                return Err(anyhow!("CPU limit exceeded for {:?}", stat.cgroup_path));
            }
            if stat.memory_limit_exceeded {
                return Err(anyhow!("Memory limit exceeded for {:?}", stat.cgroup_path));
            }
        }
        Ok(())
    }
}
```

---

## Recommended Approach

**Start with Option 1 (Leaf-Only) + Option A (Extend Process)**

### Phase 1: Simple Leaf Workloads
1. Extend `Process::create` to accept an optional existing `Cgroup`
2. Implement leaf-only CPU hog workloads
3. Verify CPU limits are respected
4. Collect and display statistics

### Phase 2: Add Workload Variety
1. Implement memory hog workloads
2. Implement mixed workloads
3. Auto-select workload based on cgroup limits

### Phase 3: Expand to All Nodes
1. Add `PlacementStrategy` enum
2. Support running at all nodes or custom strategies
3. Handle process coordination

### Phase 4: Advanced Testing
1. Randomized workload placement for property testing
2. Time-series measurement
3. Scheduler fairness analysis

---

## Example Usage

```rust
// Generate tree
let tree = CGroupTreeNode::arbitrary_tree(&mut gen, &constraints, 3, 3);
let actualized = tree.create("test_tree")?;

// Run leaf workloads
let config = WorkloadConfig {
    duration: Duration::from_secs(5),
    workload_type: WorkloadType::CpuHog,
};

actualized.run_leaf_workloads(config)?;

// Collect stats
let stats = actualized.collect_stats()?;
actualized.verify_limits_respected(&stats)?;

// Print results
for stat in stats {
    println!("Cgroup: {:?}", stat.cgroup_path);
    println!("  CPU: {} usec", stat.cpu_usage_usec);
    println!("  Memory: {} bytes", stat.memory_usage_bytes);
}
```

---

## Open Questions

1. Should workloads run for a fixed duration or until signaled?
2. How to handle OOM kills gracefully?
3. Should we test deliberate limit violations?
4. How to coordinate workload start/stop across all cgroups?
5. Should we measure scheduler latency/fairness in addition to resource usage?
