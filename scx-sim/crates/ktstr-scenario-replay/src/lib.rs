//! Replay a ktstr scenario record on the simulator.
//!
//! This is the simulator-side half of "one test definition, two backends". The
//! ktstr side exports a registered `ScenarioDef` as a `SourceScenario`-shaped
//! JSON record; this crate reads it and drives the rest of the pipeline that
//! already exists:
//!
//! ```text
//! ktstr ScenarioDef            (ktstr, the same value the VM backend runs)
//!   -> SourceScenario JSON     (ktstr scenario::export)
//!   -> SourceScenario          (deserialised HERE, with the real type)
//!   -> WorkloadIr              (scxsim_workload_ir::lower)
//!   -> scx_simulator::Scenario (scxsim_workload_ir::to_scenario)
//!   -> Trace                   (Simulator::run)
//! ```
//!
//! # Why deserialising with the real type is the load-bearing step
//!
//! ktstr cannot depend on this crate — the dependency runs ktstr -> scx-sim, and
//! a path dependency between two checkouts would bake a host-specific path into
//! a committed manifest. So the seam is JSON, and JSON seams drift.
//!
//! The defence is that nothing here re-describes the schema. The record is
//! deserialised into `scxsim_workload_ir::SourceScenario` itself, so a ktstr
//! exporter that emits the wrong shape produces a hard `serde` error naming the
//! field. A translation layer that "fixed up" the record would convert that loud
//! failure into a quiet reinterpretation, which is the failure mode worth most
//! avoiding: a simulator run that answers a question about a workload nobody
//! wrote.
//!
//! # The comparison this enables
//!
//! [`cgroup_cpu_time`] reduces a simulator run to the one quantity the VM
//! backend also reports per cgroup — total CPU time — so the two backends can be
//! held to the same oracle. It deliberately does not try to reconcile anything
//! else: the simulator has no iterations, no migrations-per-second and no wake
//! latency in the sense ktstr measures them, and inventing correspondences would
//! manufacture agreement rather than test for it.

pub mod compare;

pub use compare::{
    compare, load_baseline, BaselineProvenance, CgroupComparison, Comparison, Mismatch,
    ReadBaselineError, VmBaseline,
};

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use scx_simulator::prelude::*;
use scx_simulator::TraceKind;
use scxsim_workload_ir::{lower, to_scenario, SourceScenario, WorkloadIr};

/// Everything that went wrong, named at the stage it went wrong.
#[derive(Debug)]
pub enum ReplayError {
    /// The record could not be read.
    Read(std::io::Error),
    /// The record is not a `SourceScenario`. This is the drift alarm.
    Schema(serde_json::Error),
    /// ktstr's vocabulary could not be lowered onto the IR.
    Lower(String),
    /// The IR could not be turned into a simulator scenario.
    Ingest(String),
}

impl std::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplayError::Read(e) => write!(f, "read scenario record: {e}"),
            ReplayError::Schema(e) => write!(
                f,
                "the record is not a SourceScenario: {e}. The ktstr exporter and \
                 this crate's schema have diverged; fix the exporter rather than \
                 translating here."
            ),
            ReplayError::Lower(e) => write!(f, "lower ktstr vocabulary to IR: {e}"),
            ReplayError::Ingest(e) => write!(f, "ingest IR into a simulator scenario: {e}"),
        }
    }
}

impl std::error::Error for ReplayError {}

/// A record compiled all the way to a runnable simulator scenario, with the
/// fidelity report the lowering produced.
pub struct Compiled {
    /// The scenario the simulator will run.
    pub scenario: Scenario,
    /// The IR it came from. Carries `ir.fidelity` — what the lowering had to
    /// approximate and what it discarded doing so — so the loss travels with
    /// the workload instead of being logged and forgotten.
    pub ir: WorkloadIr,
}

/// Read a ktstr scenario record and compile it to a simulator scenario.
pub fn compile(path: &Path) -> Result<Compiled, ReplayError> {
    let bytes = std::fs::read(path).map_err(ReplayError::Read)?;
    let source: SourceScenario = serde_json::from_slice(&bytes).map_err(ReplayError::Schema)?;
    compile_source(&source)
}

/// Compile an already-deserialised record.
pub fn compile_source(source: &SourceScenario) -> Result<Compiled, ReplayError> {
    let ir = lower(source).map_err(|e| ReplayError::Lower(format!("{e:?}")))?;
    let scenario = to_scenario(&ir).map_err(|e| ReplayError::Ingest(format!("{e:?}")))?;
    Ok(Compiled { scenario, ir })
}

/// Total CPU time per cgroup, summed over that cgroup's tasks.
///
/// This is the simulator-side counterpart of ktstr's per-cgroup
/// `total_cpu_time_ns` in its stats sidecar. A task's CPU time is
/// `Trace::total_runtime(pid)`; cgroup membership comes from the scenario's
/// `TaskDef::cgroup_name`, since the trace is keyed by pid.
///
/// Tasks with no cgroup are grouped under `""` rather than dropped — a silently
/// discarded task would make the shares add up to something plausible while
/// describing a different workload.
#[must_use]
pub fn cgroup_cpu_time(scenario: &Scenario, trace: &Trace) -> BTreeMap<String, u64> {
    let mut by_cgroup: BTreeMap<String, u64> = BTreeMap::new();
    for task in &scenario.tasks {
        let cg = task.cgroup_name.clone().unwrap_or_default();
        *by_cgroup.entry(cg).or_default() += trace.total_runtime(task.pid);
    }
    by_cgroup
}

/// The largest relative gap between any two cgroups' CPU-time shares, as a
/// fraction of the largest share.
///
/// `0.0` is perfect equality. This is the shared oracle: it is computable from
/// the VM backend's stats sidecar and from a simulator trace, and for a workload
/// of equal-weight cgroups running identical work it should be near zero on
/// either backend. Returns `None` for fewer than two cgroups, where the question
/// is not meaningful, rather than a `0.0` that would look like a pass.
#[must_use]
pub fn cpu_time_spread(by_cgroup: &BTreeMap<String, u64>) -> Option<f64> {
    if by_cgroup.len() < 2 {
        return None;
    }
    let max = *by_cgroup.values().max()?;
    let min = *by_cgroup.values().min()?;
    if max == 0 {
        return None;
    }
    Some((max - min) as f64 / max as f64)
}

/// The CPUs each cgroup's tasks were actually scheduled on.
///
/// Read from `TaskScheduled` trace events, so it is what the run DID, not what
/// the scenario asked for. The distinction is the whole point: a declared
/// cpuset that is carried faithfully through every layer and then not enforced
/// looks identical to an honoured one unless somebody measures placement.
#[must_use]
pub fn observed_placement(scenario: &Scenario, trace: &Trace) -> BTreeMap<String, BTreeSet<u32>> {
    let by_pid: BTreeMap<i32, String> = scenario
        .tasks
        .iter()
        .map(|t| (t.pid.0, t.cgroup_name.clone().unwrap_or_default()))
        .collect();
    let mut out: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
    for ev in trace.events() {
        if let TraceKind::TaskScheduled { pid } = ev.kind {
            if let Some(cg) = by_pid.get(&pid.0) {
                out.entry(cg.clone()).or_default().insert(ev.cpu.0);
            }
        }
    }
    out
}

/// One cgroup that ran outside the CPUs it was confined to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpusetViolation {
    /// The cgroup that escaped its cpuset.
    pub cgroup: String,
    /// The CPUs the scenario confined it to.
    pub declared: BTreeSet<u32>,
    /// The CPUs its tasks were actually scheduled on.
    pub observed: BTreeSet<u32>,
    /// `observed - declared`: where it ran that it should not have.
    pub escaped_to: BTreeSet<u32>,
}

/// Check observed placement against every declared cgroup cpuset.
///
/// Empty means every confined cgroup stayed inside its cpuset.
///
/// This exists because the fidelity report cannot see this class of loss. The
/// lowering carries a cpuset faithfully into `Scenario.cgroups[].cpuset` and
/// reports `Exact` — correctly, as far as it can tell. Whether the simulator
/// then ENFORCES it is downstream of the IR entirely, so the only way to know
/// is to measure where tasks ran. See sim-4qlh5: today the simulator treats a
/// cgroup cpuset as metadata advertised to the BPF scheduler rather than as the
/// kernel-enforced task constraint it is, so this returns violations.
///
/// A cgroup that never ran at all contributes no violation — absence of
/// evidence is not placed outside the cpuset, and reporting it as one would
/// make an idle cgroup indistinguishable from an escaped one.
#[must_use]
pub fn cpuset_violations(scenario: &Scenario, trace: &Trace) -> Vec<CpusetViolation> {
    let placement = observed_placement(scenario, trace);
    let mut out = Vec::new();
    for cg in &scenario.cgroups {
        let Some(declared) = &cg.cpuset else { continue };
        let declared: BTreeSet<u32> = declared.iter().map(|c| c.0).collect();
        let Some(observed) = placement.get(&cg.name) else {
            continue;
        };
        let escaped_to: BTreeSet<u32> = observed.difference(&declared).copied().collect();
        if !escaped_to.is_empty() {
            out.push(CpusetViolation {
                cgroup: cg.name.clone(),
                declared,
                observed: observed.clone(),
                escaped_to,
            });
        }
    }
    out
}
