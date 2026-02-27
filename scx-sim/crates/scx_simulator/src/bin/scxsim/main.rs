//! scxsim — Run sched_ext scheduler simulations from rt-app workloads.

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand, ValueEnum};

use scx_simulator::scenario::{parse_duration_ns, parse_seed};
use scx_simulator::{
    compare_checkpoints, compute_so_hash, discover_schedulers, drain_determinism_checkpoints,
    drain_preemption_records, enable_determinism_mode, enable_preemption_collection, load_rtapp,
    scheduler_so_base, scheduler_so_path, DynamicScheduler, NativeConcurrentConfig, Phase,
    PmuEvent, PreemptMode, PreemptionTrace, PreemptiveConfig, RepeatMode, Scenario, SimFormat,
    Simulator, TaskBehavior, TraceMetadata, TraceStats, SIM_LOCK,
};

mod real_run;

/// How to run the workload.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum RealRunMode {
    /// Simulation only (default).
    #[default]
    Off,
    /// Launch virtme-ng VM with rt-app and scheduler.
    Vm,
}

/// Which PMU event to break on for preemptive interleaving.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum BreakOn {
    /// Retired conditional branches (default, lower frequency).
    #[default]
    Rbc,
    /// Instructions retired (higher frequency — use larger timeslice).
    Insn,
}

impl BreakOn {
    /// Convert to the corresponding `PmuEvent`.
    fn to_pmu_event(self) -> PmuEvent {
        match self {
            BreakOn::Rbc => PmuEvent::RetiredBranchConditional,
            BreakOn::Insn => PmuEvent::InstructionsRetired,
        }
    }
}

/// Which preemption mechanism to use for mid-C-code preemption.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum PreemptModeArg {
    /// PMU hardware timer (default, nondeterministic).
    #[default]
    Pmu,
    /// e9patch software RBC (deterministic, debugger-compatible).
    E9patch,
}

impl PreemptModeArg {
    fn to_preempt_mode(self) -> PreemptMode {
        match self {
            PreemptModeArg::Pmu => PreemptMode::Pmu,
            PreemptModeArg::E9patch => PreemptMode::E9patch,
        }
    }
}

/// sched_ext simulator.
#[derive(Parser)]
#[command(name = "scxsim", about = "sched_ext simulator")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a simulation from an rt-app workload.
    Run(RunArgs),
    /// Replay a recorded preemption trace.
    Replay(ReplayArgs),
}

/// Arguments for the `run` subcommand.
#[derive(Parser)]
struct RunArgs {
    /// Path to an rt-app JSON workload file.
    workload: Option<PathBuf>,

    /// Scheduler name.
    #[arg(short, long, default_value = "simple")]
    scheduler: String,

    /// Number of simulated CPUs.
    #[arg(short, long, default_value_t = 4)]
    cpus: u32,

    /// SMT threads per core.
    #[arg(long, default_value_t = 1)]
    smt: u32,

    /// PRNG seed (u32 integer or "entropy" for OS randomness).
    ///
    /// Controls deterministic simulation: tick jitter, context-switch
    /// overhead noise, and event tiebreaking all derive from this seed.
    /// Falls back to SCX_SIM_SEED env var, then default (42).
    #[arg(long, env = "SCX_SIM_SEED")]
    seed: Option<String>,

    /// Use insertion-order event tiebreaking instead of randomized.
    ///
    /// By default, events at the same timestamp are processed in a
    /// PRNG-randomized order to detect ordering-dependent bugs.
    /// This flag restores the deterministic insertion-order behavior.
    #[arg(long)]
    fixed_priority: bool,

    /// Simulation end time (overrides workload duration).
    ///
    /// Accepts durations with units: "1s", "0.5s", "500ms", "100us", "1000ns".
    /// A bare number is interpreted as nanoseconds.
    #[arg(long, value_name = "DURATION")]
    end_time: Option<String>,

    /// Write Perfetto trace JSON to file.
    #[arg(long, value_name = "PATH")]
    perfetto: Option<PathBuf>,

    /// Print trace events to stderr.
    #[arg(long)]
    dump_trace: bool,

    /// Disable tick jitter noise.
    #[arg(long)]
    no_noise: bool,

    /// Disable context-switch overhead.
    #[arg(long)]
    no_overhead: bool,

    /// Nanoseconds of logical time per retired conditional branch in scheduler
    /// code. Enables PMU-based scheduler overhead measurement. Default: 10.
    #[arg(long, value_name = "NS", conflicts_with = "no_rbc")]
    rbc_ns: Option<u64>,

    /// Disable PMU-based RBC scheduler overhead (equivalent to --rbc-ns 0).
    #[arg(long, conflicts_with = "rbc_ns")]
    no_rbc: bool,

    /// Watchdog timeout for stall detection.
    ///
    /// If a runnable task is not scheduled within this duration (simulated
    /// time), the simulation exits with an error. Accepts durations with
    /// units: "2s", "500ms", etc. Default: 30s.
    #[arg(long, value_name = "DURATION")]
    watchdog_timeout: Option<String>,

    /// Enable concurrent callback interleaving at kfunc yield points.
    ///
    /// Runs dispatch callbacks for multiple idle CPUs on separate OS
    /// threads with PRNG-driven token passing, enabling deterministic
    /// exploration of different interleavings.
    #[arg(long)]
    interleave: bool,

    /// Enable preemptive interleaving via PMU retired branch counter signals.
    ///
    /// Like --interleave, but also preempts mid-C-code at random retired
    /// conditional branch intervals. Implies --interleave. Falls back to
    /// cooperative-only interleaving if PMU counters are unavailable (VM).
    #[arg(long)]
    preemptive: bool,

    /// Minimum preemptive timeslice in retired conditional branches.
    ///
    /// Controls the lower bound of the random timeslice range used by
    /// --preemptive mode. Default: 1 (PMU skid means actual preemption
    /// is tens to hundreds of branches later).
    #[arg(long, default_value_t = 1, requires = "preemptive")]
    timeslice_min: u64,

    /// Maximum preemptive timeslice in retired conditional branches.
    ///
    /// Controls the upper bound of the random timeslice range used by
    /// --preemptive mode. Default: 1 (PMU skid means actual preemption
    /// is tens to hundreds of branches later).
    #[arg(long, default_value_t = 1, requires = "preemptive")]
    timeslice_max: u64,

    /// Which PMU event to break on for preemptive interleaving.
    ///
    /// rbc: Retired conditional branches (default, lower frequency).
    /// insn: Instructions retired (higher frequency — use larger timeslice).
    #[arg(long, value_enum, default_value_t = BreakOn::Rbc, requires = "preemptive")]
    break_on: BreakOn,

    /// Preemption mechanism for mid-C-code preemption.
    ///
    /// pmu: Hardware PMU timer (default, nondeterministic due to skid).
    /// e9patch: Software RBC via e9patch-instrumented .so (deterministic,
    ///          debugger-compatible, requires _e9.so variant).
    #[arg(long, value_enum, default_value_t = PreemptModeArg::Pmu, requires = "preemptive")]
    preempt_mode: PreemptModeArg,

    /// Enable native concurrent dispatch via OS threads with clock-window
    /// synchronisation.
    ///
    /// Runs dispatch callbacks for multiple CPUs on real OS threads,
    /// synchronised by a shared clock window rather than token passing.
    /// Implies --interleave.
    #[arg(long, conflicts_with = "preemptive")]
    native_concurrent: bool,

    /// Clock window size in nanoseconds (only with --native-concurrent).
    ///
    /// Controls the simulated-time window within which concurrent dispatch
    /// threads are allowed to execute. Requires --native-concurrent.
    /// Default: 10_000_000 (10 ms).
    #[arg(
        long,
        default_value_t = 10_000_000,
        requires = "native_concurrent",
        hide_default_value = true
    )]
    window_ns: u64,

    /// List available schedulers and exit.
    #[arg(long)]
    list_schedulers: bool,

    /// Run workload in real environment.
    ///
    /// off: simulation only (default)
    /// vm: launch virtme-ng VM with rt-app and scheduler
    #[arg(long, value_enum, default_value_t = RealRunMode::Off)]
    real_run: RealRunMode,

    /// Record a Perfetto trace using wprof during VM execution.
    ///
    /// Requires --real-run vm. When enabled, an extra CPU is added to the VM
    /// and isolated using isolcpus for running the wprof tracer. The trace
    /// file is written to the current working directory.
    #[arg(long, conflicts_with = "bpf_trace")]
    wprof: bool,

    /// Trace scheduler ops callbacks and kfunc calls using bpftrace.
    ///
    /// Requires --real-run vm. When enabled, an extra CPU is added to the VM
    /// and isolated for running bpftrace with trace_scx_ops.bt. This traces
    /// sched_class entry points, scx_bpf_* kfunc calls with return values,
    /// and sched_switch/sched_wakeup lifecycle events.
    ///
    /// The trace is written to bpf_trace.log in the current working directory.
    /// This is an alternative to --wprof for comparing simulator vs real runs.
    #[arg(long, conflicts_with = "wprof")]
    bpf_trace: bool,

    /// Enable strict determinism checking.
    ///
    /// Runs the simulation twice with the same seed and configuration,
    /// enables aggressive determinism mode to collect checkpoints at
    /// scheduling events, and compares checkpoint sequences from both runs.
    /// Reports any divergence found and exits with code 1 if determinism
    /// is violated.
    #[arg(long)]
    determinism_check: bool,

    /// Record preemption points to a file for later replay.
    ///
    /// After the simulation completes, drains all recorded preemption
    /// points, groups them by worker, and writes a text trace file that
    /// can be used for deterministic replay.
    #[arg(long, value_name = "PATH")]
    record_preemptions: Option<PathBuf>,

    /// Print detailed per-task and per-CPU statistics after simulation.
    ///
    /// By default, only a brief trace summary is printed. This flag
    /// enables the verbose breakdown with distribution stats, run
    /// durations, inter-arrival times, and per-CPU tick intervals.
    #[arg(long)]
    verbose_summary: bool,

    /// Pause before ops.init() so a debugger can attach.
    ///
    /// After the scheduler .so is loaded, writes an lldb breakpoint script
    /// next to the .so, prints a copy-pasteable lldb command, and spin-waits
    /// for a debugger. A single `continue` from the attach stop hits the
    /// first ops breakpoint.
    #[arg(long)]
    wait_debugger: bool,
}

/// Arguments for the `replay` subcommand.
#[derive(Parser)]
struct ReplayArgs {
    /// Path to a preemption trace file (produced by `run --record-preemptions`).
    trace_file: PathBuf,

    /// Override the scheduler .so file path stored in the trace. Only needed if the .so has moved since recording.
    #[arg(long = "scheduler-file")]
    scheduler_file: Option<PathBuf>,

    /// Print detailed per-task and per-CPU statistics after simulation.
    #[arg(long)]
    verbose_summary: bool,

    /// Re-record preemption points during replay to a new trace file.
    #[arg(long, value_name = "PATH")]
    record_preemptions: Option<PathBuf>,

    /// Skip PMU timer and use hardware breakpoint stepping only.
    ///
    /// Slower but guarantees deterministic replay by avoiding PMU skid.
    /// The breakpoint fires on every execution of the target instruction
    /// and checks the RBC count to find the right dynamic instance.
    #[arg(long)]
    no_pmu_signal: bool,

    /// Pause before ops.init() so a debugger can attach.
    ///
    /// After the scheduler .so is loaded, writes an lldb breakpoint script
    /// next to the .so, prints a copy-pasteable lldb command, and spin-waits
    /// for a debugger. A single `continue` from the attach stop hits the
    /// first ops breakpoint.
    #[arg(long)]
    wait_debugger: bool,
}

fn main() {
    let cli = Cli::parse();
    init_tracing();

    let result = match cli.command {
        Command::Run(args) => run(&args),
        Command::Replay(args) => replay_simulation(&args),
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run(args: &RunArgs) -> Result<(), String> {
    if args.list_schedulers {
        list_schedulers();
        return Ok(());
    }

    let workload_path = args
        .workload
        .as_ref()
        .ok_or("missing required argument: <WORKLOAD>")?;

    let json = std::fs::read_to_string(workload_path)
        .map_err(|e| format!("failed to read {}: {e}", workload_path.display()))?;

    let mut scenario =
        load_rtapp(&json, args.cpus).map_err(|e| format!("failed to parse workload: {e}"))?;

    // Override scenario fields from CLI flags.
    scenario.smt_threads_per_core = args.smt;
    if args.no_noise {
        scenario.noise.enabled = false;
    }
    if args.no_overhead {
        scenario.overhead.enabled = false;
    }
    if let Some(ref seed_str) = args.seed {
        scenario.seed = parse_seed(Some(seed_str));
    }
    if args.fixed_priority {
        scenario.fixed_priority = true;
    }
    if args.interleave {
        scenario.interleave = true;
    }
    if args.preemptive {
        scenario.preemptive = Some(PreemptiveConfig {
            timeslice_min: args.timeslice_min,
            timeslice_max: args.timeslice_max,
            cooperative_only: false,
            break_on: args.break_on.to_pmu_event(),
            preempt_mode: args.preempt_mode.to_preempt_mode(),
        });
        scenario.interleave = true;
    }
    if args.native_concurrent {
        scenario.native_concurrent = Some(NativeConcurrentConfig {
            window_ns: args.window_ns,
        });
        scenario.interleave = true;
    }
    if let Some(ref end_time) = args.end_time {
        scenario.duration_ns =
            parse_duration_ns(end_time).map_err(|e| format!("--end-time: {e}"))?;
    }
    if let Some(rbc_ns) = args.rbc_ns {
        scenario.sched_overhead_rbc_ns = Some(rbc_ns);
    }
    if args.no_rbc {
        scenario.sched_overhead_rbc_ns = Some(0);
    }
    if let Some(ref timeout) = args.watchdog_timeout {
        scenario.watchdog_timeout_ns =
            Some(parse_duration_ns(timeout).map_err(|e| format!("--watchdog-timeout: {e}"))?);
    }
    if args.wait_debugger {
        scenario.wait_debugger = true;
    }

    // Validate --wprof and --bpf-trace require --real-run vm
    if args.wprof && args.real_run != RealRunMode::Vm {
        return Err("--wprof requires --real-run vm".into());
    }
    if args.bpf_trace && args.real_run != RealRunMode::Vm {
        return Err("--bpf-trace requires --real-run vm".into());
    }

    // Determine trace mode
    let trace_mode = if args.wprof {
        real_run::TraceMode::Wprof
    } else if args.bpf_trace {
        real_run::TraceMode::BpfTrace
    } else {
        real_run::TraceMode::None
    };

    // Handle --determinism-check mode
    if args.determinism_check {
        if args.real_run != RealRunMode::Off {
            return Err("--determinism-check conflicts with --real-run".into());
        }
        return run_determinism_check(args, scenario);
    }

    // Handle --real-run mode
    match args.real_run {
        RealRunMode::Off => {
            run_simulation(args, scenario)?;
        }
        RealRunMode::Vm => {
            real_run::run_vm(workload_path, &args.scheduler, args.cpus, trace_mode)?;
        }
    }

    Ok(())
}

/// Extract the scheduler prefix from a .so path.
///
/// Given a path like `/path/to/libscx_simple.so`, returns `"simple"`.
/// Panics if the filename does not match the `libscx_<name>.so` pattern.
fn scheduler_prefix_from_path(path: &Path) -> String {
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_else(|| panic!("invalid scheduler path: {}", path.display()));
    filename
        .strip_prefix("libscx_")
        .and_then(|s| s.strip_suffix(".so"))
        .unwrap_or_else(|| {
            panic!("scheduler .so filename must match libscx_<name>.so, got: {filename}")
        })
        .to_string()
}

/// Resolve the scheduler .so path for replay.
///
/// Priority: CLI `--scheduler-file` override > trace metadata `so_path`.
/// Panics if neither is available. Validates that the resolved path exists.
fn resolve_scheduler_path(
    cli_override: Option<&Path>,
    metadata: &TraceMetadata,
) -> Result<PathBuf, String> {
    let path = if let Some(cli_path) = cli_override {
        eprintln!(
            "replay: using --scheduler-file override: {}",
            cli_path.display()
        );
        cli_path.to_path_buf()
    } else if let Some(ref so_path) = metadata.so_path {
        eprintln!("replay: using scheduler .so from trace: {so_path}");
        PathBuf::from(so_path)
    } else {
        return Err(
            "no scheduler .so path available: trace file has no so_path metadata              and --scheduler-file was not provided. Re-record the trace with a newer              scxsim to embed the .so path, or pass --scheduler-file <path>."
                .into(),
        );
    };

    if !path.exists() {
        return Err(format!(
            "scheduler .so not found: {}. Pass --scheduler-file <path> to override.",
            path.display()
        ));
    }

    Ok(path)
}

fn replay_simulation(args: &ReplayArgs) -> Result<(), String> {
    use std::io::BufReader;

    // Load the trace file first to read metadata (including so_path).
    let file = std::fs::File::open(&args.trace_file)
        .map_err(|e| format!("cannot open trace file {}: {e}", args.trace_file.display()))?;

    // Peek at trace metadata first (nr_cpus, so_path) to know how many CPUs
    // the scheduler needs and which .so to load. We deserialize with so_base=0
    // initially, read metadata, then re-deserialize after loading the scheduler
    // with the correct so_base.
    let mut reader = BufReader::new(file);
    let pre_trace = PreemptionTrace::deserialize(&mut reader, 0)
        .map_err(|e| format!("failed to parse trace file: {e}"))?;

    let metadata = pre_trace.metadata();

    // Resolve scheduler .so path: CLI --scheduler-file overrides trace metadata.
    let scheduler_path = resolve_scheduler_path(args.scheduler_file.as_deref(), metadata)?;
    let prefix = scheduler_prefix_from_path(&scheduler_path);
    let so_path_str = scheduler_path.to_str().unwrap_or_else(|| {
        panic!(
            "scheduler path is not valid UTF-8: {}",
            scheduler_path.display()
        )
    });

    // Extract required metadata fields, panicking on missing values.
    let nr_cpus = metadata
        .nr_cpus
        .unwrap_or_else(|| panic!("trace file missing required metadata: nr_cpus"));
    let nr_tasks = metadata
        .nr_tasks
        .unwrap_or_else(|| panic!("trace file missing required metadata: nr_tasks"));
    let seed = metadata
        .seed
        .unwrap_or_else(|| panic!("trace file missing required metadata: seed"));
    let duration_ns = metadata
        .duration_ns
        .unwrap_or_else(|| panic!("trace file missing required metadata: duration_ns"));
    let timeslice_min = metadata
        .timeslice_min
        .unwrap_or_else(|| panic!("trace file missing required metadata: timeslice_min"));
    let timeslice_max = metadata
        .timeslice_max
        .unwrap_or_else(|| panic!("trace file missing required metadata: timeslice_max"));

    // Now load the scheduler with the correct nr_cpus.
    let sched = DynamicScheduler::load(so_path_str, &prefix, nr_cpus);
    let _lock = SIM_LOCK.lock().unwrap();
    let so_base = scheduler_so_base();

    // Re-deserialize the trace with the correct so_base for ASLR-resilient RIP.
    let file2 = std::fs::File::open(&args.trace_file)
        .map_err(|e| format!("cannot open trace file {}: {e}", args.trace_file.display()))?;
    let mut reader2 = BufReader::new(file2);
    let trace = PreemptionTrace::deserialize(&mut reader2, so_base)
        .map_err(|e| format!("failed to parse trace file: {e}"))?;

    eprintln!(
        "replay: loaded {} preemption points for {} workers from {}",
        trace.len(),
        trace.num_workers(),
        args.trace_file.display()
    );

    // Validate .so hash matches the recording.
    let current_so_hash = compute_so_hash();
    if current_so_hash != 0 {
        let expected = TraceMetadata {
            so_hash: Some(current_so_hash),
            ..TraceMetadata::default()
        };
        trace.validate_metadata(&expected);
    }

    // Build a scenario from trace metadata with N identical compute tasks.
    let compute_phase = Phase::Run(10_000_000);
    let behavior = TaskBehavior {
        phases: vec![compute_phase],
        repeat: RepeatMode::Forever,
    };

    let mut builder = Scenario::builder()
        .cpus(nr_cpus)
        .seed(seed)
        .duration_ns(duration_ns)
        .preemptive(PreemptiveConfig {
            timeslice_min,
            timeslice_max,
            cooperative_only: false,
            break_on: trace.break_on(),
            preempt_mode: PreemptMode::Pmu,
        });

    for i in 0..nr_tasks {
        builder = builder.add_task(&format!("task-{i}"), 0, behavior.clone());
    }

    let mut scenario = builder.build();
    scenario.replay_trace = Some(trace);
    scenario.no_pmu_signal = args.no_pmu_signal;
    if args.wait_debugger {
        scenario.wait_debugger = true;
    }

    // Enable preemption recording if --record-preemptions is set.
    if args.record_preemptions.is_some() {
        enable_preemption_collection();
    }

    // Capture scenario metadata before the scenario is consumed by run().
    let scenario_metadata = TraceMetadata {
        nr_cpus: Some(scenario.nr_cpus),
        nr_tasks: Some(scenario.tasks.len() as u32),
        seed: Some(scenario.seed),
        duration_ns: Some(scenario.duration_ns),
        scheduler: Some(prefix.clone()),
        timeslice_min: scenario.preemptive.as_ref().map(|p| p.timeslice_min),
        timeslice_max: scenario.preemptive.as_ref().map(|p| p.timeslice_max),
        so_hash: if current_so_hash != 0 {
            Some(current_so_hash)
        } else {
            None
        },
        so_path: Some(so_path_str.to_string()),
    };

    let sim_trace = Simulator::new(sched).run(scenario);

    // Record preemption trace if requested.
    if let Some(path) = &args.record_preemptions {
        let records = drain_preemption_records();
        let num_workers = nr_cpus as usize;
        let break_on_event = pre_trace.break_on();
        let mut preemption_trace =
            PreemptionTrace::from_records(&records, num_workers, break_on_event);
        preemption_trace.set_metadata(scenario_metadata);

        let mut file = std::fs::File::create(path)
            .map_err(|e| format!("failed to create {}: {e}", path.display()))?;
        preemption_trace
            .serialize(&mut file, so_base)
            .map_err(|e| format!("failed to write preemption trace: {e}"))?;
        eprintln!(
            "wrote {} preemption records to {}",
            preemption_trace.len(),
            path.display()
        );
    }

    // Print simulation summary.
    if args.verbose_summary {
        let stats = TraceStats::from_trace(&sim_trace);
        println!();
        stats.print_summary();
    } else {
        println!();
        println!("{}", sim_trace.summary());
    }

    if sim_trace.has_error() {
        return Err(format!("simulation error: {:?}", sim_trace.exit_kind()));
    }

    Ok(())
}

fn run_determinism_check(args: &RunArgs, scenario: Scenario) -> Result<(), String> {
    let _lock = SIM_LOCK.lock().unwrap();
    let use_e9 = args.preemptive && args.preempt_mode == PreemptModeArg::E9patch;

    // Map the shared RBC state page BEFORE loading the _e9.so.
    if use_e9 {
        scx_simulator::preempt::mmap_shared_rbc();
    }

    // Run 1: collect checkpoints
    enable_determinism_mode();
    let sched1 = load_scheduler(&args.scheduler, args.cpus, use_e9)?;
    let trace1 = Simulator::new(sched1).run(scenario.clone());
    let checkpoints1 = drain_determinism_checkpoints();

    if trace1.has_error() {
        return Err(format!(
            "simulation error in run 1: {:?}",
            trace1.exit_kind()
        ));
    }

    // Run 2: collect checkpoints with same configuration
    enable_determinism_mode();
    let sched2 = load_scheduler(&args.scheduler, args.cpus, use_e9)?;
    let trace2 = Simulator::new(sched2).run(scenario);
    let checkpoints2 = drain_determinism_checkpoints();

    if trace2.has_error() {
        return Err(format!(
            "simulation error in run 2: {:?}",
            trace2.exit_kind()
        ));
    }

    // Compare checkpoints
    if let Some(divergence) = compare_checkpoints(&checkpoints1, &checkpoints2) {
        print_determinism_failure(args, &divergence, &checkpoints1, &checkpoints2);
        return Err("determinism check failed".into());
    }

    eprintln!(
        "Determinism check PASSED: {} checkpoints matched",
        checkpoints1.len()
    );
    Ok(())
}

/// Print detailed determinism failure report.
fn print_determinism_failure(
    args: &RunArgs,
    divergence: &scx_simulator::CheckpointDivergence,
    _checkpoints1: &[scx_simulator::DeterminismCheckpoint],
    _checkpoints2: &[scx_simulator::DeterminismCheckpoint],
) {
    let seed = args.seed.as_deref().unwrap_or("42");
    eprintln!("DETERMINISM FAILURE at seed {}:", seed);
    eprintln!(
        "  Divergence at checkpoint {} ({} event):",
        divergence.checkpoint_index, divergence.expected.event
    );

    // RIP comparison
    let rip_match =
        divergence.expected.instruction_pointer == divergence.actual.instruction_pointer;
    eprintln!(
        "    RIP: 0x{:x} vs 0x{:x} ({})",
        divergence.expected.instruction_pointer,
        divergence.actual.instruction_pointer,
        if rip_match { "match" } else { "MISMATCH" }
    );

    // RBC comparison
    let rbc_match = divergence.expected.rbc_count == divergence.actual.rbc_count;
    eprintln!(
        "    RBC: {} vs {} ({})",
        divergence.expected.rbc_count,
        divergence.actual.rbc_count,
        if rbc_match { "match" } else { "MISMATCH" }
    );

    // Memory hash comparison
    let hash_match = divergence.expected.memory_hash == divergence.actual.memory_hash;
    eprintln!(
        "    Memory hash: 0x{:x} vs 0x{:x} ({})",
        divergence.expected.memory_hash,
        divergence.actual.memory_hash,
        if hash_match { "match" } else { "MISMATCH" }
    );

    // Event type comparison
    let event_match = divergence.expected.event == divergence.actual.event;
    if !event_match {
        eprintln!(
            "    Event: {} vs {} (MISMATCH)",
            divergence.expected.event, divergence.actual.event
        );
    }

    // CPU ID comparison
    let cpu_match = divergence.expected.cpu_id == divergence.actual.cpu_id;
    if !cpu_match {
        eprintln!(
            "    CPU: {} vs {} (MISMATCH)",
            divergence.expected.cpu_id.0, divergence.actual.cpu_id.0
        );
    }
}

fn run_simulation(args: &RunArgs, scenario: Scenario) -> Result<(), String> {
    let use_e9 = args.preemptive && args.preempt_mode == PreemptModeArg::E9patch;

    // Map the shared RBC state page BEFORE loading the _e9.so — the e9-
    // instrumented .so accesses this address during DT_INIT.
    if use_e9 {
        scx_simulator::preempt::mmap_shared_rbc();
    }

    let sched = load_scheduler(&args.scheduler, args.cpus, use_e9)?;
    let _lock = SIM_LOCK.lock().unwrap();

    // Capture .so base address BEFORE the simulation runs. The scheduler
    // .so is unloaded when the Simulator is dropped, so scheduler_so_base()
    // must be called while the library is still mapped.
    let so_base = scheduler_so_base();

    // Enable preemption recording if --record-preemptions is set.
    if args.record_preemptions.is_some() {
        enable_preemption_collection();
    }

    // Capture scenario metadata before the scenario is consumed by run().
    let current_so_hash = compute_so_hash();
    let so_abs_path = scheduler_so_path();
    let scenario_metadata = TraceMetadata {
        nr_cpus: Some(scenario.nr_cpus),
        nr_tasks: Some(scenario.tasks.len() as u32),
        seed: Some(scenario.seed),
        duration_ns: Some(scenario.duration_ns),
        scheduler: Some(args.scheduler.clone()),
        timeslice_min: scenario.preemptive.as_ref().map(|p| p.timeslice_min),
        timeslice_max: scenario.preemptive.as_ref().map(|p| p.timeslice_max),
        so_hash: if current_so_hash != 0 {
            Some(current_so_hash)
        } else {
            None
        },
        so_path: so_abs_path,
    };

    let trace = Simulator::new(sched).run(scenario);

    if args.dump_trace {
        trace.dump();
    }

    if let Some(path) = &args.perfetto {
        let mut file = std::fs::File::create(path)
            .map_err(|e| format!("failed to create {}: {e}", path.display()))?;
        trace
            .write_perfetto_json(&mut file)
            .map_err(|e| format!("failed to write perfetto trace: {e}"))?;
        eprintln!("wrote perfetto trace to {}", path.display());
    }

    // Record preemption trace if requested.
    if let Some(path) = &args.record_preemptions {
        let records = drain_preemption_records();
        let num_workers = args.cpus as usize;
        let mut preemption_trace =
            PreemptionTrace::from_records(&records, num_workers, args.break_on.to_pmu_event());
        preemption_trace.set_metadata(scenario_metadata);

        let mut file = std::fs::File::create(path)
            .map_err(|e| format!("failed to create {}: {e}", path.display()))?;
        preemption_trace
            .serialize(&mut file, so_base)
            .map_err(|e| format!("failed to write preemption trace: {e}"))?;
        eprintln!(
            "wrote {} preemption records to {}",
            preemption_trace.len(),
            path.display()
        );
    }

    // Print simulation summary.
    if args.verbose_summary {
        let stats = TraceStats::from_trace(&trace);
        println!();
        stats.print_summary();
    } else {
        println!();
        println!("{}", trace.summary());
    }

    if trace.has_error() {
        return Err(format!("simulation error: {:?}", trace.exit_kind()));
    }

    Ok(())
}

fn load_scheduler(name: &str, nr_cpus: u32, e9patch: bool) -> Result<DynamicScheduler, String> {
    let dir = env!("SCHEDULER_SO_DIR");
    let suffix = if e9patch { "_e9" } else { "" };
    let so_path = format!("{dir}/libscx_{name}{suffix}.so");
    if Path::new(&so_path).exists() {
        Ok(DynamicScheduler::load(&so_path, name, nr_cpus))
    } else if e9patch {
        Err(format!(
            "e9patch scheduler variant not found: {so_path}\n\
             Build with: make -C schedulers e9\n\
             (requires e9tool: third_party/e9patch/e9tool)"
        ))
    } else {
        Err(format!(
            "unknown scheduler {name:?}; use --list-schedulers to see available schedulers"
        ))
    }
}

fn list_schedulers() {
    let dir = env!("SCHEDULER_SO_DIR");
    let schedulers = discover_schedulers(Path::new(dir));
    if schedulers.is_empty() {
        eprintln!("no schedulers found in {dir}");
    } else {
        for info in &schedulers {
            println!("{:<16} {}", info.name, info.path.display());
        }
    }
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .event_format(SimFormat)
        .try_init();
}
