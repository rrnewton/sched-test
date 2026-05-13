//! scxsim — Run sched_ext scheduler simulations from rt-app workloads.

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand, ValueEnum};

use scx_simulator::scenario::{parse_duration_ns, parse_seed};
use scx_simulator::{
    compare_checkpoints, compute_so_hash, discover_schedulers, drain_determinism_checkpoints,
    drain_preemption_records, enable_determinism_mode, enable_preemption_collection, load_rtapp,
    scheduler_so_base, scheduler_so_path, DynamicScheduler, ExitKind, NativeConcurrentConfig,
    Phase, PmuEvent, PreemptMode, PreemptionTrace, PreemptiveConfig, RepeatMode, Scenario,
    SimFormat, Simulator, TaskBehavior, TraceMetadata, TraceStats, SIM_LOCK,
};

mod real_run;
mod sched_config;

/// Environment variable set after ASLR is disabled to prevent infinite re-exec.
const ASLR_DISABLED_ENV: &str = "SCX_SIM_ASLR_DISABLED";

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

/// Selects the on-disk format for `--perfetto`.
///
/// `Json` keeps the legacy Chrome Trace Event Format that earlier
/// scxsim builds always wrote; `Perfetto` writes the newer
/// wprof-compatible Perfetto protobuf with `TrackEvent` slices and
/// `debug_annotations`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum TraceFormat {
    /// Chrome Trace Event JSON (default; backward-compatible, loadable
    /// in <https://ui.perfetto.dev>).
    #[default]
    Json,
    /// wprof-compatible Perfetto protobuf (loadable by scxtop's
    /// `load_perfetto_trace`; suitable for side-by-side comparison
    /// with wprof traces).
    Perfetto,
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

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a simulation from an rt-app workload.
    Run(RunArgs),
    /// Run workload in a virtme-ng VM with a real scheduler.
    VmRun(VmRunArgs),
    /// Replay a recorded preemption trace.
    Replay(ReplayArgs),
    /// Print address-space layout for ASLR verification.
    ///
    /// Loads the scheduler, prints .so base, heap, and stack addresses,
    /// then exits. Used by the ASLR stability test.
    #[command(hide = true)]
    PrintAddresses(PrintAddressesArgs),
}

/// Arguments for the `vm-run` subcommand.
#[derive(Parser)]
struct VmRunArgs {
    /// Path to an rt-app JSON workload file.
    workload: PathBuf,

    /// Scheduler name.
    #[arg(short, long, default_value = "simple")]
    scheduler: String,

    /// Number of workload CPUs to use in the VM.
    ///
    /// Tracing modes add one extra VM CPU for the tracer.
    #[arg(short, long, default_value_t = 4, value_parser = clap::value_parser!(u32).range(1..))]
    cpus: u32,

    /// Record a Perfetto trace using wprof during VM execution.
    ///
    /// When enabled, an extra CPU is added to the VM and isolated using
    /// isolcpus for running the wprof tracer. The trace file is written
    /// to the current working directory.
    #[arg(long, conflicts_with = "bpf_trace")]
    wprof: bool,

    /// Trace scheduler ops callbacks and kfunc calls using bpftrace.
    ///
    /// When enabled, an extra CPU is added to the VM and isolated for
    /// running bpftrace with trace_scx_ops.bt. This traces sched_class entry
    /// points, scx_bpf_* kfunc calls with return values, and
    /// sched_switch/sched_wakeup lifecycle events.
    ///
    /// The trace is written to bpf_trace.log in the current working directory.
    /// This is an alternative to --wprof for comparing simulator vs real runs.
    #[arg(long, conflicts_with = "wprof")]
    bpf_trace: bool,

    /// Raw shell arguments appended to the scheduler command.
    ///
    /// Use `--scheduler-args=--enable-cpu-bw` when the first scheduler
    /// argument starts with `-`.
    #[arg(long, value_name = "ARGS", allow_hyphen_values = true)]
    scheduler_args: Option<String>,

    /// Executable hook run inside the VM after the scheduler starts and before
    /// rt-app starts.
    ///
    /// The hook sees SCXSIM_* environment variables plus SCXSIM_SCHED_PID.
    #[arg(long, value_name = "PATH")]
    pre_hook: Option<PathBuf>,

    /// Executable hook run inside the VM after rt-app exits and before the
    /// scheduler/tracer are stopped.
    ///
    /// The hook sees SCXSIM_* environment variables plus SCXSIM_SCHED_PID.
    #[arg(long, value_name = "PATH")]
    post_hook: Option<PathBuf>,
}

/// Arguments for the `run` subcommand.
#[derive(Parser)]
struct RunArgs {
    /// Path to an rt-app JSON workload file.
    workload: Option<PathBuf>,

    /// Scheduler name.
    #[arg(short, long, default_value = "simple")]
    scheduler: String,

    /// Number of simulated CPUs (minimum 1).
    #[arg(short, long, default_value_t = 4, value_parser = clap::value_parser!(u32).range(1..))]
    cpus: u32,

    /// SMT threads per core (minimum 1).
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..))]
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
    ///
    /// Aliases: `--duration` (preferred for new invocations).
    #[arg(long, alias = "duration", value_name = "DURATION")]
    end_time: Option<String>,

    /// Warmup period in milliseconds.
    ///
    /// When set, trace statistics (summary, TraceStats) exclude events
    /// that occurred before this simulated time. The simulation still runs
    /// from time 0, but metrics only reflect post-warmup behavior.
    #[arg(long, value_name = "MS")]
    warmup_ms: Option<u64>,

    /// Write Perfetto trace to file. The default format is Chrome
    /// Trace Event JSON (loadable in <https://ui.perfetto.dev>);
    /// pass `--trace-format perfetto` to write a wprof-compatible
    /// Perfetto protobuf instead (loadable by scxtop's
    /// `load_perfetto_trace` and side-by-side with wprof traces).
    #[arg(long, value_name = "PATH")]
    perfetto: Option<PathBuf>,

    /// Output format for `--perfetto`.
    ///
    /// `json` (default) keeps backward-compatible Chrome JSON output.
    /// `perfetto` emits a wprof-compatible Perfetto protobuf
    /// (TrackEvent slices/instants with `debug_annotations` matching
    /// wprof's vocabulary; see
    /// `experiments/wprof_trace_baseline_20260513/REPORT.md` §3 for
    /// the wprof event schema).
    #[arg(long, value_name = "FMT", default_value = "json")]
    trace_format: TraceFormat,

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
    /// units: "2s", "500ms", etc. Use "0" or "off" to disable. Default: 30s.
    ///
    /// Aliases: `--watchdog` (preferred for new invocations).
    #[arg(long, alias = "watchdog", value_name = "DURATION")]
    watchdog_timeout: Option<String>,

    /// Path to a TOML scheduler-config file.
    ///
    /// The config file declares per-symbol BPF-global values to write through
    /// to the loaded scheduler `.so` after load. Sub-tables segregate symbols
    /// by primitive type (`bool_globals`, `u8_globals`, `u32_globals`,
    /// `u64_globals`) because the FFI layer cannot introspect symbol types
    /// from ELF/DWARF. See `tests/fixtures/h6/bug1_canonical.toml` for a
    /// minimal example (`enable_cpu_bw = true`).
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Enable concurrent callback interleaving at kfunc yield points.
    ///
    /// Runs dispatch callbacks for multiple idle CPUs on separate OS
    /// threads with PRNG-driven token passing, enabling deterministic
    /// exploration of different interleavings.
    #[arg(long)]
    interleave: bool,

    /// Pull cgroup_bw BPF timer events into cgroup_bw yield sites.
    ///
    /// Deterministic per seed. This models timer-vs-dispatch race windows
    /// that the serial event loop cannot otherwise expose.
    #[arg(long)]
    stochastic_timer_interleave: bool,

    /// Fire-ahead window for --stochastic-timer-interleave.
    ///
    /// Pending non-slot-0 BPF timers at or before current CPU time + this
    /// window are eligible to run at cgroup_bw yield sites.
    #[arg(
        long,
        default_value = "20ms",
        requires = "stochastic_timer_interleave",
        value_name = "DURATION"
    )]
    stochastic_timer_interleave_window: String,

    /// Approximate stochastic timer-interleave rate: one eligible timer per N sites.
    #[arg(long, default_value_t = 4, requires = "stochastic_timer_interleave")]
    stochastic_timer_interleave_one_in: u32,

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
    /// --preemptive mode. Default: 100. With PMU skid (~30-100 branches),
    /// actual preemption fires at ~130-200 branches after the last kfunc.
    ///
    /// WARNING: Values below 200 can cause livelock with complex schedulers
    /// (e.g. LAVD with structop_rbc up to 2026). Use --timeslice-min 300+ for LAVD.
    #[arg(long, default_value_t = 300, requires = "preemptive")]
    timeslice_min: u64,

    /// Maximum preemptive timeslice in retired conditional branches.
    ///
    /// Controls the upper bound of the random timeslice range used by
    /// --preemptive mode. Default: 500. With PMU skid (~30-100 branches),
    /// actual preemption fires at ~130-600 branches after the last kfunc.
    ///
    /// Upper bound of the PRNG-generated timeslice range.
    #[arg(long, default_value_t = 1500, requires = "preemptive")]
    timeslice_max: u64,

    /// Which PMU event to break on for preemptive interleaving.
    ///
    /// rbc: Retired conditional branches (default, lower frequency).
    /// insn: Instructions retired (higher frequency — use larger timeslice).
    #[arg(long, value_enum, default_value_t = BreakOn::Rbc, requires = "preemptive")]
    break_on: BreakOn,

    /// Preemption mechanism for mid-C-code preemption.
    ///
    /// pmu: Hardware PMU timer (default; signal delivery has skid but counter values are exact).
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

    /// Override the scheduler `.so` path used by `--scheduler`.
    ///
    /// When set, `load_scheduler` ignores the compile-time `SCHEDULER_SO_DIR`
    /// and `--scheduler` name lookup, and dlopens the file at this exact
    /// path instead. The basename must match the `libscx_<name>.so` pattern
    /// because downstream code (e.g. `scheduler_prefix_from_path`,
    /// `derive_e9rip_path`) parses the prefix from the filename.
    ///
    /// Use case: per-revision binary cache (e.g. `experiments/bin_cache/<sha>/
    /// libscx_lavd.so`) for matrix testing where each slot must load a
    /// .so built from a distinct scx submodule SHA. Without this flag,
    /// matrix runners that swap the scx submodule SHA between cargo builds
    /// get a stale .so because cargo's `rerun-if-changed` does not include
    /// `scheds/rust/scx_lavd/src/bpf` (see
    /// `experiments/bug1_scx_version_matrix_20260512/README.md` v2 notes).
    #[arg(long, value_name = "PATH")]
    scheduler_file: Option<PathBuf>,
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

fn main() {
    // Disable ASLR before anything else so that the scheduler .so is loaded
    // at a stable base address. This must happen before CLI parsing because
    // it may re-exec the process.
    ensure_aslr_disabled();

    let cli = Cli::parse();
    init_tracing();

    let result: Result<(), RunError> = match cli.command {
        Command::Run(args) => run(&args),
        Command::VmRun(args) => vm_run(&args).map_err(RunError::from),
        Command::Replay(args) => replay_simulation(&args).map_err(RunError::from),
        Command::PrintAddresses(args) => print_addresses(&args).map_err(RunError::from),
    };

    // Exit code scheme — used by automation (the canonical Bug-1 reproducer
    // test asserts exit 42, etc.). Stable across releases.
    //
    //  * 0  — Normal completion
    //  * 1  — Generic error (CLI parse, file IO, etc.)
    //  * 42 — ExitKind::ErrorStall (watchdog fired on a runnable task)
    //  * 43 — ExitKind::ErrorBpf (scheduler called scx_bpf_error())
    //  * 44 — ExitKind::ErrorDispatchLoopExhausted
    //  * 45 — ExitKind::ErrorCgroupExhausted
    //
    // For each non-Normal ExitKind we additionally print a stable
    // single-line stderr marker `scxsim: ExitKind::<Variant> ...` so
    // grep-based test assertions stay robust to surrounding noise.
    match result {
        Ok(()) => {}
        Err(RunError::Generic(msg)) => {
            eprintln!("error: {msg}");
            std::process::exit(1);
        }
        Err(RunError::Sim(kind)) => {
            print_exit_marker(&kind);
            std::process::exit(exit_code_for(&kind));
        }
    }
}

/// Top-level run errors. Either a generic CLI/IO failure (exit 1) or a
/// structured `ExitKind` from the simulator (mapped to 42-45 by exit code).
enum RunError {
    Generic(String),
    Sim(ExitKind),
}

impl From<String> for RunError {
    fn from(s: String) -> Self {
        RunError::Generic(s)
    }
}

impl From<&str> for RunError {
    fn from(s: &str) -> Self {
        RunError::Generic(s.to_string())
    }
}

/// Map a simulator `ExitKind` to its stable process exit code.
fn exit_code_for(kind: &ExitKind) -> i32 {
    match kind {
        ExitKind::Normal => 0,
        ExitKind::ErrorStall { .. } => 42,
        ExitKind::ErrorBpf(_) => 43,
        ExitKind::ErrorDispatchLoopExhausted { .. } => 44,
        ExitKind::ErrorCgroupExhausted { .. } => 45,
    }
}

/// Print the stable single-line stderr marker for a given exit kind.
fn print_exit_marker(kind: &ExitKind) {
    match kind {
        ExitKind::Normal => {}
        ExitKind::ErrorStall {
            pid,
            runnable_for_ns,
        } => {
            eprintln!(
                "scxsim: ExitKind::ErrorStall pid={} runnable_for_ns={}",
                pid.0, runnable_for_ns
            );
        }
        ExitKind::ErrorBpf(msg) => {
            eprintln!("scxsim: ExitKind::ErrorBpf {msg}");
        }
        ExitKind::ErrorDispatchLoopExhausted { cpu } => {
            eprintln!("scxsim: ExitKind::ErrorDispatchLoopExhausted cpu={}", cpu.0);
        }
        ExitKind::ErrorCgroupExhausted {
            cgroup_name,
            active_count,
            max_cgroups,
        } => {
            eprintln!(
                "scxsim: ExitKind::ErrorCgroupExhausted cgroup_name={cgroup_name:?} \
                 active_count={active_count} max_cgroups={max_cgroups}"
            );
        }
    }
}

fn vm_run(args: &VmRunArgs) -> Result<(), String> {
    // Determine trace mode
    let trace_mode = if args.wprof {
        real_run::TraceMode::Wprof
    } else if args.bpf_trace {
        real_run::TraceMode::BpfTrace
    } else {
        real_run::TraceMode::None
    };

    real_run::run_vm(
        &args.workload,
        &args.scheduler,
        args.cpus,
        trace_mode,
        real_run::VmRunConfig {
            scheduler_args: args.scheduler_args.clone(),
            pre_hook: args.pre_hook.clone(),
            post_hook: args.post_hook.clone(),
        },
    )
}

fn run(args: &RunArgs) -> Result<(), RunError> {
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
    if args.stochastic_timer_interleave {
        scenario.stochastic_timer_interleave = true;
        scenario.stochastic_timer_interleave_window_ns =
            parse_duration_ns(&args.stochastic_timer_interleave_window)
                .map_err(|e| format!("--stochastic-timer-interleave-window: {e}"))?;
        scenario.stochastic_timer_interleave_one_in =
            args.stochastic_timer_interleave_one_in.max(1);
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
    if let Some(warmup_ms) = args.warmup_ms {
        scenario.warmup_ns = warmup_ms * 1_000_000;
    }
    if args.wait_debugger {
        scenario.wait_debugger = true;
    }

    // Handle --determinism-check mode
    if args.determinism_check {
        return run_determinism_check(args, scenario);
    }

    run_simulation(args, scenario)?;

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

fn run_determinism_check(args: &RunArgs, scenario: Scenario) -> Result<(), RunError> {
    let _lock = SIM_LOCK.lock().unwrap();
    let use_e9 = args.preemptive && args.preempt_mode == PreemptModeArg::E9patch;

    // Map the shared RBC state page BEFORE loading the _e9.so.
    if use_e9 {
        scx_simulator::preempt::mmap_shared_rbc();
    }

    // Run 1: collect checkpoints
    enable_determinism_mode();
    let sched1 = load_scheduler(
        &args.scheduler,
        args.cpus,
        use_e9,
        args.scheduler_file.as_deref(),
    )?;
    apply_config_if_present(args, &sched1)?;
    let trace1 = Simulator::new(sched1).run(scenario.clone());
    let checkpoints1 = drain_determinism_checkpoints();

    if trace1.has_error() {
        return Err(RunError::Sim(trace1.exit_kind().clone()));
    }

    // Run 2: collect checkpoints with same configuration
    enable_determinism_mode();
    let sched2 = load_scheduler(
        &args.scheduler,
        args.cpus,
        use_e9,
        args.scheduler_file.as_deref(),
    )?;
    apply_config_if_present(args, &sched2)?;
    let trace2 = Simulator::new(sched2).run(scenario);
    let checkpoints2 = drain_determinism_checkpoints();

    if trace2.has_error() {
        return Err(RunError::Sim(trace2.exit_kind().clone()));
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

fn run_simulation(args: &RunArgs, scenario: Scenario) -> Result<(), RunError> {
    let use_e9 = args.preemptive && args.preempt_mode == PreemptModeArg::E9patch;

    // Map the shared RBC state page BEFORE loading the _e9.so — the e9-
    // instrumented .so accesses this address during DT_INIT.
    if use_e9 {
        scx_simulator::preempt::mmap_shared_rbc();
    }

    let sched = load_scheduler(
        &args.scheduler,
        args.cpus,
        use_e9,
        args.scheduler_file.as_deref(),
    )?;
    apply_config_if_present(args, &sched)?;
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
        let (write_result, fmt_label) = match args.trace_format {
            TraceFormat::Json => (trace.write_perfetto_json(&mut file), "chrome-json"),
            TraceFormat::Perfetto => (trace.write_perfetto_pb(&mut file), "wprof-perfetto-pb"),
        };
        write_result.map_err(|e| format!("failed to write perfetto trace: {e}"))?;
        eprintln!("wrote perfetto trace ({fmt_label}) to {}", path.display());
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
        // Surface the typed ExitKind so the top-level main() can map it to
        // the stable per-variant exit code (42-45). The Debug-format string
        // path was the previous behavior; it always became `exit 1` plus a
        // wall-of-text "error: simulation error: …". The new path emits a
        // single stable stderr marker and the per-variant exit code.
        return Err(RunError::Sim(trace.exit_kind().clone()));
    }

    Ok(())
}

/// Print address-space layout for ASLR verification.
///
/// Loads the scheduler .so, allocates a heap object, and prints three
/// addresses (so_base, heap, stack) in a stable machine-readable format.
/// The caller (ASLR test) runs this twice and compares the output.
fn print_addresses(args: &PrintAddressesArgs) -> Result<(), String> {
    let _sched = load_scheduler(&args.scheduler, 4, false, None)?;
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

/// If `--config <PATH>` is set, load it and write each declared global
/// through to the loaded scheduler `.so`. No-op otherwise.
///
/// Called immediately after `load_scheduler` so that BPF-global writes happen
/// before the scheduler's `ops.init` runs (i.e. before the simulator
/// constructs `Simulator::new(sched)`, which holds the SIM_LOCK and triggers
/// `ops.init` on first event).
fn apply_config_if_present(args: &RunArgs, sched: &DynamicScheduler) -> Result<(), String> {
    let Some(path) = args.config.as_ref() else {
        return Ok(());
    };
    let cfg = sched_config::load_config(path)?;
    sched_config::apply_to_scheduler(&cfg, sched)?;
    Ok(())
}

fn load_scheduler(
    name: &str,
    nr_cpus: u32,
    e9patch: bool,
    override_path: Option<&Path>,
) -> Result<DynamicScheduler, String> {
    if let Some(path) = override_path {
        if !path.exists() {
            return Err(format!(
                "--scheduler-file path does not exist: {}",
                path.display()
            ));
        }
        let basename = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| format!("--scheduler-file has no filename: {}", path.display()))?;
        let expected_prefix = format!("libscx_{name}");
        if !basename.starts_with(&expected_prefix) || !basename.ends_with(".so") {
            return Err(format!(
                "--scheduler-file basename must match libscx_{name}*.so (got {basename:?}); \
                 the filename prefix is parsed by downstream code (scheduler_prefix_from_path, \
                 derive_e9rip_path). Rename the file or pass a different --scheduler/-s."
            ));
        }
        let so_str = path
            .to_str()
            .ok_or_else(|| format!("--scheduler-file path is not valid UTF-8: {path:?}"))?;
        return Ok(DynamicScheduler::load(so_str, name, nr_cpus));
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vm_run_accepts_real_run_flags() {
        let cli = Cli::try_parse_from([
            "scxsim",
            "vm-run",
            "--scheduler",
            "lavd",
            "--cpus",
            "2",
            "--bpf-trace",
            "workloads/two_runners.json",
        ])
        .unwrap();

        match cli.command {
            Command::VmRun(args) => {
                assert_eq!(args.scheduler, "lavd");
                assert_eq!(args.cpus, 2);
                assert!(args.bpf_trace);
                assert!(!args.wprof);
                assert_eq!(args.workload, PathBuf::from("workloads/two_runners.json"));
            }
            _ => panic!("expected vm-run subcommand"),
        }
    }

    #[test]
    fn run_rejects_vm_only_flags() {
        assert!(
            Cli::try_parse_from(["scxsim", "run", "--wprof", "workloads/two_runners.json",])
                .is_err()
        );
        assert!(Cli::try_parse_from([
            "scxsim",
            "run",
            "--bpf-trace",
            "workloads/two_runners.json",
        ])
        .is_err());
        assert!(Cli::try_parse_from([
            "scxsim",
            "run",
            "--real-run",
            "vm",
            "workloads/two_runners.json",
        ])
        .is_err());
    }

    #[test]
    fn vm_run_rejects_simulation_only_flags() {
        assert!(Cli::try_parse_from([
            "scxsim",
            "vm-run",
            "--smt",
            "2",
            "workloads/two_runners.json",
        ])
        .is_err());
        assert!(Cli::try_parse_from([
            "scxsim",
            "vm-run",
            "--preemptive",
            "workloads/two_runners.json",
        ])
        .is_err());
    }

    #[test]
    fn vm_run_accepts_scheduler_args_and_hooks() {
        let cli = Cli::try_parse_from([
            "scxsim",
            "vm-run",
            "--scheduler",
            "lavd",
            "--scheduler-args=--enable-cpu-bw --foo=bar",
            "--pre-hook",
            "/tmp/pre.sh",
            "--post-hook",
            "/tmp/post.sh",
            "workloads/two_runners.json",
        ])
        .unwrap();

        match cli.command {
            Command::VmRun(args) => {
                assert_eq!(args.scheduler, "lavd");
                assert_eq!(
                    args.scheduler_args.as_deref(),
                    Some("--enable-cpu-bw --foo=bar")
                );
                assert_eq!(args.pre_hook, Some(PathBuf::from("/tmp/pre.sh")));
                assert_eq!(args.post_hook, Some(PathBuf::from("/tmp/post.sh")));
            }
            _ => panic!("expected vm-run subcommand"),
        }
    }
}
