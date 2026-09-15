//! Arbitrary virtual machine topologies: sockets, NUMA nodes, LLCs, SMT.
//!
//! # One description, both consumers
//!
//! Before this module the machine shape was described TWICE and independently:
//! once to the engine (`ScenarioBuilder::cpus`/`cpus_per_llc`/`smt`) and once
//! to the scheduler wrapper (`DynamicScheduler::layered_with_topology(...)`,
//! `cosmos_with_numa(...)`). Nothing checked that the two agreed, and the
//! scheduler's copy carried a NUMA partition the engine did not model at all.
//! A [`MachineTopology`] is the single description both sides are built from,
//! so "the scheduler is looking at a different machine than the engine is
//! simulating" stops being expressible.
//!
//! # What it can express
//!
//! A per-CPU `(core, llc, node)` assignment, with no requirement that any of
//! them be equally sized. That covers:
//!
//! * **sockets / NUMA nodes** — `node_id`, including NPS-style sub-NUMA where
//!   one socket is several nodes;
//! * **LLCs** — `llc_id`, e.g. an EPYC CCX at 8 cores / 16 threads;
//! * **SMT** — `core_id`, with any number of threads per core, and different
//!   counts on different cores;
//! * **asymmetric shapes** — nodes with different CPU counts, LLCs with
//!   different core counts, a machine where one socket has SMT and the other
//!   does not.
//!
//! # What it cannot express, and why
//!
//! These are refused at construction rather than silently mismodelled:
//!
//! * **An SMT core spanning two LLCs or two nodes.** No such hardware exists;
//!   both scx_layered and scx_cosmos index a CPU's LLC through its core.
//! * **An LLC spanning two nodes.** `llc_numa_id_map[llc]` is a single value
//!   in scx_layered's ABI, so a split LLC has no representation to publish.
//! * **Sparse / non-dense ids.** Core, LLC and node ids must cover
//!   `0..count` with no gaps, because every consumer indexes fixed-size
//!   arrays by them.
//! * **Offline CPUs at construction.** Bring the machine up whole and use
//!   `HotplugEvent` to take CPUs offline during the run.
//!
//! And these are expressible but NOT modelled by the engine — see
//! `ai_docs/` for the enumeration that accompanies this module:
//!
//! * **memory** — there is no per-node memory, no page placement, no
//!   bandwidth or capacity. `node_id` costs a task extra *latency* when it
//!   migrates ([`OverheadConfig::cross_node_migration_penalty_ns`]) and
//!   nothing else.
//! * **distance matrices** — inter-node cost is one flat number, not an
//!   ACPI SLIT. A 4-node machine charges the same for the near hop as the
//!   far one.
//! * **heterogeneous cores** — no big.LITTLE. Every core is
//!   `CoreType::Big`; scx_layered's `has_little_cores` is published false.
//! * **caches other than the LLC** — no L1/L2 sharing, no cache sizes.
//!
//! [`OverheadConfig::cross_node_migration_penalty_ns`]:
//!     crate::scenario::OverheadConfig::cross_node_migration_penalty_ns

use crate::types::CpuId;

/// Where one CPU sits in the machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CpuTopology {
    /// Physical core. CPUs sharing a `core_id` are SMT siblings.
    pub core_id: u32,
    /// Last-level cache domain (CCX / ring stop / cluster).
    pub llc_id: u32,
    /// NUMA node. On a plain dual-socket box this is the socket.
    pub node_id: u32,
}

/// A whole machine's CPU topology.
///
/// Build with [`MachineTopology::uniform`] for regular shapes or
/// [`MachineTopology::from_cpus`] for asymmetric ones. See the module docs for
/// what is and is not expressible.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MachineTopology {
    per_cpu: Vec<CpuTopology>,
    nr_cores: u32,
    nr_llcs: u32,
    nr_nodes: u32,
}

impl MachineTopology {
    /// The regular shape: `nr_cpus` CPUs laid out as consecutive SMT cores,
    /// consecutive LLCs, and LLCs dealt into `nr_nodes` equal groups.
    ///
    /// This reproduces exactly what `layered_set_topology()` and
    /// `Topology::simulated()` derive from the same four numbers, so an
    /// existing scenario keeps its shape byte for byte.
    ///
    /// `cpus_per_llc == 0` means one LLC covering the machine.
    ///
    /// # Panics
    /// Panics on a shape the divisions cannot produce — see the module docs.
    pub fn uniform(nr_cpus: u32, cpus_per_llc: u32, nr_nodes: u32, threads_per_core: u32) -> Self {
        assert!(nr_cpus > 0, "nr_cpus must be positive");
        assert!(threads_per_core > 0, "threads_per_core must be positive");
        assert!(nr_nodes > 0, "nr_nodes must be positive");
        assert!(
            nr_cpus.is_multiple_of(threads_per_core),
            "nr_cpus ({nr_cpus}) must be divisible by threads_per_core ({threads_per_core})"
        );
        let cpus_per_llc = if cpus_per_llc == 0 {
            nr_cpus
        } else {
            cpus_per_llc
        };
        assert!(
            nr_cpus.is_multiple_of(cpus_per_llc),
            "nr_cpus ({nr_cpus}) must be divisible by cpus_per_llc ({cpus_per_llc})"
        );
        assert!(
            cpus_per_llc.is_multiple_of(threads_per_core),
            "an SMT core may not cross an LLC boundary ({cpus_per_llc} CPUs per LLC, \
             {threads_per_core} per core)"
        );
        let nr_llcs = nr_cpus / cpus_per_llc;
        assert!(
            nr_nodes <= nr_llcs,
            "{nr_nodes} nodes need at least that many LLCs; this shape has {nr_llcs}. \
             Lower nr_nodes, or lower cpus_per_llc so there are more LLCs to group."
        );
        // Same ceiling division `layered_set_topology()` uses, so the two
        // agree on which LLC lands on which node when the split is uneven.
        let llcs_per_node = nr_llcs.div_ceil(nr_nodes);
        let per_cpu = (0..nr_cpus)
            .map(|cpu| {
                let llc_id = cpu / cpus_per_llc;
                CpuTopology {
                    core_id: cpu / threads_per_core,
                    llc_id,
                    node_id: llc_id / llcs_per_node,
                }
            })
            .collect::<Vec<_>>();
        let topo = Self::from_cpus(per_cpu);
        // A ceiling division can leave the TOP nodes empty: 5 LLCs over 4
        // nodes gives llcs_per_node = 2, so the LLCs land on nodes 0, 0, 1,
        // 1, 2 and node 3 gets nothing. The id space is still dense, so
        // `from_cpus` accepts it and the caller silently gets a 3-node
        // machine having asked for 4 — the exact class of quiet divergence
        // this type exists to stop. (The C `layered_set_topology()` is worse
        // here: it returns the requested 4 while populating 3.) Refuse, and
        // say which knob makes the division come out even.
        assert_eq!(
            topo.nr_nodes(),
            nr_nodes,
            "{nr_nodes} nodes over {nr_llcs} LLCs leaves the top node(s) empty \
             (ceil({nr_llcs}/{nr_nodes}) = {llcs_per_node} LLCs per node fills only \
             {} of them). Choose an nr_nodes that divides {nr_llcs}, or change \
             cpus_per_llc so it does.",
            topo.nr_nodes()
        );
        topo
    }

    /// An explicit per-CPU assignment, for shapes [`Self::uniform`] cannot
    /// produce: unequal nodes, unequal LLCs, SMT on part of the machine.
    ///
    /// `per_cpu[i]` describes CPU `i`.
    ///
    /// # Panics
    /// Panics, naming the offending CPU, if the shape is one the simulator
    /// cannot publish — see the module docs for the list and the reasoning.
    pub fn from_cpus(per_cpu: Vec<CpuTopology>) -> Self {
        assert!(!per_cpu.is_empty(), "a machine needs at least one CPU");
        let nr_cores = Self::check_dense(per_cpu.iter().map(|c| c.core_id), "core");
        let nr_llcs = Self::check_dense(per_cpu.iter().map(|c| c.llc_id), "LLC");
        let nr_nodes = Self::check_dense(per_cpu.iter().map(|c| c.node_id), "node");

        // A core may not straddle an LLC, and an LLC may not straddle a node.
        let mut core_llc = vec![u32::MAX; nr_cores as usize];
        let mut core_node = vec![u32::MAX; nr_cores as usize];
        let mut llc_node = vec![u32::MAX; nr_llcs as usize];
        for (cpu, t) in per_cpu.iter().enumerate() {
            let slot = &mut core_llc[t.core_id as usize];
            assert!(
                *slot == u32::MAX || *slot == t.llc_id,
                "cpu {cpu}: SMT core {} spans LLC {} and LLC {}; an SMT core may not \
                 cross an LLC boundary",
                t.core_id,
                *slot,
                t.llc_id
            );
            *slot = t.llc_id;

            let slot = &mut core_node[t.core_id as usize];
            assert!(
                *slot == u32::MAX || *slot == t.node_id,
                "cpu {cpu}: SMT core {} spans node {} and node {}; an SMT core may not \
                 cross a NUMA boundary",
                t.core_id,
                *slot,
                t.node_id
            );
            *slot = t.node_id;

            let slot = &mut llc_node[t.llc_id as usize];
            assert!(
                *slot == u32::MAX || *slot == t.node_id,
                "cpu {cpu}: LLC {} spans node {} and node {}; scx_layered publishes one \
                 node per LLC (llc_numa_id_map), so a split LLC has no representation",
                t.llc_id,
                *slot,
                t.node_id
            );
            *slot = t.node_id;
        }

        Self {
            per_cpu,
            nr_cores,
            nr_llcs,
            nr_nodes,
        }
    }

    /// Assert that `ids` covers `0..n` with no gaps, and return `n`.
    fn check_dense(ids: impl Iterator<Item = u32>, what: &str) -> u32 {
        let mut seen: Vec<u32> = ids.collect();
        seen.sort_unstable();
        seen.dedup();
        let n = seen.len() as u32;
        for (expected, &got) in seen.iter().enumerate() {
            assert_eq!(
                got, expected as u32,
                "{what} ids must be dense from 0; {n} distinct ids were used but \
                 {expected} is missing (every consumer indexes fixed-size arrays by \
                 this id, so a gap reads someone else's slot)"
            );
        }
        n
    }

    /// Number of CPUs.
    pub fn nr_cpus(&self) -> u32 {
        self.per_cpu.len() as u32
    }

    /// Number of physical cores.
    pub fn nr_cores(&self) -> u32 {
        self.nr_cores
    }

    /// Number of LLC domains.
    pub fn nr_llcs(&self) -> u32 {
        self.nr_llcs
    }

    /// Number of NUMA nodes.
    pub fn nr_nodes(&self) -> u32 {
        self.nr_nodes
    }

    /// Per-CPU assignments, indexed by CPU id.
    pub fn cpus(&self) -> &[CpuTopology] {
        &self.per_cpu
    }

    /// This CPU's placement.
    pub fn cpu(&self, cpu: CpuId) -> CpuTopology {
        self.per_cpu[cpu.0 as usize]
    }

    /// This CPU's NUMA node.
    pub fn node_of(&self, cpu: CpuId) -> u32 {
        self.per_cpu[cpu.0 as usize].node_id
    }

    /// This CPU's LLC.
    pub fn llc_of(&self, cpu: CpuId) -> u32 {
        self.per_cpu[cpu.0 as usize].llc_id
    }

    /// This CPU's physical core.
    pub fn core_of(&self, cpu: CpuId) -> u32 {
        self.per_cpu[cpu.0 as usize].core_id
    }

    /// The NUMA node each LLC belongs to, indexed by LLC id.
    pub fn llc_nodes(&self) -> Vec<u32> {
        let mut out = vec![0u32; self.nr_llcs as usize];
        for t in &self.per_cpu {
            out[t.llc_id as usize] = t.node_id;
        }
        out
    }

    /// Every CPU sharing a physical core with `cpu`, INCLUDING `cpu`, in
    /// ascending id order. Length 1 when SMT is off for that core.
    pub fn siblings(&self, cpu: CpuId) -> Vec<CpuId> {
        let core = self.core_of(cpu);
        (0..self.nr_cpus())
            .filter(|&c| self.per_cpu[c as usize].core_id == core)
            .map(CpuId)
            .collect()
    }

    /// The single SMT partner the kernel records in `__sibling_cpu`, or
    /// `None` when the core has one thread.
    ///
    /// With more than two threads per core the kernel only records one
    /// partner; take the next thread in the core, wrapping — the same choice
    /// `layered_set_topology()` makes.
    pub fn sibling_cpu(&self, cpu: CpuId) -> Option<CpuId> {
        let sibs = self.siblings(cpu);
        if sibs.len() < 2 {
            return None;
        }
        let idx = sibs
            .iter()
            .position(|&c| c == cpu)
            .expect("cpu in own core");
        Some(sibs[(idx + 1) % sibs.len()])
    }

    /// CPUs on `node`, ascending.
    pub fn cpus_of_node(&self, node: u32) -> Vec<CpuId> {
        (0..self.nr_cpus())
            .filter(|&c| self.per_cpu[c as usize].node_id == node)
            .map(CpuId)
            .collect()
    }

    /// CPUs in `llc`, ascending.
    pub fn cpus_of_llc(&self, llc: u32) -> Vec<CpuId> {
        (0..self.nr_cpus())
            .filter(|&c| self.per_cpu[c as usize].llc_id == llc)
            .map(CpuId)
            .collect()
    }

    /// Lowest-numbered CPU on each node, indexed by node id.
    ///
    /// This is what [`crate::scenario::ForkPlacement::RoundRobinNodes`] deals
    /// tasks onto, and what scx_layered publishes as `fallback_cpus[node]`.
    pub fn first_cpu_of_each_node(&self) -> Vec<CpuId> {
        (0..self.nr_nodes)
            .map(|n| {
                *self
                    .cpus_of_node(n)
                    .first()
                    .expect("node ids are dense, so every node has a CPU")
            })
            .collect()
    }

    /// True when at least one core has more than one thread.
    pub fn smt_enabled(&self) -> bool {
        self.nr_cores < self.nr_cpus()
    }

    /// Threads per core when every core has the same count, else `None`.
    ///
    /// The uniform scheduler-side APIs take a single `threads_per_core`, so
    /// an asymmetric-SMT machine has to go through the explicit publishing
    /// path instead. Returning `None` is how that is detected rather than
    /// guessed.
    pub fn uniform_threads_per_core(&self) -> Option<u32> {
        let mut counts = vec![0u32; self.nr_cores as usize];
        for t in &self.per_cpu {
            counts[t.core_id as usize] += 1;
        }
        let first = counts[0];
        counts.iter().all(|&c| c == first).then_some(first)
    }

    /// CPUs per LLC when every LLC has the same count, else `None`.
    pub fn uniform_cpus_per_llc(&self) -> Option<u32> {
        let mut counts = vec![0u32; self.nr_llcs as usize];
        for t in &self.per_cpu {
            counts[t.llc_id as usize] += 1;
        }
        let first = counts[0];
        counts.iter().all(|&c| c == first).then_some(first)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uniform_matches_the_wrapper_division() {
        // 384 CPUs, 16-CPU LLCs, 2 nodes, SMT2 — the DUAL_SOCKET_384 shape.
        let t = MachineTopology::uniform(384, 16, 2, 2);
        assert_eq!(t.nr_llcs(), 24);
        assert_eq!(t.nr_nodes(), 2);
        assert_eq!(t.nr_cores(), 192);
        for cpu in 0..384u32 {
            assert_eq!(t.llc_of(CpuId(cpu)), cpu / 16);
            // layered_set_topology: llcs_per_node = ceil(24/2) = 12.
            assert_eq!(t.node_of(CpuId(cpu)), (cpu / 16) / 12);
            assert_eq!(t.core_of(CpuId(cpu)), cpu / 2);
        }
        assert_eq!(t.sibling_cpu(CpuId(382)), Some(CpuId(383)));
        assert_eq!(t.sibling_cpu(CpuId(383)), Some(CpuId(382)));
    }

    #[test]
    fn single_thread_cores_have_no_sibling() {
        let t = MachineTopology::uniform(4, 2, 2, 1);
        assert!(!t.smt_enabled());
        assert_eq!(t.sibling_cpu(CpuId(0)), None);
        assert_eq!(t.siblings(CpuId(0)), vec![CpuId(0)]);
    }

    #[test]
    fn four_threads_per_core_wrap_in_core() {
        let t = MachineTopology::uniform(8, 8, 1, 4);
        assert_eq!(t.sibling_cpu(CpuId(0)), Some(CpuId(1)));
        assert_eq!(t.sibling_cpu(CpuId(3)), Some(CpuId(0)));
        assert_eq!(
            t.siblings(CpuId(2)),
            vec![CpuId(0), CpuId(1), CpuId(2), CpuId(3)]
        );
    }

    #[test]
    fn asymmetric_nodes_are_allowed() {
        // Node 0: 4 CPUs in one LLC. Node 1: 2 CPUs in one LLC.
        let t = MachineTopology::from_cpus(vec![
            CpuTopology {
                core_id: 0,
                llc_id: 0,
                node_id: 0,
            },
            CpuTopology {
                core_id: 1,
                llc_id: 0,
                node_id: 0,
            },
            CpuTopology {
                core_id: 2,
                llc_id: 0,
                node_id: 0,
            },
            CpuTopology {
                core_id: 3,
                llc_id: 0,
                node_id: 0,
            },
            CpuTopology {
                core_id: 4,
                llc_id: 1,
                node_id: 1,
            },
            CpuTopology {
                core_id: 5,
                llc_id: 1,
                node_id: 1,
            },
        ]);
        assert_eq!(t.nr_nodes(), 2);
        assert_eq!(t.cpus_of_node(0).len(), 4);
        assert_eq!(t.cpus_of_node(1).len(), 2);
        assert_eq!(t.uniform_cpus_per_llc(), None);
        assert_eq!(t.first_cpu_of_each_node(), vec![CpuId(0), CpuId(4)]);
    }

    #[test]
    fn smt_on_one_socket_only() {
        // Node 0: 2 SMT2 cores (4 CPUs). Node 1: 2 single-thread cores.
        let t = MachineTopology::from_cpus(vec![
            CpuTopology {
                core_id: 0,
                llc_id: 0,
                node_id: 0,
            },
            CpuTopology {
                core_id: 0,
                llc_id: 0,
                node_id: 0,
            },
            CpuTopology {
                core_id: 1,
                llc_id: 0,
                node_id: 0,
            },
            CpuTopology {
                core_id: 1,
                llc_id: 0,
                node_id: 0,
            },
            CpuTopology {
                core_id: 2,
                llc_id: 1,
                node_id: 1,
            },
            CpuTopology {
                core_id: 3,
                llc_id: 1,
                node_id: 1,
            },
        ]);
        assert!(t.smt_enabled());
        assert_eq!(t.uniform_threads_per_core(), None);
        assert_eq!(t.sibling_cpu(CpuId(0)), Some(CpuId(1)));
        assert_eq!(t.sibling_cpu(CpuId(4)), None);
    }

    #[test]
    #[should_panic(expected = "may not cross an LLC boundary")]
    fn a_core_may_not_span_two_llcs() {
        MachineTopology::from_cpus(vec![
            CpuTopology {
                core_id: 0,
                llc_id: 0,
                node_id: 0,
            },
            CpuTopology {
                core_id: 0,
                llc_id: 1,
                node_id: 0,
            },
        ]);
    }

    #[test]
    #[should_panic(expected = "has no representation")]
    fn an_llc_may_not_span_two_nodes() {
        MachineTopology::from_cpus(vec![
            CpuTopology {
                core_id: 0,
                llc_id: 0,
                node_id: 0,
            },
            CpuTopology {
                core_id: 1,
                llc_id: 0,
                node_id: 1,
            },
        ]);
    }

    #[test]
    #[should_panic(expected = "must be dense from 0")]
    fn node_ids_must_be_dense() {
        MachineTopology::from_cpus(vec![
            CpuTopology {
                core_id: 0,
                llc_id: 0,
                node_id: 0,
            },
            CpuTopology {
                core_id: 1,
                llc_id: 1,
                node_id: 2,
            },
        ]);
    }

    /// An uneven LLC-to-node split leaves the top node empty, and that must
    /// be refused rather than quietly returning a smaller machine.
    ///
    /// 40 CPUs / 8 per LLC = 5 LLCs. Over 4 nodes the ceiling division puts
    /// 2 LLCs on each of nodes 0, 1 and 2 and nothing on node 3. The node id
    /// space is still dense, so `from_cpus` alone would accept it and hand
    /// back a 3-node machine to a caller who asked for 4.
    #[test]
    #[should_panic(expected = "leaves the top node(s) empty")]
    fn an_uneven_node_split_is_refused_rather_than_silently_shrunk() {
        MachineTopology::uniform(40, 8, 4, 1);
    }

    #[test]
    #[should_panic(expected = "need at least that many LLCs")]
    fn more_nodes_than_llcs_is_refused_rather_than_clamped() {
        // The C wrapper clamps this to nr_llcs. Clamping silently is how the
        // engine and the scheduler end up on different machines, so the Rust
        // side refuses instead and says which knob to move.
        MachineTopology::uniform(8, 8, 4, 1);
    }
}
