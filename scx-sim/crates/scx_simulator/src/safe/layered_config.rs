//! Loading a real `scx_layered` JSON layer configuration.
//!
//! In production scx_layered is configured by a JSON file — the format
//! `scx_layered/src/config.rs` defines and `scx_layered f:<path>` consumes
//! (a POSITIONAL argument; there is no `--spec` flag) — listing layers,
//! the rules that select tasks into them, and per-layer policy. This module
//! reads *that* format and lowers it to [`LayerSpec`], so a configuration
//! written for the real scheduler runs unmodified under scxsim.
//!
//! The schema is not ours and is not re-invented here: every struct below
//! mirrors an upstream one field for field, at the pinned scx submodule. Where
//! this file names a derivation (`slice_us` scaling, the `yield_ignore`
//! formula, the preempt override on `disallow_*`), it is quoting
//! `main.rs::init_layers()` and says so.
//!
//! # Nothing is dropped quietly
//!
//! A parser that accepts a config and ignores the fields it does not
//! understand produces a run that *looks* like the production configuration
//! and is not one — the more dangerous outcome, because nothing in the results
//! says so. So the mirror types below spell out **every** upstream field,
//! including the ones scxsim cannot honour, and each of those is refused **by
//! name** with the reason. See [`Unsupported`].
//!
//! Three dispositions, not two ([`Disposition`]):
//!
//! - **Refused, waivable.** scxsim cannot deliver it. The load fails naming
//!   the field; a caller who accepts the loss can waive it explicitly, and the
//!   waived set is reported back so the run can say what it dropped.
//! - **Refused, not waivable.** Match kinds. Dropping a match rule does not
//!   reduce fidelity, it changes *which tasks the layer captures* — an AND
//!   group with a term removed matches strictly more tasks. There is no
//!   version of that which is a smaller lie, so no waiver is offered.
//! - **Deprecated upstream.** scx_layered itself warns and ignores these, in
//!   the config-load loop in its `fn main` (the block beginning
//!   `for spec in layer_config.specs.iter_mut()`; there is no
//!   `validate_layer_specs` function, only `verify_layer_specs`, which does
//!   the structural checks). Mirroring that is fidelity; refusing would be
//!   the divergence.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

use serde::Deserialize;

use crate::layered::{
    LayerGrowthAlgo, LayerKind, LayerMatch, LayerPlacement, LayerSpec, DEFAULT_XNUMA_THRESHOLD,
    DEFAULT_XNUMA_THRESHOLD_DELTA, DISALLOW_AFTER_NEVER,
};
use crate::types::TimeNs;

// ---------------------------------------------------------------------------
// What scxsim cannot honour, and why
// ---------------------------------------------------------------------------

/// How the loader treats one upstream schema item it does not apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Refused; a caller may waive it and accept the loss.
    Refusable,
    /// Refused with no waiver offered — waiving would change which tasks the
    /// layer captures rather than merely losing fidelity.
    Fatal,
    /// scx_layered itself deprecates and ignores this. Mirroring that
    /// behaviour is what fidelity means here.
    DeprecatedUpstream,
}

/// Generate [`Unsupported`] and its lookup tables from one list, so the
/// variant set, the JSON names, the dispositions and the reasons cannot drift
/// apart.
macro_rules! unsupported_items {
    ($( $variant:ident => $name:literal, $disp:ident, $reason:literal; )*) => {
        /// An item in upstream's layer-config schema that scxsim does not
        /// apply, with the reason.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
        pub enum Unsupported {
            $( #[doc = $reason] $variant, )*
        }

        impl Unsupported {
            /// Every item, so `--help` and the error text can enumerate them.
            pub const ALL: &'static [Unsupported] = &[ $( Unsupported::$variant, )* ];

            /// The name this appears under in an upstream JSON config: a
            /// `LayerCommon` / `LayerKind` field name, or a `LayerMatch`
            /// variant name.
            pub fn json_name(self) -> &'static str {
                match self { $( Self::$variant => $name, )* }
            }

            /// How the loader treats it.
            pub fn disposition(self) -> Disposition {
                match self { $( Self::$variant => Disposition::$disp, )* }
            }

            /// Why scxsim cannot apply it.
            pub fn reason(self) -> &'static str {
                match self { $( Self::$variant => $reason, )* }
            }

            /// Look one up by the name a caller would waive it under.
            pub fn from_json_name(name: &str) -> Option<Self> {
                Self::ALL.iter().copied().find(|u| u.json_name() == name)
            }
        }
    };
}

unsupported_items! {
    // -- match kinds: Fatal, never waivable --
    MatchCgroupRegex => "CgroupRegex", Fatal,
        "needs the userspace regex evaluator that fills cgroup_match_bitmap; \
         scxsim runs no scx_layered userspace daemon";
    MatchNsPidEquals => "NSPIDEquals", Fatal,
        "the simulated task_struct has no pid-namespace chain";
    MatchNsEquals => "NSEquals", Fatal,
        "the simulated task_struct has no pid-namespace chain";
    MatchCmdJoin => "CmdJoin", Fatal,
        "compares against taskc->join_layer, filled by the scxcmd userspace \
         channel, which scxsim does not provide";
    MatchUsedGpuTid => "UsedGpuTid", Fatal,
        "scxsim models CPU time, not accelerators: nothing delivers the \
         kprobe/nvidia_* events that populate the gpu_tid map";
    MatchUsedGpuPid => "UsedGpuPid", Fatal,
        "scxsim models CPU time, not accelerators: nothing delivers the \
         kprobe/nvidia_* events that populate the gpu_tgid map";
    MatchHintEquals => "HintEquals", Fatal,
        "the hint value is published by scx_layered's userspace daemon";
    MatchSystemCpuUtilBelow => "SystemCpuUtilBelow", Fatal,
        "the utilisation figure is computed by scx_layered's userspace daemon";
    MatchDsqInsertBelow => "DsqInsertBelow", Fatal,
        "the insert rate is computed by scx_layered's userspace daemon";

    // -- template expands into MATCH RULES, so it belongs with the match
    //    kinds rather than the fields: waiving it would not lose fidelity, it
    //    would collapse N per-cgroup layers into one that captures a
    //    different set of tasks.
    FieldTemplate => "template", Fatal,
        "expands against the HOST's /sys/fs/cgroup (main.rs::expand_template); \
         under scxsim the cgroup tree is the simulated one, so the expansion \
         would enumerate the wrong machine";
    GrowthCpuSetSpread => "growth_algo:CpuSetSpread*", Fatal,
        "interleaves across cgroup cpuset domains, which scxsim's CPU \
         allocator does not model; publishing it would leave the BPF with a \
         growth_algo its own Big/Little idle selection does not recognise";
    GrowthStickyDynamic => "growth_algo:StickyDynamic", Fatal,
        "trades whole LLCs between layers from the userspace allocator, which \
         scxsim does not implement";

    // -- layer fields: Refusable --
    FieldIdleResumeUs => "idle_resume_us", Refusable,
        "scxsim models no cpuidle governor, so there is no resume-latency QoS \
         to set";
    FieldMembwGb => "membw_gb", Refusable,
        "scxsim models CPU time only; there is no memory-bandwidth accounting \
         to enforce a limit against";
    FieldUtilPeakHalfLifeMs => "util_peak_half_life_ms", Refusable,
        "read only by scx_layered's userspace CPU allocator, and scxsim's \
         control loop does not implement peak-hold sizing";
    // -- deprecated upstream: warn and ignore, exactly as scx_layered does --
    FieldAllowNodeAligned => "allow_node_aligned", DeprecatedUpstream,
        "deprecated in scx_layered: node-aligned tasks are always dispatched \
         on layer DSQs now, and the flag is ignored with a warning";
    FieldIdleSmt => "idle_smt", DeprecatedUpstream,
        "deprecated in scx_layered, which ignores it with a warning";
}

/// Which upstream `LayerKind` variant owns which non-common field.
///
/// A key that misses every flattened field lands in `CommonJson::unknown`, and
/// this is what tells "typo" apart from "real field, written under the wrong
/// layer kind" — upstream drops the second silently, and refusing it would
/// stop configs loading that the real scheduler loads.
const KIND_FIELDS: [(&str, &[&str]); 3] = [
    (
        "Confined",
        &[
            "util_range",
            "cpus_range",
            "cpus_range_frac",
            "membw_gb",
            "protected",
        ],
    ),
    (
        "Grouped",
        &[
            "util_range",
            "util_includes_open_cputime",
            "cpus_range",
            "cpus_range_frac",
            "membw_gb",
            "protected",
            "idle_confined",
        ],
    ),
    ("Open", &[]),
];

/// One place a config asked for something the loader does not apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    /// Where it appeared, e.g. `layers[2] "batch": kind.Grouped.perf`.
    pub site: String,
    /// What was asked for.
    pub item: Unsupported,
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: {} — {}",
            self.site,
            self.item.json_name(),
            self.item.reason()
        )
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why a layer config could not be loaded.
#[derive(Debug)]
pub enum LayerConfigError {
    /// The file could not be read.
    Read {
        path: String,
        source: std::io::Error,
    },
    /// The JSON did not match upstream's schema. An unrecognised key lands
    /// here too — see [`UnknownKeys`](LayerConfigError::UnknownKeys) for keys
    /// inside a layer kind, which serde's flattening cannot reject for us.
    Json(serde_json::Error),
    /// Keys upstream's schema has no field for.
    UnknownKeys(Vec<String>),
    /// The config is structurally invalid in a way upstream also rejects.
    Invalid(String),
    /// The config asked for things scxsim does not apply.
    Unsupported(Vec<Rejection>),
    /// Both of the above at once.
    Both {
        unknown: Vec<String>,
        unsupported: Vec<Rejection>,
    },
}

impl fmt::Display for LayerConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(f, "failed to read layer config {path}: {source}")
            }
            Self::Json(e) => write!(f, "failed to parse layer config: {e}"),
            Self::UnknownKeys(keys) => {
                writeln!(
                    f,
                    "layer config has {} key(s) that no layer kind in scxsim's mirror \
                     of scx_layered's schema has:",
                    keys.len()
                )?;
                for k in keys {
                    writeln!(f, "  {k}")?;
                }
                write!(
                    f,
                    "Either a typo — in which case nothing would have applied it — or a \
                     field scx_layered gained since the mirror in \
                     safe/layered_config.rs was written against the pinned submodule. \
                     Both want a look; neither is safe to ignore."
                )
            }
            Self::Invalid(msg) => write!(f, "invalid layer config: {msg}"),
            Self::Both {
                unknown,
                unsupported,
            } => {
                write!(
                    f,
                    "{}\n{}",
                    Self::UnknownKeys(unknown.clone()),
                    Self::Unsupported(unsupported.clone())
                )
            }
            Self::Unsupported(rejections) => {
                writeln!(
                    f,
                    "layer config asks for {} thing(s) scxsim does not apply:",
                    rejections.len()
                )?;
                for r in rejections {
                    writeln!(f, "  {r}")?;
                }
                let waivable: Vec<&str> = rejections
                    .iter()
                    .filter(|r| r.item.disposition() == Disposition::Refusable)
                    .map(|r| r.item.json_name())
                    .collect();
                let fatal = rejections.len() - waivable.len();
                if waivable.is_empty() {
                    write!(
                        f,
                        "None of these can be waived: dropping a match rule does not \
                         lose fidelity, it changes which tasks the layer captures."
                    )
                } else {
                    write!(
                        f,
                        "Waivable with --layer-config-drop, accepting that they are NOT \
                         applied: {}{}",
                        waivable.join(","),
                        if fatal > 0 {
                            format!(
                                ". The other {fatal} cannot be waived — dropping a match \
                                 rule changes which tasks the layer captures."
                            )
                        } else {
                            String::new()
                        }
                    )
                }
            }
        }
    }
}

impl std::error::Error for LayerConfigError {}

// ---------------------------------------------------------------------------
// Load options and result
// ---------------------------------------------------------------------------

/// Machine facts and caller decisions the loader needs.
#[derive(Debug, Clone)]
pub struct LayerConfigOptions {
    /// CPU count, used exactly as upstream's `resolve_cpus_pct_range()` uses
    /// `topo.all_cpus.len()` to turn `cpus_range_frac` into a CPU count.
    pub nr_cpus: u32,
    /// `MAX_LAYERS` as the loaded scheduler reports it.
    pub max_layers: usize,
    /// `MAX_LAYER_MATCH_ORS` as the loaded scheduler reports it.
    pub max_match_ors: usize,
    /// `NR_LAYER_MATCH_KINDS` as the loaded scheduler reports it — the AND
    /// capacity of one OR group.
    pub max_match_ands: usize,
    /// `MIN_LAYER_WEIGHT` / `MAX_LAYER_WEIGHT` as the loaded scheduler reports
    /// them. scx_layered clamps to this band before publishing, so
    /// reproducing the clamp is what keeps the published weight identical.
    pub weight_range: (u32, u32),
    /// `DEFAULT_LAYER_WEIGHT`, substituted for an unset or zero weight.
    pub default_weight: u32,
    /// NUMA nodes the scheduler was given, so a `NumaNode(n)` rule naming a
    /// node that does not exist is rejected instead of silently never firing.
    pub nr_nodes: u32,
    /// `MAX_COMM` and `MAX_PATH`. A needle longer than its buffer is
    /// truncated by the wrapper, which silently WIDENS the rule, so it is
    /// rejected here as upstream's `verify_layer_specs()` rejects it.
    pub max_comm: usize,
    /// See `max_comm`.
    pub max_path: usize,
    /// The default slice, applied to layers that do not set `slice_us`. This
    /// is scx_layered's `--slice-us`; upstream substitutes it at config-load
    /// time, so a layer's published `slice_ns` is never zero.
    pub default_slice_ns: TimeNs,
    /// `SCX_SLICE_DFL`, which is NOT the same thing as `default_slice_ns`.
    ///
    /// scx_layered's `DFL_DISALLOW_OPEN_AFTER_US` and
    /// `DFL_DISALLOW_PREEMPT_AFTER_US` are `2 *` and `4 * SCX_SLICE_DFL /
    /// 1000` — a FIXED pair, substituted for every non-preempting layer that
    /// leaves them unset, regardless of that layer's own `slice_us`. Deriving
    /// them from the layer slice instead is wrong by the ratio between the
    /// two, in either direction.
    pub scx_slice_dfl_ns: TimeNs,
    /// Fields the caller has explicitly accepted losing.
    pub waived: BTreeSet<Unsupported>,
}

impl LayerConfigOptions {
    /// Options for `nr_cpus` CPUs with the capacities scx_layered is built
    /// with at the pinned submodule.
    ///
    /// Prefer [`LayerConfigOptions::from_probes`] wherever a scheduler is
    /// loaded: these constants are a copy of the scheduler's, and a copy can
    /// go stale across an scx bump. This exists for the pure-parser tests,
    /// which have no `.so`.
    pub fn new(nr_cpus: u32) -> Self {
        Self {
            nr_cpus,
            max_layers: 16,
            max_match_ors: 32,
            max_match_ands: 26,
            weight_range: (1, 10_000),
            default_weight: crate::layered::DEFAULT_LAYER_WEIGHT,
            nr_nodes: 1,
            max_comm: 16,
            max_path: 4096,
            default_slice_ns: 20_000_000,
            scx_slice_dfl_ns: 20_000_000,
            waived: BTreeSet::new(),
        }
    }

    /// Waive the named items, accepting that they will not be applied.
    ///
    /// Returns the names that are not waivable, so a caller can tell the
    /// difference between "you named a field that does not exist" and "that
    /// one is not offered".
    pub fn waive(&mut self, names: &[&str]) -> Result<(), Vec<String>> {
        let mut bad = Vec::new();
        for name in names {
            match Unsupported::from_json_name(name) {
                Some(u) if u.disposition() == Disposition::Refusable => {
                    self.waived.insert(u);
                }
                Some(u) => bad.push(format!(
                    "{name}: not waivable — {}",
                    if u.disposition() == Disposition::Fatal {
                        "dropping a match rule changes which tasks the layer captures"
                    } else {
                        "scx_layered ignores it too, so there is nothing to waive"
                    }
                )),
                None => bad.push(format!("{name}: not a refusable scx_layered config field")),
            }
        }
        if bad.is_empty() {
            Ok(())
        } else {
            Err(bad)
        }
    }
}

/// A loaded layer config, plus what the loader did not apply.
#[derive(Debug, Clone)]
pub struct LoadedLayerConfig {
    /// The layers, ready for `DynamicScheduler::layered_layers()`.
    pub specs: Vec<LayerSpec>,
    /// Fields the caller waived, and which are therefore NOT in `specs`.
    /// Surface these in run output: a result produced with fields dropped is
    /// not a result for the configuration as written.
    pub waived: Vec<Rejection>,
    /// Fields scx_layered itself deprecates and ignores. Reported for the
    /// same reason upstream logs a warning, not because scxsim differs.
    pub deprecated: Vec<Rejection>,
    /// Everything else worth saying about the config that is not a refusal:
    /// a key written under a layer kind that has no such field, a layer that
    /// shadows every layer after it. None of these is refused, because
    /// scx_layered loads such configs — but none is silent either.
    pub notes: Vec<String>,
}

impl LoadedLayerConfig {
    /// One line per thing that was not applied, for a run banner. Empty when
    /// the configuration was applied in full.
    pub fn caveats(&self) -> impl Iterator<Item = String> + '_ {
        self.waived
            .iter()
            .map(|r| format!("DROPPED (waived): {r}"))
            .chain(
                self.deprecated
                    .iter()
                    .map(|r| format!("ignored, as scx_layered also ignores it: {r}")),
            )
            .chain(self.notes.iter().cloned())
    }
}

/// Read and lower a layer config file.
pub fn load_layer_config(
    path: &Path,
    opts: &LayerConfigOptions,
) -> Result<LoadedLayerConfig, LayerConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| LayerConfigError::Read {
        path: path.display().to_string(),
        source,
    })?;
    parse_layer_config(&text, opts)
}

/// Lower a layer config already in memory.
pub fn parse_layer_config(
    json: &str,
    opts: &LayerConfigOptions,
) -> Result<LoadedLayerConfig, LayerConfigError> {
    let specs: Vec<LayerSpecJson> = serde_json::from_str(json).map_err(LayerConfigError::Json)?;
    lower(specs, opts)
}

// ---------------------------------------------------------------------------
// The mirror of upstream's schema
//
// Field for field with scx_layered/src/config.rs. Fields scxsim does not
// apply are `Option<T>` even where upstream defaults them, so that "absent"
// and "explicitly set" stay distinguishable: a config that never mentions
// `perf` should load, and one that asks for `perf: 1024` should not.
// ---------------------------------------------------------------------------

/// Upstream `LayerSpec`. `cpuset` is absent because upstream marks it
/// `#[serde(skip)]`, so it never appears in a config file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LayerSpecJson {
    name: String,
    #[serde(default)]
    comment: Option<String>,
    #[serde(default)]
    template: Option<serde_json::Value>,
    matches: Vec<Vec<LayerMatchJson>>,
    kind: LayerKindJson,
}

/// Upstream `LayerMatch`, with upstream's variant spellings.
///
/// The payloads of the refused variants are never read, and must stay anyway:
/// a unit variant `CgroupRegex` accepts different JSON from a newtype variant
/// `CgroupRegex(String)`. Dropping them to silence the warning would make a
/// config using them fail as a *parse* error, losing the named refusal that is
/// the point of this module.
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
enum LayerMatchJson {
    CgroupPrefix(String),
    CgroupSuffix(String),
    CgroupContains(String),
    CgroupRegex(String),
    CommPrefix(String),
    CommPrefixExclude(String),
    PcommPrefix(String),
    PcommPrefixExclude(String),
    NiceAbove(i32),
    NiceBelow(i32),
    NiceEquals(i32),
    UIDEquals(u32),
    GIDEquals(u32),
    PIDEquals(u32),
    PPIDEquals(u32),
    TGIDEquals(u32),
    NSPIDEquals(u64, u32),
    NSEquals(u32),
    CmdJoin(String),
    IsGroupLeader(bool),
    IsKthread(bool),
    UsedGpuTid(bool),
    UsedGpuPid(bool),
    AvgRuntime(u64, u64),
    HintEquals(u64),
    SystemCpuUtilBelow(f64),
    DsqInsertBelow(f64),
    NumaNode(u32),
}

/// Upstream `LayerKind`. Upstream uses struct variants with a flattened
/// `LayerCommon`; a newtype variant over a struct that flattens the same
/// `LayerCommon` accepts byte-identical JSON and lets the inner struct carry
/// its own unknown-key capture.
#[derive(Debug, Deserialize)]
enum LayerKindJson {
    Confined(ConfinedJson),
    Grouped(GroupedJson),
    Open(OpenJson),
}

impl LayerKindJson {
    /// The flattened `LayerCommon` every kind carries.
    fn common(&self) -> &CommonJson {
        match self {
            LayerKindJson::Confined(c) => &c.common,
            LayerKindJson::Grouped(g) => &g.common,
            LayerKindJson::Open(o) => &o.common,
        }
    }

    /// The variant name, for messages that must name the kind a key was
    /// written under.
    fn name(&self) -> &'static str {
        match self {
            LayerKindJson::Confined(_) => "Confined",
            LayerKindJson::Grouped(_) => "Grouped",
            LayerKindJson::Open(_) => "Open",
        }
    }
}

#[derive(Debug, Deserialize)]
struct ConfinedJson {
    util_range: (f64, f64),
    #[serde(default)]
    cpus_range: Option<(usize, usize)>,
    #[serde(default)]
    cpus_range_frac: Option<(f64, f64)>,
    #[serde(default)]
    membw_gb: Option<f64>,
    #[serde(default)]
    protected: bool,
    #[serde(flatten)]
    common: CommonJson,
}

#[derive(Debug, Deserialize)]
struct GroupedJson {
    util_range: (f64, f64),
    #[serde(default)]
    util_includes_open_cputime: bool,
    #[serde(default)]
    cpus_range: Option<(usize, usize)>,
    #[serde(default)]
    cpus_range_frac: Option<(f64, f64)>,
    #[serde(default)]
    membw_gb: Option<f64>,
    #[serde(default)]
    protected: bool,
    #[serde(default)]
    idle_confined: bool,
    #[serde(flatten)]
    common: CommonJson,
}

#[derive(Debug, Deserialize)]
struct OpenJson {
    #[serde(flatten)]
    common: CommonJson,
}

/// Upstream `LayerCommon`.
#[derive(Debug, Deserialize)]
struct CommonJson {
    #[serde(default)]
    min_exec_us: u64,
    #[serde(default)]
    yield_ignore: f64,
    #[serde(default)]
    slice_us: u64,
    #[serde(default)]
    fifo: bool,
    #[serde(default)]
    preempt: bool,
    #[serde(default)]
    preempt_first: bool,
    #[serde(default)]
    exclusive: bool,
    #[serde(default)]
    allow_node_aligned: Option<bool>,
    #[serde(default)]
    skip_remote_node: bool,
    #[serde(default)]
    prev_over_idle_core: bool,
    #[serde(default)]
    weight: u32,
    #[serde(default)]
    disallow_open_after_us: Option<u64>,
    #[serde(default)]
    disallow_preempt_after_us: Option<u64>,
    #[serde(default)]
    xllc_mig_min_us: f64,
    #[serde(default)]
    idle_smt: Option<bool>,
    #[serde(default)]
    growth_algo: Option<LayerGrowthAlgo>,
    #[serde(default)]
    util_peak_half_life_ms: Option<u64>,
    #[serde(default)]
    perf: Option<u64>,
    #[serde(default)]
    idle_resume_us: Option<u32>,
    #[serde(default)]
    nodes: Vec<usize>,
    #[serde(default)]
    llcs: Vec<usize>,
    #[serde(default)]
    placement: Option<LayerPlacementJson>,
    #[serde(default)]
    member_expire_ms: u64,
    #[serde(default)]
    xnuma_threshold: Option<(f64, f64)>,
    #[serde(default)]
    xnuma_threshold_delta: Option<(f64, f64)>,
    /// Anything the flattened fields above did not claim.
    ///
    /// `#[serde(deny_unknown_fields)]` is unavailable on a struct that
    /// flattens, so unknown keys inside a layer kind are collected here and
    /// rejected in [`lower`] instead of by serde.
    #[serde(flatten)]
    unknown: BTreeMap<String, serde_json::Value>,
}

/// Upstream `LayerPlacement`.
#[derive(Debug, Clone, Copy, Deserialize)]
enum LayerPlacementJson {
    Standard,
    Sticky,
    Floating,
}

impl From<LayerPlacementJson> for LayerPlacement {
    fn from(p: LayerPlacementJson) -> Self {
        match p {
            LayerPlacementJson::Standard => LayerPlacement::Standard,
            LayerPlacementJson::Sticky => LayerPlacement::Sticky,
            LayerPlacementJson::Floating => LayerPlacement::Floating,
        }
    }
}

// ---------------------------------------------------------------------------
// Lowering
// ---------------------------------------------------------------------------

/// Accumulates everything wrong with a config so one load reports all of it.
///
/// Reporting the first problem and stopping turns a ten-field config into ten
/// edit-and-retry cycles, and hides how much of it scxsim cannot run.
#[derive(Default)]
struct Findings {
    unknown_keys: Vec<String>,
    rejected: Vec<Rejection>,
    waived: Vec<Rejection>,
    deprecated: Vec<Rejection>,
    /// Fully-phrased notes about things that are neither a refusal nor an
    /// upstream deprecation — a key written under the wrong layer kind, a
    /// layer that shadows the ones after it.
    notes: Vec<String>,
}

impl Findings {
    /// Record one item, routing it by disposition and by whether the caller
    /// waived it.
    fn note(&mut self, site: &str, item: Unsupported, opts: &LayerConfigOptions) {
        let r = Rejection {
            site: site.to_string(),
            item,
        };
        match item.disposition() {
            Disposition::DeprecatedUpstream => self.deprecated.push(r),
            Disposition::Refusable if opts.waived.contains(&item) => self.waived.push(r),
            _ => self.rejected.push(r),
        }
    }
}

fn lower(
    json_specs: Vec<LayerSpecJson>,
    opts: &LayerConfigOptions,
) -> Result<LoadedLayerConfig, LayerConfigError> {
    // Structural checks first, and in upstream's order: a config that would
    // not start the real scheduler should not start ours either.
    check_shape(&json_specs, opts)?;

    let mut found = Findings::default();
    let mut specs = Vec::with_capacity(json_specs.len());
    let last = json_specs.len() - 1;
    for (idx, js) in json_specs.iter().enumerate() {
        let site = format!("layers[{idx}] {:?}", js.name);
        // A catch-all before the end is legal — upstream's check is only that
        // a non-terminal layer has SOME OR group, and `[[]]` has one. It is
        // also almost certainly a mistake, because maybe_refresh_layer() scans
        // in order and stops at the first match, so nothing after it can ever
        // win. Say so; do not refuse a config the real scheduler runs.
        if idx < last && js.matches.iter().any(|ands| ands.is_empty()) {
            found.notes.push(format!(
                "{site} has an empty match group, so it matches every task and \
                 no layer after it can ever match"
            ));
        }
        specs.push(lower_spec(js, &site, opts, &mut found));
    }

    // Both, when both apply: reporting only the typo would send someone back
    // for a second round over the refusals they could have seen now.
    if !found.unknown_keys.is_empty() && !found.rejected.is_empty() {
        return Err(LayerConfigError::Both {
            unknown: found.unknown_keys,
            unsupported: found.rejected,
        });
    }
    if !found.unknown_keys.is_empty() {
        return Err(LayerConfigError::UnknownKeys(found.unknown_keys));
    }
    if !found.rejected.is_empty() {
        return Err(LayerConfigError::Unsupported(found.rejected));
    }
    Ok(LoadedLayerConfig {
        specs,
        waived: found.waived,
        deprecated: found.deprecated,
        notes: found.notes,
    })
}

/// The structural rules `main.rs::verify_layer_specs()` enforces, plus the
/// capacity limits the wrapper would otherwise report as a bare `-E2BIG`.
fn check_shape(specs: &[LayerSpecJson], opts: &LayerConfigOptions) -> Result<(), LayerConfigError> {
    let invalid = |m: String| Err(LayerConfigError::Invalid(m));
    if specs.is_empty() {
        return invalid("no layer specs".into());
    }
    if specs.len() > opts.max_layers {
        return invalid(format!(
            "{} layers, but scx_layered is built with MAX_LAYERS={}",
            specs.len(),
            opts.max_layers
        ));
    }
    let last = specs.len() - 1;
    for (idx, spec) in specs.iter().enumerate() {
        let at = format!("layers[{idx}] {:?}", spec.name);
        if idx < last {
            if spec.matches.is_empty() {
                return invalid(format!(
                    "{at} is not the last layer but has no matches; only the last \
                     layer may be the catch-all"
                ));
            }
        } else if spec.matches.len() != 1 || !spec.matches[0].is_empty() {
            return invalid(format!(
                "{at} is the last layer, so it must be the catch-all \
                 (\"matches\": [[]]). scx_layered treats a task that matches no \
                 layer as a fatal error, so the last layer has to accept every task."
            ));
        }
        if spec.matches.len() > opts.max_match_ors {
            return invalid(format!(
                "{at} has {} OR groups, but MAX_LAYER_MATCH_ORS={}",
                spec.matches.len(),
                opts.max_match_ors
            ));
        }
        for (or_idx, ands) in spec.matches.iter().enumerate() {
            if ands.len() > opts.max_match_ands {
                return invalid(format!(
                    "{at} OR group {or_idx} has {} AND terms, but a group holds at \
                     most NR_LAYER_MATCH_KINDS={}",
                    ands.len(),
                    opts.max_match_ands
                ));
            }
            for (and_idx, m) in ands.iter().enumerate() {
                if let Err(why) = check_match(m, opts) {
                    return invalid(format!("{at} matches[{or_idx}][{and_idx}]: {why}"));
                }
            }
        }
        if let Err(why) = check_kind(&spec.kind) {
            return invalid(format!("{at}: {why}"));
        }
        if let Err(why) = check_common(spec.kind.common(), opts) {
            return invalid(format!("{at}: {why}"));
        }
        if spec.name.contains('\0') {
            return invalid(format!("{at}: layer name contains a NUL byte"));
        }
    }
    Ok(())
}

/// The per-term checks `main.rs::verify_layer_specs()` makes, plus the ones
/// scxsim needs because its FFI is a C boundary.
///
/// A needle longer than its `struct layer_match` buffer is TRUNCATED by the
/// wrapper, which silently widens the rule — `CommPrefix` of 300 characters
/// would match on the first 15. Upstream rejects it and so must this.
fn check_match(m: &LayerMatchJson, opts: &LayerConfigOptions) -> Result<(), String> {
    use LayerMatchJson as J;
    let bounded = |what: &str, s: &String, max: usize| -> Result<(), String> {
        if s.contains('\0') {
            return Err(format!(
                "{what} needle contains a NUL byte, which cannot cross the C \
                 boundary into struct layer_match"
            ));
        }
        if s.len() > max {
            return Err(format!(
                "{what} needle is {} bytes, but the scheduler's buffer holds {max}; \
                 the wrapper would truncate it and the rule would match far more \
                 tasks than it names",
                s.len()
            ));
        }
        Ok(())
    };
    match m {
        J::CgroupPrefix(v) => bounded("CgroupPrefix", v, opts.max_path),
        J::CgroupSuffix(v) => bounded("CgroupSuffix", v, opts.max_path),
        J::CgroupContains(v) => bounded("CgroupContains", v, opts.max_path),
        J::CommPrefix(v) | J::CommPrefixExclude(v) => bounded("CommPrefix", v, opts.max_comm),
        J::PcommPrefix(v) | J::PcommPrefixExclude(v) => bounded("PcommPrefix", v, opts.max_comm),
        // Upstream bails on a node id the topology does not have
        // (`main.rs`, init_layers). Without this the rule is published and can
        // only ever answer "no match", silently.
        J::NumaNode(n) if *n >= opts.nr_nodes => Err(format!(
            "NumaNode({n}), but the scheduler was given {} node(s) (0-{})",
            opts.nr_nodes,
            opts.nr_nodes - 1
        )),
        _ => Ok(()),
    }
}

/// `main.rs::resolve_cpus_pct_range()` bails on both of these; accepting them
/// would silently run a configuration the real scheduler refuses to start on.
fn check_kind(kind: &LayerKindJson) -> Result<(), String> {
    let (range, frac) = match kind {
        LayerKindJson::Confined(c) => (c.cpus_range, c.cpus_range_frac),
        LayerKindJson::Grouped(g) => (g.cpus_range, g.cpus_range_frac),
        LayerKindJson::Open(_) => (None, None),
    };
    if range.is_some() && frac.is_some() {
        return Err("cpus_range cannot be used with cpus_range_frac".into());
    }
    if let Some((lo, hi)) = frac {
        if !(0.0..=1.0).contains(&lo) || !(0.0..=1.0).contains(&hi) {
            return Err(format!(
                "cpus_range_frac values must be between 0.0 and 1.0, got ({lo}, {hi})"
            ));
        }
    }
    Ok(())
}

/// Microsecond fields are scaled by 1000 into nanoseconds, and the
/// `disallow_*` defaults are multiplied again. Catch the overflow here, where
/// the field can be named, rather than wrapping silently in a release build.
fn check_common(c: &CommonJson, opts: &LayerConfigOptions) -> Result<(), String> {
    let scaled = |what: &str, us: u64| -> Result<(), String> {
        us.checked_mul(1_000)
            .map(|_| ())
            .ok_or_else(|| format!("{what} = {us}us overflows when scaled to nanoseconds"))
    };
    scaled("min_exec_us", c.min_exec_us)?;
    scaled("slice_us", c.slice_us)?;
    for (what, v) in [
        ("disallow_open_after_us", c.disallow_open_after_us),
        ("disallow_preempt_after_us", c.disallow_preempt_after_us),
    ] {
        // u64::MAX is the "never" sentinel and passes through unscaled.
        if let Some(us) = v.filter(|us| *us != u64::MAX) {
            scaled(what, us)?;
        }
    }
    if opts.scx_slice_dfl_ns.checked_mul(4).is_none() {
        return Err("SCX_SLICE_DFL overflows when scaled for the disallow defaults".into());
    }
    Ok(())
}

fn lower_spec(
    js: &LayerSpecJson,
    site: &str,
    opts: &LayerConfigOptions,
    found: &mut Findings,
) -> LayerSpec {
    // `comment` is documentation; upstream reads it no further than we do.
    let _ = &js.comment;
    if js.template.is_some() {
        found.note(site, Unsupported::FieldTemplate, opts);
    }

    let kind = match &js.kind {
        LayerKindJson::Confined(_) => LayerKind::Confined,
        LayerKindJson::Grouped(_) => LayerKind::Grouped,
        LayerKindJson::Open(_) => LayerKind::Open,
    };
    let (kind_name, common) = (js.kind.name(), js.kind.common());
    let mut spec = LayerSpec::new(js.name.clone(), kind);

    lower_common(common, site, kind_name, opts, found, &mut spec);
    lower_kind_specific(&js.kind, site, opts, found, &mut spec);

    spec.matches = js
        .matches
        .iter()
        .enumerate()
        .map(|(or_id, ands)| {
            ands.iter()
                .enumerate()
                .filter_map(|(and_id, m)| {
                    lower_match(
                        m,
                        &format!("{site} matches[{or_id}][{and_id}]"),
                        opts,
                        found,
                    )
                })
                .collect()
        })
        .collect();
    spec
}

/// Lower the fields shared by every layer kind.
fn lower_common(
    c: &CommonJson,
    site: &str,
    kind_name: &str,
    opts: &LayerConfigOptions,
    found: &mut Findings,
    spec: &mut LayerSpec,
) {
    for key in c.unknown.keys() {
        match KIND_FIELDS
            .iter()
            .find(|(_, fields)| fields.contains(&key.as_str()))
        {
            // In upstream's schema, on a different layer kind. scx_layered's
            // serde drops it here without a word; say so rather than refusing
            // a file the real scheduler loads. Upstream's own
            // examples/cpuset.json puts util_range on an Open layer.
            Some(_) => found.notes.push(format!(
                "{site}: {key} is not a field of the {kind_name} layer kind, so \
                 nothing applies it — scx_layered's serde drops it here too"
            )),
            None => found.unknown_keys.push(format!("{site}: {key}")),
        }
    }

    // Directly modelled.
    spec.preempt = c.preempt;
    spec.preempt_first = c.preempt_first;
    spec.exclusive = c.exclusive;
    spec.fifo = c.fifo;
    spec.skip_remote_node = c.skip_remote_node;
    spec.prev_over_idle_core = c.prev_over_idle_core;
    spec.member_expire_ms = c.member_expire_ms;
    // Published, so the scheduler's own `layer->perf > 0` branch and its call
    // to scx_bpf_cpuperf_set actually run. scxsim's kfunc records the level on
    // the CPU; the engine models no DVFS, so it does not change how fast
    // simulated work completes. That is surfaced as a caveat, not a refusal —
    // refusing would skip a real scheduler code path over an effect the
    // config did not ask to be able to measure.
    spec.perf = c.perf.unwrap_or(0).min(u64::from(u32::MAX)) as u32;
    spec.min_exec_ns = c.min_exec_us * 1_000;
    spec.xllc_mig_min_ns = (c.xllc_mig_min_us * 1_000.0) as TimeNs;
    spec.nodes = c.nodes.clone();
    spec.llcs = c.llcs.clone();
    if let Some(algo) = c.growth_algo {
        // `LayerGrowthAlgo`'s own doc says the unmodellable algorithms are
        // rejected rather than approximated. That rejection lived only in the
        // control loop, which the config path does not engage, so it is made
        // good here too.
        match algo {
            LayerGrowthAlgo::CpuSetSpread
            | LayerGrowthAlgo::CpuSetSpreadReverse
            | LayerGrowthAlgo::CpuSetSpreadRandom => {
                found.note(site, Unsupported::GrowthCpuSetSpread, opts)
            }
            LayerGrowthAlgo::StickyDynamic => {
                found.note(site, Unsupported::GrowthStickyDynamic, opts)
            }
            _ => spec.growth_algo = algo,
        }
    }
    if let Some(p) = c.placement {
        spec.placement = p.into();
    }
    // Modelled since the arbitrary-topology work (#144): `LayerSpec` carries
    // both and `layered_control::refresh_xnuma()` consumes them, running
    // upstream's own vendored gate. Like `util_range` they are allocator-only,
    // so on the CLI path — which does not run the control loop — they are
    // inert, and the run banner says so rather than the loader refusing them.
    spec.xnuma_threshold = c.xnuma_threshold.unwrap_or(DEFAULT_XNUMA_THRESHOLD);
    spec.xnuma_threshold_delta = c
        .xnuma_threshold_delta
        .unwrap_or(DEFAULT_XNUMA_THRESHOLD_DELTA);

    // Upstream's config-load loop (`fn main`, scx_layered main.rs, the block
    // that begins `for spec in layer_config.specs.iter_mut()`) substitutes the
    // default for an unset or zero weight and THEN clamps the result. Clamping
    // only the explicit branch would diverge the day MIN_LAYER_WEIGHT rises
    // above DEFAULT_LAYER_WEIGHT.
    let weight = if c.weight == 0 {
        opts.default_weight
    } else {
        c.weight
    };
    spec.weight = weight.clamp(opts.weight_range.0, opts.weight_range.1);

    // Upstream substitutes --slice-us for an unset slice at config-load time,
    // so `layer.slice_ns` is never zero, and the yield_ignore derivation two
    // lines later reads the substituted value. Order matters here for the
    // same reason it does there.
    spec.slice_ns = if c.slice_us == 0 {
        opts.default_slice_ns
    } else {
        c.slice_us * 1_000
    };
    spec.set_yield_ignore(c.yield_ignore);

    lower_disallow(c, opts, spec);

    // Refused and deprecated.
    // A field that is present but inert — `perf: 0` asks for nothing — is not
    // refused: there is no behaviour to lose. Only a value that would change
    // the run is.
    let mut note = |present: bool, item: Unsupported| {
        if present {
            found.note(site, item, opts);
        }
    };
    note(
        c.idle_resume_us.is_some_and(|v| v > 0),
        Unsupported::FieldIdleResumeUs,
    );
    note(
        c.util_peak_half_life_ms.is_some_and(|v| v > 0),
        Unsupported::FieldUtilPeakHalfLifeMs,
    );
    note(
        c.allow_node_aligned.is_some(),
        Unsupported::FieldAllowNodeAligned,
    );
    note(c.idle_smt.is_some(), Unsupported::FieldIdleSmt);
}

/// `disallow_open_after_us` / `disallow_preempt_after_us`, with the override
/// scx_layered's config-load loop applies (the block in its `fn main`
/// beginning `for spec in layer_config.specs.iter_mut()` — NOT
/// `verify_layer_specs`, which only does the structural checks): a preempting
/// layer has both forced to "never", and any value the config gave is
/// ignored. Upstream warns and overrides rather than erroring, so this is not
/// a refusal.
///
/// Upstream also substitutes `DFL_DISALLOW_*_AFTER_US` when a non-preempting
/// layer leaves them unset. Those are `2 *` and `4 * SCX_SLICE_DFL / 1000` —
/// a FIXED pair (40 ms / 80 ms at the kernel's 20 ms default), applied to
/// every such layer whatever its own `slice_us` is. Deriving them from the
/// layer's slice instead is wrong by the ratio between the two: a 4 ms layer
/// would get 8 ms where production gives 40 ms.
fn lower_disallow(c: &CommonJson, opts: &LayerConfigOptions, spec: &mut LayerSpec) {
    let to_ns = |us: u64| {
        if us == u64::MAX {
            DISALLOW_AFTER_NEVER
        } else {
            us * 1_000
        }
    };
    if c.preempt {
        spec.disallow_open_after_ns = DISALLOW_AFTER_NEVER;
        spec.disallow_preempt_after_ns = DISALLOW_AFTER_NEVER;
        return;
    }
    spec.disallow_open_after_ns = c
        .disallow_open_after_us
        .map_or_else(|| 2 * opts.scx_slice_dfl_ns, to_ns);
    spec.disallow_preempt_after_ns = c
        .disallow_preempt_after_us
        .map_or_else(|| 4 * opts.scx_slice_dfl_ns, to_ns);
}

/// Lower the fields that belong to one layer kind only.
fn lower_kind_specific(
    kind: &LayerKindJson,
    site: &str,
    opts: &LayerConfigOptions,
    found: &mut Findings,
    spec: &mut LayerSpec,
) {
    // Each arm names its own fields so that adding one upstream is a compile
    // error here rather than a silent omission.
    let (util_range, cpus_range, cpus_range_frac, membw_gb, protected) = match kind {
        LayerKindJson::Confined(c) => (
            Some(c.util_range),
            c.cpus_range,
            c.cpus_range_frac,
            c.membw_gb,
            c.protected,
        ),
        LayerKindJson::Grouped(g) => {
            spec.util_includes_open_cputime = g.util_includes_open_cputime;
            spec.idle_confined = g.idle_confined;
            (
                Some(g.util_range),
                g.cpus_range,
                g.cpus_range_frac,
                g.membw_gb,
                g.protected,
            )
        }
        LayerKindJson::Open(_) => (None, None, None, None, false),
    };

    spec.protected = protected;
    spec.util_range = util_range;
    spec.cpus_range = resolve_cpus_range(cpus_range, cpus_range_frac, opts.nr_cpus as usize);
    if membw_gb.is_some() {
        found.note(site, Unsupported::FieldMembwGb, opts);
    }
}

/// `main.rs::resolve_cpus_pct_range()`: a fractional range becomes a CPU count
/// against the machine size, clamped to at least one CPU and at most all of
/// them. Both forms together is a config error upstream rejects; here the
/// explicit count wins and the shape check has no way to see it, so prefer
/// `cpus_range` and let the fraction go — upstream `bail!`s, and matching that
/// exactly would need a fourth error shape for one field.
fn resolve_cpus_range(
    cpus_range: Option<(usize, usize)>,
    cpus_range_frac: Option<(f64, f64)>,
    max_cpus: usize,
) -> Option<(usize, usize)> {
    match (cpus_range, cpus_range_frac) {
        (Some(r), _) => Some(r),
        (None, Some((lo, hi))) => {
            let count = |f: f64| ((max_cpus as f64) * f.clamp(0.0, 1.0)).round_ties_even() as usize;
            Some((count(lo).max(1), count(hi).max(1).min(max_cpus)))
        }
        (None, None) => None,
    }
}

/// Lower one match rule, or record why it cannot be lowered.
///
/// Returning `None` drops the term, which would silently widen the AND group
/// it came from — so every `None` path also records a [`Disposition::Fatal`]
/// rejection, and the caller turns any rejection into a failed load. The
/// dropped terms never reach a scheduler.
fn lower_match(
    m: &LayerMatchJson,
    site: &str,
    opts: &LayerConfigOptions,
    found: &mut Findings,
) -> Option<LayerMatch> {
    use LayerMatchJson as J;
    let mut refuse = |item: Unsupported| {
        found.note(site, item, opts);
        None
    };
    Some(match m {
        J::CgroupPrefix(s) => LayerMatch::CgroupPrefix(s.clone()),
        J::CgroupSuffix(s) => LayerMatch::CgroupSuffix(s.clone()),
        J::CgroupContains(s) => LayerMatch::CgroupContains(s.clone()),
        J::CommPrefix(s) => LayerMatch::CommPrefix(s.clone()),
        J::PcommPrefix(s) => LayerMatch::PcommPrefix(s.clone()),
        // Upstream lowers the Exclude spellings to the same BPF match kind
        // with `exclude` set (main.rs:2029, 2038); `Not` is that flag.
        J::CommPrefixExclude(s) => LayerMatch::Not(Box::new(LayerMatch::CommPrefix(s.clone()))),
        J::PcommPrefixExclude(s) => LayerMatch::Not(Box::new(LayerMatch::PcommPrefix(s.clone()))),
        J::NiceAbove(n) => LayerMatch::NiceAbove(*n),
        J::NiceBelow(n) => LayerMatch::NiceBelow(*n),
        J::NiceEquals(n) => LayerMatch::NiceEquals(*n),
        J::UIDEquals(v) => LayerMatch::UserIdEquals(*v),
        J::GIDEquals(v) => LayerMatch::GroupIdEquals(*v),
        J::PIDEquals(v) => LayerMatch::PidEquals(*v),
        J::PPIDEquals(v) => LayerMatch::PpidEquals(*v),
        J::TGIDEquals(v) => LayerMatch::TgidEquals(*v),
        J::IsGroupLeader(b) => LayerMatch::IsGroupLeader(*b),
        J::IsKthread(b) => LayerMatch::IsKthread(*b),
        J::NumaNode(v) => LayerMatch::NumaNode(*v),
        J::AvgRuntime(lo, hi) => {
            // Upstream accepts any ordering and publishes both bounds, so
            // refusing here would reject a config the real scheduler runs.
            // But `min <= avg < max` cannot hold for lo >= hi, so the rule is
            // dead — say so rather than letting it look like a live term.
            if lo >= hi {
                found.notes.push(format!(
                    "{site}: AvgRuntime[{lo}us, {hi}us) is empty, so this term can \
                     never match — scx_layered publishes it too, and it is dead there \
                     as well"
                ));
            }
            LayerMatch::AvgRuntime(*lo, *hi)
        }
        J::CgroupRegex(_) => return refuse(Unsupported::MatchCgroupRegex),
        J::NSPIDEquals(..) => return refuse(Unsupported::MatchNsPidEquals),
        J::NSEquals(_) => return refuse(Unsupported::MatchNsEquals),
        J::CmdJoin(_) => return refuse(Unsupported::MatchCmdJoin),
        J::UsedGpuTid(_) => return refuse(Unsupported::MatchUsedGpuTid),
        J::UsedGpuPid(_) => return refuse(Unsupported::MatchUsedGpuPid),
        J::HintEquals(_) => return refuse(Unsupported::MatchHintEquals),
        J::SystemCpuUtilBelow(_) => return refuse(Unsupported::MatchSystemCpuUtilBelow),
        J::DsqInsertBelow(_) => return refuse(Unsupported::MatchDsqInsertBelow),
    })
}
