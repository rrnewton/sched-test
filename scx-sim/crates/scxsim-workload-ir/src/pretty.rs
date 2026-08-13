//! Human-readable rendering of a lowered workload.
//!
//! The IR is a compiler artifact, so the usual reason to read one is to answer
//! "is this the workload I meant, and what did the lowering cost me?". The
//! printer therefore puts the fidelity report *in the same output* as the
//! structure rather than leaving it to a separate call — a reader who forgets to
//! ask about approximations is exactly the reader who most needs to be told.

use std::fmt::Write as _;

use crate::ir::{CpuSetDisplay, Mutation, Phase, Probe, Repeat, SchedPolicy, WorkloadIr};

/// Render a workload as an indented, human-readable block.
pub fn pretty(ir: &WorkloadIr) -> String {
    let mut s = String::new();
    let t = &ir.topology;
    let _ = writeln!(
        s,
        "workload `{}`  {}n/{}l/{}c/{}t = {} cpus  duration {}  seed {}",
        ir.name,
        t.numa_nodes,
        t.llcs,
        t.cores_per_llc,
        t.threads_per_core,
        t.total_cpus(),
        ir.duration,
        ir.seed
    );

    if !ir.cgroups.is_empty() {
        let _ = writeln!(s, "  cgroups:");
        for cg in &ir.cgroups {
            let mut attrs = Vec::new();
            if let Some(p) = &cg.parent {
                attrs.push(format!("parent={p}"));
            }
            if let Some(c) = &cg.cpuset {
                attrs.push(format!("cpuset={}", CpuSetDisplay(c)));
            }
            if let Some(b) = &cg.bandwidth {
                attrs.push(format!("cpu.max={}/{}", b.quota, b.period));
            }
            if let Some(w) = cg.weight {
                attrs.push(format!("cpu.weight={w}"));
            }
            let _ = writeln!(
                s,
                "    {}{}",
                cg.name,
                if attrs.is_empty() {
                    String::new()
                } else {
                    format!("  [{}]", attrs.join(" "))
                }
            );
        }
    }

    if !ir.tasks.is_empty() {
        let _ = writeln!(s, "  tasks:");
        for task in &ir.tasks {
            let mut attrs = vec![format!("{}", task.nice)];
            if task.policy != SchedPolicy::Normal {
                attrs.push(policy_str(task.policy));
            }
            if let Some(cg) = &task.cgroup {
                attrs.push(format!("in={cg}"));
            }
            if let Some(a) = &task.affinity {
                attrs.push(format!("aff={}", CpuSetDisplay(a)));
            }
            if !task.start.is_zero() {
                attrs.push(format!("start={}", task.start));
            }
            let _ = writeln!(
                s,
                "    {} {:<16} [{}] {} {}",
                task.id,
                task.name,
                attrs.join(" "),
                phases_str(&task.phases),
                repeat_str(task.repeat)
            );
        }
    }

    if !ir.timeline.is_empty() {
        let _ = writeln!(s, "  timeline:");
        for tm in &ir.timeline {
            let _ = writeln!(
                s,
                "    @{:<10} {}",
                tm.at.to_string(),
                mutation_str(&tm.mutation)
            );
        }
    }

    // Fidelity last, and always present — "exact" is information too.
    let _ = writeln!(s, "  fidelity: {}", ir.fidelity.overall());
    for a in ir.fidelity.approximations() {
        let _ = writeln!(s, "    - {a}");
    }
    s
}

fn phases_str(phases: &[Phase]) -> String {
    if phases.is_empty() {
        return "(no phases)".into();
    }
    let parts: Vec<String> = phases
        .iter()
        .map(|p| match p {
            Phase::Run(d) => format!("run {d}"),
            Phase::Sleep(d) => format!("sleep {d}"),
            Phase::Yield => "yield".to_string(),
            Phase::Wake(t) => format!("wake {t}"),
        })
        .collect();
    parts.join(" -> ")
}

fn repeat_str(r: Repeat) -> &'static str {
    match r {
        Repeat::Once => "(once)",
        Repeat::Forever => "(forever)",
        Repeat::Times(_) => "(repeat)",
    }
}

fn policy_str(p: SchedPolicy) -> String {
    match p {
        SchedPolicy::Normal => "normal".into(),
        SchedPolicy::Batch => "batch".into(),
        SchedPolicy::Idle => "idle".into(),
        SchedPolicy::Fifo { priority } => format!("fifo:{priority}"),
        SchedPolicy::RoundRobin { priority } => format!("rr:{priority}"),
    }
}

fn mutation_str(m: &Mutation) -> String {
    match m {
        Mutation::CreateCgroup(cg) => format!("create cgroup {}", cg.name),
        Mutation::DestroyCgroup(n) => format!("destroy cgroup {n}"),
        Mutation::SetCpuset { cgroup, cpus } => {
            format!("set cpuset {cgroup} = {}", CpuSetDisplay(cpus))
        }
        Mutation::ClearCpuset { cgroup } => format!("clear cpuset {cgroup}"),
        Mutation::MoveTasks { from, to } => format!("move tasks {from} -> {to}"),
        Mutation::SetBandwidth { cgroup, bandwidth } => match bandwidth {
            Some(b) => format!("set cpu.max {cgroup} = {}/{}", b.quota, b.period),
            None => format!("clear cpu.max {cgroup}"),
        },
        Mutation::Observe { label, probe } => {
            format!("observe `{label}` <- {}", probe_str(probe))
        }
    }
}

fn probe_str(p: &Probe) -> String {
    match p {
        Probe::RunqueueState => "runqueue state".into(),
        Probe::CgroupMembers(n) => format!("members of {n}"),
        Probe::SchedulerValue(v) => format!("scheduler value `{v}`"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lower::lower;
    use crate::source::*;
    use crate::units::DurationNs;

    #[test]
    fn pretty_renders_structure_and_fidelity_together() {
        let s = SourceScenario::new("demo").step(SourceStep::new(
            vec![SourceCgroupDef::named("cg_0")
                .work(
                    SourceWorkSpec::new(SourceWorkType::CachePressure {
                        size_kib: 256,
                        stride: 64,
                    })
                    .workers(1),
                )
                .cpuset(SourceCpuset::Llc(0))],
            SourceHold::Frac(1.0),
        ));
        let ir = lower(&s).expect("lowers");
        let out = pretty(&ir);

        assert!(out.contains("workload `demo`"), "{out}");
        assert!(out.contains("cg_0"), "{out}");
        assert!(out.contains("cpuset=llc0"), "{out}");
        assert!(out.contains("run "), "{out}");
        // The point of the printer: the loss is in the same output as the shape.
        assert!(out.contains("fidelity: approximated"), "{out}");
        assert!(out.contains("size_kib=256"), "{out}");
    }

    /// "exact" must be printed too — silence would be indistinguishable from a
    /// printer that forgot to check.
    #[test]
    fn pretty_states_exact_fidelity_explicitly() {
        let s = SourceScenario::new("clean").step(SourceStep::new(
            vec![SourceCgroupDef::named("cg_0")
                .work(SourceWorkSpec::new(SourceWorkType::SpinWait).workers(1))],
            SourceHold::Frac(1.0),
        ));
        let ir = lower(&s).expect("lowers");
        let out = pretty(&ir);
        assert!(out.contains("fidelity: exact"), "{out}");
    }

    #[test]
    fn pretty_renders_wake_edges_and_timeline() {
        let s = SourceScenario {
            duration: DurationNs::from_secs(4),
            ..SourceScenario::new("pp")
        }
        .step(SourceStep::new(
            vec![SourceCgroupDef::named("a").work(
                SourceWorkSpec::new(SourceWorkType::FutexPingPong { spin_iters: 10 }).workers(1),
            )],
            SourceHold::Frac(0.5),
        ))
        .step(SourceStep::new(
            vec![SourceCgroupDef::named("b")],
            SourceHold::Frac(0.5),
        ));
        let ir = lower(&s).expect("lowers");
        let out = pretty(&ir);
        assert!(out.contains("wake t"), "{out}");
        assert!(out.contains("timeline:"), "{out}");
        assert!(out.contains("create cgroup b"), "{out}");
    }
}
