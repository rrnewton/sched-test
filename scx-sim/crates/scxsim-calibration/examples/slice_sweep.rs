//! Why does the simulator produce 68x more slices than the live guest?
//!
//! The first wprof trace comparison measured, for `sched_basic_proportional`:
//!
//! ```text
//!   workload on-CPU   live 24042.9ms   sim 23988.0ms   agree to 0.23%
//!   workload slices   live      710    sim     48067   68x
//!   mean slice        live 33.863ms    sim   0.499ms
//! ```
//!
//! Same total CPU, completely different scheduling. This isolates the cause by
//! holding EVERYTHING constant — topology, task count, cgroups, duration,
//! scheduler — and varying exactly one thing: the length of the workload's
//! `Phase::Run` chunk.
//!
//! # The two candidate explanations, and how this separates them
//!
//! 1. **The simulator ignores the scheduler's requested slice.** `scx_simple`
//!    asks for `SCX_SLICE_DFL` (20 ms) on every `scx_bpf_dsq_insert`. If the
//!    engine re-dispatched on some fixed internal granularity instead, slice
//!    count would be INSENSITIVE to the phase length — a flat line here.
//! 2. **The workload model ends the slice, not the scheduler.** The IR lowers
//!    ktstr's `SpinWait` to `Phase::Run(500us)` with `Repeat::Forever`. If a
//!    completed run-phase yields the CPU, slice count is set by the PHASE, and
//!    scales as `duration / phase_len` — a straight line through the origin
//!    here, and nothing to do with the scheduler at all.
//!
//! These predict different shapes, so one run of this settles it.
//!
//! ```sh
//! cargo run -p scxsim-calibration --features sim --example slice_sweep
//! ```

use scx_simulator::{DynamicScheduler, ExitKind, Phase, Scenario, Simulator, TraceKind};
use scxsim_workload_ir::{
    lower, to_scenario, DurationNs, SourceCgroupDef, SourceHold, SourceScenario, SourceStep,
    SourceTopology, SourceWorkSpec, SourceWorkType,
};

/// One second of simulated time, to keep the sweep quick. The question is the
/// SHAPE of the relationship, which does not need a 12 s run.
const RUN_NS: u64 = 1_000_000_000;

/// What `scx_simple` requests on every dispatch (`SCX_SLICE_DFL`).
const SCHED_REQUESTED_SLICE_NS: u64 = 20_000_000;

/// The real `sched_basic_proportional`, with the run-phase length overridden.
///
/// Built through the ACTUAL `lower()` + `to_scenario()` path rather than
/// hand-assembled, so topology, cgroups, pids and task count are exactly what
/// the calibration runs. Only the phase length is then rewritten, which is the
/// one variable under test — the lowering hard-codes it to a private
/// `DEFAULT_SLICE` and offers no way to set it.
fn scenario(phase_ns: u64) -> Scenario {
    let src = SourceScenario {
        duration: DurationNs::from_nanos(RUN_NS),
        topology: SourceTopology {
            numa_nodes: 1,
            llcs: 1,
            cores: 2,
            threads: 1,
        },
        default_workers_per_cgroup: 1,
        ..SourceScenario::new("sched_basic_proportional")
    }
    .step(SourceStep::new(
        vec![
            SourceCgroupDef::named("cg_0").work(SourceWorkSpec::new(SourceWorkType::SpinWait)),
            SourceCgroupDef::named("cg_1").work(SourceWorkSpec::new(SourceWorkType::SpinWait)),
        ],
        SourceHold::FULL,
    ));
    let ir = lower(&src).expect("lowers");
    let mut sc = to_scenario(&ir).expect("ingests");
    sc.duration_ns = RUN_NS;
    for t in &mut sc.tasks {
        t.behavior.phases = vec![Phase::Run(phase_ns)];
    }
    sc
}

struct Row {
    phase_ns: u64,
    slices: usize,
    on_cpu_ns: u64,
}

fn measure(phase_ns: u64) -> Row {
    let _guard = scx_simulator::SIM_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let sc = scenario(phase_ns);
    let trace = Simulator::new(DynamicScheduler::simple()).run(sc.clone());
    assert_eq!(*trace.exit_kind(), ExitKind::Normal, "must finish normally");

    let slices = trace
        .events()
        .iter()
        .filter(|e| matches!(e.kind, TraceKind::TaskScheduled { .. }))
        .count();
    let on_cpu_ns: u64 = sc.tasks.iter().map(|t| trace.total_runtime(t.pid)).sum();
    Row {
        phase_ns,
        slices,
        on_cpu_ns,
    }
}

fn main() {
    // Spans the lowering's 500us default, the scheduler's 20ms request, and
    // "one phase for the whole run" (no phase boundary at all).
    let phases = [
        100_000u64, 500_000, 1_000_000, 5_000_000, 20_000_000, 50_000_000, RUN_NS,
    ];

    println!(
        "scenario: 2 spinners, 2 cgroups, 2 cpus, {}s, scheduler `simple` \
         (requests SCX_SLICE_DFL = {}ms on every dispatch)\n",
        RUN_NS / 1_000_000_000,
        SCHED_REQUESTED_SLICE_NS / 1_000_000
    );
    println!(
        "{:>12} {:>10} {:>12} {:>12} {:>14}",
        "phase", "slices", "mean slice", "on-CPU", "slices if the"
    );
    println!(
        "{:>12} {:>10} {:>12} {:>12} {:>14}",
        "len", "", "", "", "PHASE governs"
    );

    for p in phases {
        let r = measure(p);
        let mean_ms = if r.slices > 0 {
            r.on_cpu_ns as f64 / r.slices as f64 / 1e6
        } else {
            0.0
        };
        // If the phase boundary ends the slice, each task yields every
        // `phase_ns`, so both tasks together produce 2 * duration / phase_ns.
        let predicted_by_phase = 2 * RUN_NS / r.phase_ns;
        println!(
            "{:>10.3}ms {:>10} {:>10.3}ms {:>10.1}ms {:>14}",
            r.phase_ns as f64 / 1e6,
            r.slices,
            mean_ms,
            r.on_cpu_ns as f64 / 1e6,
            predicted_by_phase,
        );
    }

    println!(
        "\nIf slice count tracks the right-hand column, the WORKLOAD PHASE is \
         ending the slice and the scheduler's {}ms request never binds.\n\
         If it is flat, the engine is re-dispatching on its own granularity.",
        SCHED_REQUESTED_SLICE_NS / 1_000_000
    );
}
