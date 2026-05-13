//! Cgroup modeling for the simulator.
//!
//! This module provides a cgroup registry that tracks a hierarchy of cgroups,
//! each with an ID, level, parent, and optional cpuset. It delegates all C
//! struct allocation/deallocation to the RAII wrappers in
//! [`crate::cgroup_wrapper`], keeping this module free of `unsafe` code.

use std::collections::HashMap;
use std::ffi::c_void;

use crate::cgroup_wrapper::{
    free_cgroup_raw, CgroupAlloc, CgroupPtr, CssIterGuard, SimCgroupHandle,
};
use crate::types::CpuId;

/// Unique cgroup identifier (kernel's cgroup->kn->id).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CgroupId(pub u64);

impl CgroupId {
    /// The root cgroup's ID (always 1).
    pub const ROOT: CgroupId = CgroupId(1);
}

/// Information about a cgroup in the hierarchy.
///
/// Owns the underlying C `struct cgroup` allocation via [`CgroupAlloc`].
/// The root cgroup uses `CgroupAlloc::Root` (non-owning, never freed);
/// all other cgroups use `CgroupAlloc::Owned` (RAII, freed on drop).
#[derive(Debug)]
pub struct CgroupInfo {
    /// Unique cgroup ID.
    pub cgid: CgroupId,
    /// Depth in the hierarchy (root = 0).
    pub level: u32,
    /// Parent cgroup's ID (0 for root which has no parent).
    pub parent_cgid: CgroupId,
    /// Optional cpuset.cpus configuration (None = all CPUs allowed).
    pub cpuset: Option<Vec<CpuId>>,
    /// Name of the cgroup (for debugging/scenario API).
    pub name: String,
    /// RAII handle to the C `struct cgroup` allocation.
    alloc: CgroupAlloc,
}

impl CgroupInfo {
    /// Get the raw C `struct cgroup` pointer for FFI calls.
    pub fn raw(&self) -> *mut c_void {
        self.alloc.as_raw()
    }
}

/// Default maximum cgroup limit (high value for normal tests).
pub const DEFAULT_MAX_CGROUPS: u32 = 10000;

/// Registry managing the cgroup hierarchy.
///
/// The registry always contains the root cgroup (cgid=1, level=0).
/// Additional cgroups can be created as children of existing cgroups.
///
/// The registry also tracks BPF map-style resource limits. In production,
/// LAVD uses BPF hash maps with size limits (CBW_NR_CGRP_MAX = 2048,
/// CBW_NR_CGRP_LLC_MAX = 65536). When these maps fill up, cgroup init
/// fails with ENOMEM. The registry simulates this with configurable limits.
///
/// ## RAII ownership
///
/// Each `CgroupInfo` owns its C allocation via [`CgroupAlloc`]. When a
/// `CgroupInfo` is removed from the registry (via `destroy_by_name`) or
/// the registry is dropped, the C struct is freed automatically. The root
/// cgroup uses `CgroupAlloc::Root` which does not free on drop.
pub struct CgroupRegistry {
    /// Map from cgroup ID to cgroup info.
    cgroups: HashMap<CgroupId, CgroupInfo>,
    /// Map from cgroup name to cgroup ID (for scenario API lookups).
    name_to_id: HashMap<String, CgroupId>,
    /// Next available cgroup ID for auto-assignment.
    next_cgid: u64,
    /// Number of CPUs in the system (for cpuset validation in Phase 5).
    #[allow(dead_code)]
    nr_cpus: u32,
    /// Maximum number of cgroups that can have BPF map entries allocated.
    /// This simulates BPF map capacity limits (e.g., CBW_NR_CGRP_MAX = 2048).
    max_cgroups: u32,
    /// Number of cgroups with BPF map entries currently allocated.
    /// Incremented by `try_allocate_bpf_entry()`, decremented by `free_bpf_entry()`.
    allocated_bpf_entries: u32,
}

impl CgroupRegistry {
    /// Create a new cgroup registry with only the root cgroup.
    ///
    /// # Arguments
    /// * `nr_cpus` - Number of CPUs in the system.
    /// * `max_cgroups` - Maximum number of cgroups that can have BPF map entries.
    ///   Use `DEFAULT_MAX_CGROUPS` (10000) for normal tests, or a lower value
    ///   (e.g., 50) to test resource exhaustion scenarios.
    pub fn new(nr_cpus: u32, max_cgroups: u32) -> Self {
        let root = CgroupInfo {
            cgid: CgroupId::ROOT,
            level: 0,
            parent_cgid: CgroupId(0), // No parent
            cpuset: None,             // All CPUs
            name: String::new(),      // Root has no name
            alloc: CgroupAlloc::Root(SimCgroupHandle::root()),
        };

        let mut cgroups = HashMap::new();
        cgroups.insert(CgroupId::ROOT, root);

        CgroupRegistry {
            cgroups,
            name_to_id: HashMap::new(),
            next_cgid: 2, // Start after root (1)
            nr_cpus,
            max_cgroups,
            allocated_bpf_entries: 0,
        }
    }

    /// Try to allocate a BPF map entry for a cgroup.
    ///
    /// Returns `Ok(())` if allocation succeeds, `Err(-12)` (ENOMEM) if the
    /// maximum cgroup limit has been reached.
    ///
    /// This simulates the behavior of BPF hash map insertions that fail
    /// when the map is full (e.g., cgroup_bw_map in LAVD).
    pub fn try_allocate_bpf_entry(&mut self) -> Result<(), i32> {
        if self.allocated_bpf_entries >= self.max_cgroups {
            return Err(-12); // ENOMEM
        }
        self.allocated_bpf_entries += 1;
        Ok(())
    }

    /// Free a BPF map entry for a cgroup.
    ///
    /// Decrements the allocated entry count. Safe to call even if no entry
    /// was allocated (saturates at 0).
    pub fn free_bpf_entry(&mut self) {
        self.allocated_bpf_entries = self.allocated_bpf_entries.saturating_sub(1);
    }

    /// Get the current number of allocated BPF entries.
    pub fn allocated_bpf_entries(&self) -> u32 {
        self.allocated_bpf_entries
    }

    /// Get the maximum cgroup limit.
    pub fn max_cgroups(&self) -> u32 {
        self.max_cgroups
    }

    /// Set the maximum cgroup limit.
    ///
    /// This can be used to dynamically change the limit during simulation.
    pub fn set_max_cgroups(&mut self, max: u32) {
        self.max_cgroups = max;
    }

    /// Look up a cgroup by ID.
    pub fn get(&self, cgid: CgroupId) -> Option<&CgroupInfo> {
        self.cgroups.get(&cgid)
    }

    /// Look up a cgroup by name.
    pub fn get_by_name(&self, name: &str) -> Option<&CgroupInfo> {
        self.name_to_id
            .get(name)
            .and_then(|cgid| self.cgroups.get(cgid))
    }

    /// Get the raw C `struct cgroup` pointer for a cgroup ID.
    pub fn get_raw(&self, cgid: CgroupId) -> Option<*mut c_void> {
        self.cgroups.get(&cgid).map(|info| info.raw())
    }

    /// Get the root cgroup's raw pointer.
    pub fn root_raw(&self) -> *mut c_void {
        self.cgroups[&CgroupId::ROOT].raw()
    }

    /// Find the cgroup ID that corresponds to a raw pointer.
    ///
    /// Used by FFI lookup functions that receive raw pointers from C and
    /// need to resolve them back to a `CgroupId`.
    pub fn find_cgid_by_raw(&self, raw: *mut c_void) -> Option<CgroupId> {
        self.cgroups
            .values()
            .find(|info| info.raw() == raw)
            .map(|info| info.cgid)
    }

    /// Create a new cgroup as a child of the given parent.
    ///
    /// Returns the new cgroup's ID.
    pub fn create(
        &mut self,
        name: &str,
        parent_cgid: CgroupId,
        cpuset: Option<Vec<CpuId>>,
    ) -> CgroupId {
        let parent = self.cgroups.get(&parent_cgid).unwrap_or_else(|| {
            panic!("parent cgroup {:?} not found", parent_cgid);
        });

        let cgid = CgroupId(self.next_cgid);
        self.next_cgid += 1;

        let level = parent.level + 1;
        let parent_ptr = parent.alloc.as_ptr();

        // Allocate the C struct cgroup via the RAII handle.
        let handle = SimCgroupHandle::new(cgid.0, level, parent_ptr);

        // Set cpuset if specified.
        if let Some(ref cpus) = cpuset {
            handle.set_cpuset(cpus);
        }

        let info = CgroupInfo {
            cgid,
            level,
            parent_cgid,
            cpuset,
            name: name.to_string(),
            alloc: CgroupAlloc::Owned(handle),
        };

        self.cgroups.insert(cgid, info);
        if !name.is_empty() {
            self.name_to_id.insert(name.to_string(), cgid);
        }

        cgid
    }

    /// Create a cgroup with an auto-generated name under the root.
    pub fn create_under_root(&mut self, cpuset: Option<Vec<CpuId>>) -> CgroupId {
        let name = format!("cgroup_{}", self.next_cgid);
        self.create(&name, CgroupId::ROOT, cpuset)
    }

    /// Get the ancestor of a cgroup at a given level.
    ///
    /// Returns `None` if the level is invalid (> cgroup's level).
    pub fn ancestor(&self, cgid: CgroupId, level: u32) -> Option<&CgroupInfo> {
        let cgrp = self.cgroups.get(&cgid)?;
        if level > cgrp.level {
            return None;
        }
        if level == cgrp.level {
            return Some(cgrp);
        }
        // Walk up the tree
        let mut current = cgrp;
        while current.level > level {
            current = self.cgroups.get(&current.parent_cgid)?;
        }
        Some(current)
    }

    /// Iterate all cgroups in pre-order (depth-first, parent before children).
    ///
    /// Starts from the given root and yields descendant cgroups.
    pub fn iter_descendants(&self, root_cgid: CgroupId) -> impl Iterator<Item = &CgroupInfo> {
        // Collect descendants in pre-order
        let mut result = Vec::new();
        let mut stack = vec![root_cgid];

        while let Some(cgid) = stack.pop() {
            if let Some(info) = self.cgroups.get(&cgid) {
                result.push(info);
                // Add children in reverse sorted order so they come out in
                // ascending order. Sorting ensures deterministic traversal
                // since HashMap iteration order is non-deterministic.
                let mut children: Vec<CgroupId> = self
                    .cgroups
                    .values()
                    .filter(|c| c.parent_cgid == cgid && c.cgid != cgid)
                    .map(|c| c.cgid)
                    .collect();
                children.sort_by_key(|c| c.0);
                for child_cgid in children.into_iter().rev() {
                    stack.push(child_cgid);
                }
            }
        }

        result.into_iter()
    }

    /// Iterate all cgroups in **post-order** (depth-first, children before
    /// parent), starting from `root_cgid`.
    ///
    /// This is the order the production cgroup_bw library walks when it
    /// charges or replenishes the bandwidth-control tree bottom-up
    /// (`scx/lib/cgroup_bw.bpf.c` calls
    /// `bpf_for_each(css, pos, start_css, BPF_CGROUP_ITER_DESCENDANTS_POST)`
    /// at lines 1186, 1318, 1874).
    ///
    /// Phase 1 BPF infra scale-up item 3: pre-populate the C-side
    /// post-order buffer so Phase 2's compiled-in cgroup_bw.bpf.c can
    /// iterate POST without surprises. Children are visited in ascending
    /// `CgroupId` order under each parent for determinism (HashMap
    /// iteration order is otherwise nondeterministic).
    pub fn iter_descendants_postorder(
        &self,
        root_cgid: CgroupId,
    ) -> impl Iterator<Item = &CgroupInfo> {
        let mut result = Vec::new();
        // Iterative post-order with a single stack: push (cgid, visited_flag).
        // On first visit (flag = false) we push `(cgid, true)` followed by
        // each child (flag = false) in REVERSE ascending order, so when we
        // pop them they come out in ascending order. On second visit
        // (flag = true) we emit the cgroup itself.
        let mut stack: Vec<(CgroupId, bool)> = vec![(root_cgid, false)];
        while let Some((cgid, visited)) = stack.pop() {
            if visited {
                if let Some(info) = self.cgroups.get(&cgid) {
                    result.push(info);
                }
                continue;
            }
            // Re-push this node as "visited"; it will pop after all of its
            // descendants have been emitted.
            stack.push((cgid, true));
            let mut children: Vec<CgroupId> = self
                .cgroups
                .values()
                .filter(|c| c.parent_cgid == cgid && c.cgid != cgid)
                .map(|c| c.cgid)
                .collect();
            children.sort_by_key(|c| c.0);
            // Push children in reverse so the LEFTMOST child pops first ->
            // that subtree gets emitted first -> ascending child order under
            // each parent.
            for child_cgid in children.into_iter().rev() {
                stack.push((child_cgid, false));
            }
        }
        result.into_iter()
    }

    /// Get all cgroup IDs in pre-order starting from the root.
    pub fn all_cgids_preorder(&self) -> Vec<CgroupId> {
        self.iter_descendants(CgroupId::ROOT)
            .map(|info| info.cgid)
            .collect()
    }

    /// Number of cgroups in the registry.
    pub fn len(&self) -> usize {
        self.cgroups.len()
    }

    /// Check if the registry is empty (should never be true due to root).
    pub fn is_empty(&self) -> bool {
        self.cgroups.is_empty()
    }

    /// Update the cpuset for an existing cgroup.
    ///
    /// Updates both the Rust-side CgroupInfo and the C-side struct.
    pub fn update_cpuset(&mut self, name: &str, new_cpuset: Vec<CpuId>) -> bool {
        let cgid = match self.name_to_id.get(name) {
            Some(&id) => id,
            None => return false,
        };

        if let Some(info) = self.cgroups.get_mut(&cgid) {
            info.alloc.set_cpuset(&new_cpuset);
            info.cpuset = Some(new_cpuset);
            true
        } else {
            false
        }
    }

    /// Destroy a cgroup by name.
    ///
    /// Returns the raw pointer to the destroyed cgroup (for calling cgroup_exit),
    /// or `None` if the cgroup was not found. The C allocation is detached from
    /// the RAII handle via `into_raw()` so it is NOT freed here — the caller
    /// must call [`free_cgroup_raw`] after `cgroup_exit`.
    ///
    /// # Panics
    /// Panics if attempting to destroy the root cgroup.
    pub fn destroy_by_name(&mut self, name: &str) -> Option<*mut c_void> {
        let cgid = self.name_to_id.remove(name)?;
        assert!(cgid != CgroupId::ROOT, "cannot destroy root cgroup");

        let info = self.cgroups.remove(&cgid)?;
        // Detach the raw pointer from the RAII handle so it is not freed
        // on drop. The caller will free it after cgroup_exit.
        Some(info.alloc.into_raw())
    }

    /// Free a raw cgroup pointer after cgroup_exit has been called.
    ///
    /// Must only be called with a pointer returned from `destroy_by_name`,
    /// and only after `cgroup_exit` has been called for that cgroup.
    ///
    /// Delegates to [`free_cgroup_raw`] from the cgroup_wrapper module.
    pub fn free_raw(&self, raw: *mut c_void) {
        free_cgroup_raw(raw);
    }

    /// Prepare the CSS iterator for iteration from the given root.
    ///
    /// Populates BOTH C-side iteration lists -- pre-order in
    /// `sim_css_list_pre[]` and post-order in `sim_css_list_post[]` --
    /// so that subsequent `bpf_for_each(css, pos, root, flags)` loops
    /// using either `BPF_CGROUP_ITER_DESCENDANTS_PRE` or
    /// `BPF_CGROUP_ITER_DESCENDANTS_POST` see the right traversal
    /// order. Phase 1 BPF infra scale-up item 3 -- the production
    /// cgroup_bw library walks POST when charging / replenishing the
    /// bandwidth-control tree (`scx/lib/cgroup_bw.bpf.c:1186, 1318,
    /// 1874`).
    ///
    /// Both lists are bounded by `MAX_CSS_ITER_CGROUPS` (2 048 in
    /// `csrc/sim_cgroup.c`); cgroups beyond that capacity are silently
    /// dropped. Bump the constant if the workload ever needs more.
    ///
    /// Must be called from the simulator's single-threaded context
    /// (which is guaranteed by the Arc<Mutex> / token-ring protocol).
    pub fn prepare_css_iter(&self, root_cgid: CgroupId) {
        if let Some(root) = self.cgroups.get(&root_cgid) {
            let root_ptr = root.alloc.as_ptr();
            let descendants_pre: Vec<CgroupPtr> = self
                .iter_descendants(root_cgid)
                .map(|info| info.alloc.as_ptr())
                .collect();
            let descendants_post: Vec<CgroupPtr> = self
                .iter_descendants_postorder(root_cgid)
                .map(|info| info.alloc.as_ptr())
                .collect();
            let _guard = CssIterGuard::prepare(root_ptr, &descendants_pre, &descendants_post);
        }
    }

    /// Prepare the CSS iterator for iteration from the root cgroup.
    ///
    /// Convenience wrapper for `prepare_css_iter(CgroupId::ROOT)`.
    ///
    /// Must be called from the simulator's single-threaded context.
    pub fn prepare_css_iter_from_root(&self) {
        self.prepare_css_iter(CgroupId::ROOT);
    }
}

// No manual Drop needed — `CgroupAlloc::Owned` frees via RAII on drop,
// and `CgroupAlloc::Root` is a non-owning pointer that is not freed.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_registry_has_root() {
        let registry = CgroupRegistry::new(4, DEFAULT_MAX_CGROUPS);
        assert!(registry.get(CgroupId::ROOT).is_some());
        assert_eq!(registry.get(CgroupId::ROOT).unwrap().level, 0);
    }

    /// Phase 1 BPF infra scale-up item 3: post-order CSS iter walks
    /// children before parent and is deterministic across runs (children
    /// emitted in ascending CgroupId order under each parent).
    ///
    /// Tree built (mirrors a multi-level cpu.max hierarchy). `CgroupId`
    /// numbering starts at ROOT=1 and `next_cgid` begins at 2:
    ///
    ///         ROOT (cgid=1)
    ///         /    |    \
    ///        a     b     c     (cgid 2, 3, 4)
    ///       /|          /|
    ///      d e         f g     (cgid 5, 6, 7, 8)
    ///
    /// Pre-order from ROOT  : ROOT, a, d, e, b, c, f, g
    /// Post-order from ROOT : d, e, a, b, f, g, c, ROOT
    #[test]
    fn test_iter_descendants_postorder_matches_design() {
        let mut registry = CgroupRegistry::new(4, DEFAULT_MAX_CGROUPS);
        let a = registry.create("a", CgroupId::ROOT, None); // 2
        let b = registry.create("b", CgroupId::ROOT, None); // 3
        let c = registry.create("c", CgroupId::ROOT, None); // 4
        let _d = registry.create("d", a, None); // 5
        let _e = registry.create("e", a, None); // 6
        let _f = registry.create("f", c, None); // 7
        let _g = registry.create("g", c, None); // 8

        let pre: Vec<u64> = registry
            .iter_descendants(CgroupId::ROOT)
            .map(|info| info.cgid.0)
            .collect();
        assert_eq!(
            pre,
            vec![1, 2, 5, 6, 3, 4, 7, 8],
            "pre-order traversal should be parent-before-children, \
             children in ascending cgid order"
        );

        let post: Vec<u64> = registry
            .iter_descendants_postorder(CgroupId::ROOT)
            .map(|info| info.cgid.0)
            .collect();
        assert_eq!(
            post,
            vec![5, 6, 2, 3, 7, 8, 4, 1],
            "post-order traversal should be children-before-parent, \
             children in ascending cgid order; this is the order \
             scx/lib/cgroup_bw.bpf.c walks during charge / replenish \
             (lib/cgroup_bw.bpf.c:1186, 1318, 1874)"
        );

        // Sub-tree post-order: from `a` we should see d, e, a (no siblings).
        let sub: Vec<u64> = registry
            .iter_descendants_postorder(a)
            .map(|info| info.cgid.0)
            .collect();
        assert_eq!(sub, vec![5, 6, 2]);

        // Determinism across two calls — must be byte-identical.
        let post2: Vec<u64> = registry
            .iter_descendants_postorder(CgroupId::ROOT)
            .map(|info| info.cgid.0)
            .collect();
        assert_eq!(post, post2);
        // Make sure b/c are unused so clippy doesn't complain.
        let _ = (b, c);
    }

    /// Single-cgroup post-order (just root) should yield [ROOT] and never
    /// underflow. Edge case used by `prepare_single`.
    #[test]
    fn test_iter_descendants_postorder_single_root() {
        let registry = CgroupRegistry::new(4, DEFAULT_MAX_CGROUPS);
        let post: Vec<u64> = registry
            .iter_descendants_postorder(CgroupId::ROOT)
            .map(|info| info.cgid.0)
            .collect();
        assert_eq!(post, vec![CgroupId::ROOT.0]);
    }

    #[test]
    fn test_bpf_entry_allocation() {
        let mut registry = CgroupRegistry::new(4, 3);
        assert_eq!(registry.allocated_bpf_entries(), 0);
        assert_eq!(registry.max_cgroups(), 3);

        // First 3 allocations should succeed
        assert!(registry.try_allocate_bpf_entry().is_ok());
        assert!(registry.try_allocate_bpf_entry().is_ok());
        assert!(registry.try_allocate_bpf_entry().is_ok());
        assert_eq!(registry.allocated_bpf_entries(), 3);

        // 4th allocation should fail with ENOMEM
        assert_eq!(registry.try_allocate_bpf_entry(), Err(-12));
        assert_eq!(registry.allocated_bpf_entries(), 3);

        // Free one entry
        registry.free_bpf_entry();
        assert_eq!(registry.allocated_bpf_entries(), 2);

        // Now allocation should succeed again
        assert!(registry.try_allocate_bpf_entry().is_ok());
        assert_eq!(registry.allocated_bpf_entries(), 3);
    }

    #[test]
    fn test_free_bpf_entry_saturates() {
        let mut registry = CgroupRegistry::new(4, 10);
        assert_eq!(registry.allocated_bpf_entries(), 0);

        // Free with zero entries should not underflow
        registry.free_bpf_entry();
        assert_eq!(registry.allocated_bpf_entries(), 0);
    }
}
