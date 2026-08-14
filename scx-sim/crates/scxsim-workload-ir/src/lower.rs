//! The lowering compiler: ktstr's op vocabulary down to the restricted IR.
//!
//! # The modelling premise, and what follows from it
//!
//! The simulator models how much real time passes and what the scheduler sees.
//! It has no caches, no page tables, no devices. So a work type whose point is a
//! cache footprint has no dimension to land in, and the honest thing is to keep
//! the part that *is* modelled — the time it occupies a CPU — and record that
//! the rest was dropped.
//!
//! That makes the lowering permissive on purpose. Three outcomes, no fourth:
//!
//! * **Exact** — the scheduler-visible semantics survive.
//! * **Approximated** — lowered to time plus scheduler state, with the dropped
//!   dimension and its values recorded in the [`FidelityReport`].
//! * **Refused** — [`LoweringError`]. A construct that cannot be expressed
//!   without inventing behaviour is rejected. `WorkType::Custom` is a raw Rust
//!   fn pointer; `Schbench` and `Taobench` are real benchmark binaries. Emitting
//!   a phase list for any of them would be fabricating a workload nobody
//!   modelled, and fabricated data gets believed.
//!
//! # Iteration counts
//!
//! ktstr tunes several work types in *iterations*, which is not a unit the
//! simulator advances in. Converting them requires a rate, and there is no
//! honest one — it depends on the machine. [`ITER_NS`] is an explicit, single,
//! documented constant, and every use of it is recorded as a
//! [`Cause::IterationsToTime`] approximation so no reader mistakes a converted
//! duration for a measured one.

use crate::fidelity::{Approximation, Cause, FidelityReport};
use crate::ir::{
    Bandwidth, Cgroup, CpuSet, Mutation, Phase, Repeat, SchedPolicy, Task, TimedMutation, Topology,
    WorkloadIr,
};
use crate::source::*;
use crate::units::{CgroupName, DurationNs, Nice, TaskId};

/// Nominal cost of one source "iteration" when a work type is tuned in
/// iterations rather than time.
///
/// This is a stipulated constant, not a measurement. It exists because the
/// alternative — refusing every iteration-tuned work type — would reject a large
/// part of ktstr's vocabulary for a unit-conversion reason rather than a
/// modelling one. Every conversion through it is recorded as an approximation.
pub const ITER_NS: u64 = 100;

/// Chunk length used when a work type has a yield point but does not say how
/// much work sits between yields.
///
/// This is an INVENTED number and every use of it records an approximation.
/// It must never be used for a work type that runs continuously — see
/// [`CONTINUOUS_RUN`] and the measurement that motivated the split.
const DEFAULT_SLICE: DurationNs = DurationNs::from_micros(500);

/// A construct the lowering will not invent behaviour for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoweringError {
    /// Recognised, deliberately refused.
    Unsupported { construct: String, why: String },
    /// Structurally impossible input (e.g. a step referencing a cgroup that was
    /// never declared).
    Malformed(String),
    /// The IR the lowering produced does not satisfy its own invariants. A bug
    /// in the lowering, surfaced rather than shipped.
    ProducedInvalidIr(crate::ir::ValidationError),
}

impl std::fmt::Display for LoweringError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoweringError::Unsupported { construct, why } => write!(
                f,
                "cannot lower {construct}: {why}. Refusing rather than emitting an \
                 approximation — a fabricated workload would be indistinguishable \
                 from a real one in the results."
            ),
            LoweringError::Malformed(m) => write!(f, "malformed source scenario: {m}"),
            LoweringError::ProducedInvalidIr(e) => {
                write!(
                    f,
                    "lowering produced invalid IR (this is a compiler bug): {e}"
                )
            }
        }
    }
}

impl std::error::Error for LoweringError {}

/// Lower a ktstr scenario to the restricted IR.
///
/// Returns the IR with its [`FidelityReport`] attached, or refuses. The IR is
/// validated before it is handed back, so a caller never receives a structurally
/// broken workload.
pub fn lower(scenario: &SourceScenario) -> Result<WorkloadIr, LoweringError> {
    let mut ctx = Ctx {
        report: FidelityReport::new(),
        next_task: 0,
        default_workers: scenario.default_workers_per_cgroup.max(1),
        scenario_duration: scenario.duration,
    };

    let topology = Topology {
        numa_nodes: scenario.topology.numa_nodes,
        llcs: scenario.topology.llcs,
        cores_per_llc: scenario.topology.cores,
        threads_per_core: scenario.topology.threads,
    };
    let mut ir = WorkloadIr::new(scenario.name.clone(), topology, scenario.duration);

    // ktstr holds are relative (a fraction of the scenario, or a fixed span);
    // the IR timeline is absolute. Walk the steps accumulating a clock.
    let mut clock = DurationNs::ZERO;
    for (idx, step) in scenario.steps.iter().enumerate() {
        lower_step(&mut ir, &mut ctx, step, clock, idx)?;
        clock = clock.saturating_add(hold_duration(step.hold, scenario.duration, &mut ctx, idx));
    }

    ir.timeline.sort_by_key(|t| t.at);
    ir.fidelity = ctx.report;
    ir.validate().map_err(LoweringError::ProducedInvalidIr)?;
    Ok(ir)
}

struct Ctx {
    report: FidelityReport,
    next_task: u32,
    default_workers: u32,
    /// How long the whole scenario runs.
    ///
    /// Needed by the CONTINUOUS work types: a task that never yields must be
    /// given a run phase that outlasts the run, so the only thing that can end
    /// its slice is the scheduler. See [`CONTINUOUS_RUN`].
    scenario_duration: DurationNs,
}

impl Ctx {
    fn task_id(&mut self) -> TaskId {
        let id = TaskId(self.next_task);
        self.next_task += 1;
        id
    }

    fn approx(&mut self, source: &str, lowered_to: String, cause: Cause, dropped: String) {
        self.report.record(Approximation {
            source: source.to_string(),
            lowered_to,
            cause,
            dropped,
        });
    }

    /// Supply a work quantum the source did not specify — and say so.
    ///
    /// Every use of [`DEFAULT_SLICE`] must go through here. Returning the
    /// constant directly is what let `SpinWait` fabricate the scheduling
    /// quantum while reporting EXACT; routing it through a method that records
    /// makes the honest path the only path.
    fn invented_slice(&mut self, source: &str, what: &str) -> DurationNs {
        self.approx(
            source,
            format!("Run({DEFAULT_SLICE})"),
            Cause::UnspecifiedWorkQuantum,
            format!(
                "{what}: the source declares the behaviour but not how much work per \
                 phase; {DEFAULT_SLICE} supplied by the lowering"
            ),
        );
        DEFAULT_SLICE
    }

    /// Convert an iteration count to a duration, recording the conversion.
    fn iters(&mut self, source: &str, label: &str, iters: u64) -> DurationNs {
        let d = DurationNs::from_nanos(iters.saturating_mul(ITER_NS));
        self.approx(
            source,
            format!("{d}"),
            Cause::IterationsToTime,
            format!("{label}={iters} at the stipulated {ITER_NS}ns/iter"),
        );
        d
    }
}

fn hold_duration(
    hold: SourceHold,
    scenario_duration: DurationNs,
    ctx: &mut Ctx,
    step_idx: usize,
) -> DurationNs {
    match hold {
        SourceHold::Fixed(d) => d,
        SourceHold::Frac(f) => {
            let f = if f.is_finite() && f > 0.0 { f } else { 0.0 };
            DurationNs::from_nanos((scenario_duration.as_nanos() as f64 * f) as u64)
        }
        SourceHold::Loop { interval } => {
            // A looping hold repeats its ops until scenario time runs out. The IR
            // timeline is a flat list of absolute events, so the repetition is
            // not represented; the step is given one interval's worth of time.
            ctx.approx(
                &format!("Step[{step_idx}].hold"),
                format!("Fixed({interval})"),
                Cause::TaskLifecycle,
                format!(
                    "HoldSpec::Loop{{interval={interval}}} repeats its ops until the \
                     scenario ends; the IR timeline records one pass"
                ),
            );
            interval
        }
    }
}

fn lower_step(
    ir: &mut WorkloadIr,
    ctx: &mut Ctx,
    step: &SourceStep,
    at: DurationNs,
    step_idx: usize,
) -> Result<(), LoweringError> {
    for def in &step.setup {
        lower_cgroup_def(ir, ctx, def, at, step_idx > 0)?;
    }
    for op in &step.ops {
        lower_op(ir, ctx, op, at, step_idx)?;
    }
    Ok(())
}

/// Declare a cgroup and the tasks it hosts.
///
/// `mid_run` distinguishes a cgroup declared in the first step (part of the
/// initial world) from one appearing later (a timeline event).
fn lower_cgroup_def(
    ir: &mut WorkloadIr,
    ctx: &mut Ctx,
    def: &SourceCgroupDef,
    at: DurationNs,
    mid_run: bool,
) -> Result<(), LoweringError> {
    let cg = Cgroup {
        name: CgroupName::new(&def.name),
        parent: None,
        cpuset: def.cpuset.as_ref().map(lower_cpuset),
        bandwidth: def
            .cpu_quota
            .map(|(quota, period)| Bandwidth { quota, period }),
        weight: def.cpu_weight,
    };

    if ir.cgroups.iter().any(|c| c.name == cg.name) {
        return Err(LoweringError::Malformed(format!(
            "cgroup `{}` declared twice",
            def.name
        )));
    }

    if mid_run {
        ir.timeline.push(TimedMutation {
            at,
            mutation: Mutation::CreateCgroup(cg.clone()),
        });
    }
    ir.cgroups.push(cg);

    // `works` empty means "one default work spec", matching ktstr's
    // merged_works() behaviour.
    let works: Vec<SourceWorkSpec> = if def.works.is_empty() {
        vec![SourceWorkSpec::new(SourceWorkType::SpinWait)]
    } else {
        def.works.clone()
    };

    for work in &works {
        let n = work.workers.unwrap_or(ctx.default_workers).max(1);
        lower_work(ir, ctx, work, n, Some(&def.name), at)?;
    }
    Ok(())
}

fn lower_cpuset(c: &SourceCpuset) -> CpuSet {
    match c {
        SourceCpuset::Llc(i) => CpuSet::Llc(*i),
        SourceCpuset::Numa(i) => CpuSet::NumaNode(*i),
        SourceCpuset::Disjoint { index, of } => CpuSet::Partition {
            index: *index,
            of: *of,
        },
        // Range and Overlap are fractional views of the CPU list. The IR keeps
        // them as partitions; resolving fractions needs the topology, which is
        // the backend's job.
        SourceCpuset::Range { start_frac, .. } => CpuSet::Partition {
            index: (*start_frac).max(0.0) as u32,
            of: 1,
        },
        SourceCpuset::Overlap { index, of, .. } => CpuSet::Partition {
            index: *index,
            of: *of,
        },
        SourceCpuset::Exact(v) => CpuSet::explicit(v.iter().copied()),
    }
}

/// Lower one work spec into `n` tasks.
///
/// This is the table the whole design turns on. Each arm either produces phases
/// (recording what it dropped) or refuses.
fn lower_work(
    ir: &mut WorkloadIr,
    ctx: &mut Ctx,
    work: &SourceWorkSpec,
    n: u32,
    cgroup: Option<&str>,
    start: DurationNs,
) -> Result<(), LoweringError> {
    let src = work.work_type.qualified_name();
    let wt = &work.work_type;

    // Refusals first, so no partially-built task set is left behind.
    match wt {
        SourceWorkType::Custom { name } => {
            return Err(LoweringError::Unsupported {
                construct: format!("WorkType::Custom {{ name: \"{name}\" }}"),
                why: "its body is a raw Rust fn pointer, so there is nothing to lower \
                      — ktstr itself marks it #[serde(skip)] and it cannot even round-trip"
                    .into(),
            })
        }
        SourceWorkType::Schbench => {
            return Err(LoweringError::Unsupported {
                construct: "WorkType::Schbench".into(),
                why: "schbench is a real benchmark binary whose behaviour is not modelled; \
                      a phase list for it would be invented, not derived"
                    .into(),
            })
        }
        SourceWorkType::Taobench => {
            return Err(LoweringError::Unsupported {
                construct: "WorkType::Taobench".into(),
                why: "taobench is a real benchmark binary whose behaviour is not modelled; \
                      a phase list for it would be invented, not derived"
                    .into(),
            })
        }
        _ => {}
    }

    let plan = plan_work(ctx, wt, &src, n)?;
    let base = ctx.next_task;

    for i in 0..plan.tasks {
        let id = ctx.task_id();
        let mut t = Task::new(id, format!("{}#{i}", plan.name_prefix));
        t.cgroup = cgroup.map(CgroupName::new);
        t.start = start;
        t.repeat = plan.repeat;
        t.nice = work
            .nice
            .map(Nice)
            .unwrap_or(plan.role_nice(i).unwrap_or_default());
        if let Some(p) = plan.role_policy(i) {
            t.policy = p;
        }
        t.phases = plan.phases_for(i, base);
        ir.tasks.push(t);
    }
    Ok(())
}

/// What one work type expands into: a task count, per-role attributes, and a
/// phase builder. Keeping this a value (rather than pushing tasks directly)
/// makes the multi-task expansions testable in isolation.
struct Plan {
    name_prefix: String,
    tasks: u32,
    repeat: Repeat,
    kind: PlanKind,
}

enum PlanKind {
    /// Every task runs the same phases.
    Uniform(Vec<Phase>),
    /// Task 0 wakes task 1, task 1 wakes task 0 — a ping-pong pair.
    PingPong { work: DurationNs },
    /// Task 0 wakes each of the others in turn.
    FanOut { work: DurationNs, waiters: u32 },
    /// Task i wakes task i+1; the last one loops back.
    Chain { work: DurationNs },
    /// Roles with distinct nice values: (count, nice) runs.
    Tiered {
        tiers: Vec<(u32, i8)>,
        work: DurationNs,
    },
    /// Roles with distinct policies: (count, policy) runs.
    Classed {
        classes: Vec<(u32, SchedPolicy)>,
        work: DurationNs,
        sleep: DurationNs,
    },
}

impl Plan {
    fn role_nice(&self, i: u32) -> Option<Nice> {
        match &self.kind {
            PlanKind::Tiered { tiers, .. } => {
                let mut acc = 0;
                for (count, nice) in tiers {
                    acc += count;
                    if i < acc {
                        return Some(Nice(*nice));
                    }
                }
                None
            }
            _ => None,
        }
    }

    fn role_policy(&self, i: u32) -> Option<SchedPolicy> {
        match &self.kind {
            PlanKind::Classed { classes, .. } => {
                let mut acc = 0;
                for (count, p) in classes {
                    acc += count;
                    if i < acc {
                        return Some(*p);
                    }
                }
                None
            }
            _ => None,
        }
    }

    /// Phases for role `i`. `base` is the TaskId of role 0, so wake targets are
    /// absolute ids in the finished IR.
    fn phases_for(&self, i: u32, base: u32) -> Vec<Phase> {
        match &self.kind {
            PlanKind::Uniform(p) => p.clone(),
            PlanKind::PingPong { work } => {
                let peer = TaskId(base + if i == 0 { 1 } else { 0 });
                vec![Phase::Run(*work), Phase::Wake(peer), Phase::Sleep(*work)]
            }
            PlanKind::FanOut { work, waiters } => {
                if i == 0 {
                    let mut v = vec![Phase::Run(*work)];
                    for w in 0..*waiters {
                        v.push(Phase::Wake(TaskId(base + 1 + w)));
                    }
                    v.push(Phase::Sleep(*work));
                    v
                } else {
                    vec![Phase::Sleep(*work), Phase::Run(*work)]
                }
            }
            PlanKind::Chain { work } => {
                let next = TaskId(base + (i + 1) % self.tasks);
                vec![Phase::Run(*work), Phase::Wake(next), Phase::Sleep(*work)]
            }
            PlanKind::Tiered { work, .. } => vec![Phase::Run(*work)],
            PlanKind::Classed { work, sleep, .. } => {
                if sleep.is_zero() {
                    vec![Phase::Run(*work)]
                } else {
                    vec![Phase::Run(*work), Phase::Sleep(*sleep)]
                }
            }
        }
    }
}

fn uniform(name: &str, tasks: u32, phases: Vec<Phase>) -> Plan {
    Plan {
        name_prefix: name.to_string(),
        tasks,
        repeat: Repeat::Forever,
        kind: PlanKind::Uniform(phases),
    }
}

/// The lowering table: one arm per ktstr work type.
/// A run phase for work that never voluntarily yields.
///
/// # Why this is not `DEFAULT_SLICE`, and why it mattered by 68x
///
/// ktstr's `SpinWait` is a busy loop with no yield point. Lowering it to a
/// short repeating `Run` chunk inserts voluntary yields the real workload does
/// not have, and the simulator — correctly — ends the slice at each one.
///
/// Measured on `sched_basic_proportional` (2 spinners, 2 cpus, 12 s, `simple`,
/// which requests `SCX_SLICE_DFL` = 20 ms):
///
/// ```text
///   phase len     slices   mean slice      the phase, or the scheduler?
///     0.100ms      19904      0.100ms      phase  (predicted 20000)
///     0.500ms       4013      0.498ms      phase  (predicted 4000)
///     5.000ms        405      4.916ms      phase  (predicted 400)
///    20.000ms        149     13.341ms      scheduler starts to bind
///  1000.000ms        102     19.322ms      scheduler — mean pins to SCX_SLICE_DFL
/// ```
///
/// The engine implements `min(phase, scheduler_slice)`, which is right. At the
/// old 500 us default the phase always won, so the scheduler's slice never
/// bound and the trace showed 48067 slices against the live guest's 710 — a
/// 68x divergence that was entirely self-inflicted by this constant.
///
/// Making the phase outlast the run hands the decision back to the scheduler,
/// which is the whole point of simulating a scheduler.
fn continuous_run(ctx: &Ctx) -> Phase {
    Phase::Run(ctx.scenario_duration)
}

fn plan_work(ctx: &mut Ctx, wt: &SourceWorkType, src: &str, n: u32) -> Result<Plan, LoweringError> {
    use SourceWorkType as W;
    let forever = continuous_run(ctx);
    // THERE IS DELIBERATELY NO BARE `spin = DEFAULT_SLICE` BINDING HERE.
    //
    // There used to be, guarded by a comment asking every arm that reached for
    // it to disclose the invention. Twelve arms did not: they recorded some
    // OTHER cause (IoMechanism, Microarchitectural, TaskLifecycle, ...) and
    // then used the bare value, so the fabricated quantum was never recorded as
    // `UnspecifiedWorkQuantum` — which is the cause the port gate filters on.
    // The old tripwire could not see them because it only examined arms
    // reporting `Exact`, and these do not.
    //
    // A convention that must be remembered at twelve call sites is not a
    // convention. Every arm now calls `ctx.invented_slice`, which returns the
    // value AND records it, so the two cannot come apart. Do not reintroduce a
    // shared binding; take the slice from `invented_slice` at the point of use.
    // `an_invented_quantum_is_recorded_as_one_even_when_something_else_is_too`
    // enforces this.

    let plan = match wt {
        // ---- exact: pure time, nothing dropped -------------------------------
        // Continuous: no yield point, so the scheduler's slice is the only
        // thing that may end it. Genuinely exact — nothing is invented.
        W::SpinWait => uniform("spin", n, vec![forever]),
        // Was EXACT while inventing the work between yields. ktstr says the
        // workload yields often; it does not say how much work sits between
        // yields, so this quantum is the lowering's and must be disclosed.
        W::YieldHeavy => {
            let q = ctx.invented_slice(src, "YieldHeavy work between yields");
            uniform("yield", n, vec![Phase::Run(q), Phase::Yield])
        }
        // Same defect as YieldHeavy, twice over.
        W::Mixed => {
            let q = ctx.invented_slice(src, "Mixed work between yields");
            uniform("mixed", n, vec![Phase::Run(q), Phase::Yield, Phase::Run(q)])
        }
        W::Bursty {
            burst_duration,
            sleep_duration,
        } => uniform(
            "bursty",
            n,
            vec![Phase::Run(*burst_duration), Phase::Sleep(*sleep_duration)],
        ),
        W::Sequence { first, rest } => {
            let mut phases = vec![lower_phase(ctx, src, first)];
            for p in rest {
                phases.push(lower_phase(ctx, src, p));
            }
            uniform("seq", n, phases)
        }

        // ---- approximated: time kept, named dimension dropped ---------------
        W::IdleChurn {
            burst_duration,
            sleep_duration,
            precise_timing,
        } => {
            if *precise_timing {
                ctx.approx(
                    src,
                    format!("Run({burst_duration}) Sleep({sleep_duration})"),
                    Cause::Microarchitectural,
                    "precise_timing=true requests busy-wait timing accuracy the \
                     simulator's discrete clock does not model"
                        .into(),
                );
            }
            uniform(
                "idlechurn",
                n,
                vec![Phase::Run(*burst_duration), Phase::Sleep(*sleep_duration)],
            )
        }
        W::AluHot { width } => {
            let spin = ctx.invented_slice(src, "AluHot spin duration");
            ctx.approx(
                src,
                format!("Run({spin})"),
                Cause::Microarchitectural,
                format!("width={width}: ALU width changes IPC, not elapsed scheduler time"),
            );
            uniform("aluhot", n, vec![Phase::Run(spin)])
        }
        W::SmtSiblingSpin => {
            let spin = ctx.invented_slice(src, "SmtSiblingSpin spin duration");
            ctx.approx(
                src,
                format!("Run({spin})"),
                Cause::Microarchitectural,
                "SMT sibling contention is a pipeline-sharing effect; the simulator \
                 models SMT topology but not shared-core throughput loss"
                    .into(),
            );
            uniform("smtspin", n, vec![Phase::Run(spin)])
        }
        W::IpcVariance {
            hot_iters,
            cold_iters,
            period_iters,
        } => {
            let hot = ctx.iters(src, "hot_iters", *hot_iters);
            let cold = ctx.iters(src, "cold_iters", *cold_iters);
            ctx.approx(
                src,
                format!("Run({hot}) Run({cold})"),
                Cause::Microarchitectural,
                format!(
                    "period_iters={period_iters}: the hot/cold IPC difference is \
                     microarchitectural; both phases become plain CPU time"
                ),
            );
            uniform("ipcvar", n, vec![Phase::Run(hot), Phase::Run(cold)])
        }
        W::CachePressure { size_kib, stride } => {
            let spin = ctx.invented_slice(src, "CachePressure sweep duration");
            ctx.approx(
                src,
                format!("Run({spin})"),
                Cause::Microarchitectural,
                format!("size_kib={size_kib}, stride={stride}"),
            );
            uniform("cachepress", n, vec![Phase::Run(spin)])
        }
        W::CacheYield { size_kib, stride } => {
            let spin = ctx.invented_slice(src, "CacheYield work between yields");
            ctx.approx(
                src,
                format!("Run({spin}) Yield"),
                Cause::Microarchitectural,
                format!("size_kib={size_kib}, stride={stride}"),
            );
            uniform("cacheyield", n, vec![Phase::Run(spin), Phase::Yield])
        }
        W::CachePipe {
            size_kib,
            burst_iters,
        } => {
            let burst = ctx.iters(src, "burst_iters", *burst_iters);
            let spin = ctx.invented_slice(src, "CachePipe blocked duration");
            ctx.approx(
                src,
                format!("Run({burst}) Sleep({spin})"),
                Cause::Microarchitectural,
                format!("size_kib={size_kib}"),
            );
            uniform("cachepipe", n, vec![Phase::Run(burst), Phase::Sleep(spin)])
        }
        W::PageFaultChurn {
            region_kib,
            touches_per_cycle,
            spin_iters,
        } => {
            let run = ctx.iters(src, "spin_iters", *spin_iters);
            ctx.approx(
                src,
                format!("Run({run})"),
                Cause::MemoryPlacement,
                format!("region_kib={region_kib}, touches_per_cycle={touches_per_cycle}"),
            );
            uniform("pfchurn", n, vec![Phase::Run(run)])
        }
        W::NumaWorkingSetSweep {
            region_kib,
            sweep_period_ms,
            target_nodes,
        } => {
            let period = DurationNs::from_millis(*sweep_period_ms);
            ctx.approx(
                src,
                format!("Run({period})"),
                Cause::MemoryPlacement,
                format!("region_kib={region_kib}, target_nodes={target_nodes:?}"),
            );
            uniform("numasweep", n, vec![Phase::Run(period)])
        }
        W::NumaMigrationChurn { period_ms } => {
            let period = DurationNs::from_millis(*period_ms);
            ctx.approx(
                src,
                format!("Run({period})"),
                Cause::MemoryPlacement,
                format!("period_ms={period_ms}: page migration is not modelled"),
            );
            uniform("numamig", n, vec![Phase::Run(period)])
        }

        // ---- I/O: off-CPU time kept, device dropped -------------------------
        //
        // HOW WRONG THE INVENTED DUTY CYCLE CAN BE, measured 2026-08-13 by
        // replicating ktstr's IoSyncWrite inner loop exactly (16 x 4 KiB pwrite
        // per iteration, O_SYNC) -- experiments/io_worktype_offcpu_20260813/ in
        // the dev harness:
        //
        //     tmpfs     O_SYNC    4 KiB     0.0% off-CPU   (no device: a no-op)
        //     disk      O_SYNC    4 KiB    29.8%
        //     disk      O_SYNC   64 KiB    42.8%
        //     disk      buffered           0.0%
        //
        // 0% to 43%, with iteration counts spanning 400x. THE OFF-CPU FRACTION
        // IS A PROPERTY OF THE STORAGE BACKEND, NOT OF THE WORKLOAD, and ktstr
        // chooses that backend at runtime (/dev/vda when present, else a
        // tempfile) -- so two runs of the SAME scenario can sit at different
        // points on that range. `Run(spin) Sleep(spin)` emits 50%, which is
        // DEFAULT_SLICE used twice rather than a claim about I/O.
        //
        // NO TUNED CONSTANT IS INTRODUCED HERE, DELIBERATELY. Fitting one to
        // the 21.4% observed on one host with one filesystem would look like a
        // fix and would be a number chosen to make a comparison agree. Since
        // the fraction is not a workload property, no constant is correct --
        // including a better-measured one. The end state is a DECLARED quantum
        // rather than an invented one; sim-1zqbl carries that.
        W::IoSyncWrite | W::IoRandRead | W::IoConvoy => {
            // These three are NOT one mechanism, though this arm has always
            // treated them as one. IoSyncWrite opens O_SYNC and writes through
            // the page cache; IoRandRead and IoConvoy open O_DIRECT and always
            // reach the device. Their real off-CPU fractions differ for that
            // reason, so the shared arm is itself an approximation and is now
            // named per variant rather than silently collapsed.
            let mechanism = match wt {
                W::IoSyncWrite => "O_SYNC buffered writes",
                W::IoRandRead => "O_DIRECT random reads",
                _ => "O_DIRECT convoy reads",
            };
            let spin = ctx.invented_slice(src, "I/O compute and off-CPU wait durations");
            ctx.approx(
                src,
                format!("Run({spin}) Sleep({spin}) = 50% off-CPU"),
                Cause::IoMechanism,
                format!(
                    "{mechanism}: block device, queue depth and byte counts are not \
                     modelled. The off-CPU wait is represented as a Sleep phase, but \
                     its DURATION is the lowering's invention, not the device's — see \
                     the UnspecifiedWorkQuantum record for the same source. Do not \
                     read this as a preserved wait time. MEASURED reality for this \
                     loop spans 0%-43% off-CPU depending on the storage backend \
                     (tmpfs 0%, disk 4 KiB 30%, disk 64 KiB 43%), so the 50% emitted \
                     here is wrong for every backend but at most one, and a \
                     cross-backend share comparison must not be gated on it."
                ),
            );
            uniform("io", n, vec![Phase::Run(spin), Phase::Sleep(spin)])
        }
        W::PipeIo { burst_iters } => {
            let burst = ctx.iters(src, "burst_iters", *burst_iters);
            let spin = ctx.invented_slice(src, "PipeIo blocked duration");
            ctx.approx(
                src,
                format!("Run({burst}) Sleep({spin})"),
                Cause::BlockingMechanism,
                "pipe read/write becomes a plain block".into(),
            );
            uniform("pipeio", n, vec![Phase::Run(burst), Phase::Sleep(spin)])
        }

        // ---- multi-task expansions ------------------------------------------
        W::FutexPingPong { spin_iters } => {
            let work = ctx.iters(src, "spin_iters", *spin_iters);
            ctx.approx(
                src,
                "2 tasks alternating Wake".into(),
                Cause::BlockingMechanism,
                "futex wait/wake becomes a Sleep/Wake pair".into(),
            );
            Plan {
                name_prefix: "futexpp".into(),
                tasks: 2,
                repeat: Repeat::Forever,
                kind: PlanKind::PingPong { work },
            }
        }
        W::FutexFanOut {
            fan_out,
            spin_iters,
        } => {
            let work = ctx.iters(src, "spin_iters", *spin_iters);
            let waiters = (*fan_out).max(1) as u32;
            ctx.approx(
                src,
                format!("1 waker + {waiters} waiters"),
                Cause::BlockingMechanism,
                "futex wait/wake becomes Sleep/Wake".into(),
            );
            Plan {
                name_prefix: "futexfan".into(),
                tasks: waiters + 1,
                repeat: Repeat::Forever,
                kind: PlanKind::FanOut { work, waiters },
            }
        }
        W::WakeChain {
            depth,
            wake,
            work_per_hop,
        } => {
            let d = (*depth).max(2) as u32;
            ctx.approx(
                src,
                format!("{d}-task wake chain"),
                Cause::BlockingMechanism,
                format!("wake mechanism {wake:?} becomes a plain Wake edge"),
            );
            Plan {
                name_prefix: "wakechain".into(),
                tasks: d,
                repeat: Repeat::Forever,
                kind: PlanKind::Chain {
                    work: *work_per_hop,
                },
            }
        }
        W::MutexContention {
            contenders,
            hold_iters,
            work_iters,
        } => {
            let hold = ctx.iters(src, "hold_iters", *hold_iters);
            let work = ctx.iters(src, "work_iters", *work_iters);
            let c = (*contenders).max(1) as u32;
            ctx.approx(
                src,
                format!("{c} tasks, Run(hold+work)"),
                Cause::BlockingMechanism,
                "mutex acquire/release is not modelled; the critical section becomes \
                 plain CPU time and contention emerges from CPU count alone"
                    .into(),
            );
            let combined = hold.saturating_add(work);
            ctx.approx(
                src,
                format!("Run({combined})"),
                Cause::BlockingMechanism,
                format!(
                    "hold({hold}) + work({work}) collapsed into one run phase; \
                     neither the lock nor the split survives"
                ),
            );
            uniform("mutex", c, vec![Phase::Run(combined)])
        }
        W::ThunderingHerd {
            waiters,
            batches,
            inter_batch_ms,
        } => {
            let w = (*waiters).max(1) as u32;
            let gap = DurationNs::from_millis(*inter_batch_ms);
            ctx.approx(
                src,
                format!("1 waker + {w} waiters"),
                Cause::BlockingMechanism,
                format!("batches={batches}; the herd becomes one waker waking {w} sleepers"),
            );
            Plan {
                name_prefix: "herd".into(),
                tasks: w + 1,
                repeat: Repeat::Forever,
                kind: PlanKind::FanOut {
                    work: gap,
                    waiters: w,
                },
            }
        }
        W::ProducerConsumerImbalance {
            producers,
            consumers,
            produce_rate_hz,
            consume_iters,
        } => {
            let consume = ctx.iters(src, "consume_iters", *consume_iters);
            let p = (*producers).max(1) as u32;
            let c = (*consumers).max(1) as u32;
            let period = if *produce_rate_hz > 0 {
                DurationNs::from_nanos(1_000_000_000 / produce_rate_hz)
            } else {
                // produce_rate_hz = 0 declares no rate at all, so the period
                // below is entirely the lowering's. Disclosed as such; the
                // nonzero branch is a real conversion and is not.
                ctx.invented_slice(src, "ProducerConsumer period (produce_rate_hz = 0)")
            };
            ctx.approx(
                src,
                format!("{p} producers + {c} consumers"),
                Cause::BlockingMechanism,
                format!(
                    "queue depth and the produce/consume handoff are not modelled; \
                     produce_rate_hz={produce_rate_hz} becomes a {period} period"
                ),
            );
            Plan {
                name_prefix: "prodcons".into(),
                tasks: p + c,
                repeat: Repeat::Forever,
                kind: PlanKind::Classed {
                    classes: vec![(p, SchedPolicy::Normal), (c, SchedPolicy::Normal)],
                    work: consume,
                    sleep: period,
                },
            }
        }
        W::PriorityInversion {
            high_count,
            medium_count,
            low_count,
            hold_iters,
            work_iters,
        } => {
            let hold = ctx.iters(src, "hold_iters", *hold_iters);
            let work = ctx.iters(src, "work_iters", *work_iters);
            let (h, m, l) = (
                (*high_count).max(1) as u32,
                *medium_count as u32,
                (*low_count).max(1) as u32,
            );
            ctx.approx(
                src,
                format!("{h} high / {m} medium / {l} low by nice"),
                Cause::BlockingMechanism,
                "the inversion depends on a lock the simulator does not model; the \
                 priority tiers are preserved as nice values"
                    .into(),
            );
            Plan {
                name_prefix: "prioinv".into(),
                tasks: h + m + l,
                repeat: Repeat::Forever,
                kind: PlanKind::Tiered {
                    tiers: vec![(h, -10), (m, 0), (l, 10)],
                    work: hold.saturating_add(work),
                },
            }
        }
        W::RtStarvation {
            rt_workers,
            cfs_workers,
            rt_priority,
            burst_iters,
        } => {
            let burst = ctx.iters(src, "burst_iters", *burst_iters);
            let (r, c) = ((*rt_workers).max(1) as u32, (*cfs_workers).max(1) as u32);
            Plan {
                name_prefix: "rtstarve".into(),
                tasks: r + c,
                repeat: Repeat::Forever,
                kind: PlanKind::Classed {
                    classes: vec![
                        (
                            r,
                            SchedPolicy::Fifo {
                                priority: *rt_priority,
                            },
                        ),
                        (c, SchedPolicy::Normal),
                    ],
                    work: burst,
                    sleep: DurationNs::ZERO,
                },
            }
        }
        W::PreemptStorm {
            cfs_workers,
            rt_burst_iters,
            rt_sleep_us,
        } => {
            let burst = ctx.iters(src, "rt_burst_iters", *rt_burst_iters);
            let c = (*cfs_workers).max(1) as u32;
            // PreemptStorm has NO priority field; 50 is the lowering's. The
            // arm was already non-exact via the iters conversion, which is
            // exactly why this went unnoticed — a non-green report is not the
            // same as a disclosed one.
            ctx.approx(
                src,
                "Fifo { priority: 50 }".into(),
                Cause::UnspecifiedWorkQuantum,
                "PreemptStorm does not specify an RT priority; 50 supplied by the lowering".into(),
            );
            Plan {
                name_prefix: "preemptstorm".into(),
                tasks: c + 1,
                repeat: Repeat::Forever,
                kind: PlanKind::Classed {
                    classes: vec![
                        (1, SchedPolicy::Fifo { priority: 50 }),
                        (c, SchedPolicy::Normal),
                    ],
                    work: burst,
                    sleep: DurationNs::from_micros(*rt_sleep_us),
                },
            }
        }
        W::EpollStorm {
            producers,
            consumers,
            events_per_burst,
        } => {
            let p = (*producers).max(1) as u32;
            let c = (*consumers).max(1) as u32;
            ctx.approx(
                src,
                format!("{p} wakers + {c} waiters"),
                Cause::BlockingMechanism,
                format!("epoll readiness becomes Wake; events_per_burst={events_per_burst}"),
            );
            // EpollStorm says nothing about how long a consumer works per
            // event, so the quantum is the lowering's. Recording the wake
            // mechanism above is NOT recording this — that was the
            // PreemptStorm shape, and the provenance check caught it here.
            let work = ctx.invented_slice(src, "EpollStorm work per event");
            Plan {
                name_prefix: "epoll".into(),
                tasks: p + c,
                repeat: Repeat::Forever,
                kind: PlanKind::FanOut { work, waiters: c },
            }
        }
        W::AsymmetricWaker {
            waker_class,
            wakee_class,
            burst_iters,
        } => {
            let work = ctx.iters(src, "burst_iters", *burst_iters);
            ctx.approx(
                src,
                "2 tasks, waker/wakee with distinct policies".into(),
                Cause::BlockingMechanism,
                "the wake mechanism is not modelled; the classes are preserved".into(),
            );
            Plan {
                name_prefix: "asymwake".into(),
                tasks: 2,
                repeat: Repeat::Forever,
                kind: PlanKind::Classed {
                    classes: vec![
                        (1, sched_class(ctx, src, *waker_class)),
                        (1, sched_class(ctx, src, *wakee_class)),
                    ],
                    work,
                    sleep: work,
                },
            }
        }
        W::FanOutCompute {
            fan_out,
            cache_footprint_kib,
            operations,
            sleep_usec,
        } => {
            let f = (*fan_out).max(1) as u32;
            let work = ctx.iters(src, "operations", *operations as u64);
            ctx.approx(
                src,
                format!("1 + {f} tasks"),
                Cause::Microarchitectural,
                format!("cache_footprint_kib={cache_footprint_kib}"),
            );
            let combined = work.saturating_add(DurationNs::from_micros(*sleep_usec));
            ctx.approx(
                src,
                format!("Run({combined})"),
                Cause::BlockingMechanism,
                format!("operations({work}) + sleep_usec({sleep_usec}us) collapsed into one phase"),
            );
            Plan {
                name_prefix: "fanoutcompute".into(),
                tasks: f + 1,
                repeat: Repeat::Forever,
                kind: PlanKind::FanOut {
                    work: combined,
                    waiters: f,
                },
            }
        }

        // ---- lifecycle / sched-attribute churn ------------------------------
        W::ForkExit => {
            let spin = ctx.invented_slice(src, "ForkExit per-task run duration");
            ctx.approx(
                src,
                format!("Run({spin}) once, {n} task(s)"),
                Cause::TaskLifecycle,
                "repeated fork/exit becomes a fixed task set that runs once; the IR \
                 has no task-creation churn"
                    .into(),
            );
            Plan {
                name_prefix: "forkexit".into(),
                tasks: n,
                repeat: Repeat::Once,
                kind: PlanKind::Uniform(vec![Phase::Run(spin)]),
            }
        }
        W::NiceSweep => {
            let spin = ctx.invented_slice(src, "NiceSweep per-task run duration");
            ctx.approx(
                src,
                format!("Run({spin}) at a fixed nice"),
                Cause::DynamicSchedAttr,
                "the sweep changes nice at runtime; the IR records a single initial \
                 nice per task"
                    .into(),
            );
            uniform("nicesweep", n, vec![Phase::Run(spin)])
        }
        W::AffinityChurn { spin_iters } | W::CrossAffinityChurn { spin_iters } => {
            let run = ctx.iters(src, "spin_iters", *spin_iters);
            ctx.approx(
                src,
                format!("Run({run}) with no affinity change"),
                Cause::DynamicSchedAttr,
                "runtime sched_setaffinity churn is not represented; the IR carries \
                 one initial affinity per task"
                    .into(),
            );
            uniform("affchurn", n, vec![Phase::Run(run)])
        }
        W::PolicyChurn { spin_iters } => {
            let run = ctx.iters(src, "spin_iters", *spin_iters);
            ctx.approx(
                src,
                format!("Run({run}) at a fixed policy"),
                Cause::DynamicSchedAttr,
                "runtime sched_setscheduler churn is not represented".into(),
            );
            uniform("polchurn", n, vec![Phase::Run(run)])
        }
        W::SignalStorm {
            signals_per_iter,
            work_iters,
        } => {
            let run = ctx.iters(src, "work_iters", *work_iters);
            ctx.approx(
                src,
                format!("Run({run})"),
                Cause::BlockingMechanism,
                format!("signals_per_iter={signals_per_iter}: signal delivery is not modelled"),
            );
            uniform("sigstorm", n, vec![Phase::Run(run)])
        }

        // ---- cgroup structures ----------------------------------------------
        W::CgroupChurn { groups, cycle_ms } => {
            ctx.approx(
                src,
                format!("Run({}) x {n}", DurationNs::from_millis(*cycle_ms)),
                Cause::TaskLifecycle,
                format!(
                    "groups={groups}: the create/destroy cycle belongs on the timeline, \
                     which this work type has no way to reach from inside a task"
                ),
            );
            uniform(
                "cgchurn",
                n,
                vec![Phase::Run(DurationNs::from_millis(*cycle_ms))],
            )
        }
        W::CgroupAttachStorm { dest, reap } => {
            let spin = ctx.invented_slice(src, "CgroupAttachStorm per-task run duration");
            ctx.approx(
                src,
                format!("Run({spin})"),
                Cause::TaskLifecycle,
                format!("dest={dest}, reap={reap}: repeated cgroup attach is not modelled"),
            );
            uniform("cgstorm", n, vec![Phase::Run(spin)])
        }

        // ---- periodic external stimulus --------------------------------------
        W::TimerLatency { interval_us } => {
            let iv = DurationNs::from_micros(*interval_us);
            ctx.approx(
                src,
                format!("Sleep({iv}) Run(short)"),
                Cause::BlockingMechanism,
                "the timer becomes a periodic self-wake; timer-delivery latency itself \
                 is not measured by the IR"
                    .into(),
            );
            uniform(
                "timerlat",
                n,
                vec![Phase::Sleep(iv), Phase::Run(DurationNs::from_micros(1))],
            )
        }
        W::NetTraffic {
            interval_us,
            frame_bytes,
        }
        | W::IrqWake {
            interval_us,
            frame_bytes,
        } => {
            let iv = DurationNs::from_micros(*interval_us);
            ctx.approx(
                src,
                format!("Sleep({iv}) Run(short)"),
                Cause::IoMechanism,
                format!(
                    "frame_bytes={frame_bytes}: there is no NIC, so the traffic becomes \
                     a periodic wake"
                ),
            );
            uniform(
                "netirq",
                n,
                vec![Phase::Sleep(iv), Phase::Run(DurationNs::from_micros(1))],
            )
        }

        // Refused above; unreachable.
        W::Custom { .. } | W::Schbench | W::Taobench => unreachable!("refused before planning"),
    };
    Ok(plan)
}

/// The RT priority the lowering supplies for a class that names no number.
///
/// ktstr's `SourceSchedClass` is a class NAME — `Fifo`, `RoundRobin` — with no
/// priority attached. A real-time policy needs one, so this is the lowering's.
/// Every use records it: the mechanical provenance check found this helper
/// after the hand audit had already been through the same arms, because a
/// shared helper's fabrication does not look like a fabrication at the call
/// site.
const SUPPLIED_RT_PRIORITY: i32 = 50;

fn sched_class(ctx: &mut Ctx, src: &str, c: SourceSchedClass) -> SchedPolicy {
    match c {
        SourceSchedClass::Normal => SchedPolicy::Normal,
        SourceSchedClass::Batch => SchedPolicy::Batch,
        SourceSchedClass::Idle => SchedPolicy::Idle,
        SourceSchedClass::Fifo | SourceSchedClass::RoundRobin => {
            ctx.approx(
                src,
                format!("priority {SUPPLIED_RT_PRIORITY}"),
                Cause::UnspecifiedWorkQuantum,
                format!(
                    "SchedClass::{c:?} names no RT priority; {SUPPLIED_RT_PRIORITY} \
                     supplied by the lowering"
                ),
            );
            match c {
                SourceSchedClass::RoundRobin => SchedPolicy::RoundRobin {
                    priority: SUPPLIED_RT_PRIORITY,
                },
                _ => SchedPolicy::Fifo {
                    priority: SUPPLIED_RT_PRIORITY,
                },
            }
        }
    }
}

fn lower_phase(ctx: &mut Ctx, src: &str, p: &SourceWorkPhase) -> Phase {
    match p {
        SourceWorkPhase::Spin(d) => Phase::Run(*d),
        SourceWorkPhase::Sleep(d) => Phase::Sleep(*d),
        // `Phase::Yield` carries no duration, so the declared one is DROPPED.
        // It was dropped silently until the exact-arm audit: a Sequence
        // containing Yield(9ms) reported EXACT with the 9ms gone.
        SourceWorkPhase::Yield(d) => {
            ctx.approx(
                src,
                "Yield".into(),
                Cause::UnrepresentableWorkQuantum,
                format!("WorkPhase::Yield({d}) — Phase::Yield carries no duration"),
            );
            Phase::Yield
        }
        SourceWorkPhase::Io(d) => {
            ctx.approx(
                src,
                format!("Sleep({d})"),
                Cause::IoMechanism,
                "WorkPhase::Io becomes off-CPU time".into(),
            );
            Phase::Sleep(*d)
        }
        SourceWorkPhase::AluHot(d) => {
            ctx.approx(
                src,
                format!("Run({d})"),
                Cause::Microarchitectural,
                "WorkPhase::AluHot becomes plain CPU time".into(),
            );
            Phase::Run(*d)
        }
    }
}

fn lower_op(
    ir: &mut WorkloadIr,
    ctx: &mut Ctx,
    op: &SourceOp,
    at: DurationNs,
    step_idx: usize,
) -> Result<(), LoweringError> {
    let m = match op {
        SourceOp::AddCgroup { name } => Mutation::CreateCgroup(Cgroup::named(name)),
        SourceOp::AddCgroupDef { def } => {
            lower_cgroup_def(ir, ctx, def, at, true)?;
            return Ok(());
        }
        SourceOp::RemoveCgroup { cgroup } => Mutation::DestroyCgroup(CgroupName::new(cgroup)),
        SourceOp::SetCpuset { cgroup, cpus } => Mutation::SetCpuset {
            cgroup: CgroupName::new(cgroup),
            cpus: lower_cpuset(cpus),
        },
        SourceOp::ClearCpuset { cgroup } => Mutation::ClearCpuset {
            cgroup: CgroupName::new(cgroup),
        },
        SourceOp::SwapCpusets { a, b } => {
            // A swap needs both cgroups' current cpusets, which the IR does not
            // track symbolically. Refusing beats emitting one arbitrary half.
            return Err(LoweringError::Unsupported {
                construct: format!("Op::SwapCpusets {{ a: {a}, b: {b} }}"),
                why: "a swap reads both cgroups' live cpusets and writes them back \
                      crossed; the IR carries cpusets symbolically and cannot resolve \
                      the read half without the backend's CPU numbering"
                    .into(),
            });
        }
        SourceOp::MoveAllTasks { from, to } => Mutation::MoveTasks {
            from: CgroupName::new(from),
            to: CgroupName::new(to),
        },
        SourceOp::Observe { label, what } => Mutation::Observe {
            label: label.clone(),
            probe: crate::ir::Probe::SchedulerValue(what.clone()),
        },
        SourceOp::Unmodelled { op } => {
            return Err(LoweringError::Unsupported {
                construct: format!("Op::{op}"),
                why: format!(
                    "`{op}` has no counterpart in the simulator's world (step {step_idx}). \
                     Payload spawning, scheduler attach/detach, IRQ steering and BPF map \
                     pinning act on a VM, not on a simulated scheduler"
                ),
            });
        }
    };
    ir.timeline.push(TimedMutation { at, mutation: m });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scenario_with(wt: SourceWorkType) -> SourceScenario {
        SourceScenario::new("t").step(SourceStep::new(
            vec![SourceCgroupDef::named("cg_0").work(SourceWorkSpec::new(wt).workers(1))],
            SourceHold::Frac(1.0),
        ))
    }

    /// The three refusals are the No-Stub line. Each must fail loudly, and the
    /// message must say why rather than just "unsupported".
    #[test]
    fn custom_schbench_taobench_are_refused_not_approximated() {
        for wt in [
            SourceWorkType::Custom {
                name: "fault_under_lock".into(),
            },
            SourceWorkType::Schbench,
            SourceWorkType::Taobench,
        ] {
            let name = wt.variant_name();
            let err = lower(&scenario_with(wt)).expect_err("must refuse");
            match &err {
                LoweringError::Unsupported { construct, why } => {
                    assert!(construct.contains(name), "{construct} should name {name}");
                    assert!(!why.is_empty(), "refusal must explain itself");
                }
                other => panic!("{name}: expected Unsupported, got {other:?}"),
            }
            // And the message must say it is refusing rather than approximating.
            assert!(err.to_string().contains("Refusing rather than emitting"));
        }
    }

    /// Approximation must never be silent: the report has to name the dropped
    /// values, not merely that something was dropped.
    #[test]
    fn cache_pressure_records_the_footprint_it_discarded() {
        let ir = lower(&scenario_with(SourceWorkType::CachePressure {
            size_kib: 256,
            stride: 64,
        }))
        .expect("lowers");
        assert!(!ir.fidelity.is_exact());
        let a = ir
            .fidelity
            .by_cause(Cause::Microarchitectural)
            .next()
            .expect("records a microarchitectural approximation");
        assert!(a.dropped.contains("size_kib=256"), "got {}", a.dropped);
        assert!(a.dropped.contains("stride=64"), "got {}", a.dropped);
        assert_eq!(a.source, "WorkType::CachePressure");
    }

    /// The work types whose behaviour is FULLY DETERMINED BY THE SOURCE must
    /// lower with no approximation — otherwise the report is noise and callers
    /// stop reading it.
    ///
    /// `YieldHeavy` and `Mixed` were in this list and should never have been.
    /// They declare a yielding pattern but NOT how much work sits between
    /// yields, so the lowering supplies that quantum — and this test asserted
    /// the result was exact, which is how the fabrication survived review. It
    /// was the third test to assert on the false exactness, alongside the
    /// calibration test and the trace comparison. Removing them from here is
    /// the fix, not a relaxation: see
    /// `ai_docs/EXACT_ARM_AUDIT_20260812.md`.
    ///
    /// What remains are the two arms that genuinely carry only declared values:
    /// `SpinWait` (a continuous spin, no quantum to invent) and `Bursty` (both
    /// durations declared).
    #[test]
    fn pure_timing_work_types_lower_exactly() {
        for wt in [
            SourceWorkType::SpinWait,
            SourceWorkType::Bursty {
                burst_duration: DurationNs::from_millis(1),
                sleep_duration: DurationNs::from_millis(2),
            },
        ] {
            let name = wt.variant_name();
            let ir = lower(&scenario_with(wt)).expect("lowers");
            assert!(
                ir.fidelity.is_exact(),
                "{name} should lower exactly, got {:?}",
                ir.fidelity.approximations()
            );
        }
    }

    #[test]
    fn bursty_preserves_its_durations() {
        let ir = lower(&scenario_with(SourceWorkType::Bursty {
            burst_duration: DurationNs::from_millis(1),
            sleep_duration: DurationNs::from_micros(500),
        }))
        .expect("lowers");
        assert_eq!(
            ir.tasks[0].phases,
            vec![
                Phase::Run(DurationNs::from_millis(1)),
                Phase::Sleep(DurationNs::from_micros(500))
            ]
        );
    }

    /// The multi-task expansions are where the IR earns its keep. A ping-pong
    /// must produce two tasks that actually wake each other — and validate(),
    /// which rejects dangling wakes, is what proves the ids line up.
    #[test]
    fn futex_ping_pong_expands_to_a_mutually_waking_pair() {
        let ir = lower(&scenario_with(SourceWorkType::FutexPingPong {
            spin_iters: 1000,
        }))
        .expect("lowers");
        assert_eq!(ir.tasks.len(), 2);
        assert!(ir.tasks[0].phases.contains(&Phase::Wake(ir.tasks[1].id)));
        assert!(ir.tasks[1].phases.contains(&Phase::Wake(ir.tasks[0].id)));
        assert_eq!(ir.validate(), Ok(()));
    }

    #[test]
    fn wake_chain_forms_a_cycle_of_the_requested_depth() {
        let ir = lower(&scenario_with(SourceWorkType::WakeChain {
            depth: 4,
            wake: WakeMechanism::Pipe,
            work_per_hop: DurationNs::from_millis(10),
        }))
        .expect("lowers");
        assert_eq!(ir.tasks.len(), 4);
        // Each task wakes the next; the last wraps to the first.
        for (i, t) in ir.tasks.iter().enumerate() {
            let expect = TaskId(((i + 1) % 4) as u32);
            assert!(
                t.phases.contains(&Phase::Wake(expect)),
                "task {i} should wake {expect}"
            );
        }
        assert_eq!(ir.validate(), Ok(()));
    }

    /// Priority inversion cannot be modelled, but the priority *tiers* can, and
    /// losing them would make the workload meaningless.
    #[test]
    fn priority_inversion_preserves_the_tiers_as_nice_values() {
        let ir = lower(&scenario_with(SourceWorkType::PriorityInversion {
            high_count: 1,
            medium_count: 2,
            low_count: 1,
            hold_iters: 100,
            work_iters: 100,
        }))
        .expect("lowers");
        assert_eq!(ir.tasks.len(), 4);
        let nices: Vec<i8> = ir.tasks.iter().map(|t| t.nice.0).collect();
        assert_eq!(nices, vec![-10, 0, 0, 10]);
    }

    #[test]
    fn rt_starvation_preserves_the_scheduling_classes() {
        let ir = lower(&scenario_with(SourceWorkType::RtStarvation {
            rt_workers: 1,
            cfs_workers: 2,
            rt_priority: 80,
            burst_iters: 50,
        }))
        .expect("lowers");
        assert_eq!(ir.tasks.len(), 3);
        assert_eq!(ir.tasks[0].policy, SchedPolicy::Fifo { priority: 80 });
        assert_eq!(ir.tasks[1].policy, SchedPolicy::Normal);
        assert_eq!(ir.tasks[2].policy, SchedPolicy::Normal);
    }

    /// Iteration->time conversion is a stipulation, not a measurement, so it must
    /// always be recorded.
    #[test]
    fn iteration_counts_are_recorded_as_converted() {
        let ir = lower(&scenario_with(SourceWorkType::AffinityChurn {
            spin_iters: 128,
        }))
        .expect("lowers");
        let conv: Vec<_> = ir.fidelity.by_cause(Cause::IterationsToTime).collect();
        assert_eq!(conv.len(), 1);
        assert!(conv[0].dropped.contains("spin_iters=128"));
        assert!(conv[0].dropped.contains("100ns/iter"));
    }

    /// Ops with no simulator counterpart must be refused with their own name in
    /// the message, so the author knows which line to change.
    #[test]
    fn unmodelled_op_refused_by_name() {
        let s = SourceScenario::new("t").step(SourceStep {
            setup: vec![SourceCgroupDef::named("cg_0")],
            ops: vec![SourceOp::Unmodelled {
                op: "RunPayload".into(),
            }],
            hold: SourceHold::Frac(1.0),
        });
        let err = lower(&s).expect_err("must refuse");
        assert!(err.to_string().contains("RunPayload"), "{err}");
    }

    /// SwapCpusets is refused rather than half-applied — emitting one direction
    /// of a swap would silently produce a different workload.
    #[test]
    fn swap_cpusets_is_refused() {
        let s = SourceScenario::new("t").step(SourceStep {
            setup: vec![SourceCgroupDef::named("a"), SourceCgroupDef::named("b")],
            ops: vec![SourceOp::SwapCpusets {
                a: "a".into(),
                b: "b".into(),
            }],
            hold: SourceHold::Frac(1.0),
        });
        let err = lower(&s).expect_err("must refuse");
        assert!(matches!(err, LoweringError::Unsupported { .. }));
    }

    /// Mid-run cgroups become timeline events at the right absolute time; a
    /// second step's mutations must not land at t=0.
    #[test]
    fn later_steps_become_timeline_events_at_absolute_times() {
        let s = SourceScenario {
            duration: DurationNs::from_secs(10),
            ..SourceScenario::new("t")
        }
        .step(SourceStep::new(
            vec![SourceCgroupDef::named("cg_0")],
            SourceHold::Frac(0.5),
        ))
        .step(SourceStep::new(
            vec![SourceCgroupDef::named("cg_1")],
            SourceHold::Frac(0.5),
        ));
        let ir = lower(&s).expect("lowers");
        assert_eq!(ir.cgroups.len(), 2);
        let created: Vec<_> = ir
            .timeline
            .iter()
            .filter(|t| matches!(t.mutation, Mutation::CreateCgroup(_)))
            .collect();
        assert_eq!(created.len(), 1, "only the second step creates mid-run");
        assert_eq!(created[0].at, DurationNs::from_secs(5));
        assert_eq!(ir.validate(), Ok(()));
    }

    #[test]
    fn duplicate_cgroup_is_malformed() {
        let s = SourceScenario::new("t").step(SourceStep::new(
            vec![SourceCgroupDef::named("dup"), SourceCgroupDef::named("dup")],
            SourceHold::Frac(1.0),
        ));
        assert!(matches!(
            lower(&s).expect_err("must reject"),
            LoweringError::Malformed(_)
        ));
    }

    /// The default worker count must come from the scenario, staying symbolic in
    /// the source, so a backend can bind its own.
    #[test]
    fn default_worker_count_comes_from_the_scenario() {
        let s = SourceScenario {
            default_workers_per_cgroup: 3,
            ..SourceScenario::new("t")
        }
        .step(SourceStep::new(
            vec![SourceCgroupDef::named("cg_0")],
            SourceHold::Frac(1.0),
        ));
        let ir = lower(&s).expect("lowers");
        assert_eq!(ir.tasks.len(), 3);
    }

    fn all_supported_work_types() -> Vec<SourceWorkType> {
        vec![
            SourceWorkType::SpinWait,
            SourceWorkType::YieldHeavy,
            SourceWorkType::Mixed,
            SourceWorkType::Bursty {
                burst_duration: DurationNs::from_millis(1),
                sleep_duration: DurationNs::from_millis(1),
            },
            SourceWorkType::IdleChurn {
                burst_duration: DurationNs::from_millis(1),
                sleep_duration: DurationNs::from_millis(1),
                precise_timing: true,
            },
            SourceWorkType::Sequence {
                first: SourceWorkPhase::Spin(DurationNs::from_millis(1)),
                rest: vec![
                    SourceWorkPhase::Sleep(DurationNs::from_millis(1)),
                    SourceWorkPhase::Yield(DurationNs::ZERO),
                    SourceWorkPhase::Io(DurationNs::from_millis(1)),
                    SourceWorkPhase::AluHot(DurationNs::from_millis(1)),
                ],
            },
            SourceWorkType::AluHot {
                width: "Widest".into(),
            },
            SourceWorkType::SmtSiblingSpin,
            SourceWorkType::IpcVariance {
                hot_iters: 10,
                cold_iters: 10,
                period_iters: 20,
            },
            SourceWorkType::IoSyncWrite,
            SourceWorkType::IoRandRead,
            SourceWorkType::IoConvoy,
            SourceWorkType::PipeIo { burst_iters: 64 },
            SourceWorkType::CachePressure {
                size_kib: 256,
                stride: 64,
            },
            SourceWorkType::CacheYield {
                size_kib: 256,
                stride: 64,
            },
            SourceWorkType::CachePipe {
                size_kib: 256,
                burst_iters: 64,
            },
            SourceWorkType::PageFaultChurn {
                region_kib: 1024,
                touches_per_cycle: 64,
                spin_iters: 64,
            },
            SourceWorkType::NumaWorkingSetSweep {
                region_kib: 1024,
                sweep_period_ms: 10,
                target_nodes: vec![0, 1],
            },
            SourceWorkType::NumaMigrationChurn { period_ms: 10 },
            SourceWorkType::FutexPingPong { spin_iters: 256 },
            SourceWorkType::FutexFanOut {
                fan_out: 3,
                spin_iters: 128,
            },
            SourceWorkType::WakeChain {
                depth: 4,
                wake: WakeMechanism::Futex,
                work_per_hop: DurationNs::from_millis(10),
            },
            SourceWorkType::MutexContention {
                contenders: 4,
                hold_iters: 256,
                work_iters: 1024,
            },
            SourceWorkType::ThunderingHerd {
                waiters: 8,
                batches: 4,
                inter_batch_ms: 10,
            },
            SourceWorkType::ProducerConsumerImbalance {
                producers: 2,
                consumers: 1,
                produce_rate_hz: 1000,
                consume_iters: 100,
            },
            SourceWorkType::PriorityInversion {
                high_count: 1,
                medium_count: 1,
                low_count: 1,
                hold_iters: 10,
                work_iters: 10,
            },
            SourceWorkType::RtStarvation {
                rt_workers: 1,
                cfs_workers: 2,
                rt_priority: 80,
                burst_iters: 50,
            },
            SourceWorkType::PreemptStorm {
                cfs_workers: 2,
                rt_burst_iters: 50,
                rt_sleep_us: 100,
            },
            SourceWorkType::EpollStorm {
                producers: 2,
                consumers: 2,
                events_per_burst: 8,
            },
            SourceWorkType::AsymmetricWaker {
                waker_class: SourceSchedClass::Fifo,
                wakee_class: SourceSchedClass::Normal,
                burst_iters: 50,
            },
            SourceWorkType::FanOutCompute {
                fan_out: 3,
                cache_footprint_kib: 256,
                operations: 8,
                sleep_usec: 50,
            },
            SourceWorkType::ForkExit,
            SourceWorkType::NiceSweep,
            SourceWorkType::AffinityChurn { spin_iters: 128 },
            SourceWorkType::CrossAffinityChurn { spin_iters: 128 },
            SourceWorkType::PolicyChurn { spin_iters: 128 },
            SourceWorkType::SignalStorm {
                signals_per_iter: 4,
                work_iters: 100,
            },
            SourceWorkType::CgroupChurn {
                groups: 2,
                cycle_ms: 10,
            },
            SourceWorkType::CgroupAttachStorm {
                dest: "dest".into(),
                reap: "SigIgn".into(),
            },
            SourceWorkType::TimerLatency { interval_us: 1000 },
            SourceWorkType::NetTraffic {
                interval_us: 100,
                frame_bytes: 60,
            },
            SourceWorkType::IrqWake {
                interval_us: 100,
                frame_bytes: 60,
            },
        ]
    }

    /// Whatever else changes, every lowered IR must satisfy its own invariants —
    /// this is the guard against the lowering shipping a broken workload.
    #[test]

    fn every_supported_work_type_lowers_to_valid_ir() {
        let all = all_supported_work_types();
        // 42 supported + 3 refused = ktstr's 45.
        assert_eq!(
            all.len(),
            42,
            "supported arm count drifted from ktstr's 45-3"
        );

        for wt in all {
            let name = wt.variant_name();
            let ir = lower(&scenario_with(wt)).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(ir.validate(), Ok(()), "{name} produced invalid IR");
            assert!(!ir.tasks.is_empty(), "{name} produced no tasks");
        }
    }
    /// THE SYSTEMIC TRIPWIRE for the whole fabricated-value class.
    ///
    /// An arm may report `Fidelity::Exact` only if every duration in its output
    /// came from the input. `DEFAULT_SLICE` is by definition not in the input —
    /// it is the lowering's own number — so its appearance in an arm that
    /// claims exact is proof of fabrication.
    ///
    /// This single assertion would have caught all three known instances:
    /// `SpinWait` (which cost a 68x slice-count divergence and was invisible
    /// because two tests asserted on `is_exact()` and passed), `YieldHeavy`,
    /// and `Mixed`. Written as a loop over every supported work type so a NEW
    /// arm cannot reintroduce it either.
    #[test]
    fn no_arm_reports_exact_while_using_the_invented_default_slice() {
        for wt in all_supported_work_types() {
            let name = wt.variant_name();
            let ir = lower(&scenario_with(wt)).unwrap_or_else(|e| panic!("{name}: {e}"));
            if !ir.fidelity.is_exact() {
                continue;
            }
            for t in &ir.tasks {
                for p in &t.phases {
                    if let Phase::Run(d) = p {
                        assert_ne!(
                            *d, DEFAULT_SLICE,
                            "{name} reports EXACT but its run phase is exactly \
                             DEFAULT_SLICE ({DEFAULT_SLICE}) — a value the source \
                             never specified. Either carry a declared duration or \
                             record it via Ctx::invented_slice.",
                        );
                    }
                }
            }
        }
    }

    /// Fabricating a quantum must be disclosed AS a fabrication, whatever else
    /// the arm discloses.
    ///
    /// The tripwire above is scoped to arms that report `Exact`, which leaves a
    /// hole: an arm that records SOME approximation escapes it entirely and can
    /// then use `DEFAULT_SLICE` freely. `IoSyncWrite` is the worked example —
    /// it records `IoMechanism` ("block device, queue depth and byte counts are
    /// not modelled; the off-CPU wait is preserved as Sleep") and then lowers
    /// to `Run(500us) Sleep(500us)`, both invented. A reader of that report is
    /// told the device was dropped and told the off-CPU wait was PRESERVED,
    /// which is the opposite of true: the wait is `DEFAULT_SLICE`.
    ///
    /// This matters beyond tidiness because `UnspecifiedWorkQuantum` is the
    /// documented port gate — a scenario whose lowering records it against the
    /// construct its assertion depends on is meant to be treated as not-ported,
    /// however green it comes out. An arm that fabricates a quantum while
    /// recording only `IoMechanism` walks straight through that gate.
    #[test]
    fn an_invented_quantum_is_recorded_as_one_even_when_something_else_is_too() {
        let mut offenders: Vec<(&str, Vec<Cause>)> = Vec::new();
        for wt in all_supported_work_types() {
            let name = wt.variant_name();
            let ir = lower(&scenario_with(wt)).unwrap_or_else(|e| panic!("{name}: {e}"));
            let fabricated = ir
                .tasks
                .iter()
                .flat_map(|t| &t.phases)
                .any(|p| matches!(p, Phase::Run(d) | Phase::Sleep(d) if *d == DEFAULT_SLICE));
            if !fabricated {
                continue;
            }
            let disclosed = ir
                .fidelity
                .by_cause(Cause::UnspecifiedWorkQuantum)
                .next()
                .is_some();
            if !disclosed {
                offenders.push((name, ir.fidelity.causes()));
            }
        }
        assert!(
            offenders.is_empty(),
            "these arms lower to a DEFAULT_SLICE ({DEFAULT_SLICE}) phase the source \
             never specified, without recording Cause::UnspecifiedWorkQuantum. \
             They disclose only: {offenders:#?}\n\n\
             Route the duration through `Ctx::invented_slice` (which records the \
             right cause) IN ADDITION to whatever else the arm records. Recording \
             a different cause is not a substitute: UnspecifiedWorkQuantum is what \
             the port gate filters on.",
        );
    }

    /// `Bursty` is the control: it MUST stay exact, and its phases must be the
    /// declared values. Without this, the tripwire above could be satisfied by
    /// an arm that stopped reporting exact for the wrong reason.
    #[test]
    fn bursty_is_exact_and_carries_both_declared_durations() {
        let burst = DurationNs::from_millis(3);
        let sleep = DurationNs::from_millis(7);
        let ir = lower(&scenario_with(SourceWorkType::Bursty {
            burst_duration: burst,
            sleep_duration: sleep,
        }))
        .expect("lowers");
        assert!(ir.fidelity.is_exact(), "{:?}", ir.fidelity.approximations());
        assert_eq!(
            ir.tasks[0].phases,
            vec![Phase::Run(burst), Phase::Sleep(sleep)],
            "the declared durations must survive verbatim",
        );
    }

    /// The two arms that fabricated the work quantum while claiming exact.
    #[test]
    fn yieldheavy_and_mixed_disclose_the_invented_quantum() {
        for wt in [SourceWorkType::YieldHeavy, SourceWorkType::Mixed] {
            let name = wt.variant_name();
            let ir = lower(&scenario_with(wt)).expect("lowers");
            assert!(!ir.fidelity.is_exact(), "{name} must not claim exact");
            assert!(
                ir.fidelity
                    .approximations()
                    .iter()
                    .any(|a| a.cause == Cause::UnspecifiedWorkQuantum),
                "{name} must record WHAT it invented, not merely be non-exact: {:?}",
                ir.fidelity.approximations(),
            );
        }
    }

    /// A declared `Yield` duration cannot be carried by `Phase::Yield`, so it is
    /// dropped — which must be recorded. It was silent until the exact-arm
    /// audit, and `Sequence` reported EXACT with the duration gone.
    #[test]
    fn a_dropped_yield_duration_is_recorded_not_silent() {
        let ir = lower(&scenario_with(SourceWorkType::Sequence {
            first: SourceWorkPhase::Spin(DurationNs::from_millis(4)),
            rest: vec![SourceWorkPhase::Yield(DurationNs::from_millis(9))],
        }))
        .expect("lowers");
        assert!(!ir.fidelity.is_exact(), "a dropped duration is not exact");
        let a = ir
            .fidelity
            .approximations()
            .iter()
            .find(|a| a.cause == Cause::UnrepresentableWorkQuantum)
            .expect("the drop must be recorded");
        assert!(
            a.dropped.contains("9.000ms"),
            "the record must name the value that was lost, got {:?}",
            a.dropped,
        );
        // And the surviving phases must still be the declared ones.
        assert_eq!(
            ir.tasks[0].phases,
            vec![Phase::Run(DurationNs::from_millis(4)), Phase::Yield],
        );
    }

    /// `PreemptStorm` has no priority field; 50 is the lowering's. It was
    /// already non-exact via the iterations conversion, which is exactly why
    /// this went unnoticed — a non-green report is not a disclosed one.
    #[test]
    fn preemptstorm_discloses_its_fabricated_rt_priority() {
        let ir = lower(&scenario_with(SourceWorkType::PreemptStorm {
            cfs_workers: 2,
            rt_burst_iters: 100,
            rt_sleep_us: 5,
        }))
        .expect("lowers");
        assert!(
            ir.fidelity
                .approximations()
                .iter()
                .any(|a| a.dropped.contains("RT priority")),
            "the invented priority must be named: {:?}",
            ir.fidelity.approximations(),
        );
    }
    /// THE MECHANICAL AUDIT INSTRUMENT, run over every supported arm.
    ///
    /// "Does every value in the output appear in the input?" is the question
    /// that found five defects in eight exact-claiming arms. This is that
    /// question as a test, so the next fabrication fails the build instead of
    /// waiting for someone to think to ask.
    ///
    /// A value is allowed to be absent from the input ONLY if the lowering
    /// recorded that it supplied it. See [`crate::provenance`].
    #[test]
    fn every_arm_emits_only_values_that_trace_to_the_source_or_are_disclosed() {
        let mut offenders = Vec::new();
        for wt in all_supported_work_types() {
            let name = wt.variant_name();
            let src = scenario_with(wt);
            let ir = lower(&src).unwrap_or_else(|e| panic!("{name}: {e}"));
            for u in crate::provenance::check(&src, &ir) {
                offenders.push(format!("{name}: {u}"));
            }
        }
        assert!(
            offenders.is_empty(),
            "the lowering emitted values with no provenance:\n  {}",
            offenders.join("\n  "),
        );
    }
}
