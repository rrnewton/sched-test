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
use scxsim_workload_ir::{
    lower, lower_with_options, to_scenario, LoweringOptions, SourceScenario, WorkloadIr,
};

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

/// Read and compile an explicitly abstract IoModelV1 record.
///
/// This is deliberately not named replay: an IoModelV1 source is a separate,
/// typed simulator model paired with VM measurements, not the fieldless ktstr
/// record the VM backend executed.
pub fn compile_model(path: &Path, options: &LoweringOptions) -> Result<Compiled, ReplayError> {
    let bytes = std::fs::read(path).map_err(ReplayError::Read)?;
    let source: SourceScenario = serde_json::from_slice(&bytes).map_err(ReplayError::Schema)?;
    compile_model_source(&source, options)
}

/// Compile an already-deserialised, explicitly abstract IoModelV1 source.
pub fn compile_model_source(
    source: &SourceScenario,
    options: &LoweringOptions,
) -> Result<Compiled, ReplayError> {
    let ir =
        lower_with_options(source, options).map_err(|e| ReplayError::Lower(format!("{e:?}")))?;
    if ir.applied_io_profiles.len() != 1 {
        return Err(ReplayError::Lower(format!(
            "compile_model_source requires exactly one applied IoModelV1 profile, got {}",
            ir.applied_io_profiles.len()
        )));
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use scxsim_workload_ir::{
        IoCalibrationScheduler, IoModelBacking, IoModelFlush, IoModelOpenMode, IoModelOperation,
        IoModelSpec, Phase, Repeat, ResolvedIoProfile, SourceCgroupDef, SourceHold, SourceStep,
        SourceWorkSpec, SourceWorkType,
    };

    #[test]
    fn ordinary_replay_is_refused_and_typed_model_forwards_its_profile() {
        let ordinary = SourceScenario::new("ordinary-io").step(SourceStep::new(
            vec![SourceCgroupDef::named("io")
                .work(SourceWorkSpec::new(SourceWorkType::IoSyncWrite).workers(1))],
            SourceHold::FULL,
        ));
        assert!(matches!(
            compile_source(&ordinary),
            Err(ReplayError::Lower(message)) if message.contains("UnmodelledIoSource")
        ));
        let cpu_only = SourceScenario::new("not-a-model").step(SourceStep::new(
            vec![SourceCgroupDef::named("cpu")
                .work(SourceWorkSpec::new(SourceWorkType::SpinWait).workers(1))],
            SourceHold::FULL,
        ));
        assert!(matches!(
            compile_model_source(&cpu_only, &LoweringOptions::default()),
            Err(ReplayError::Lower(message)) if message.contains("exactly one applied IoModelV1")
        ));

        let spec = IoModelSpec {
            operation: IoModelOperation::SequentialWrite,
            backing: IoModelBacking::FreshRawUnthrottledBlockDevice,
            backing_capacity_bytes: 256 * 1024 * 1024,
            open_mode: IoModelOpenMode::OSync,
            write_size_bytes: 4096,
            writes_per_cycle: 16,
            flush: IoModelFlush::FdatasyncPerCycle,
            queue_depth: 1,
            calibration_scheduler: IoCalibrationScheduler::ScxKtstr,
            declared_bytes_per_worker: 2 * 1024 * 1024,
        };
        let source = SourceScenario {
            topology: scxsim_workload_ir::SourceTopology {
                numa_nodes: 1,
                llcs: 1,
                cores: 4,
                threads: 1,
            },
            ..SourceScenario::new("modelled-io")
        }
        .step(SourceStep::new(
            vec![SourceCgroupDef::named("io").work(
                SourceWorkSpec::new(SourceWorkType::IoModelV1 { spec: spec.clone() }).workers(1),
            )],
            SourceHold::FULL,
        ));
        assert!(matches!(
            compile_source(&source),
            Err(ReplayError::Lower(message)) if message.contains("MissingIoProfile")
        ));

        let profile = ResolvedIoProfile::frozen_v1(spec).expect("spec is inside frozen v1");
        let options = LoweringOptions::default().with_io_profile(profile.clone());
        let compiled = compile_model_source(&source, &options).expect("profile is forwarded");
        assert_eq!(compiled.ir.tasks[0].repeat, Repeat::Once);
        // v2 forwards the profile as the block/wake alternation rather than one
        // aggregate pair, so what must be asserted is that BOTH estimands
        // survive the forwarding intact -- as sums -- and that the terminal
        // Park is still there. Asserting the sums is stronger than the old
        // literal-vector equality: it would catch a forwarding path that
        // rescaled or dropped either estimand.
        let phases = &compiled.ir.tasks[0].phases;
        assert_eq!(phases.last(), Some(&Phase::Park));
        let system: u64 = phases
            .iter()
            .filter_map(|p| match p {
                Phase::SystemCpu(d) => Some(d.as_nanos()),
                _ => None,
            })
            .sum();
        let nonrunning: u64 = phases
            .iter()
            .filter_map(|p| match p {
                Phase::NonRunning(d) => Some(d.as_nanos()),
                _ => None,
            })
            .sum();
        assert_eq!(system, profile.system_cpu_per_worker.as_nanos());
        assert_eq!(nonrunning, profile.nonrunning_per_worker.as_nanos());
        assert!(phases.len() > 3, "the alternation must not be collapsed");
    }
}
