//! The restricted workload IR.
//!
//! Deliberately a *short hop* from `scx_simulator::Scenario`: topology, a cgroup
//! tree, a task set, timed mutation events, a duration and a seed. Ingestion on
//! the simulator side should be a field-by-field walk, not a second compiler.
//!
//! It is not identical to `Scenario`, for two reasons:
//!
//! * It is a compiler target, so it keeps things `Scenario` has no slot for —
//!   most importantly the [`crate::FidelityReport`], which must survive as far
//!   as whoever decides whether this workload can answer their question.
//! * It adds [`Phase::Yield`]. `Scenario`'s phases are Run/Sleep/Wake, but
//!   `sched_yield` is scheduler state, not compute — the simulator's own trace
//!   already counts yields. Folding yields into `Run` would erase exactly what a
//!   yield-heavy workload exists to test.

use serde::{Deserialize, Serialize};

use crate::fidelity::FidelityReport;
use crate::units::{CgroupName, CpuIndex, DurationNs, Nice, TaskId};

/// CPU topology the workload runs on.
///
/// Mirrors the three dimensions `Scenario` carries (`nr_cpus`,
/// `smt_threads_per_core`, `cpus_per_llc`) but is expressed the way the source
/// declares it — LLCs x cores-per-LLC x threads-per-core — so lowering does not
/// have to guess a factorisation it was never given.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Topology {
    pub numa_nodes: u32,
    pub llcs: u32,
    pub cores_per_llc: u32,
    pub threads_per_core: u32,
}

impl Default for Topology {
    /// Matches ktstr's `#[ktstr_test]` defaults (1 node, 1 LLC, 2 cores, 1 thread).
    fn default() -> Self {
        Topology {
            numa_nodes: 1,
            llcs: 1,
            cores_per_llc: 1,
            threads_per_core: 1,
        }
    }
}

impl Topology {
    pub fn total_cpus(&self) -> u32 {
        self.llcs
            .saturating_mul(self.cores_per_llc)
            .saturating_mul(self.threads_per_core)
    }

    /// `Scenario.cpus_per_llc`. Zero LLCs would be a malformed topology; the
    /// validator rejects it before this is called.
    pub fn cpus_per_llc(&self) -> u32 {
        self.cores_per_llc.saturating_mul(self.threads_per_core)
    }
}

/// Which CPUs a cgroup or task may run on.
///
/// Resolved against the topology at ingestion rather than at lowering time, so
/// the IR stays symbolic and a backend can bind its own CPU numbering — the same
/// property that lets ktstr and the simulator disagree about CPU count without
/// the IR lying to either.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CpuSet {
    /// Every CPU in the topology.
    All,
    /// Every CPU of one LLC.
    Llc(u32),
    /// Every CPU of one NUMA node.
    NumaNode(u32),
    /// Partition `of` ways, take partition `index`.
    Partition { index: u32, of: u32 },
    /// An explicit set. Sorted and deduplicated by the constructor.
    Explicit(Vec<CpuIndex>),
}

impl CpuSet {
    pub fn explicit(cpus: impl IntoIterator<Item = u32>) -> Self {
        let mut v: Vec<CpuIndex> = cpus.into_iter().map(CpuIndex).collect();
        v.sort_unstable();
        v.dedup();
        CpuSet::Explicit(v)
    }
}

/// cgroup CPU bandwidth (`cpu.max`): `quota` per `period`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bandwidth {
    pub quota: DurationNs,
    pub period: DurationNs,
}

/// A cgroup in the declared hierarchy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cgroup {
    pub name: CgroupName,
    /// `None` = child of root.
    pub parent: Option<CgroupName>,
    /// `None` = inherit the parent's cpuset.
    pub cpuset: Option<CpuSet>,
    pub bandwidth: Option<Bandwidth>,
    /// cgroup-v2 `cpu.weight`, when the source set one.
    pub weight: Option<u32>,
}

impl Cgroup {
    pub fn named(name: impl Into<String>) -> Self {
        Cgroup {
            name: CgroupName::new(name),
            parent: None,
            cpuset: None,
            bandwidth: None,
            weight: None,
        }
    }
}

/// Scheduling policy, as the scheduler sees it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SchedPolicy {
    #[default]
    Normal,
    Batch,
    Idle,
    Fifo {
        priority: i32,
    },
    RoundRobin {
        priority: i32,
    },
}

/// One step of a task's scripted behaviour.
///
/// This is the restricted vocabulary the whole design turns on: the simulator
/// models elapsed time and scheduler state, so every one of ktstr's 45 work
/// types must land here or be refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    /// Occupy a CPU for this long.
    Run(DurationNs),
    /// Be off-CPU, unrunnable, for this long. Covers sleeping and blocking
    /// alike — the distinction is a mechanism the simulator does not model.
    Sleep(DurationNs),
    /// `sched_yield`: stay runnable, give up the CPU now.
    Yield,
    /// Make another task runnable.
    Wake(TaskId),
}

/// How a task's phase sequence repeats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Repeat {
    Once,
    Times(u32),
    /// Until the workload's duration elapses.
    Forever,
}

/// A task, with its scheduler-visible attributes and its scripted behaviour.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    /// Source-meaningful label, carried through for diagnostics.
    pub name: String,
    pub cgroup: Option<CgroupName>,
    pub nice: Nice,
    pub policy: SchedPolicy,
    pub affinity: Option<CpuSet>,
    /// When the task first becomes runnable.
    pub start: DurationNs,
    pub phases: Vec<Phase>,
    pub repeat: Repeat,
}

impl Task {
    pub fn new(id: TaskId, name: impl Into<String>) -> Self {
        Task {
            id,
            name: name.into(),
            cgroup: None,
            nice: Nice::default(),
            policy: SchedPolicy::default(),
            affinity: None,
            start: DurationNs::ZERO,
            phases: Vec::new(),
            repeat: Repeat::Forever,
        }
    }
}

/// A change to the world at a point in time.
///
/// Each variant has a direct counterpart in `Scenario`'s event lists, which is
/// what keeps ingestion a short hop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mutation {
    CreateCgroup(Cgroup),
    DestroyCgroup(CgroupName),
    SetCpuset {
        cgroup: CgroupName,
        cpus: CpuSet,
    },
    /// Return a cgroup to its parent's cpuset.
    ClearCpuset {
        cgroup: CgroupName,
    },
    MoveTasks {
        from: CgroupName,
        to: CgroupName,
    },
    SetBandwidth {
        cgroup: CgroupName,
        bandwidth: Option<Bandwidth>,
    },
    /// Ask the simulator for state and append it to the run's trace record.
    ///
    /// The owner's framing: introspection is a *simulator system call*. There is
    /// no userspace to return to, so an observation is a write into a global
    /// record of the run, not a value handed back to a caller. The IR reserves
    /// the shape; what is observable is the backend's business.
    Observe {
        label: String,
        probe: Probe,
    },
}

/// What an [`Mutation::Observe`] asks the simulator for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Probe {
    /// Per-CPU runqueue state.
    RunqueueState,
    /// Which tasks are in a cgroup.
    CgroupMembers(CgroupName),
    /// A named scheduler-internal value the backend knows how to read (e.g. a
    /// BPF global or per-CPU field). Opaque here on purpose: the IR does not
    /// model the scheduler's internals, it only carries the request.
    SchedulerValue(String),
}

/// A mutation scheduled at a time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimedMutation {
    pub at: DurationNs,
    pub mutation: Mutation,
}

/// A complete lowered workload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkloadIr {
    /// Source-meaningful name (usually the ktstr test name).
    pub name: String,
    pub topology: Topology,
    pub cgroups: Vec<Cgroup>,
    pub tasks: Vec<Task>,
    /// Sorted by `at` by the lowering.
    pub timeline: Vec<TimedMutation>,
    pub duration: DurationNs,
    /// PRNG seed, so a lowered workload is reproducible.
    pub seed: u64,
    /// What this lowering cost in fidelity. Never empty silently — see
    /// [`crate::fidelity`].
    pub fidelity: FidelityReport,
}

impl WorkloadIr {
    pub fn new(name: impl Into<String>, topology: Topology, duration: DurationNs) -> Self {
        WorkloadIr {
            name: name.into(),
            topology,
            cgroups: Vec::new(),
            tasks: Vec::new(),
            timeline: Vec::new(),
            duration,
            seed: 42,
            fidelity: FidelityReport::new(),
        }
    }

    pub fn task(&self, id: TaskId) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }

    /// Structural checks a backend may assume were already done.
    ///
    /// Runs before the IR is handed out, so a malformed workload fails at the
    /// compiler rather than deep inside a simulation where the symptom would be
    /// a wrong number instead of an error.
    pub fn validate(&self) -> Result<(), ValidationError> {
        let t = &self.topology;
        if t.numa_nodes == 0 || t.llcs == 0 || t.cores_per_llc == 0 || t.threads_per_core == 0 {
            return Err(ValidationError::EmptyTopology(*t));
        }
        if !t.llcs.is_multiple_of(t.numa_nodes) {
            return Err(ValidationError::LlcsNotDivisibleByNodes {
                llcs: t.llcs,
                numa_nodes: t.numa_nodes,
            });
        }

        let mut seen_cg = std::collections::BTreeSet::new();
        for cg in &self.cgroups {
            if !seen_cg.insert(cg.name.clone()) {
                return Err(ValidationError::DuplicateCgroup(cg.name.clone()));
            }
        }
        for cg in &self.cgroups {
            if let Some(p) = &cg.parent {
                if !seen_cg.contains(p) {
                    return Err(ValidationError::UnknownCgroup {
                        referenced_by: cg.name.to_string(),
                        name: p.clone(),
                    });
                }
            }
        }

        let mut seen_task = std::collections::BTreeSet::new();
        for task in &self.tasks {
            if !seen_task.insert(task.id) {
                return Err(ValidationError::DuplicateTask(task.id));
            }
        }
        for task in &self.tasks {
            if let Some(cg) = &task.cgroup {
                if !seen_cg.contains(cg) {
                    return Err(ValidationError::UnknownCgroup {
                        referenced_by: task.name.clone(),
                        name: cg.clone(),
                    });
                }
            }
            // A Wake naming a task that does not exist would silently never fire.
            for ph in &task.phases {
                if let Phase::Wake(target) = ph {
                    if !seen_task.contains(target) {
                        return Err(ValidationError::UnknownTask {
                            referenced_by: task.name.clone(),
                            id: *target,
                        });
                    }
                }
            }
        }

        for tm in &self.timeline {
            if tm.at > self.duration {
                return Err(ValidationError::MutationAfterEnd {
                    at: tm.at,
                    duration: self.duration,
                });
            }
        }
        if self.timeline.windows(2).any(|w| w[0].at > w[1].at) {
            return Err(ValidationError::TimelineNotSorted);
        }
        Ok(())
    }
}

/// A structurally malformed IR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    EmptyTopology(Topology),
    LlcsNotDivisibleByNodes {
        llcs: u32,
        numa_nodes: u32,
    },
    DuplicateCgroup(CgroupName),
    DuplicateTask(TaskId),
    UnknownCgroup {
        referenced_by: String,
        name: CgroupName,
    },
    UnknownTask {
        referenced_by: String,
        id: TaskId,
    },
    MutationAfterEnd {
        at: DurationNs,
        duration: DurationNs,
    },
    TimelineNotSorted,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValidationError::EmptyTopology(t) => write!(
                f,
                "topology has a zero dimension: {}n/{}l/{}c/{}t",
                t.numa_nodes, t.llcs, t.cores_per_llc, t.threads_per_core
            ),
            ValidationError::LlcsNotDivisibleByNodes { llcs, numa_nodes } => write!(
                f,
                "llcs ({llcs}) is not divisible by numa_nodes ({numa_nodes}); LLCs cannot straddle nodes"
            ),
            ValidationError::DuplicateCgroup(n) => write!(f, "duplicate cgroup `{n}`"),
            ValidationError::DuplicateTask(id) => write!(f, "duplicate task {id}"),
            ValidationError::UnknownCgroup { referenced_by, name } => {
                write!(f, "`{referenced_by}` references undeclared cgroup `{name}`")
            }
            ValidationError::UnknownTask { referenced_by, id } => {
                write!(f, "`{referenced_by}` wakes undeclared task {id}")
            }
            ValidationError::MutationAfterEnd { at, duration } => write!(
                f,
                "timeline mutation at {at} is past the workload duration {duration}"
            ),
            ValidationError::TimelineNotSorted => f.write_str("timeline is not sorted by time"),
        }
    }
}

impl std::error::Error for ValidationError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn ir() -> WorkloadIr {
        WorkloadIr::new("t", Topology::default(), DurationNs::from_secs(1))
    }

    #[test]
    fn topology_arithmetic() {
        let t = Topology {
            numa_nodes: 2,
            llcs: 4,
            cores_per_llc: 3,
            threads_per_core: 2,
        };
        assert_eq!(t.total_cpus(), 24);
        assert_eq!(t.cpus_per_llc(), 6);
    }

    #[test]
    fn explicit_cpuset_is_sorted_and_deduped() {
        assert_eq!(
            CpuSet::explicit([3, 1, 3, 0]),
            CpuSet::Explicit(vec![CpuIndex(0), CpuIndex(1), CpuIndex(3)])
        );
    }

    #[test]
    fn default_ir_validates() {
        assert_eq!(ir().validate(), Ok(()));
    }

    #[test]
    fn zero_topology_dimension_rejected() {
        let mut w = ir();
        w.topology.cores_per_llc = 0;
        assert!(matches!(
            w.validate(),
            Err(ValidationError::EmptyTopology(_))
        ));
    }

    #[test]
    fn llcs_must_divide_by_nodes() {
        let mut w = ir();
        w.topology = Topology {
            numa_nodes: 2,
            llcs: 3,
            cores_per_llc: 1,
            threads_per_core: 1,
        };
        assert!(matches!(
            w.validate(),
            Err(ValidationError::LlcsNotDivisibleByNodes { .. })
        ));
    }

    /// A dangling Wake is the dangerous one: it would simply never fire, and the
    /// run would look fine while testing nothing.
    #[test]
    fn wake_of_undeclared_task_is_rejected() {
        let mut w = ir();
        let mut t = Task::new(TaskId(0), "waker");
        t.phases = vec![Phase::Wake(TaskId(9))];
        w.tasks.push(t);
        assert_eq!(
            w.validate(),
            Err(ValidationError::UnknownTask {
                referenced_by: "waker".into(),
                id: TaskId(9)
            })
        );
    }

    #[test]
    fn task_in_undeclared_cgroup_is_rejected() {
        let mut w = ir();
        let mut t = Task::new(TaskId(0), "worker");
        t.cgroup = Some(CgroupName::new("ghost"));
        w.tasks.push(t);
        assert!(matches!(
            w.validate(),
            Err(ValidationError::UnknownCgroup { .. })
        ));
    }

    #[test]
    fn duplicate_cgroup_and_task_rejected() {
        let mut w = ir();
        w.cgroups = vec![Cgroup::named("a"), Cgroup::named("a")];
        assert!(matches!(
            w.validate(),
            Err(ValidationError::DuplicateCgroup(_))
        ));

        let mut w = ir();
        w.tasks = vec![Task::new(TaskId(0), "x"), Task::new(TaskId(0), "y")];
        assert!(matches!(
            w.validate(),
            Err(ValidationError::DuplicateTask(_))
        ));
    }

    #[test]
    fn cgroup_parent_must_exist() {
        let mut w = ir();
        let mut child = Cgroup::named("child");
        child.parent = Some(CgroupName::new("missing"));
        w.cgroups = vec![child];
        assert!(matches!(
            w.validate(),
            Err(ValidationError::UnknownCgroup { .. })
        ));
    }

    #[test]
    fn timeline_must_be_sorted_and_within_duration() {
        let mut w = ir();
        w.timeline = vec![
            TimedMutation {
                at: DurationNs::from_millis(500),
                mutation: Mutation::DestroyCgroup(CgroupName::new("a")),
            },
            TimedMutation {
                at: DurationNs::from_millis(100),
                mutation: Mutation::DestroyCgroup(CgroupName::new("b")),
            },
        ];
        assert_eq!(w.validate(), Err(ValidationError::TimelineNotSorted));

        let mut w = ir();
        w.timeline = vec![TimedMutation {
            at: DurationNs::from_secs(99),
            mutation: Mutation::DestroyCgroup(CgroupName::new("a")),
        }];
        assert!(matches!(
            w.validate(),
            Err(ValidationError::MutationAfterEnd { .. })
        ));
    }

    /// The IR is a wire format between two repos; a silent serde break would
    /// desynchronise ktstr and the simulator.
    #[test]
    fn ir_round_trips_through_json() {
        let mut w = ir();
        w.cgroups.push(Cgroup::named("cg_0"));
        let mut t = Task::new(TaskId(0), "w0");
        t.cgroup = Some(CgroupName::new("cg_0"));
        t.phases = vec![
            Phase::Run(DurationNs::from_millis(1)),
            Phase::Sleep(DurationNs::from_micros(500)),
            Phase::Yield,
        ];
        w.tasks.push(t);
        w.timeline.push(TimedMutation {
            at: DurationNs::from_millis(10),
            mutation: Mutation::Observe {
                label: "mid".into(),
                probe: Probe::RunqueueState,
            },
        });
        assert_eq!(w.validate(), Ok(()));

        let json = serde_json::to_string(&w).expect("serialize");
        let back: WorkloadIr = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(w, back);
    }
}
