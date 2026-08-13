//! Write the simulator's Perfetto protobuf for `sched_basic_proportional`.
//!
//! The live half of a trace comparison comes out of a ktstr guest run as
//! `{sidecar_dir}/{test}-{variant_hash:016x}.wprof.pb`. This produces the
//! matching simulated half.
//!
//! The scenario is built through the SAME `ktstr ops -> IR -> Scenario` bridge
//! the calibration uses, not restated here, so the traced workload is provably
//! the calibrated one.
//!
//! ```sh
//! cargo run -p scxsim-calibration --features sim --example dump_sim_perfetto -- /tmp/sim.pb
//! ```
//!
//! Emits the wprof-compatible protobuf (`write_perfetto_pb`), the same encoder
//! `scxsim run --trace-format perfetto` uses, whose category and annotation
//! vocabulary deliberately mirrors wprof's so both traces load side by side.

use std::path::PathBuf;

use scx_simulator::{DynamicScheduler, ExitKind, Simulator};
use scxsim_workload_ir::{
    lower, to_scenario, DurationNs, SourceCgroupDef, SourceHold, SourceScenario, SourceStep,
    SourceTopology, SourceWorkSpec, SourceWorkType,
};

/// ktstr's `sched_basic_proportional`, as ktstr declares it.
fn scenario_source() -> SourceScenario {
    SourceScenario {
        duration: DurationNs::from_secs(12),
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
    ))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out: PathBuf = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "sim_sched_basic_proportional.pb".into())
        .into();

    let ir = lower(&scenario_source())?;
    if !ir.fidelity.is_exact() {
        // Refuse rather than trace a workload that is not the one ktstr ran.
        return Err(format!(
            "scenario did not lower exactly: {:?}",
            ir.fidelity.approximations()
        )
        .into());
    }
    let scenario = to_scenario(&ir)?;

    let trace = Simulator::new(DynamicScheduler::simple()).run(scenario.clone());
    if *trace.exit_kind() != ExitKind::Normal {
        return Err(format!(
            "simulation did not finish normally: {:?}",
            trace.exit_kind()
        )
        .into());
    }

    let mut f = std::io::BufWriter::new(std::fs::File::create(&out)?);
    trace.write_perfetto_pb(&mut f)?;
    drop(f);

    let bytes = std::fs::metadata(&out)?.len();
    eprintln!(
        "wrote {} ({bytes} bytes) — {} cpus, {} tasks, {} trace events",
        out.display(),
        scenario.nr_cpus,
        scenario.tasks.len(),
        trace.events().len(),
    );
    for t in &scenario.tasks {
        eprintln!(
            "  pid {:?} cgroup {:?}",
            t.pid,
            t.cgroup_name.as_deref().unwrap_or("<root>")
        );
    }
    Ok(())
}
