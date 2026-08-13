//! Build adapter for scx_layered's real `layer_core_growth.rs` policy.
//!
//! The production module depends on scx_utils topology containers and on a
//! few scx_layered configuration types, but not on the BPF skeleton. This
//! crate supplies those data containers from the simulator topology and then
//! compiles the upstream policy file verbatim. It deliberately exposes no
//! fake CpuSetSpread topology: scxsim has no cgroup-cpuset model, so callers
//! must reject those algorithms rather than deriving placement from the host.

extern crate self as scx_utils;
extern crate self as walkdir;

use anyhow::{bail, Result};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// An empty virtual directory walk.
///
/// `layer_core_growth.rs` probes `/sys/fs/cgroup` only to discover CpuSetSpread
/// domains. The simulator has no such domains and must never consume the host
/// machine's cgroups. CpuSetSpread is rejected by the public simulator API;
/// for every other algorithm, the honest virtual cgroup tree is empty.
pub struct WalkDir;

impl WalkDir {
    pub fn new(_path: impl AsRef<Path>) -> Self {
        Self
    }
}

pub struct DirEntry {
    path: PathBuf,
}

impl DirEntry {
    pub fn file_name(&self) -> &OsStr {
        self.path.file_name().unwrap_or_else(|| OsStr::new(""))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl IntoIterator for WalkDir {
    type Item = std::io::Result<DirEntry>;
    type IntoIter = std::iter::Empty<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        std::iter::empty()
    }
}

#[derive(Clone, Debug)]
pub struct Cpu {
    pub id: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CoreType {
    Big,
    Little,
}

#[derive(Clone, Debug)]
pub struct Core {
    pub id: usize,
    pub node_id: usize,
    pub llc_id: usize,
    pub core_type: CoreType,
    pub cpus: BTreeMap<usize, Arc<Cpu>>,
}

#[derive(Clone, Debug)]
pub struct Llc {
    pub id: usize,
    pub cores: BTreeMap<usize, Arc<Core>>,
}

#[derive(Clone, Debug)]
pub struct Node {
    pub llcs: BTreeMap<usize, Arc<Llc>>,
    pub all_cores: BTreeMap<usize, Arc<Core>>,
}

#[derive(Clone, Debug)]
pub struct Topology {
    pub nodes: BTreeMap<usize, Arc<Node>>,
    pub all_cores: BTreeMap<usize, Arc<Core>>,
}

impl Topology {
    /// Build the exact CPU/core/LLC/node numbering published by wrapper.c.
    pub fn simulated(
        nr_cpus: usize,
        cpus_per_llc: usize,
        nr_nodes: usize,
        threads_per_core: usize,
    ) -> Result<Self> {
        if nr_cpus == 0 || cpus_per_llc == 0 || threads_per_core == 0 {
            bail!("topology dimensions must be positive");
        }
        if !nr_cpus.is_multiple_of(cpus_per_llc) || !nr_cpus.is_multiple_of(threads_per_core) {
            bail!("CPU count must be divisible by LLC and core widths");
        }
        if !cpus_per_llc.is_multiple_of(threads_per_core) {
            bail!("an SMT core may not cross an LLC boundary");
        }
        let nr_llcs = nr_cpus.div_ceil(cpus_per_llc);
        let nr_nodes = nr_nodes.clamp(1, nr_llcs);
        let llcs_per_node = nr_llcs.div_ceil(nr_nodes);

        let mut all_cores = BTreeMap::new();
        for core_id in 0..nr_cpus / threads_per_core {
            let first_cpu = core_id * threads_per_core;
            let llc_id = first_cpu / cpus_per_llc;
            let node_id = llc_id / llcs_per_node;
            let cpus = (first_cpu..first_cpu + threads_per_core)
                .map(|cpu| (cpu, Arc::new(Cpu { id: cpu })))
                .collect();
            all_cores.insert(
                core_id,
                Arc::new(Core {
                    id: core_id,
                    node_id,
                    llc_id,
                    // scxsim currently publishes has_little_cores=false.
                    core_type: CoreType::Big,
                    cpus,
                }),
            );
        }

        let mut all_llcs = BTreeMap::new();
        for llc_id in 0..nr_llcs {
            let cores = all_cores
                .iter()
                .filter(|(_, core)| core.llc_id == llc_id)
                .map(|(&id, core)| (id, Arc::clone(core)))
                .collect();
            all_llcs.insert(llc_id, Arc::new(Llc { id: llc_id, cores }));
        }

        let mut nodes = BTreeMap::new();
        for node_id in 0..nr_nodes {
            let llcs = all_llcs
                .iter()
                .filter(|(&llc_id, _)| llc_id / llcs_per_node == node_id)
                .map(|(&id, llc)| (id, Arc::clone(llc)))
                .collect();
            let node_cores = all_cores
                .iter()
                .filter(|(_, core)| core.node_id == node_id)
                .map(|(&id, core)| (id, Arc::clone(core)))
                .collect();
            nodes.insert(
                node_id,
                Arc::new(Node {
                    llcs,
                    all_cores: node_cores,
                }),
            );
        }
        Ok(Self { nodes, all_cores })
    }
}

#[derive(Clone, Debug)]
pub struct CpuPool;

impl CpuPool {
    pub fn core_seq(&self, core: &Core) -> usize {
        core.id
    }
}

#[derive(Clone, Debug)]
pub struct Cpumask {
    cpus: Vec<bool>,
}

impl Cpumask {
    pub fn test_cpu(&self, cpu: usize) -> bool {
        self.cpus.get(cpu).copied().unwrap_or(false)
    }
}

#[derive(Clone, Debug)]
pub struct LayerCommon {
    pub growth_algo: layer_core_growth::LayerGrowthAlgo,
}

#[derive(Clone, Debug)]
pub struct LayerKind {
    common: LayerCommon,
}

impl LayerKind {
    pub fn common(&self) -> &LayerCommon {
        &self.common
    }
}

#[derive(Clone, Debug)]
pub struct LayerSpec {
    pub kind: LayerKind,
    pub cpuset: Option<Cpumask>,
    nodes: Vec<usize>,
    llcs: Vec<usize>,
}

impl LayerSpec {
    pub fn new(algo: layer_core_growth::LayerGrowthAlgo) -> Self {
        Self {
            kind: LayerKind {
                common: LayerCommon { growth_algo: algo },
            },
            cpuset: None,
            nodes: Vec::new(),
            llcs: Vec::new(),
        }
    }

    pub fn with_nodes(mut self, nodes: Vec<usize>) -> Self {
        self.nodes = nodes;
        self
    }

    pub fn with_llcs(mut self, llcs: Vec<usize>) -> Self {
        self.llcs = llcs;
        self
    }

    pub fn nodes(&self) -> &Vec<usize> {
        &self.nodes
    }

    pub fn llcs(&self) -> &Vec<usize> {
        &self.llcs
    }
}

#[allow(non_upper_case_globals)]
pub mod bpf_intf {
    pub const layer_growth_algo_GROWTH_ALGO_STICKY: u32 = 0;
    pub const layer_growth_algo_GROWTH_ALGO_LINEAR: u32 = 1;
    pub const layer_growth_algo_GROWTH_ALGO_REVERSE: u32 = 2;
    pub const layer_growth_algo_GROWTH_ALGO_RANDOM: u32 = 3;
    pub const layer_growth_algo_GROWTH_ALGO_TOPO: u32 = 4;
    pub const layer_growth_algo_GROWTH_ALGO_ROUND_ROBIN: u32 = 5;
    pub const layer_growth_algo_GROWTH_ALGO_BIG_LITTLE: u32 = 6;
    pub const layer_growth_algo_GROWTH_ALGO_LITTLE_BIG: u32 = 7;
    pub const layer_growth_algo_GROWTH_ALGO_NODE_SPREAD: u32 = 8;
    pub const layer_growth_algo_GROWTH_ALGO_NODE_SPREAD_REVERSE: u32 = 9;
    pub const layer_growth_algo_GROWTH_ALGO_NODE_SPREAD_RANDOM: u32 = 10;
    pub const layer_growth_algo_GROWTH_ALGO_CPUSET_SPREAD: u32 = 11;
    pub const layer_growth_algo_GROWTH_ALGO_CPUSET_SPREAD_REVERSE: u32 = 12;
    pub const layer_growth_algo_GROWTH_ALGO_CPUSET_SPREAD_RANDOM: u32 = 13;
    pub const layer_growth_algo_GROWTH_ALGO_RANDOM_TOPO: u32 = 14;
    pub const layer_growth_algo_GROWTH_ALGO_STICKY_DYNAMIC: u32 = 15;
}

#[path = "../../../../scx/scheds/rust/scx_layered/src/layer_core_growth.rs"]
pub mod layer_core_growth;

pub use layer_core_growth::LayerGrowthAlgo;

pub fn algorithm_from_bpf(value: i32) -> Result<LayerGrowthAlgo> {
    Ok(match value {
        0 => LayerGrowthAlgo::Sticky,
        1 => LayerGrowthAlgo::Linear,
        2 => LayerGrowthAlgo::Reverse,
        3 => LayerGrowthAlgo::Random,
        4 => LayerGrowthAlgo::Topo,
        5 => LayerGrowthAlgo::RoundRobin,
        6 => LayerGrowthAlgo::BigLittle,
        7 => LayerGrowthAlgo::LittleBig,
        8 => LayerGrowthAlgo::NodeSpread,
        9 => LayerGrowthAlgo::NodeSpreadReverse,
        10 => LayerGrowthAlgo::NodeSpreadRandom,
        11 => LayerGrowthAlgo::CpuSetSpread,
        12 => LayerGrowthAlgo::CpuSetSpreadReverse,
        13 => LayerGrowthAlgo::CpuSetSpreadRandom,
        14 => LayerGrowthAlgo::RandomTopo,
        15 => LayerGrowthAlgo::StickyDynamic,
        _ => bail!("unknown layer growth algorithm {value}"),
    })
}
