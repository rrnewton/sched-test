# Parameterizing QuickCheck's Arbitrary Trait

## The Problem

The `Arbitrary` trait from QuickCheck has this signature:

```rust
trait Arbitrary {
    fn arbitrary(g: &mut Gen) -> Self;
}
```

It doesn't accept additional parameters, which makes it difficult to generate values that respect system constraints like:
- Number of CPUs available on the machine
- Total system memory
- Other hardware-specific limits

When generating random cgroup resource limits, we need to ensure CPUSETs don't reference non-existent CPUs and memory limits don't exceed system capacity.

## The Solution

We implemented a multi-layered approach that provides flexibility while maintaining compatibility with QuickCheck's ecosystem:

### 1. System Constraints Type

```rust
pub struct SystemConstraints {
    pub num_cpus: usize,
    pub total_memory_bytes: u64,
}
```

This encapsulates machine-specific limits and can:
- Auto-detect from the current system via `SystemConstraints::detect()`
- Be manually constructed for testing different machine configurations

### 2. Parameterized Generator Method

```rust
impl RandResources {
    pub fn arbitrary_with_constraints(g: &mut Gen, constraints: &SystemConstraints) -> Self {
        // Generate resources respecting the constraints
    }
}
```

This is the **recommended approach** when you need explicit control over constraints.

### 3. Default Arbitrary Implementation

```rust
impl Arbitrary for RandResources {
    fn arbitrary(g: &mut Gen) -> Self {
        // Uses SystemConstraints::detect() automatically
        Self::arbitrary_internal(g, None)
    }
}
```

This maintains compatibility with QuickCheck macros and derives while automatically detecting system limits.

## Usage Patterns

### Pattern 1: Explicit Constraints (Recommended)

```rust
use quickcheck::Gen;

let mut gen = Gen::new(100);
let constraints = SystemConstraints {
    num_cpus: 4,
    total_memory_bytes: 8 * 1024 * 1024 * 1024, // 8GB
};
let resources = RandResources::arbitrary_with_constraints(&mut gen, &constraints);
```

### Pattern 2: Auto-detect System Constraints

```rust
let mut gen = Gen::new(100);
let constraints = SystemConstraints::detect();
let resources = RandResources::arbitrary_with_constraints(&mut gen, &constraints);
```

### Pattern 3: Use Default Arbitrary

```rust
use quickcheck::Arbitrary;

let mut gen = Gen::new(100);
let resources = RandResources::arbitrary(&mut gen); // Auto-detects
```

### Pattern 4: With QuickCheck Property Testing

```rust
#[test]
fn test_property() {
    fn prop_validates_constraints(seed: u64) -> TestResult {
        let constraints = SystemConstraints {
            num_cpus: 4,
            total_memory_bytes: 8 * 1024 * 1024 * 1024,
        };
        
        let mut gen = quickcheck::Gen::new(seed as usize % 100);
        let resources = RandResources::arbitrary_with_constraints(&mut gen, &constraints);
        
        // Validate that resources respect constraints
        // ...
        
        TestResult::passed()
    }
    
    quickcheck(prop_validates_constraints as fn(u64) -> TestResult);
}
```

## Implementation Details

The key is using an internal method that takes optional constraints:

```rust
impl RandResources {
    fn arbitrary_internal(g: &mut Gen, constraints: Option<&SystemConstraints>) -> Self {
        let default_constraints = SystemConstraints::default();
        let constraints = constraints.unwrap_or(&default_constraints);
        
        // Use constraints.num_cpus and constraints.total_memory_bytes
        // when generating random values
        // ...
    }
}
```

This allows:
- `arbitrary_with_constraints()` to pass `Some(constraints)`
- Default `Arbitrary::arbitrary()` to pass `None` (which auto-detects)

## Benefits of This Approach

1. **Type Safety**: Constraints are explicitly typed and documented
2. **Flexibility**: Supports both auto-detection and manual specification
3. **Compatibility**: Works with QuickCheck's standard API
4. **Testability**: Easy to test with different machine configurations
5. **Correctness**: Prevents generating invalid configurations (e.g., CPU 100 on a 4-core machine)

## Alternative Approaches (Not Used)

### 1. Thread-Local Storage
Could store constraints in thread-local storage, but this is:
- Non-obvious and hard to understand
- Error-prone (need to remember to set it)
- Not composable with nested generators

### 2. Custom Gen Wrapper
Could wrap `Gen` with constraints, but:
- Breaks compatibility with QuickCheck's API
- Can't use standard `quickcheck!` macro
- More complex to maintain

### 3. Build-time Configuration
Could use compile-time constants, but:
- Not flexible for testing different configurations
- Requires recompilation for different machines
- Can't auto-detect at runtime

## Example Output

Here's what the system detects on a 64-core, 125GB machine:

```
Detected 64 CPUs
Detected 134225903616 bytes of memory (125 GB)
```

And an example generated cpuset respecting constraints:
```
cpuset: "0-3"    # Valid for 4-CPU constraint
cpuset: "60-63"  # Valid for 64-CPU machine
```

## Next Steps

To generate random cgroup **trees** (hierarchies), we can:

1. Create a `CGroupTreeNode` structure:
   ```rust
   pub struct CGroupTreeNode {
       pub resources: RandResources,
       pub children: Vec<CGroupTreeNode>,
   }
   ```

2. Implement a parameterized generator:
   ```rust
   impl CGroupTreeNode {
       pub fn arbitrary_tree(
           g: &mut Gen,
           constraints: &SystemConstraints,
           max_depth: usize,
           max_children: usize,
       ) -> Self {
           // Recursively generate tree structure
       }
   }
   ```

3. Use QuickCheck to generate various tree shapes and verify invariants
