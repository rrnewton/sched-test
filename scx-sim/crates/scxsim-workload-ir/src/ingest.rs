//! IR -> `scx_simulator::Scenario`: the far half of the bridge.
//!
//! With [`crate::lower`] this completes `ktstr ops -> IR -> Scenario`, which is
//! what lets one ktstr scenario execute on the simulator backend.
//!
//! # Why this is a walk and not a compiler
//!
//! The IR was shaped to be a short hop from `Scenario`: topology, cgroup tree,
//! task set and timed mutations all have counterparts there. Almost everything
//! here is a field copy. The interesting parts are the three places where
//! `Scenario` has **no** counterpart, and those are hard errors.
//!
//! # What it refuses, and why refusing is the correct answer
//!
//! `Scenario` cannot express these today. Lowering them anyway would produce a
//! simulation that runs, looks plausible, and tests something other than what
//! the author wrote — the failure mode that is worse than an error because it
//! is never noticed:
//!
//! * **[`Phase::Yield`]** — `scx_simulator::Phase` is `Run | Sleep | Wake`.
//!   There is no way for a task to script a `sched_yield`. Mapping it to `Run(0)`
//!   would silently delete it, and a yield-heavy workload would become a
//!   different workload that still passes.
//! * **A non-`Normal` [`SchedPolicy`]** — `scx_simulator::TaskDef` has no policy
//!   field at all. Dropping it turns an RT-starvation scenario into an ordinary
//!   CFS one that cannot exhibit the thing it exists to test.
//! * **[`Mutation::Observe`]** — an observation is a request for a record. Silently
//!   not making it means the record the author asked for is simply absent.
//!
//! Each is reported with the task or label that caused it, so the caller can
//! route it as "this scenario cannot cross to the simulator backend" rather than
//! as a failure of the scenario itself. These are gaps in `Scenario`, and closing
//! them is tracked work — not something this module should paper over.

use scx_simulator::{
    CgroupCpusetChangeEvent, CpuId, Phase as SimPhase, Pid, RepeatMode, Scenario, TaskBehavior,
    TaskDef,
};

use crate::ir::{
    CpuSet, Mutation, Phase, Repeat, SchedPolicy, Task, Topology, ValidationError, WorkloadIr,
};
use crate::units::TaskId;

/// A construct the simulator's `Scenario` cannot represent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestError {
    /// The public ingestion boundary received structurally malformed IR.
    InvalidIr(ValidationError),
    /// `Scenario`'s phase vocabulary has no yield.
    YieldNotRepresentable { task: String },
    /// `TaskDef` carries no scheduling policy.
    PolicyNotRepresentable { task: String, policy: String },
    /// An observation has nowhere to land.
    ObserveNotRepresentable { label: String },
    /// A cpuset that cannot be resolved against the declared topology.
    CpuSetUnresolvable { what: String, detail: String },
    /// A wake naming a task that is not in the IR. `lower()` rejects these, so
    /// reaching this means the IR was built by hand and skipped validation.
    UnknownWakeTarget { task: String, target: TaskId },
    /// The IR declares no tasks. `Scenario::build()` panics on this, so it is
    /// caught here and returned instead — a panic from inside the simulator is a
    /// much worse diagnostic than an error naming the workload.
    NoTasks { workload: String },
}

impl std::fmt::Display for IngestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IngestError::InvalidIr(error) => write!(f, "invalid workload IR: {error}"),
            IngestError::YieldNotRepresentable { task } => write!(
                f,
                "task `{task}` yields, but scx_simulator::Phase is Run|Sleep|Wake with no \
                 yield. Refusing rather than dropping it: a yield-heavy workload with its \
                 yields removed still runs, and still passes, while testing something else. \
                 Closing this means adding a Yield phase to the simulator."
            ),
            IngestError::PolicyNotRepresentable { task, policy } => write!(
                f,
                "task `{task}` requests scheduling policy {policy}, but \
                 scx_simulator::TaskDef has no policy field. Refusing rather than \
                 silently running it as SCHED_NORMAL, which would turn an RT scenario \
                 into a CFS one that cannot exhibit what it was written to show."
            ),
            IngestError::ObserveNotRepresentable { label } => write!(
                f,
                "observation `{label}` has no counterpart in Scenario. Refusing rather \
                 than dropping it: the caller asked for a record, and silently not \
                 making one is indistinguishable from making an empty one."
            ),
            IngestError::CpuSetUnresolvable { what, detail } => {
                write!(f, "cannot resolve cpuset for {what}: {detail}")
            }
            IngestError::UnknownWakeTarget { task, target } => write!(
                f,
                "task `{task}` wakes {target}, which is not in the IR (lower() rejects \
                 this; a hand-built IR bypassed validate())"
            ),
            IngestError::NoTasks { workload } => write!(
                f,
                "workload `{workload}` declares no tasks; the simulator requires at \
                 least one. A run with nothing to schedule would produce an empty trace \
                 that could be mistaken for a clean result."
            ),
        }
    }
}

impl std::error::Error for IngestError {}

/// Resolve a symbolic [`CpuSet`] against the topology to concrete CPU ids.
///
/// The IR keeps cpusets symbolic (`Llc(0)`, `Partition{..}`) precisely so the
/// backend can bind its own CPU numbering; this is where that binding happens.
/// CPUs are numbered densely, LLC-major then core then thread, matching how the
/// simulator groups them (`cpus_per_llc`, `smt_threads_per_core`).
pub fn resolve_cpuset(
    set: &CpuSet,
    topo: &Topology,
    what: &str,
) -> Result<Vec<CpuId>, IngestError> {
    let total = topo.total_cpus();
    let all: Vec<u32> = (0..total).collect();
    let ids = match set {
        CpuSet::All => all,
        CpuSet::Llc(i) => {
            let per = topo.cpus_per_llc();
            if *i >= topo.llcs {
                return Err(IngestError::CpuSetUnresolvable {
                    what: what.into(),
                    detail: format!("llc {i} but topology has {} llcs", topo.llcs),
                });
            }
            (i * per..(i + 1) * per).collect()
        }
        CpuSet::NumaNode(n) => {
            if *n >= topo.numa_nodes {
                return Err(IngestError::CpuSetUnresolvable {
                    what: what.into(),
                    detail: format!("node {n} but topology has {} nodes", topo.numa_nodes),
                });
            }
            let per_node = total / topo.numa_nodes;
            (n * per_node..(n + 1) * per_node).collect()
        }
        CpuSet::Partition { index, of } => {
            if *of == 0 || index >= of {
                return Err(IngestError::CpuSetUnresolvable {
                    what: what.into(),
                    detail: format!("partition {index} of {of} is out of range"),
                });
            }
            let per = total.div_ceil(*of);
            let start = index * per;
            if start >= total {
                return Err(IngestError::CpuSetUnresolvable {
                    what: what.into(),
                    detail: format!(
                        "partition {index} of {of} starts past the end of {total} cpus"
                    ),
                });
            }
            (start..(start + per).min(total)).collect()
        }
        CpuSet::Explicit(v) => {
            for c in v {
                if c.0 >= total {
                    return Err(IngestError::CpuSetUnresolvable {
                        what: what.into(),
                        detail: format!("cpu {} but topology has {total} cpus", c.0),
                    });
                }
            }
            v.iter().map(|c| c.0).collect()
        }
    };
    if ids.is_empty() {
        return Err(IngestError::CpuSetUnresolvable {
            what: what.into(),
            detail: "resolves to an empty CPU set; a task with nowhere to run would \
                     never be scheduled and the run would silently do nothing"
                .into(),
        });
    }
    Ok(ids.into_iter().map(CpuId).collect())
}

/// `TaskId` -> `Pid`. Dense from 1: pid 0 is conventionally not a task.
fn pid_of(id: TaskId) -> Pid {
    Pid(id.0 as i32 + 1)
}

fn repeat_of(r: Repeat) -> RepeatMode {
    match r {
        Repeat::Once => RepeatMode::Once,
        Repeat::Times(n) => RepeatMode::Count(n),
        Repeat::Forever => RepeatMode::Forever,
    }
}

fn phases_of(task: &Task, known: &[TaskId]) -> Result<Vec<SimPhase>, IngestError> {
    task.phases
        .iter()
        .map(|p| match p {
            Phase::Run(d) => Ok(SimPhase::Run(d.as_nanos())),
            // System CPU is task-context execution charged to this task.  It
            // traverses the ordinary scheduler path, but retains its tag so
            // generic compute jitter cannot perturb a calibrated duration.
            Phase::SystemCpu(d) => Ok(SimPhase::SystemCpu(d.as_nanos())),
            Phase::Sleep(d) => Ok(SimPhase::Sleep(d.as_nanos())),
            // This is known only to be unrunnable time; mapping it to Sleep
            // preserves that scheduler state without claiming a syscall or
            // block-device mechanism.
            Phase::NonRunning(d) => Ok(SimPhase::Sleep(d.as_nanos())),
            Phase::Park => Ok(SimPhase::Park),
            Phase::Wake(t) => {
                if known.contains(t) {
                    Ok(SimPhase::Wake(pid_of(*t)))
                } else {
                    Err(IngestError::UnknownWakeTarget {
                        task: task.name.clone(),
                        target: *t,
                    })
                }
            }
            Phase::Yield => Err(IngestError::YieldNotRepresentable {
                task: task.name.clone(),
            }),
        })
        .collect()
}

fn policy_name(p: SchedPolicy) -> &'static str {
    match p {
        SchedPolicy::Normal => "Normal",
        SchedPolicy::Batch => "Batch",
        SchedPolicy::Idle => "Idle",
        SchedPolicy::Fifo { .. } => "Fifo",
        SchedPolicy::RoundRobin { .. } => "RoundRobin",
    }
}

/// Build a `Scenario` from the IR, or refuse.
///
/// The [`crate::FidelityReport`] on the IR is NOT consumed here — it belongs to
/// the caller, who is the one deciding whether this workload can answer their
/// question. Folding it away at ingestion would hide it at exactly the moment it
/// becomes actionable.
pub fn to_scenario(ir: &WorkloadIr) -> Result<Scenario, IngestError> {
    ir.validate().map_err(IngestError::InvalidIr)?;
    if ir.tasks.is_empty() {
        return Err(IngestError::NoTasks {
            workload: ir.name.clone(),
        });
    }
    let topo = &ir.topology;
    let mut b = Scenario::builder()
        .cpus(topo.total_cpus())
        .smt(topo.threads_per_core)
        .cpus_per_llc(topo.cpus_per_llc())
        .duration_ns(ir.duration.as_nanos())
        .seed(ir.seed as u32);
    if let Some(profile) = ir.applied_io_profiles.first() {
        b = b.require_scheduler_identity(profile.spec.calibration_scheduler.simulator_identity());
    }

    for cg in &ir.cgroups {
        let cpus = match &cg.cpuset {
            Some(cs) => resolve_cpuset(cs, topo, &format!("cgroup `{}`", cg.name))?,
            // No cpuset declared: inherit every CPU. The simulator's cgroup
            // constructor requires an explicit set.
            None => resolve_cpuset(&CpuSet::All, topo, &format!("cgroup `{}`", cg.name))?,
        };
        b = match (&cg.parent, &cg.bandwidth) {
            (None, None) => b.cgroup(cg.name.as_str(), &cpus),
            (None, Some(bw)) => b.cgroup_with_bandwidth(
                cg.name.as_str(),
                &cpus,
                bw.period.as_nanos() / 1_000,
                bw.quota.as_nanos() / 1_000,
                0,
            ),
            (Some(p), _) => b.cgroup_nested(cg.name.as_str(), p.as_str(), Some(&cpus)),
        };
    }

    let known: Vec<TaskId> = ir.tasks.iter().map(|t| t.id).collect();
    for task in &ir.tasks {
        if task.policy != SchedPolicy::Normal {
            return Err(IngestError::PolicyNotRepresentable {
                task: task.name.clone(),
                policy: policy_name(task.policy).into(),
            });
        }
        let allowed_cpus = match &task.affinity {
            Some(cs) => Some(resolve_cpuset(cs, topo, &format!("task `{}`", task.name))?),
            None => None,
        };
        b = b.task(TaskDef {
            name: task.name.clone(),
            pid: pid_of(task.id),
            nice: task.nice.0,
            behavior: TaskBehavior {
                phases: phases_of(task, &known)?,
                repeat: repeat_of(task.repeat),
            },
            start_time_ns: task.start.as_nanos(),
            mm_id: None,
            allowed_cpus,
            parent_pid: None,
            cgroup_name: task.cgroup.as_ref().map(|c| c.0.clone()),
            task_flags: 0,
            migration_disabled: 0,
        });
    }

    for tm in &ir.timeline {
        let at = tm.at.as_nanos();
        b = match &tm.mutation {
            Mutation::CreateCgroup(cg) => {
                let cpus = match &cg.cpuset {
                    Some(cs) => Some(resolve_cpuset(cs, topo, &format!("cgroup `{}`", cg.name))?),
                    None => None,
                };
                b.cgroup_create_at(
                    cg.name.as_str(),
                    cg.parent.as_ref().map(|p| p.as_str()),
                    cpus.as_deref(),
                    at,
                )
            }
            Mutation::DestroyCgroup(n) => b.cgroup_destroy_at(n.as_str(), at),
            Mutation::SetCpuset { cgroup, cpus } => {
                let new_cpuset = resolve_cpuset(cpus, topo, &format!("cgroup `{cgroup}`"))?;
                b.cgroup_cpuset_change(CgroupCpusetChangeEvent {
                    cgroup_name: cgroup.0.clone(),
                    new_cpuset,
                    at_ns: at,
                })
            }
            Mutation::ClearCpuset { cgroup } => {
                // "Inherit the parent's set" has no direct event; the nearest
                // faithful thing is the full machine, which is what the root
                // cgroup's set is.
                let new_cpuset = resolve_cpuset(&CpuSet::All, topo, &format!("cgroup `{cgroup}`"))?;
                b.cgroup_cpuset_change(CgroupCpusetChangeEvent {
                    cgroup_name: cgroup.0.clone(),
                    new_cpuset,
                    at_ns: at,
                })
            }
            Mutation::MoveTasks { from, to } => {
                // Scenario migrates one pid at a time; move every task the IR
                // places in `from`.
                let movers: Vec<Pid> = ir
                    .tasks
                    .iter()
                    .filter(|t| t.cgroup.as_ref() == Some(from))
                    .map(|t| pid_of(t.id))
                    .collect();
                let mut acc = b;
                for pid in movers {
                    acc = acc.cgroup_migrate(pid, from.as_str(), to.as_str(), at);
                }
                acc
            }
            Mutation::SetBandwidth { .. } => {
                // Scenario sets bandwidth at cgroup construction, not on a
                // timeline. Rather than pretend, leave the declared value in
                // place and say nothing changed — but that WOULD be a silent
                // drop, so refuse instead.
                return Err(IngestError::ObserveNotRepresentable {
                    label: "SetBandwidth mid-run".into(),
                });
            }
            Mutation::Observe { label, .. } => {
                return Err(IngestError::ObserveNotRepresentable {
                    label: label.clone(),
                })
            }
        };
    }

    Ok(b.build())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Cgroup, TimedMutation};
    use crate::lower::lower;
    use crate::source::*;
    use crate::units::{CgroupName, DurationNs, Nice};

    /// `scx_simulator::Phase` derives no PartialEq, so tests compare a
    /// structural projection rather than the value itself.
    fn phase_shape(p: &SimPhase) -> (&'static str, u64) {
        match p {
            SimPhase::Run(d) => ("run", *d),
            SimPhase::SystemCpu(d) => ("system-cpu", *d),
            SimPhase::Sleep(d) => ("sleep", *d),
            SimPhase::Park => ("park", 0),
            SimPhase::Wake(pid) => ("wake", pid.0 as u64),
            // Unit variant: no payload, so 0. This is a TEST-HELPER projection
            // and deliberately asserts nothing about semantics.
            //
            // NOTE for this crate's owner: `scx_simulator::Phase` gained a
            // `Yield` variant, which makes the module doc above ("Phase is
            // Run | Sleep | Wake") stale and probably makes the
            // `YieldNotRepresentable` refusal obsolete — a yield may now be
            // lowerable rather than rejected. That is a design call for whoever
            // owns the lowering, not something to decide from here; this arm
            // only unbreaks the build.
            SimPhase::Yield => ("yield", 0),
        }
    }

    fn wakes(phases: &[SimPhase]) -> Vec<i32> {
        phases
            .iter()
            .filter_map(|p| match p {
                SimPhase::Wake(pid) => Some(pid.0),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn calibrated_io_phases_keep_scheduler_state_at_ingestion() {
        let mut task = Task::new(TaskId(0), "io");
        task.phases = vec![
            Phase::SystemCpu(DurationNs::from_millis(3)),
            Phase::NonRunning(DurationNs::from_millis(7)),
            Phase::Park,
        ];
        let phases = phases_of(&task, &[TaskId(0)]).expect("ingest phases");
        assert_eq!(
            phases.iter().map(phase_shape).collect::<Vec<_>>(),
            vec![("system-cpu", 3_000_000), ("sleep", 7_000_000), ("park", 0)]
        );
    }

    fn simple_source(wt: SourceWorkType) -> SourceScenario {
        SourceScenario {
            duration: DurationNs::from_secs(1),
            ..SourceScenario::new("t")
        }
        .step(SourceStep::new(
            vec![SourceCgroupDef::named("cg_0").work(SourceWorkSpec::new(wt).workers(2))],
            SourceHold::Frac(1.0),
        ))
    }

    /// END TO END: ktstr source -> IR -> Scenario. This is the whole bridge, and
    /// it is the thing the one-test-both-backends milestone needs from this side.
    #[test]
    fn ktstr_source_lowers_and_ingests_into_a_scenario() {
        let src = simple_source(SourceWorkType::Bursty {
            burst_duration: DurationNs::from_millis(1),
            sleep_duration: DurationNs::from_millis(1),
        });
        let ir = lower(&src).expect("lowers");
        let scenario = to_scenario(&ir).expect("ingests");

        assert_eq!(scenario.nr_cpus, ir.topology.total_cpus());
        assert_eq!(scenario.duration_ns, ir.duration.as_nanos());
        assert_eq!(scenario.tasks.len(), 2, "two workers");
        assert_eq!(scenario.cgroups.len(), 1);
        assert_eq!(scenario.cgroups[0].name, "cg_0");
        // scx_simulator::Phase does not derive PartialEq, so compare structurally.
        let shape: Vec<(&str, u64)> = scenario.tasks[0]
            .behavior
            .phases
            .iter()
            .map(phase_shape)
            .collect();
        assert_eq!(shape, vec![("run", 1_000_000), ("sleep", 1_000_000)]);
    }

    /// A yield must NOT silently vanish. YieldHeavy is a real ktstr work type,
    /// and a scenario with its yields removed runs and passes while testing
    /// something else.
    #[test]
    fn yield_is_refused_rather_than_dropped() {
        let ir = lower(&simple_source(SourceWorkType::YieldHeavy)).expect("lowers");
        let err = to_scenario(&ir).expect_err("must refuse");
        assert!(matches!(err, IngestError::YieldNotRepresentable { .. }));
        let msg = err.to_string();
        assert!(msg.contains("no yield"), "{msg}");
        assert!(
            msg.contains("still passes"),
            "must say why dropping is worse"
        );
    }

    /// Likewise a scheduling policy: TaskDef has no field for it, and running an
    /// RT task as SCHED_NORMAL removes the starvation the test exists to show.
    #[test]
    fn non_normal_policy_is_refused_rather_than_downgraded() {
        let ir = lower(&simple_source(SourceWorkType::RtStarvation {
            rt_workers: 1,
            cfs_workers: 1,
            rt_priority: 80,
            burst_iters: 10,
        }))
        .expect("lowers");
        let err = to_scenario(&ir).expect_err("must refuse");
        match &err {
            IngestError::PolicyNotRepresentable { policy, .. } => assert_eq!(policy, "Fifo"),
            other => panic!("expected PolicyNotRepresentable, got {other:?}"),
        }
    }

    /// Wake edges must survive with correct pid mapping, or a ping-pong becomes
    /// two tasks that never interact.
    #[test]
    fn wake_edges_survive_with_consistent_pids() {
        let ir = lower(&simple_source(SourceWorkType::FutexPingPong {
            spin_iters: 10,
        }))
        .expect("lowers");
        let s = to_scenario(&ir).expect("ingests");
        assert_eq!(s.tasks.len(), 2);
        let p0 = s.tasks[0].pid;
        let p1 = s.tasks[1].pid;
        assert!(wakes(&s.tasks[0].behavior.phases).contains(&p1.0));
        assert!(wakes(&s.tasks[1].behavior.phases).contains(&p0.0));
    }

    #[test]
    fn cpuset_resolves_llc_and_partition_against_topology() {
        let topo = Topology {
            numa_nodes: 1,
            llcs: 2,
            cores_per_llc: 2,
            threads_per_core: 1,
        };
        assert_eq!(
            resolve_cpuset(&CpuSet::Llc(1), &topo, "x").unwrap(),
            vec![CpuId(2), CpuId(3)]
        );
        assert_eq!(
            resolve_cpuset(&CpuSet::Partition { index: 0, of: 2 }, &topo, "x").unwrap(),
            vec![CpuId(0), CpuId(1)]
        );
        assert_eq!(resolve_cpuset(&CpuSet::All, &topo, "x").unwrap().len(), 4);
    }

    /// An out-of-range cpuset must fail loudly: silently clamping would put a
    /// task on a CPU the author did not ask for.
    #[test]
    fn out_of_range_cpuset_is_rejected() {
        let topo = Topology::default();
        assert!(matches!(
            resolve_cpuset(&CpuSet::Llc(9), &topo, "x"),
            Err(IngestError::CpuSetUnresolvable { .. })
        ));
        assert!(matches!(
            resolve_cpuset(&CpuSet::explicit([99]), &topo, "x"),
            Err(IngestError::CpuSetUnresolvable { .. })
        ));
    }

    /// An empty cpuset is worse than an error: the task never runs and the
    /// simulation quietly does nothing.
    #[test]
    fn empty_cpuset_is_rejected_with_its_consequence_stated() {
        let topo = Topology::default();
        let err = resolve_cpuset(&CpuSet::Explicit(vec![]), &topo, "cgroup `x`").unwrap_err();
        assert!(err.to_string().contains("never be scheduled"), "{err}");
    }

    /// Observations are requests for a record; dropping one is indistinguishable
    /// from producing an empty one.
    #[test]
    fn observe_is_refused() {
        let mut ir = WorkloadIr::new("t", Topology::default(), DurationNs::from_secs(1));
        let mut t = Task::new(TaskId(0), "w");
        t.phases = vec![Phase::Run(DurationNs::from_millis(1))];
        ir.tasks.push(t);
        ir.timeline.push(TimedMutation {
            at: DurationNs::ZERO,
            mutation: Mutation::Observe {
                label: "mid".into(),
                probe: crate::ir::Probe::RunqueueState,
            },
        });
        assert!(matches!(
            to_scenario(&ir),
            Err(IngestError::ObserveNotRepresentable { .. })
        ));
    }

    /// Mid-run cgroup creation must land on the timeline at its absolute time.
    #[test]
    fn timeline_cgroup_creation_becomes_a_scenario_event() {
        let mut ir = WorkloadIr::new("t", Topology::default(), DurationNs::from_secs(10));
        ir.cgroups.push(Cgroup::named("late"));
        let mut t = Task::new(TaskId(0), "w");
        t.phases = vec![Phase::Run(DurationNs::from_millis(1))];
        ir.tasks.push(t);
        ir.timeline.push(TimedMutation {
            at: DurationNs::from_secs(5),
            mutation: Mutation::CreateCgroup(Cgroup::named("late")),
        });
        let s = to_scenario(&ir).expect("ingests");
        assert_eq!(s.cgroup_create_events.len(), 1);
        assert_eq!(s.cgroup_create_events[0].at_ns, 5_000_000_000);
    }

    /// An empty task set makes Scenario::build() panic. Surface it as an error
    /// instead: a panic from inside the simulator points at simulator source,
    /// not at the workload that caused it.
    #[test]
    fn empty_task_set_is_an_error_not_a_panic() {
        let ir = WorkloadIr::new("empty", Topology::default(), DurationNs::from_secs(1));
        let err = to_scenario(&ir).expect_err("must refuse");
        assert!(matches!(err, IngestError::NoTasks { .. }));
        assert!(err.to_string().contains("mistaken for a clean result"));
    }

    /// `to_scenario()` is a public boundary used by deserialized and hand-built
    /// IR as well as by `lower()`. It must enforce the structural invariants
    /// itself instead of assuming every caller already validated them.
    #[test]
    fn public_ingress_rejects_invalid_ir_before_translation() {
        let mut ir = WorkloadIr::new("invalid", Topology::default(), DurationNs::from_secs(1));
        let mut task = Task::new(TaskId(0), "worker");
        task.repeat = Repeat::Once;
        task.phases = vec![Phase::Park, Phase::Yield];
        ir.tasks.push(task);

        assert!(matches!(
            to_scenario(&ir),
            Err(IngestError::InvalidIr(
                ValidationError::NonTerminalPark { .. }
            ))
        ));
    }

    /// nice must survive; it is the only priority signal the simulator has, so
    /// losing it would flatten a tiered workload.
    #[test]
    fn nice_values_survive_ingestion() {
        let mut ir = WorkloadIr::new("t", Topology::default(), DurationNs::from_secs(1));
        let mut task = Task::new(TaskId(0), "hi");
        task.nice = Nice(-10);
        task.phases = vec![Phase::Run(DurationNs::from_millis(1))];
        ir.tasks.push(task);
        let s = to_scenario(&ir).expect("ingests");
        assert_eq!(s.tasks[0].nice, -10);
    }

    #[test]
    fn cgroup_membership_and_bandwidth_survive() {
        let mut ir = WorkloadIr::new("t", Topology::default(), DurationNs::from_secs(1));
        let mut cg = Cgroup::named("throttled");
        cg.bandwidth = Some(crate::ir::Bandwidth {
            quota: DurationNs::from_millis(50),
            period: DurationNs::from_millis(100),
        });
        ir.cgroups.push(cg);
        let mut task = Task::new(TaskId(0), "w");
        task.cgroup = Some(CgroupName::new("throttled"));
        task.phases = vec![Phase::Run(DurationNs::from_millis(1))];
        ir.tasks.push(task);

        let s = to_scenario(&ir).expect("ingests");
        assert_eq!(s.tasks[0].cgroup_name.as_deref(), Some("throttled"));
        let bw = s.cgroups[0]
            .bandwidth
            .as_ref()
            .expect("bandwidth carried through");
        assert_eq!(bw.quota_us, 50_000);
        assert_eq!(bw.period_us, 100_000);
    }
}
