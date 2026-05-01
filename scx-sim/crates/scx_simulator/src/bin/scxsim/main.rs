//! scxsim — Run sched_ext scheduler simulations from rt-app workloads.

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand, ValueEnum};
use serde::Deserialize;

use scx_simulator::scenario::{parse_duration_ns, parse_seed};
use scx_simulator::{
    compare_checkpoints, compute_so_hash, discover_schedulers, drain_determinism_checkpoints,
    drain_preemption_records, enable_determinism_mode, enable_preemption_collection, load_rtapp,
    scheduler_so_base, scheduler_so_path, DynamicScheduler, NativeConcurrentConfig, Phase,
    PmuEvent, PreemptMode, PreemptionTrace, PreemptiveConfig, RepeatMode, Scenario, SimFormat,
    Simulator, TaskBehavior, TraceMetadata, TraceStats, SIM_LOCK,
};

mod real_run;

/// Environment variable set after ASLR is disabled to prevent infinite re-exec.
const ASLR_DISABLED_ENV: &str = "SCX_SIM_ASLR_DISABLED";
const DEFAULT_SCHEDULER: &str = "simple";
const DEFAULT_CPUS: u32 = 4;
const DEFAULT_SMT: u32 = 1;
const DEFAULT_TIMESLICE_MIN: u64 = 300;
const DEFAULT_TIMESLICE_MAX: u64 = 1500;
const DEFAULT_WINDOW_NS: u64 = 10_000_000;

/// Disable ASLR for this process by setting the `ADDR_NO_RANDOMIZE` personality
/// flag and re-executing. This ensures scheduler .so base addresses are stable
/// across runs, which is critical for deterministic instruction pointer values
/// in preemption traces and replay.
///
/// The re-exec pattern is standard (used by rr, valgrind, etc.): `personality()`
/// only affects new process images, so we must re-exec for it to take effect.
///
/// Returns without re-exec if:
/// - `SCX_SIM_ASLR_DISABLED=1` is set (already re-exec'd)
/// - `--no-disable-aslr` is present in argv
fn ensure_aslr_disabled() {
    // Already re-exec'd — nothing to do.
    if std::env::var(ASLR_DISABLED_ENV).as_deref() == Ok("1") {
        return;
    }

    // User explicitly opted out.
    if std::env::args().any(|a| a == "--no-disable-aslr") {
        return;
    }

    // Query current personality.
    let current = unsafe { libc::personality(0xffff_ffff) };
    if current < 0 {
        eprintln!("warning: personality() query failed, skipping ASLR disable");
        return;
    }

    let no_randomize = libc::ADDR_NO_RANDOMIZE as libc::c_ulong;

    // Already disabled (e.g. by parent process or kernel config).
    if current as libc::c_ulong & no_randomize != 0 {
        return;
    }

    // Set ADDR_NO_RANDOMIZE and re-exec.
    let new_persona = current as libc::c_ulong | no_randomize;
    let ret = unsafe { libc::personality(new_persona) };
    if ret < 0 {
        eprintln!("warning: personality(ADDR_NO_RANDOMIZE) failed, skipping ASLR disable");
        return;
    }

    reexec_with_aslr_disabled();
}

/// Re-exec the current process with `SCX_SIM_ASLR_DISABLED=1` set. The new
/// process image inherits the `ADDR_NO_RANDOMIZE` personality flag set by the
/// caller. This function never returns — it exits after the child completes.
fn reexec_with_aslr_disabled() -> ! {
    // Mark that we've disabled ASLR so the re-exec'd process skips this path.
    std::env::set_var(ASLR_DISABLED_ENV, "1");

    let exe = std::env::current_exe().unwrap_or_else(|e| {
        panic!("failed to determine current executable for ASLR re-exec: {e}");
    });
    let args: Vec<String> = std::env::args().collect();

    eprintln!("scxsim: disabling ASLR and re-executing...");

    let status = std::process::Command::new(&exe)
        .args(&args[1..])
        .status()
        .unwrap_or_else(|e| {
            panic!("failed to re-exec {} for ASLR disable: {e}", exe.display());
        });

    std::process::exit(status.code().unwrap_or(1));
}

/// How to run the workload.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RealRunMode {
    /// Simulation only (default).
    #[default]
    Off,
    /// Launch virtme-ng VM with rt-app and scheduler.
    Vm,
}

/// Which PMU event to break on for preemptive interleaving.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum, Deserialize)]
#[serde(rename_all = "lowercase")]
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PreemptModeArg {
    /// PMU hardware timer (default; signal delivery has skid).
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
    /// Do not disable ASLR. By default scxsim disables ASLR via
    /// personality(ADDR_NO_RANDOMIZE) and re-execs so that .so base addresses
    /// are stable across runs (important for deterministic replay).
    #[arg(long, global = true)]
    no_disable_aslr: bool,

    /// Load simulator defaults from a JSON config file.
    ///
    /// Precedence is: built-in defaults < config file < explicit CLI flags.
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct SimConfig {
    scheduler: Option<String>,
    cpus: Option<u32>,
    smt: Option<u32>,
    seed: Option<ConfigSeed>,
    fixed_priority: Option<bool>,
    end_time: Option<String>,
    warmup_ms: Option<u64>,
    perfetto: Option<PathBuf>,
    dump_trace: Option<bool>,
    noise_enabled: Option<bool>,
    tick_jitter_stddev_ns: Option<u64>,
    run_jitter_cv_ppm: Option<u64>,
    overhead_enabled: Option<bool>,
    context_switch_overhead_ns: Option<u64>,
    voluntary_context_switch_overhead_ns: Option<u64>,
    involuntary_context_switch_overhead_ns: Option<u64>,
    context_switch_jitter_stddev_ns: Option<u64>,
    dsq_consume_ns: Option<u64>,
    running_overhead_ns: Option<u64>,
    update_idle_overhead_ns: Option<u64>,
    ipi_delivery_ns: Option<u64>,
    wakeup_latency_floor_ns: Option<u64>,
    wakeup_jitter_stddev_ns: Option<u64>,
    migration_overhead_ns: Option<u64>,
    cross_llc_penalty_ns: Option<u64>,
    rbc_ns: Option<u64>,
    watchdog_timeout: Option<String>,
    interleave: Option<bool>,
    preemptive: Option<bool>,
    timeslice_min: Option<u64>,
    timeslice_max: Option<u64>,
    break_on: Option<BreakOn>,
    preempt_mode: Option<PreemptModeArg>,
    native_concurrent: Option<bool>,
    window_ns: Option<u64>,
    real_run: Option<RealRunMode>,
    wprof: Option<bool>,
    bpf_trace: Option<bool>,
    determinism_check: Option<bool>,
    record_preemptions: Option<PathBuf>,
    verbose_summary: Option<bool>,
    wait_debugger: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum ConfigSeed {
    Number(u32),
    String(String),
}

impl ConfigSeed {
    fn into_string(self) -> String {
        match self {
            ConfigSeed::Number(seed) => seed.to_string(),
            ConfigSeed::String(seed) => seed,
        }
    }
}

#[derive(Debug, Clone)]
struct ResolvedRunConfig {
    scheduler: String,
    cpus: u32,
    smt: u32,
    seed: Option<String>,
    fixed_priority: bool,
    end_time: Option<String>,
    warmup_ms: Option<u64>,
    perfetto: Option<PathBuf>,
    dump_trace: bool,
    noise_enabled: bool,
    tick_jitter_stddev_ns: Option<u64>,
    run_jitter_cv_ppm: Option<u64>,
    overhead_enabled: bool,
    context_switch_overhead_ns: Option<u64>,
    voluntary_context_switch_overhead_ns: Option<u64>,
    involuntary_context_switch_overhead_ns: Option<u64>,
    context_switch_jitter_stddev_ns: Option<u64>,
    dsq_consume_ns: Option<u64>,
    running_overhead_ns: Option<u64>,
    update_idle_overhead_ns: Option<u64>,
    ipi_delivery_ns: Option<u64>,
    wakeup_latency_floor_ns: Option<u64>,
    wakeup_jitter_stddev_ns: Option<u64>,
    migration_overhead_ns: Option<u64>,
    cross_llc_penalty_ns: Option<u64>,
    rbc_ns: Option<u64>,
    watchdog_timeout: Option<String>,
    interleave: bool,
    preemptive: bool,
    timeslice_min: u64,
    timeslice_max: u64,
    break_on: BreakOn,
    preempt_mode: PreemptModeArg,
    native_concurrent: bool,
    window_ns: u64,
    real_run: RealRunMode,
    wprof: bool,
    bpf_trace: bool,
    determinism_check: bool,
    record_preemptions: Option<PathBuf>,
    verbose_summary: bool,
    wait_debugger: bool,
}

#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
enum Command {
    /// Run a simulation from an rt-app workload.
    Run(RunArgs),
    /// Replay a recorded preemption trace.
    Replay(ReplayArgs),
    /// Print address-space layout for ASLR verification.
    ///
    /// Loads the scheduler, prints .so base, heap, and stack addresses,
    /// then exits. Used by the ASLR stability test.
    #[command(hide = true)]
    PrintAddresses(PrintAddressesArgs),
}

/// Arguments for the `run` subcommand.
#[derive(Parser)]
struct RunArgs {
    /// Path to an rt-app JSON workload file.
    workload: Option<PathBuf>,

    /// Scheduler name. Default: simple.
    #[arg(short, long)]
    scheduler: Option<String>,

    /// Number of simulated CPUs (minimum 1). Default: 4.
    #[arg(short, long, value_parser = clap::value_parser!(u32).range(1..))]
    cpus: Option<u32>,

    /// SMT threads per core (minimum 1). Default: 1.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    smt: Option<u32>,

    /// PRNG seed (u32 integer or "entropy" for OS randomness).
    ///
    /// Controls deterministic simulation: tick jitter, context-switch
    /// overhead noise, and event tiebreaking all derive from this seed.
    /// Defaults to 42.
    #[arg(long)]
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

    /// Warmup period in milliseconds.
    ///
    /// When set, trace statistics (summary, TraceStats) exclude events
    /// that occurred before this simulated time. The simulation still runs
    /// from time 0, but metrics only reflect post-warmup behavior.
    #[arg(long, value_name = "MS")]
    warmup_ms: Option<u64>,

    /// Write Perfetto trace JSON to file.
    #[arg(long, value_name = "PATH")]
    perfetto: Option<PathBuf>,

    /// Print trace events to stderr.
    #[arg(long)]
    dump_trace: bool,

    /// Disable tick jitter noise.
    #[arg(long)]
    no_noise: bool,

    /// Standard deviation for tick jitter (ns).
    #[arg(long, value_name = "NS")]
    tick_jitter_stddev_ns: Option<u64>,

    /// Coefficient of variation for run-time jitter (ppm).
    #[arg(long, value_name = "PPM")]
    run_jitter_cv_ppm: Option<u64>,

    /// Disable context-switch overhead.
    #[arg(long)]
    no_overhead: bool,

    /// Set both voluntary and involuntary context-switch overhead (ns).
    #[arg(long, value_name = "NS")]
    context_switch_overhead_ns: Option<u64>,

    /// Voluntary context-switch overhead (ns).
    #[arg(long, value_name = "NS")]
    voluntary_context_switch_overhead_ns: Option<u64>,

    /// Involuntary context-switch overhead (ns).
    #[arg(long, value_name = "NS")]
    involuntary_context_switch_overhead_ns: Option<u64>,

    /// Standard deviation for context-switch jitter (ns).
    #[arg(long, value_name = "NS")]
    context_switch_jitter_stddev_ns: Option<u64>,

    /// Global DSQ consume overhead (ns).
    #[arg(long, value_name = "NS")]
    dsq_consume_ns: Option<u64>,

    /// Overhead for `ops.running()` callback dispatch (ns).
    #[arg(long, value_name = "NS")]
    running_overhead_ns: Option<u64>,

    /// Overhead for `ops.update_idle()` callback dispatch (ns).
    #[arg(long, value_name = "NS")]
    update_idle_overhead_ns: Option<u64>,

    /// IPI delivery latency for `scx_bpf_kick_cpu` (ns).
    #[arg(long, value_name = "NS")]
    ipi_delivery_ns: Option<u64>,

    /// Minimum wakeup latency floor for wake→running transitions (ns).
    #[arg(long, value_name = "NS")]
    wakeup_latency_floor_ns: Option<u64>,

    /// Standard deviation for wakeup latency jitter (ns).
    #[arg(long, value_name = "NS")]
    wakeup_jitter_stddev_ns: Option<u64>,

    /// Extra latency when a task migrates to a different CPU (ns).
    #[arg(long, value_name = "NS")]
    migration_overhead_ns: Option<u64>,

    /// Extra latency for cross-LLC migrations (ns).
    #[arg(long, value_name = "NS")]
    cross_llc_penalty_ns: Option<u64>,

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
    /// units: "2s", "500ms", etc. Use "0" or "off" to disable. Default: 30s.
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
    /// --preemptive mode. Default: 300. With PMU skid (~30-100 branches),
    /// actual preemption fires at ~130-200 branches after the last kfunc.
    ///
    /// WARNING: Values below 200 can cause livelock with complex schedulers
    /// (e.g. LAVD with structop_rbc up to 2026). Use --timeslice-min 300+ for LAVD.
    #[arg(long)]
    timeslice_min: Option<u64>,

    /// Maximum preemptive timeslice in retired conditional branches.
    ///
    /// Controls the upper bound of the random timeslice range used by
    /// --preemptive mode. Default: 1500. With PMU skid (~30-100 branches),
    /// actual preemption fires at ~130-600 branches after the last kfunc.
    ///
    /// Upper bound of the PRNG-generated timeslice range.
    #[arg(long)]
    timeslice_max: Option<u64>,

    /// Which PMU event to break on for preemptive interleaving.
    ///
    /// rbc: Retired conditional branches (default, lower frequency).
    /// insn: Instructions retired (higher frequency — use larger timeslice).
    #[arg(long, value_enum)]
    break_on: Option<BreakOn>,

    /// Preemption mechanism for mid-C-code preemption.
    ///
    /// pmu: Hardware PMU timer (default; signal delivery has skid but counter values are exact).
    /// e9patch: Software RBC via e9patch-instrumented .so (deterministic,
    ///          debugger-compatible, requires _e9.so variant).
    #[arg(long, value_enum)]
    preempt_mode: Option<PreemptModeArg>,

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
    #[arg(long)]
    window_ns: Option<u64>,

    /// List available schedulers and exit.
    #[arg(long)]
    list_schedulers: bool,

    /// Run workload in real environment.
    ///
    /// off: simulation only (default)
    /// vm: launch virtme-ng VM with rt-app and scheduler
    #[arg(long, value_enum)]
    real_run: Option<RealRunMode>,

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

    /// Preemption mechanism for replay.
    ///
    /// pmu: PMU + hardware breakpoint replay (default, requires PMU hardware).
    /// e9patch: Software RBC via e9patch-instrumented .so (deterministic,
    ///          no PMU needed, requires _e9.so variant).
    #[arg(long, value_enum, default_value_t = PreemptModeArg::Pmu)]
    preempt_mode: PreemptModeArg,

    /// Pause before ops.init() so a debugger can attach.
    ///
    /// After the scheduler .so is loaded, writes an lldb breakpoint script
    /// next to the .so, prints a copy-pasteable lldb command, and spin-waits
    /// for a debugger. A single `continue` from the attach stop hits the
    /// first ops breakpoint.
    #[arg(long)]
    wait_debugger: bool,
}

/// Arguments for the `print-addresses` subcommand.
#[derive(Parser)]
struct PrintAddressesArgs {
    /// Scheduler name.
    #[arg(short, long, default_value = "simple")]
    scheduler: String,
}

fn load_sim_config(path: &Path) -> Result<SimConfig, String> {
    let json = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read config {}: {e}", path.display()))?;
    serde_json::from_str(&json)
        .map_err(|e| format!("failed to parse config {}: {e}", path.display()))
}

fn resolve_run_config(args: &RunArgs, config: SimConfig) -> Result<ResolvedRunConfig, String> {
    let mut noise_enabled = config.noise_enabled.unwrap_or(true);
    if args.no_noise {
        noise_enabled = false;
    }

    let mut overhead_enabled = config.overhead_enabled.unwrap_or(true);
    if args.no_overhead {
        overhead_enabled = false;
    }

    let resolved = ResolvedRunConfig {
        scheduler: args
            .scheduler
            .clone()
            .or(config.scheduler)
            .unwrap_or_else(|| DEFAULT_SCHEDULER.to_string()),
        cpus: args.cpus.or(config.cpus).unwrap_or(DEFAULT_CPUS),
        smt: args.smt.or(config.smt).unwrap_or(DEFAULT_SMT),
        seed: args
            .seed
            .clone()
            .or(config.seed.map(ConfigSeed::into_string)),
        fixed_priority: args.fixed_priority || config.fixed_priority.unwrap_or(false),
        end_time: args.end_time.clone().or(config.end_time),
        warmup_ms: args.warmup_ms.or(config.warmup_ms),
        perfetto: args.perfetto.clone().or(config.perfetto),
        dump_trace: args.dump_trace || config.dump_trace.unwrap_or(false),
        noise_enabled,
        tick_jitter_stddev_ns: args.tick_jitter_stddev_ns.or(config.tick_jitter_stddev_ns),
        run_jitter_cv_ppm: args.run_jitter_cv_ppm.or(config.run_jitter_cv_ppm),
        overhead_enabled,
        context_switch_overhead_ns: args
            .context_switch_overhead_ns
            .or(config.context_switch_overhead_ns),
        voluntary_context_switch_overhead_ns: args
            .voluntary_context_switch_overhead_ns
            .or(config.voluntary_context_switch_overhead_ns),
        involuntary_context_switch_overhead_ns: args
            .involuntary_context_switch_overhead_ns
            .or(config.involuntary_context_switch_overhead_ns),
        context_switch_jitter_stddev_ns: args
            .context_switch_jitter_stddev_ns
            .or(config.context_switch_jitter_stddev_ns),
        dsq_consume_ns: args.dsq_consume_ns.or(config.dsq_consume_ns),
        running_overhead_ns: args.running_overhead_ns.or(config.running_overhead_ns),
        update_idle_overhead_ns: args
            .update_idle_overhead_ns
            .or(config.update_idle_overhead_ns),
        ipi_delivery_ns: args.ipi_delivery_ns.or(config.ipi_delivery_ns),
        wakeup_latency_floor_ns: args
            .wakeup_latency_floor_ns
            .or(config.wakeup_latency_floor_ns),
        wakeup_jitter_stddev_ns: args
            .wakeup_jitter_stddev_ns
            .or(config.wakeup_jitter_stddev_ns),
        migration_overhead_ns: args.migration_overhead_ns.or(config.migration_overhead_ns),
        cross_llc_penalty_ns: args.cross_llc_penalty_ns.or(config.cross_llc_penalty_ns),
        rbc_ns: if args.no_rbc {
            Some(0)
        } else {
            args.rbc_ns.or(config.rbc_ns)
        },
        watchdog_timeout: args.watchdog_timeout.clone().or(config.watchdog_timeout),
        interleave: args.interleave || config.interleave.unwrap_or(false),
        preemptive: args.preemptive || config.preemptive.unwrap_or(false),
        timeslice_min: args
            .timeslice_min
            .or(config.timeslice_min)
            .unwrap_or(DEFAULT_TIMESLICE_MIN),
        timeslice_max: args
            .timeslice_max
            .or(config.timeslice_max)
            .unwrap_or(DEFAULT_TIMESLICE_MAX),
        break_on: args.break_on.or(config.break_on).unwrap_or(BreakOn::Rbc),
        preempt_mode: args
            .preempt_mode
            .or(config.preempt_mode)
            .unwrap_or(PreemptModeArg::Pmu),
        native_concurrent: args.native_concurrent || config.native_concurrent.unwrap_or(false),
        window_ns: args
            .window_ns
            .or(config.window_ns)
            .unwrap_or(DEFAULT_WINDOW_NS),
        real_run: args
            .real_run
            .or(config.real_run)
            .unwrap_or(RealRunMode::Off),
        wprof: args.wprof || config.wprof.unwrap_or(false),
        bpf_trace: args.bpf_trace || config.bpf_trace.unwrap_or(false),
        determinism_check: args.determinism_check || config.determinism_check.unwrap_or(false),
        record_preemptions: args
            .record_preemptions
            .clone()
            .or(config.record_preemptions),
        verbose_summary: args.verbose_summary || config.verbose_summary.unwrap_or(false),
        wait_debugger: args.wait_debugger || config.wait_debugger.unwrap_or(false),
    };

    if resolved.cpus == 0 {
        return Err("cpus must be at least 1".into());
    }
    if resolved.smt == 0 {
        return Err("smt must be at least 1".into());
    }
    if resolved.preemptive && resolved.native_concurrent {
        return Err("--preemptive conflicts with --native-concurrent".into());
    }
    if resolved.wprof && resolved.bpf_trace {
        return Err("--wprof conflicts with --bpf-trace".into());
    }

    Ok(resolved)
}

fn apply_run_config(scenario: &mut Scenario, config: &ResolvedRunConfig) -> Result<(), String> {
    scenario.smt_threads_per_core = config.smt;
    if let Some(ref seed_str) = config.seed {
        scenario.seed = parse_seed(Some(seed_str));
    }
    scenario.noise.enabled = config.noise_enabled;
    if let Some(ns) = config.tick_jitter_stddev_ns {
        scenario.noise.tick_jitter_stddev_ns = ns;
    }
    if let Some(ppm) = config.run_jitter_cv_ppm {
        scenario.noise.run_jitter_cv_ppm = ppm;
    }
    scenario.overhead.enabled = config.overhead_enabled;
    if let Some(ns) = config.context_switch_overhead_ns {
        scenario.overhead.voluntary_csw_ns = ns;
        scenario.overhead.involuntary_csw_ns = ns;
    }
    if let Some(ns) = config.voluntary_context_switch_overhead_ns {
        scenario.overhead.voluntary_csw_ns = ns;
    }
    if let Some(ns) = config.involuntary_context_switch_overhead_ns {
        scenario.overhead.involuntary_csw_ns = ns;
    }
    if let Some(ns) = config.context_switch_jitter_stddev_ns {
        scenario.overhead.csw_jitter_stddev_ns = ns;
    }
    if let Some(ns) = config.dsq_consume_ns {
        scenario.overhead.dsq_consume_ns = ns;
    }
    if let Some(ns) = config.running_overhead_ns {
        scenario.overhead.running_overhead_ns = ns;
    }
    if let Some(ns) = config.update_idle_overhead_ns {
        scenario.overhead.update_idle_overhead_ns = ns;
    }
    if let Some(ns) = config.ipi_delivery_ns {
        scenario.overhead.ipi_delivery_ns = ns;
    }
    if let Some(ns) = config.wakeup_latency_floor_ns {
        scenario.overhead.wakeup_latency_floor_ns = ns;
    }
    if let Some(ns) = config.wakeup_jitter_stddev_ns {
        scenario.overhead.wakeup_jitter_stddev_ns = ns;
    }
    if let Some(ns) = config.migration_overhead_ns {
        scenario.overhead.migration_penalty_ns = ns;
    }
    if let Some(ns) = config.cross_llc_penalty_ns {
        scenario.overhead.cross_llc_migration_penalty_ns = ns;
    }
    scenario.fixed_priority = config.fixed_priority;
    scenario.interleave = config.interleave;
    if config.preemptive {
        scenario.preemptive = Some(PreemptiveConfig {
            timeslice_min: config.timeslice_min,
            timeslice_max: config.timeslice_max,
            cooperative_only: false,
            break_on: config.break_on.to_pmu_event(),
            preempt_mode: config.preempt_mode.to_preempt_mode(),
        });
        scenario.interleave = true;
    }
    if config.native_concurrent {
        scenario.native_concurrent = Some(NativeConcurrentConfig {
            window_ns: config.window_ns,
        });
        scenario.interleave = true;
    }
    if let Some(ref end_time) = config.end_time {
        scenario.duration_ns =
            parse_duration_ns(end_time).map_err(|e| format!("--end-time: {e}"))?;
    }
    if let Some(rbc_ns) = config.rbc_ns {
        scenario.sched_overhead_rbc_ns = Some(rbc_ns);
    }
    if let Some(ref timeout) = config.watchdog_timeout {
        let normalized = timeout.trim().to_lowercase();
        if normalized == "off" || normalized == "none" || normalized == "0" {
            scenario.watchdog_timeout_ns = None;
        } else {
            let ns = parse_duration_ns(timeout).map_err(|e| format!("--watchdog-timeout: {e}"))?;
            if ns == 0 {
                scenario.watchdog_timeout_ns = None;
            } else {
                scenario.watchdog_timeout_ns = Some(ns);
            }
        }
    }
    if let Some(warmup_ms) = config.warmup_ms {
        scenario.warmup_ns = warmup_ms * 1_000_000;
    }
    if config.wait_debugger {
        scenario.wait_debugger = true;
    }

    Ok(())
}

fn main() {
    // Disable ASLR before anything else so that the scheduler .so is loaded
    // at a stable base address. This must happen before CLI parsing because
    // it may re-exec the process.
    ensure_aslr_disabled();

    let cli = Cli::parse();
    init_tracing();

    let result = match &cli.command {
        Command::Run(args) => run(args, cli.config.as_deref()),
        Command::Replay(args) => replay_simulation(&args),
        Command::PrintAddresses(args) => print_addresses(&args),
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run(args: &RunArgs, config_path: Option<&Path>) -> Result<(), String> {
    if args.list_schedulers {
        list_schedulers();
        return Ok(());
    }

    let config = if let Some(path) = config_path {
        load_sim_config(path)?
    } else {
        SimConfig::default()
    };
    let resolved = resolve_run_config(args, config)?;

    let workload_path = args
        .workload
        .as_ref()
        .ok_or("missing required argument: <WORKLOAD>")?;

    let json = std::fs::read_to_string(workload_path)
        .map_err(|e| format!("failed to read {}: {e}", workload_path.display()))?;

    let mut scenario =
        load_rtapp(&json, resolved.cpus).map_err(|e| format!("failed to parse workload: {e}"))?;
    apply_run_config(&mut scenario, &resolved)?;

    // Validate --wprof and --bpf-trace require --real-run vm
    if resolved.wprof && resolved.real_run != RealRunMode::Vm {
        return Err("--wprof requires --real-run vm".into());
    }
    if resolved.bpf_trace && resolved.real_run != RealRunMode::Vm {
        return Err("--bpf-trace requires --real-run vm".into());
    }

    // Determine trace mode
    let trace_mode = if resolved.wprof {
        real_run::TraceMode::Wprof
    } else if resolved.bpf_trace {
        real_run::TraceMode::BpfTrace
    } else {
        real_run::TraceMode::None
    };

    // Handle --determinism-check mode
    if resolved.determinism_check {
        if resolved.real_run != RealRunMode::Off {
            return Err("--determinism-check conflicts with --real-run".into());
        }
        return run_determinism_check(&resolved, scenario);
    }

    // Handle --real-run mode
    match resolved.real_run {
        RealRunMode::Off => {
            run_simulation(&resolved, scenario)?;
        }
        RealRunMode::Vm => {
            real_run::run_vm(
                workload_path,
                &resolved.scheduler,
                resolved.cpus,
                trace_mode,
            )?;
        }
    }

    Ok(())
}

/// Extract the scheduler prefix from a .so path.
///
/// Given a path like `/path/to/libscx_simple.so`, returns `"simple"`.
/// For e9 variants like `/path/to/libscx_simple_e9.so`, also returns
/// `"simple"` (strips the `_e9` suffix).
/// Panics if the filename does not match the `libscx_<name>.so` pattern.
fn scheduler_prefix_from_path(path: &Path) -> String {
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_else(|| panic!("invalid scheduler path: {}", path.display()));
    let name = filename
        .strip_prefix("libscx_")
        .and_then(|s| s.strip_suffix(".so"))
        .unwrap_or_else(|| {
            panic!("scheduler .so filename must match libscx_<name>.so, got: {filename}")
        });
    // Strip _e9 suffix if present — the ops symbols use the base name.
    name.strip_suffix("_e9").unwrap_or(name).to_string()
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

/// Derive the `_e9.so` variant path from a regular `.so` path.
///
/// Transforms `libscx_foo.so` into `libscx_foo_e9.so`. Used when
/// `--preempt-mode e9patch` is specified for replay without an explicit
/// `--scheduler-file` override.
fn derive_e9_scheduler_path(base_path: &Path) -> Result<PathBuf, String> {
    let stem = base_path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| format!("cannot derive e9 path from: {}", base_path.display()))?;

    // If it already ends with _e9, use as-is.
    if stem.ends_with("_e9") {
        return Ok(base_path.to_path_buf());
    }

    let e9_name = format!("{stem}_e9.so");
    let e9_path = base_path.with_file_name(e9_name);
    if e9_path.exists() {
        eprintln!("replay: using e9patch variant: {}", e9_path.display());
        Ok(e9_path)
    } else {
        Err(format!(
            "e9patch scheduler variant not found: {}\n\
             Build with: make -C schedulers e9\n\
             Or pass --scheduler-file <path_to_e9.so>",
            e9_path.display()
        ))
    }
}

/// Create a RIP-patched `_e9rip.so` for e9patch RIP-targeted replay.
///
/// Loads the base scheduler `.so` temporarily to determine `so_base`,
/// re-loads the trace with the correct base, extracts unique RIP offsets,
/// then runs e9tool to create the patched `.so` with both Jcc and
/// RIP-specific trampolines.
///
/// Returns the path to the `_e9rip.so` on success.
fn create_e9rip_scheduler(base_so_path: &Path, trace_file: &Path) -> Result<PathBuf, String> {
    use scx_simulator::backend::e9patch;
    use std::io::BufReader;

    // Check if a cached _e9rip.so already exists.
    let e9rip_path = e9patch::derive_e9rip_path(base_so_path);
    if e9rip_path.exists() {
        eprintln!(
            "replay: using cached e9patch RIP variant: {}",
            e9rip_path.display()
        );
        return Ok(e9rip_path);
    }

    // Load the base .so temporarily to discover so_base.
    let prefix = scheduler_prefix_from_path(base_so_path);
    let base_so_str = base_so_path.to_str().unwrap_or_else(|| {
        panic!(
            "scheduler path is not valid UTF-8: {}",
            base_so_path.display()
        )
    });

    // Temporarily load the base .so to get so_base for RIP reconstruction.
    // This load is scoped so the .so is unloaded before we create and load
    // the patched variant.
    let so_base = {
        let _sched = DynamicScheduler::load(base_so_str, &prefix, 1);
        let _lock = SIM_LOCK.lock().unwrap();
        scheduler_so_base()
    };

    // Re-deserialize the trace with the correct so_base.
    let file = std::fs::File::open(trace_file)
        .map_err(|e| format!("cannot open trace file {}: {e}", trace_file.display()))?;
    let mut reader = BufReader::new(file);
    let trace = PreemptionTrace::deserialize(&mut reader, so_base)
        .map_err(|e| format!("failed to parse trace file: {e}"))?;

    // Extract unique RIPs and convert to .so-relative offsets for e9tool.
    let rips = e9patch::collect_trace_rips(&trace);
    if rips.is_empty() {
        return Err("e9patch RIP mode: no non-zero RIPs found in trace. \
             Cannot create RIP-patched .so."
            .to_string());
    }

    // Convert absolute RIPs to .so-relative offsets (ELF virtual addresses).
    let rip_offsets: Vec<u64> = rips
        .iter()
        .filter_map(|&rip| {
            if so_base > 0 && rip >= so_base {
                Some(rip - so_base)
            } else {
                None
            }
        })
        .collect();

    if rip_offsets.is_empty() {
        return Err("e9patch RIP mode: no RIPs within the scheduler .so range. \
             The trace may have been recorded with a different scheduler."
            .to_string());
    }

    eprintln!(
        "replay: creating e9patch RIP variant with {} target addresses",
        rip_offsets.len()
    );

    // Run e9tool to create the _e9rip.so.
    e9patch::create_e9rip_so(base_so_path, &rip_offsets)
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
    let mut scheduler_path = resolve_scheduler_path(args.scheduler_file.as_deref(), metadata)?;

    // Extract required metadata fields with clear error messages.
    let nr_cpus = metadata
        .nr_cpus
        .ok_or("trace file missing required metadata: nr_cpus")?;
    let nr_tasks = metadata
        .nr_tasks
        .ok_or("trace file missing required metadata: nr_tasks")?;
    let seed = metadata
        .seed
        .ok_or("trace file missing required metadata: seed")?;
    let duration_ns = metadata
        .duration_ns
        .ok_or("trace file missing required metadata: duration_ns")?;
    let timeslice_min = metadata
        .timeslice_min
        .ok_or("trace file missing required metadata: timeslice_min")?;
    let timeslice_max = metadata
        .timeslice_max
        .ok_or("trace file missing required metadata: timeslice_max")?;

    let use_e9_replay = args.preempt_mode == PreemptModeArg::E9patch;
    let use_e9_rip_mode = use_e9_replay && pre_trace.break_on() == PmuEvent::InstructionsRetired;

    // Map the shared RBC state page if e9patch replay mode is requested.
    // Must happen BEFORE loading the _e9.so (the instrumented Jcc
    // instructions access the fixed address during DT_INIT).
    if use_e9_replay {
        scx_simulator::preempt::mmap_shared_rbc();
    }

    // Map the RIP shared state page if e9patch RIP mode is detected.
    // Must happen BEFORE loading the _e9rip.so.
    if use_e9_rip_mode {
        scx_simulator::backend::e9patch::mmap_rip_shared();
        eprintln!("replay: detected break_on=insn trace, using e9patch RIP mode");
    }

    // For e9patch replay, derive the _e9.so path from the trace's .so path
    // if the user didn't provide an explicit --scheduler-file override.
    // In RIP mode, we skip this — we'll create a _e9rip.so at runtime.
    if use_e9_replay && !use_e9_rip_mode && args.scheduler_file.is_none() {
        scheduler_path = derive_e9_scheduler_path(&scheduler_path)?;
    }

    // For e9patch RIP mode: load the base .so first to get so_base,
    // then create the _e9rip.so with RIP-targeted trampolines, then
    // reload with the patched .so.
    if use_e9_rip_mode {
        scheduler_path = create_e9rip_scheduler(&scheduler_path, &args.trace_file)?;
    }

    let prefix = scheduler_prefix_from_path(&scheduler_path);
    let so_path_str = scheduler_path.to_str().unwrap_or_else(|| {
        panic!(
            "scheduler path is not valid UTF-8: {}",
            scheduler_path.display()
        )
    });

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
            preempt_mode: args.preempt_mode.to_preempt_mode(),
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

fn run_determinism_check(config: &ResolvedRunConfig, scenario: Scenario) -> Result<(), String> {
    let _lock = SIM_LOCK.lock().unwrap();
    let use_e9 = config.preemptive && config.preempt_mode == PreemptModeArg::E9patch;
    let scenario_seed = scenario.seed;

    // Map the shared RBC state page BEFORE loading the _e9.so.
    if use_e9 {
        scx_simulator::preempt::mmap_shared_rbc();
    }

    // Run 1: collect checkpoints
    enable_determinism_mode();
    let sched1 = load_scheduler(&config.scheduler, config.cpus, use_e9)?;
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
    let sched2 = load_scheduler(&config.scheduler, config.cpus, use_e9)?;
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
        print_determinism_failure(
            config,
            scenario_seed,
            &divergence,
            &checkpoints1,
            &checkpoints2,
        );
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
    config: &ResolvedRunConfig,
    scenario_seed: u32,
    divergence: &scx_simulator::CheckpointDivergence,
    _checkpoints1: &[scx_simulator::DeterminismCheckpoint],
    _checkpoints2: &[scx_simulator::DeterminismCheckpoint],
) {
    let seed = config.seed.as_deref().unwrap_or("42");
    eprintln!("DETERMINISM FAILURE at seed {}:", seed);
    eprintln!("  Effective scenario seed: {scenario_seed}");
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

fn run_simulation(config: &ResolvedRunConfig, scenario: Scenario) -> Result<(), String> {
    let use_e9 = config.preemptive && config.preempt_mode == PreemptModeArg::E9patch;

    // Map the shared RBC state page BEFORE loading the _e9.so — the e9-
    // instrumented .so accesses this address during DT_INIT.
    if use_e9 {
        scx_simulator::preempt::mmap_shared_rbc();
    }

    let sched = load_scheduler(&config.scheduler, config.cpus, use_e9)?;
    let _lock = SIM_LOCK.lock().unwrap();

    // Capture .so base address BEFORE the simulation runs. The scheduler
    // .so is unloaded when the Simulator is dropped, so scheduler_so_base()
    // must be called while the library is still mapped.
    let so_base = scheduler_so_base();

    // Enable preemption recording if --record-preemptions is set.
    if config.record_preemptions.is_some() {
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
        scheduler: Some(config.scheduler.clone()),
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

    if config.dump_trace {
        trace.dump();
    }

    if let Some(path) = &config.perfetto {
        let mut file = std::fs::File::create(path)
            .map_err(|e| format!("failed to create {}: {e}", path.display()))?;
        trace
            .write_perfetto_json(&mut file)
            .map_err(|e| format!("failed to write perfetto trace: {e}"))?;
        eprintln!("wrote perfetto trace to {}", path.display());
    }

    // Record preemption trace if requested.
    if let Some(path) = &config.record_preemptions {
        let records = drain_preemption_records();
        let num_workers = config.cpus as usize;
        let mut preemption_trace =
            PreemptionTrace::from_records(&records, num_workers, config.break_on.to_pmu_event());
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
    if config.verbose_summary {
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

/// Print address-space layout for ASLR verification.
///
/// Loads the scheduler .so, allocates a heap object, and prints three
/// addresses (so_base, heap, stack) in a stable machine-readable format.
/// The caller (ASLR test) runs this twice and compares the output.
fn print_addresses(args: &PrintAddressesArgs) -> Result<(), String> {
    let _sched = load_scheduler(&args.scheduler, 4, false)?;
    let _lock = SIM_LOCK.lock().unwrap();
    let so_base = scheduler_so_base();

    // Heap: allocate a boxed value and take its address.
    let heap_obj = Box::new(42u64);
    let heap_addr = &*heap_obj as *const u64 as u64;

    // Stack: address of a local variable.
    let stack_var: u64 = 0xDEAD;
    let stack_addr = &stack_var as *const u64 as u64;

    println!("so_base=0x{so_base:x}");
    println!("heap=0x{heap_addr:x}");
    println!("stack=0x{stack_addr:x}");

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

#[cfg(test)]
mod tests {
    use super::*;

    fn base_run_args() -> RunArgs {
        RunArgs {
            workload: Some(PathBuf::from("workload.json")),
            scheduler: None,
            cpus: None,
            smt: None,
            seed: None,
            fixed_priority: false,
            end_time: None,
            warmup_ms: None,
            perfetto: None,
            dump_trace: false,
            no_noise: false,
            tick_jitter_stddev_ns: None,
            run_jitter_cv_ppm: None,
            no_overhead: false,
            context_switch_overhead_ns: None,
            voluntary_context_switch_overhead_ns: None,
            involuntary_context_switch_overhead_ns: None,
            context_switch_jitter_stddev_ns: None,
            dsq_consume_ns: None,
            running_overhead_ns: None,
            update_idle_overhead_ns: None,
            ipi_delivery_ns: None,
            wakeup_latency_floor_ns: None,
            wakeup_jitter_stddev_ns: None,
            migration_overhead_ns: None,
            cross_llc_penalty_ns: None,
            rbc_ns: None,
            no_rbc: false,
            watchdog_timeout: None,
            interleave: false,
            preemptive: false,
            timeslice_min: None,
            timeslice_max: None,
            break_on: None,
            preempt_mode: None,
            native_concurrent: false,
            window_ns: None,
            list_schedulers: false,
            real_run: None,
            wprof: false,
            bpf_trace: false,
            determinism_check: false,
            record_preemptions: None,
            verbose_summary: false,
            wait_debugger: false,
        }
    }

    #[test]
    fn resolve_run_config_prefers_cli_over_config() {
        let mut args = base_run_args();
        args.scheduler = Some("lavd".into());
        args.cpus = Some(8);
        args.preemptive = true;
        args.timeslice_min = Some(777);
        args.no_noise = true;

        let config = SimConfig {
            scheduler: Some("simple".into()),
            cpus: Some(6),
            preemptive: Some(false),
            timeslice_min: Some(333),
            noise_enabled: Some(true),
            ..SimConfig::default()
        };

        let resolved = resolve_run_config(&args, config).unwrap();
        assert_eq!(resolved.scheduler, "lavd");
        assert_eq!(resolved.cpus, 8);
        assert!(resolved.preemptive);
        assert_eq!(resolved.timeslice_min, 777);
        assert!(!resolved.noise_enabled);
    }

    #[test]
    fn resolve_run_config_uses_config_when_cli_absent() {
        let args = base_run_args();
        let config = SimConfig {
            scheduler: Some("lavd".into()),
            cpus: Some(12),
            smt: Some(2),
            interleave: Some(true),
            real_run: Some(RealRunMode::Vm),
            break_on: Some(BreakOn::Insn),
            preempt_mode: Some(PreemptModeArg::E9patch),
            ..SimConfig::default()
        };

        let resolved = resolve_run_config(&args, config).unwrap();
        assert_eq!(resolved.scheduler, "lavd");
        assert_eq!(resolved.cpus, 12);
        assert_eq!(resolved.smt, 2);
        assert!(resolved.interleave);
        assert_eq!(resolved.real_run, RealRunMode::Vm);
        assert_eq!(resolved.break_on, BreakOn::Insn);
        assert_eq!(resolved.preempt_mode, PreemptModeArg::E9patch);
    }

    #[test]
    fn resolve_run_config_rejects_conflicting_modes() {
        let args = base_run_args();
        let config = SimConfig {
            preemptive: Some(true),
            native_concurrent: Some(true),
            ..SimConfig::default()
        };

        let err = resolve_run_config(&args, config).unwrap_err();
        assert!(err.contains("conflicts"));
    }

    #[test]
    fn resolve_run_config_accepts_numeric_config_seed() {
        let args = base_run_args();
        let config: SimConfig = serde_json::from_str(r#"{"seed": 42}"#).unwrap();

        let resolved = resolve_run_config(&args, config).unwrap();
        assert_eq!(resolved.seed.as_deref(), Some("42"));
    }
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .event_format(SimFormat)
        .try_init();
}
