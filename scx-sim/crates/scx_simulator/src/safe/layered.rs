//! Layer specifications for the `scx_layered` scheduler.
//!
//! These are the simulator's stand-in for scx_layered's userspace layer
//! configuration (the `LayerSpec` / `LayerMatch` / `LayerKind` types in
//! `scx_layered/src/lib.rs`, normally parsed from a JSON layer config).
//! [`DynamicScheduler::layered_layers`] publishes them into the BPF
//! `layers[]` array exactly as `main.rs::init_layers()` does.
//!
//! The discriminants below MUST match `enum layer_kind`,
//! `enum layer_match_kind` and `enum layer_growth_algo` in
//! `scx_layered/src/bpf/intf.h`. `tests/layered.rs` asserts that against the
//! values the compiled scheduler `.so` reports, so an upstream reordering
//! fails a test rather than silently mis-configuring every layer.
//!
//! [`DynamicScheduler::layered_layers`]: crate::ffi::DynamicScheduler::layered_layers

use crate::types::{CpuId, TimeNs};

/// How a layer's CPUs relate to other layers' — `enum layer_kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum LayerKind {
    /// May run on any CPU, including CPUs owned by other layers.
    Open = 0,
    /// Owns its CPUs but tasks may spill onto open CPUs.
    Grouped = 1,
    /// Strictly confined to its own CPUs.
    Confined = 2,
}

/// How userspace grows a layer's CPU set — `enum layer_growth_algo`.
///
/// The periodic Tier-3 control loop executes the matching implementation from
/// upstream `layer_core_growth.rs`. Algorithms that require simulator
/// substrate which does not exist (`CpuSetSpread*`, and multi-LLC
/// `StickyDynamic` trading) are rejected rather than approximated.
///
/// [`DynamicScheduler::layered`]: crate::ffi::DynamicScheduler::layered
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum LayerGrowthAlgo {
    /// Prefer CPUs the layer already had.
    Sticky = 0,
    /// Grow from the lowest CPU id upward.
    Linear = 1,
    /// Grow from the highest CPU id downward.
    Reverse = 2,
    /// Random core selection within each node.
    Random = 3,
    /// Grow in topology order (core, then LLC, then node).
    Topo = 4,
    /// Round-robin across LLCs.
    RoundRobin = 5,
    /// Prefer big cores.
    BigLittle = 6,
    /// Prefer little cores.
    LittleBig = 7,
    /// Equal per-node allocation with linear intra-node order.
    NodeSpread = 8,
    /// Equal per-node allocation with reverse intra-node order.
    NodeSpreadReverse = 9,
    /// Equal per-node allocation with random intra-node order.
    NodeSpreadRandom = 10,
    /// Interleave cores across cgroup cpuset domains.
    CpuSetSpread = 11,
    /// Reverse-interleave cores across cgroup cpuset domains.
    CpuSetSpreadReverse = 12,
    /// Randomly interleave cores across cgroup cpuset domains.
    CpuSetSpreadRandom = 13,
    /// Random node, LLC, and core order.
    RandomTopo = 14,
    /// Dynamically trade whole LLCs between layers.
    StickyDynamic = 15,
}

/// One match rule — a variant of scx_layered's `LayerMatch`.
///
/// Only the kinds the simulator can honestly configure are represented. The
/// omitted ones need substrate scxsim does not model: `NsPidEquals` / `NsEquals`
/// (no pid-namespace chain on the simulated `task_struct`), `UsedGpuTid` /
/// `UsedGpuPid` (no GPU), `CgroupRegex` (needs the userspace regex evaluator
/// that populates `cgroup_match_bitmap`), and the `AvgRuntime` /
/// `HintEquals` / `SystemCpuUtilBelow` / `DsqInsertBelow` matchers (driven by
/// userspace-computed EWMAs). Attempting to pass one of those through the FFI
/// is rejected by the C side rather than silently producing a rule that never
/// matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayerMatch {
    /// Task's cgroup path starts with this.
    CgroupPrefix(String),
    /// Task's cgroup path ends with this.
    CgroupSuffix(String),
    /// Task's cgroup path contains this.
    CgroupContains(String),
    /// `p->comm` starts with this (max 15 chars + NUL).
    CommPrefix(String),
    /// The thread-group leader's comm starts with this.
    PcommPrefix(String),
    /// Task nice value is strictly greater than this.
    NiceAbove(i32),
    /// Task nice value is strictly less than this.
    NiceBelow(i32),
    /// Task nice value equals this.
    NiceEquals(i32),
    /// Task's uid equals this.
    UserIdEquals(u32),
    /// Task's gid equals this.
    GroupIdEquals(u32),
    /// Task's pid equals this.
    PidEquals(u32),
    /// Task's parent pid equals this.
    PpidEquals(u32),
    /// Task's tgid equals this.
    TgidEquals(u32),
    /// Task is (or is not) its thread group's leader.
    IsGroupLeader(bool),
    /// Task is (or is not) a kernel thread.
    IsKthread(bool),
    /// Task's affinity is a subset of this NUMA node's CPUs.
    NumaNode(u32),
    /// Inverts the wrapped rule (scx_layered's `exclude` flag).
    Not(Box<LayerMatch>),
}

impl LayerMatch {
    /// Whether this rule is negated (`layer_match.exclude`).
    pub fn exclude(&self) -> bool {
        matches!(self, LayerMatch::Not(_))
    }

    /// The `enum layer_match_kind` value this rule lowers to.
    ///
    /// Exposed so `tests/layered.rs` can compare every variant against the
    /// value the compiled scheduler reports, catching an upstream reordering
    /// of the enum instead of silently mis-configuring layers.
    pub fn match_kind(&self) -> i32 {
        self.to_ffi().0
    }

    /// Lower to the C `layered_add_layer_match()` argument triple:
    /// `(enum layer_match_kind, string arg, integer arg)`.
    pub(crate) fn to_ffi(&self) -> (i32, Option<&str>, i64) {
        match self {
            LayerMatch::Not(inner) => inner.to_ffi(),
            LayerMatch::CgroupPrefix(s) => (MATCH_CGROUP_PREFIX, Some(s), 0),
            LayerMatch::CgroupSuffix(s) => (MATCH_CGROUP_SUFFIX, Some(s), 0),
            LayerMatch::CgroupContains(s) => (MATCH_CGROUP_CONTAINS, Some(s), 0),
            LayerMatch::CommPrefix(s) => (MATCH_COMM_PREFIX, Some(s), 0),
            LayerMatch::PcommPrefix(s) => (MATCH_PCOMM_PREFIX, Some(s), 0),
            LayerMatch::NiceAbove(n) => (MATCH_NICE_ABOVE, None, i64::from(*n)),
            LayerMatch::NiceBelow(n) => (MATCH_NICE_BELOW, None, i64::from(*n)),
            LayerMatch::NiceEquals(n) => (MATCH_NICE_EQUALS, None, i64::from(*n)),
            LayerMatch::UserIdEquals(v) => (MATCH_USER_ID_EQUALS, None, i64::from(*v)),
            LayerMatch::GroupIdEquals(v) => (MATCH_GROUP_ID_EQUALS, None, i64::from(*v)),
            LayerMatch::PidEquals(v) => (MATCH_PID_EQUALS, None, i64::from(*v)),
            LayerMatch::PpidEquals(v) => (MATCH_PPID_EQUALS, None, i64::from(*v)),
            LayerMatch::TgidEquals(v) => (MATCH_TGID_EQUALS, None, i64::from(*v)),
            LayerMatch::IsGroupLeader(b) => (MATCH_IS_GROUP_LEADER, None, i64::from(*b)),
            LayerMatch::IsKthread(b) => (MATCH_IS_KTHREAD, None, i64::from(*b)),
            LayerMatch::NumaNode(v) => (MATCH_NUMA_NODE, None, i64::from(*v)),
        }
    }
}

// `enum layer_match_kind` values from scx_layered/src/bpf/intf.h. Verified
// against the compiled `.so` by `tests/layered.rs::layer_enum_abi_matches_bpf`.
pub(crate) const MATCH_CGROUP_PREFIX: i32 = 0;
pub(crate) const MATCH_COMM_PREFIX: i32 = 1;
pub(crate) const MATCH_PCOMM_PREFIX: i32 = 2;
pub(crate) const MATCH_NICE_ABOVE: i32 = 3;
pub(crate) const MATCH_NICE_BELOW: i32 = 4;
pub(crate) const MATCH_NICE_EQUALS: i32 = 5;
pub(crate) const MATCH_USER_ID_EQUALS: i32 = 6;
pub(crate) const MATCH_GROUP_ID_EQUALS: i32 = 7;
pub(crate) const MATCH_PID_EQUALS: i32 = 8;
pub(crate) const MATCH_PPID_EQUALS: i32 = 9;
pub(crate) const MATCH_TGID_EQUALS: i32 = 10;
pub(crate) const MATCH_IS_GROUP_LEADER: i32 = 14;
pub(crate) const MATCH_IS_KTHREAD: i32 = 15;
pub(crate) const MATCH_CGROUP_SUFFIX: i32 = 19;
pub(crate) const MATCH_CGROUP_CONTAINS: i32 = 20;
pub(crate) const MATCH_NUMA_NODE: i32 = 25;

/// scx_layered's `DEFAULT_LAYER_WEIGHT`.
pub const DEFAULT_LAYER_WEIGHT: u32 = 100;

/// scx_layered's `default_xnuma_threshold()` (`scx_layered/src/config.rs`).
pub const DEFAULT_XNUMA_THRESHOLD: (f64, f64) = (0.6, 0.7);

/// scx_layered's `default_xnuma_threshold_delta()` (`scx_layered/src/config.rs`).
pub const DEFAULT_XNUMA_THRESHOLD_DELTA: (f64, f64) = (0.2, 0.3);

/// One layer of an scx_layered configuration.
///
/// Build with [`LayerSpec::new`] and the chaining setters; the defaults match
/// scx_layered's own `LayerCommon` defaults (weight 100, inherit the global
/// slice, linear growth, no preemption, not exclusive).
#[derive(Debug, Clone, PartialEq)]
pub struct LayerSpec {
    /// Layer name, as it appears in `ops.dump` output.
    pub name: String,
    /// Open / grouped / confined.
    pub kind: LayerKind,
    /// Tasks in this layer preempt lower-priority layers.
    pub preempt: bool,
    /// Try preemption before looking for an idle CPU.
    pub preempt_first: bool,
    /// Never co-schedule with an SMT sibling from another layer.
    pub exclusive: bool,
    /// Protected layers are not preemptible by non-members.
    pub protected: bool,
    /// Relative weight, used for the iteration order and the static CPU split.
    pub weight: u32,
    /// Desired per-CPU utilization range used by scx_layered's userspace
    /// reallocation loop. Required for grouped/confined layers when the loop
    /// is enabled; open layers do not participate in allocation.
    pub util_range: Option<(f64, f64)>,
    /// Optional hard minimum/maximum CPU count for the userspace loop.
    pub cpus_range: Option<(usize, usize)>,
    /// Include time spent running on open CPUs when sizing this grouped layer.
    pub util_includes_open_cputime: bool,
    /// Per-layer slice; 0 inherits the global `slice_ns`.
    pub slice_ns: TimeNs,
    /// Minimum execution time before a task can be preempted.
    pub min_exec_ns: TimeNs,
    /// Maximum execution time before the layer forces a re-dispatch; 0
    /// inherits the global `max_exec_ns`.
    pub max_exec_ns: TimeNs,
    /// Growth algorithm published into `layer->growth_algo`.
    pub growth_algo: LayerGrowthAlgo,
    /// Preferred NUMA nodes for topology-aware growth. The engine models the
    /// node partition and charges a flat cross-node migration penalty, but
    /// no memory placement and no distance matrix — see
    /// [`MachineTopology`](crate::topology::MachineTopology) for the full
    /// list of what a node does and does not mean here.
    pub nodes: Vec<usize>,
    /// Preferred LLCs for topology-aware growth.
    pub llcs: Vec<usize>,
    /// Match rules, as a list of OR groups whose members are ANDed.
    /// An empty outer list — or a single empty inner group — is the catch-all
    /// that matches every task.
    pub matches: Vec<Vec<LayerMatch>>,
    /// Explicit CPU set. `None` lets the wrapper allocate: every CPU for an
    /// open layer, a contiguous weight-proportional slice otherwise.
    pub cpus: Option<Vec<CpuId>>,
    /// Cross-NUMA migration gate, `(close, open)` load-ratio hysteresis.
    ///
    /// Upstream `LayerCommon::xnuma_threshold`, default `(0.6, 0.7)`
    /// (`scx_layered/src/config.rs::default_xnuma_threshold`). Setting BOTH
    /// components `<= 0.0` turns gating OFF — upstream then publishes an
    /// infinite budget in every direction, so cross-node placement and
    /// cross-node DSQ consumption are unrestricted.
    ///
    /// This only has an effect once the userspace control loop is enabled
    /// ([`crate::DynamicScheduler::layered_enable_control_loop`]): upstream
    /// writes the gate from `refresh_xnuma()` every control iteration and
    /// from nowhere else. See [`crate::layered_xnuma`].
    pub xnuma_threshold: (f64, f64),
    /// Cross-NUMA migration gate, `(close, open)` surplus-ratio hysteresis.
    ///
    /// Upstream `LayerCommon::xnuma_threshold_delta`, default `(0.2, 0.3)`.
    pub xnuma_threshold_delta: (f64, f64),
}

impl LayerSpec {
    /// A layer named `name` of the given kind, with scx_layered's defaults.
    pub fn new(name: impl Into<String>, kind: LayerKind) -> Self {
        Self {
            name: name.into(),
            kind,
            preempt: false,
            preempt_first: false,
            exclusive: false,
            protected: false,
            weight: DEFAULT_LAYER_WEIGHT,
            util_range: None,
            cpus_range: None,
            util_includes_open_cputime: false,
            slice_ns: 0,
            min_exec_ns: 0,
            max_exec_ns: 0,
            growth_algo: LayerGrowthAlgo::Linear,
            nodes: Vec::new(),
            llcs: Vec::new(),
            matches: Vec::new(),
            cpus: None,
            xnuma_threshold: DEFAULT_XNUMA_THRESHOLD,
            xnuma_threshold_delta: DEFAULT_XNUMA_THRESHOLD_DELTA,
        }
    }

    /// A catch-all open layer. scx_layered configs conventionally end with
    /// one, because a task that matches no layer is a fatal error
    /// (`maybe_refresh_layer` calls `scx_bpf_error`).
    pub fn catch_all(name: impl Into<String>) -> Self {
        Self::new(name, LayerKind::Open).with_or(Vec::new())
    }

    /// Append an OR group. Rules within `ands` must all match.
    pub fn with_or(mut self, ands: Vec<LayerMatch>) -> Self {
        self.matches.push(ands);
        self
    }

    /// Append an OR group holding a single rule.
    pub fn with_match(self, m: LayerMatch) -> Self {
        self.with_or(vec![m])
    }

    /// Set the layer weight.
    pub fn with_weight(mut self, weight: u32) -> Self {
        self.weight = weight;
        self
    }

    /// Configure the utilization band used by the periodic CPU allocator.
    pub fn with_util_range(mut self, low: f64, high: f64) -> Self {
        assert!(
            low >= 0.0 && low < high,
            "invalid util range ({low}, {high})"
        );
        self.util_range = Some((low, high));
        self
    }

    /// Clamp the periodic allocator to `min..=max` CPUs.
    pub fn with_cpus_range(mut self, min: usize, max: usize) -> Self {
        assert!(min <= max, "invalid CPU range ({min}, {max})");
        self.cpus_range = Some((min, max));
        self
    }

    /// Size a grouped layer from owned plus open CPU time, as production can.
    pub fn with_open_cputime(mut self, include: bool) -> Self {
        self.util_includes_open_cputime = include;
        self
    }

    /// Select the real upstream core-growth algorithm.
    pub fn with_growth_algo(mut self, growth_algo: LayerGrowthAlgo) -> Self {
        self.growth_algo = growth_algo;
        self
    }

    /// Prefer or restrict growth to the listed harness NUMA-node groups.
    pub fn with_nodes(mut self, nodes: Vec<usize>) -> Self {
        self.nodes = nodes;
        self
    }

    /// Prefer the listed LLC ids for topology-aware growth.
    pub fn with_llcs(mut self, llcs: Vec<usize>) -> Self {
        self.llcs = llcs;
        self
    }

    /// Mark the layer preempting.
    pub fn with_preempt(mut self, preempt: bool) -> Self {
        self.preempt = preempt;
        self
    }

    /// Mark the layer exclusive (no cross-layer SMT sibling sharing).
    pub fn with_exclusive(mut self, exclusive: bool) -> Self {
        self.exclusive = exclusive;
        self
    }

    /// Override the per-layer slice.
    pub fn with_slice_ns(mut self, slice_ns: TimeNs) -> Self {
        self.slice_ns = slice_ns;
        self
    }

    /// Pin the layer to an explicit CPU set instead of auto-allocating.
    pub fn with_cpus(mut self, cpus: Vec<CpuId>) -> Self {
        self.cpus = Some(cpus);
        self
    }

    /// Override the cross-NUMA migration gate's hysteresis bands.
    ///
    /// `threshold` is the `(close, open)` pair on load/alloc and
    /// `threshold_delta` the `(close, open)` pair on surplus/alloc; see
    /// [`LayerSpec::xnuma_threshold`]. Passing `(0.0, 0.0)` for `threshold`
    /// selects upstream's "gating off" branch: infinite budget in every
    /// direction, which is how a config asks for unrestricted cross-node
    /// migration.
    pub fn with_xnuma_threshold(
        mut self,
        threshold: (f64, f64),
        threshold_delta: (f64, f64),
    ) -> Self {
        self.xnuma_threshold = threshold;
        self.xnuma_threshold_delta = threshold_delta;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catch_all_has_one_empty_or_group() {
        let spec = LayerSpec::catch_all("default");
        assert_eq!(spec.kind, LayerKind::Open);
        assert_eq!(spec.matches, vec![Vec::<LayerMatch>::new()]);
    }

    #[test]
    fn not_inverts_without_changing_the_kind() {
        let inner = LayerMatch::CommPrefix("bench".into());
        let negated = LayerMatch::Not(Box::new(inner.clone()));
        assert!(!inner.exclude());
        assert!(negated.exclude());
        assert_eq!(inner.to_ffi().0, negated.to_ffi().0);
        assert_eq!(inner.to_ffi().1, negated.to_ffi().1);
    }

    #[test]
    fn builder_defaults_match_scx_layered() {
        let spec = LayerSpec::new("l", LayerKind::Grouped);
        assert_eq!(spec.weight, DEFAULT_LAYER_WEIGHT);
        assert!(!spec.preempt);
        assert!(!spec.exclusive);
        assert_eq!(spec.slice_ns, 0);
        assert_eq!(spec.growth_algo, LayerGrowthAlgo::Linear);
        assert!(spec.cpus.is_none());
    }
}
