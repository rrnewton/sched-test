//! VM-based real run infrastructure for scxsim.
//!
//! This module launches a virtme-ng VM to run the same rt-app workload
//! with a real sched_ext scheduler, enabling comparison between simulated
//! and real behavior.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Resolve the rt-app binary path. `SCXSIM_RTAPP_BIN` overrides it; otherwise
/// it defaults to `$HOME/bin/rt-app` (the documented build location) so the
/// source carries no hardcoded username. Used for the host-side existence check
/// and the command run inside the VM (virtme-ng preserves `$HOME`).
fn rtapp_bin() -> String {
    std::env::var("SCXSIM_RTAPP_BIN").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{home}/bin/rt-app")
    })
}

/// Path to the workspace directory inside the VM (mounted from host CWD).
const VM_WORKSPACE: &str = "/usr/workspace";

/// Name of the trace file written by wprof.
const WPROF_TRACE_FILENAME: &str = "trace.proto";

/// Name of the trace file written by bpftrace.
const BPF_TRACE_FILENAME: &str = "bpf_trace.log";

/// Absolute path to the bpftrace script (resolved at compile time).
const BPFTRACE_SCRIPT: &str = env!("BPFTRACE_SCRIPT");

/// Which tracing to enable during a VM run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceMode {
    /// No tracing.
    None,
    /// Perfetto trace via wprof.
    Wprof,
    /// Ops + kfunc trace via bpftrace (trace_scx_ops.bt).
    BpfTrace,
}

/// Extra orchestration options for `scxsim vm-run`.
#[derive(Debug, Clone, Default)]
pub struct VmRunConfig {
    /// Raw shell arguments appended to the scheduler command.
    pub scheduler_args: Option<String>,
    /// Hook run after the scheduler starts and before rt-app starts.
    pub pre_hook: Option<PathBuf>,
    /// Hook run after rt-app exits and before cleanup.
    pub post_hook: Option<PathBuf>,
}

/// Run the workload in a virtme-ng VM with the specified scheduler.
///
/// This function:
/// 1. Copies the workload file into the VM
/// 2. Launches vng with the specified number of CPUs
/// 3. Loads the scheduler and runs rt-app
/// 4. Captures and displays the output
///
/// When `trace_mode` is `Wprof`:
/// - Adds one extra CPU for tracing
/// - Isolates that CPU using `isolcpus=N` kernel parameter
/// - Runs wprof pinned to the isolated CPU
/// - Writes Perfetto trace to the current working directory
///
/// When `trace_mode` is `BpfTrace`:
/// - Adds one extra CPU for tracing
/// - Isolates that CPU using `isolcpus=N` kernel parameter
/// - Runs bpftrace with trace_scx_ops.bt pinned to the isolated CPU
/// - Writes ops/kfunc trace to bpf_trace.log in the current working directory
pub fn run_vm(
    workload_path: &Path,
    scheduler: &str,
    nr_cpus: u32,
    trace_mode: TraceMode,
    config: VmRunConfig,
) -> Result<(), String> {
    // Validate prerequisites
    validate_prerequisites(scheduler, trace_mode, &config)?;

    // The workload file path will be accessible inside the VM because vng
    // shares the host filesystem. Just use the absolute path.
    let workload_abs = workload_path
        .canonicalize()
        .map_err(|e| format!("failed to canonicalize workload path: {e}"))?;

    let tracing = trace_mode != TraceMode::None;

    // When tracing is enabled, add an extra CPU for the tracer
    let vm_cpus = if tracing { nr_cpus + 1 } else { nr_cpus };
    let isolated_cpu = nr_cpus; // The last CPU (0-indexed)

    eprintln!("=== Real VM Run ===");
    eprintln!("  scheduler:  scx_{scheduler}");
    eprintln!("  workload:   {}", workload_abs.display());
    eprintln!("  cpus:       {nr_cpus}");
    if let Some(args) = normalized_scheduler_args(&config.scheduler_args) {
        eprintln!("  sched args: {args}");
    }
    let pre_hook = canonicalize_hook(config.pre_hook.as_deref())?;
    let post_hook = canonicalize_hook(config.post_hook.as_deref())?;

    if let Some(path) = &pre_hook {
        eprintln!("  pre-hook:   {}", path.display());
    }
    if let Some(path) = &post_hook {
        eprintln!("  post-hook:  {}", path.display());
    }
    match trace_mode {
        TraceMode::None => {}
        TraceMode::Wprof => {
            eprintln!("  trace:      wprof (isolated CPU {isolated_cpu})");
        }
        TraceMode::BpfTrace => {
            eprintln!("  trace:      bpftrace (isolated CPU {isolated_cpu})");
        }
    }
    eprintln!();

    // Build the command to run inside the VM.
    let sched_bin = find_scheduler_binary(scheduler)?;
    let inner_cmd = build_inner_cmd(
        scheduler,
        &sched_bin,
        &workload_abs,
        nr_cpus,
        trace_mode,
        normalized_scheduler_args(&config.scheduler_args),
        pre_hook.as_deref(),
        post_hook.as_deref(),
    );
    // Launch vng with the host kernel (-r).
    // Use root when tracing is enabled (bpftrace/wprof need CAP_SYS_ADMIN).
    let user = if tracing {
        "root".to_string()
    } else {
        std::env::var("USER").unwrap_or_else(|_| "root".into())
    };

    // Get current working directory for rwdir mount
    let cwd =
        std::env::current_dir().map_err(|e| format!("failed to get current directory: {e}"))?;

    // Build vng command with optional tracing-related flags
    let mut vng_args = format!("vng -r --user {user} --cpus {vm_cpus} --memory 4G");

    if tracing {
        // Add rwdir mount for trace output
        vng_args.push_str(&format!(" --rwdir {VM_WORKSPACE}={}", cwd.display()));
        // Note: we previously used --append isolcpus={isolated_cpu} here to prevent
        // scheduler noise from the tracing CPU. However, isolcpus causes sched_ext
        // struct_ops attachment to fail with EINVAL. Use taskset pinning only.
    }

    vng_args.push_str(&format!(" --exec {}", shell_escape(&inner_cmd)));

    // vng requires /proc/self/fd/{0,1,2} to be re-openable, which fails when
    // stdio are sockets or pipes. Wrap with `script` to allocate a real PTY.
    let mut cmd = Command::new("script");
    cmd.arg("-q") // no header/footer
        .arg("-e") // propagate exit code
        .arg("-c")
        .arg(&vng_args)
        .arg("/dev/null"); // discard typescript file

    eprintln!("Launching VM...");
    eprintln!(
        "  vng -r --user {user} --cpus {vm_cpus} --memory 4G{}",
        if tracing {
            format!(" --rwdir {VM_WORKSPACE}={}", cwd.display())
        } else {
            String::new()
        },
    );
    eprintln!();

    let status = cmd
        .status()
        .map_err(|e| format!("failed to launch vng: {e}"))?;

    if !status.success() {
        return Err(format!("vng exited with status: {status}"));
    }

    eprintln!();
    eprintln!("=== VM run completed ===");

    // Report trace file location
    match trace_mode {
        TraceMode::Wprof => {
            let trace_path = cwd.join(WPROF_TRACE_FILENAME);
            eprintln!();
            eprintln!("Perfetto trace written to: {}", trace_path.display());
        }
        TraceMode::BpfTrace => {
            let trace_path = cwd.join(BPF_TRACE_FILENAME);
            eprintln!();
            eprintln!("bpftrace log written to: {}", trace_path.display());
        }
        TraceMode::None => {}
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)] // wide vm-run subprocess builder; struct refactor would scatter knobs across two files
fn build_inner_cmd(
    scheduler: &str,
    sched_bin: &Path,
    workload_abs: &Path,
    nr_cpus: u32,
    trace_mode: TraceMode,
    scheduler_args: Option<&str>,
    pre_hook_path: Option<&Path>,
    post_hook_path: Option<&Path>,
) -> String {
    let tracing = trace_mode != TraceMode::None;
    let isolated_cpu = nr_cpus;
    let workload_cpus = if tracing {
        format!("0-{}", nr_cpus - 1)
    } else {
        String::new()
    };
    let scheduler_cmd = scheduler_command(sched_bin, scheduler_args);
    let workload_arg = shell_escape(&workload_abs.to_string_lossy());
    let pre_hook = hook_command("pre-hook", pre_hook_path);
    let post_hook = hook_command("post-hook", post_hook_path);
    // Shell-escaped for embedding in the in-VM command line (like workload_arg);
    // the path is user-overridable via SCXSIM_RTAPP_BIN, so a space/metachar in
    // it must not word-split the taskset/exec line.
    let rtapp_bin = shell_escape(&rtapp_bin());
    let setup_env = setup_env(
        scheduler,
        sched_bin,
        workload_abs,
        nr_cpus,
        trace_mode,
        scheduler_args,
    );
    match trace_mode {
        TraceMode::Wprof => {
            // With wprof: pin tracer to isolated CPU, run workload on remaining CPUs.
            // Use -T for Perfetto trace output, -d60000 for 60s max duration.
            // Send SIGINT to wprof for clean shutdown and trace flush.
            // Note: We run as root in the VM when tracing is enabled, so no sudo needed.
            format!(
                "{setup_env}\
                 taskset -c {isolated_cpu} wprof -d60000 -T {VM_WORKSPACE}/{WPROF_TRACE_FILENAME} &\n\
                 TRACER_PID=$!\n\
                 export SCXSIM_TRACER_PID=$TRACER_PID\n\
                 sleep 0.5\n\
                 {scheduler_cmd} &\n\
                 SCHED_PID=$!\n\
                 export SCXSIM_SCHED_PID=$SCHED_PID\n\
                 sleep 1\n\
                 {pre_hook}\
                 echo '=== Running rt-app ==='\n\
                 taskset -c {workload_cpus} {rtapp_bin} {workload_arg}\n\
                 echo '=== rt-app completed ==='\n\
                 {post_hook}\
                 kill -INT $TRACER_PID 2>/dev/null || true\n\
                kill $SCHED_PID 2>/dev/null || true\n\
                wait $TRACER_PID 2>/dev/null || true\n\
                wait $SCHED_PID 2>/dev/null || true",
            )
        }
        TraceMode::BpfTrace => {
            // With bpftrace: start scheduler first, then attach tracer.
            // Order matters: the scheduler must attach its struct_ops before
            // bpftrace attaches fexit probes to scx_bpf_* kfuncs, otherwise
            // EINVAL occurs on struct_ops attachment.
            // The script accepts nr_cpus as $1 to filter probes to CPUs 0..nr_cpus-1.
            // Output goes to a file in the mounted workspace directory.
            format!(
                "{setup_env}\
                 {scheduler_cmd} &\n\
                 SCHED_PID=$!\n\
                 export SCXSIM_SCHED_PID=$SCHED_PID\n\
                 sleep 1\n\
                 taskset -c {isolated_cpu} bpftrace {bpftrace_script} {nr_cpus} \
                 > {VM_WORKSPACE}/{BPF_TRACE_FILENAME} 2>&1 &\n\
                 TRACER_PID=$!\n\
                 export SCXSIM_TRACER_PID=$TRACER_PID\n\
                 sleep 1\n\
                 {pre_hook}\
                 echo '=== Running rt-app ==='\n\
                 taskset -c {workload_cpus} {rtapp_bin} {workload_arg}\n\
                 echo '=== rt-app completed ==='\n\
                 {post_hook}\
                 sleep 1\n\
                 kill $TRACER_PID 2>/dev/null || true\n\
                 kill $SCHED_PID 2>/dev/null || true\n\
                 wait $TRACER_PID 2>/dev/null || true\n\
                 wait $SCHED_PID 2>/dev/null || true",
                bpftrace_script = shell_escape(BPFTRACE_SCRIPT),
            )
        }
        TraceMode::None => {
            // No tracing: standard execution
            format!(
                "{setup_env}\
                 {scheduler_cmd} &\n\
                 SCHED_PID=$!\n\
                 export SCXSIM_SCHED_PID=$SCHED_PID\n\
                 sleep 1\n\
                 {pre_hook}\
                 echo '=== Running rt-app ==='\n\
                 {rtapp_bin} {workload_arg}\n\
                 echo '=== rt-app completed ==='\n\
                 {post_hook}\
                kill $SCHED_PID 2>/dev/null || true\n\
                wait $SCHED_PID 2>/dev/null || true",
            )
        }
    }
}

/// Validate that all prerequisites are available.
fn validate_prerequisites(
    scheduler: &str,
    trace_mode: TraceMode,
    config: &VmRunConfig,
) -> Result<(), String> {
    // Check vng
    if !command_exists("vng") {
        return Err("vng (virtme-ng) not found in PATH".into());
    }

    // Check rt-app
    let rtapp_bin = rtapp_bin();
    if !Path::new(&rtapp_bin).exists() {
        return Err(format!(
            "rt-app not found at {rtapp_bin} (override with SCXSIM_RTAPP_BIN)"
        ));
    }

    // Check scheduler binary
    find_scheduler_binary(scheduler)?;

    // Check hooks
    validate_hook("pre-hook", config.pre_hook.as_deref())?;
    validate_hook("post-hook", config.post_hook.as_deref())?;

    // Check bpftrace when --bpf-trace is requested
    if trace_mode == TraceMode::BpfTrace {
        if !command_exists("bpftrace") {
            return Err("bpftrace not found in PATH (required for --bpf-trace)".into());
        }
        if !Path::new(BPFTRACE_SCRIPT).exists() {
            return Err(format!("bpftrace script not found at {BPFTRACE_SCRIPT}"));
        }
    }

    Ok(())
}

fn validate_hook(label: &str, hook: Option<&Path>) -> Result<(), String> {
    let Some(path) = hook else {
        return Ok(());
    };
    if !path.exists() {
        return Err(format!("{label} not found: {}", path.display()));
    }
    if !path.is_file() {
        return Err(format!("{label} is not a file: {}", path.display()));
    }
    if !is_executable(path)? {
        return Err(format!("{label} is not executable: {}", path.display()));
    }
    Ok(())
}

#[cfg(unix)]
fn is_executable(path: &Path) -> Result<bool, String> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = path
        .metadata()
        .map_err(|e| format!("failed to inspect hook {}: {e}", path.display()))?;
    Ok(metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> Result<bool, String> {
    Ok(true)
}

fn canonicalize_hook(hook: Option<&Path>) -> Result<Option<PathBuf>, String> {
    hook.map(|path| {
        path.canonicalize()
            .map_err(|e| format!("failed to canonicalize hook {}: {e}", path.display()))
    })
    .transpose()
}

/// Find the scheduler binary.
///
/// Search order:
/// 1. `SCX_SCHED_BIN` environment variable (explicit override)
/// 2. Repo-relative paths from CWD (`../target/{release,debug}/scx_<name>`)
/// 3. CARGO_MANIFEST_DIR-relative paths (when run via `cargo run`)
/// 4. System PATH / well-known locations
fn find_scheduler_binary(scheduler: &str) -> Result<PathBuf, String> {
    let bin_name = format!("scx_{scheduler}");

    // 1. Explicit override via environment variable
    if let Ok(sched_bin) = std::env::var("SCX_SCHED_BIN") {
        let path = PathBuf::from(&sched_bin);
        if path.exists() {
            return Ok(path);
        }
        return Err(format!(
            "SCX_SCHED_BIN set to {sched_bin} but file does not exist"
        ));
    }

    // 2. Repo-relative from CWD (scxsim is typically run from scx-sim/)
    // The repo root is ../ relative to that, so target/ is at ../target/
    let cwd_candidates = ["../target/release", "../target/debug"];
    for dir in cwd_candidates {
        let path = PathBuf::from(dir).join(&bin_name);
        if path.exists() {
            return Ok(path.canonicalize().unwrap_or(path));
        }
    }

    // 3. CARGO_MANIFEST_DIR-relative (crates/scx_simulator -> scx-sim -> repo_root)
    if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
        let manifest_path = PathBuf::from(&manifest_dir);
        // Navigate: crates/scx_simulator -> crates -> scx-sim -> repo_root
        if let Some(repo_root) = manifest_path
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
        {
            for profile in ["release", "debug"] {
                let path = repo_root.join("target").join(profile).join(&bin_name);
                if path.exists() {
                    return Ok(path);
                }
            }
        }
    }

    // 4. System locations and PATH
    let system_candidates = [
        format!("/usr/bin/{bin_name}"),
        format!(
            "{}/target/release/{bin_name}",
            std::env::var("HOME").unwrap_or_default()
        ),
        format!(
            "{}/target/debug/{bin_name}",
            std::env::var("HOME").unwrap_or_default()
        ),
    ];

    for candidate in &system_candidates {
        let path = PathBuf::from(candidate);
        if path.exists() {
            return Ok(path);
        }
    }

    // Try PATH resolution
    if let Ok(output) = Command::new("which").arg(&bin_name).output() {
        if output.status.success() {
            let resolved = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !resolved.is_empty() {
                return Ok(PathBuf::from(resolved));
            }
        }
    }

    Err(format!(
        "scheduler binary {bin_name} not found. Build it with:\n\
         cd ../  # repo root\n\
         cargo build --bin {bin_name} --release\n\n\
         Or set SCX_SCHED_BIN environment variable to the path of the scheduler binary."
    ))
}

/// Single-quote a string for sh, escaping any embedded single quotes.
fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn normalized_scheduler_args(args: &Option<String>) -> Option<&str> {
    args.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

fn scheduler_command(sched_bin: &Path, scheduler_args: Option<&str>) -> String {
    let mut cmd = shell_escape(&sched_bin.to_string_lossy());
    if let Some(args) = scheduler_args {
        cmd.push(' ');
        cmd.push_str(args);
    }
    cmd
}

fn setup_env(
    scheduler: &str,
    sched_bin: &Path,
    workload: &Path,
    nr_cpus: u32,
    trace_mode: TraceMode,
    scheduler_args: Option<&str>,
) -> String {
    let trace_mode = match trace_mode {
        TraceMode::None => "none",
        TraceMode::Wprof => "wprof",
        TraceMode::BpfTrace => "bpftrace",
    };
    let scheduler_args = scheduler_args.unwrap_or("");
    let rtapp_bin = rtapp_bin();
    format!(
        "export SCXSIM_SCHEDULER={scheduler}\n\
         export SCXSIM_SCHED_BIN={sched_bin}\n\
         export SCXSIM_SCHEDULER_ARGS={scheduler_args}\n\
         export SCXSIM_WORKLOAD={workload}\n\
         export SCXSIM_RTAPP_BIN={rtapp_bin}\n\
         export SCXSIM_CPUS={nr_cpus}\n\
         export SCXSIM_TRACE_MODE={trace_mode}\n",
        scheduler = shell_escape(scheduler),
        sched_bin = shell_escape(&sched_bin.to_string_lossy()),
        scheduler_args = shell_escape(scheduler_args),
        workload = shell_escape(&workload.to_string_lossy()),
        rtapp_bin = shell_escape(&rtapp_bin),
        trace_mode = shell_escape(trace_mode),
    )
}

fn hook_command(label: &str, hook: Option<&Path>) -> String {
    hook.map(|path| {
        format!(
            "echo '=== Running {label} ==='\n\
             {hook}\n\
             echo '=== {label} completed ==='\n",
            label = label,
            hook = shell_escape(&path.to_string_lossy()),
        )
    })
    .unwrap_or_default()
}

/// Check if a command exists in PATH.
fn command_exists(cmd: &str) -> bool {
    Command::new("which")
        .arg(cmd)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Reject a task carrying state stock rt-app cannot reproduce.
///
/// Every one of these is something scxsim can drive a layer match from, so a
/// silent drop turns "this config classified my workload" into a claim about a
/// run that never happened.
fn refuse_unrepresentable_attributes(task: &scx_simulator::TaskDef) -> Result<(), String> {
    let refuse = |what: &str, why: &str| -> Result<(), String> {
        Err(format!(
            "task {:?} carries {what}, which stock rt-app has no syntax for: {why}. \
             Refusing to emit a spec that would run as a different workload; see the \
             rt-app++ section of scx_simulator::rtapp.",
            task.name
        ))
    };

    if task.thread_group_leader.is_some_and(|l| l != task.pid) {
        return refuse(
            "a thread-group membership (thread_of)",
            "rt-app runs every task as a pthread of one process whose comm is `rt-app`, so \
             the exported spec would give every task the same pcomm rather than this one",
        );
    }
    if task.uid.0 != 0 || task.gid.0 != 0 {
        return refuse(
            "a non-root uid/gid",
            "rt-app runs every thread under the invoking user's credentials",
        );
    }
    if task.task_flags != 0 {
        return refuse(
            "task_struct flags (e.g. PF_KTHREAD)",
            "rt-app creates ordinary user threads and cannot set PF_* flags",
        );
    }
    if task.parent_pid.is_some_and(|parent| parent != task.pid) {
        return refuse(
            "an explicit parent",
            "every rt-app thread has the same real_parent, the shell that launched rt-app",
        );
    }
    if task.migration_disabled > 0 {
        return refuse(
            "a migration_disabled count",
            "only the kernel raises this; no rt-app spec can",
        );
    }
    Ok(())
}

/// Generate an rt-app JSON workload file from a scenario.
///
/// This is the reverse of `load_rtapp`: given a parsed Scenario, generate
/// a JSON file that rt-app can execute to produce the same workload.
///
/// Note: This is a simplified generator that may not preserve all rt-app
/// features, but it handles the common run/sleep/wake patterns used in
/// simulation testing.
///
/// It **refuses** rather than emitting a spec that means something different
/// on real hardware. `Phase::SystemCpu` was the first such refusal; the
/// rt-app++ identity attributes are the rest, because stock rt-app has no
/// syntax for any of them (see [`RTAPP_PP_KEYS`]) and a silently-dropped
/// attribute is a rule that matched in simulation and misses for real.
///
/// [`RTAPP_PP_KEYS`]: scx_simulator::RTAPP_PP_KEYS
#[allow(dead_code)]
pub fn scenario_to_rtapp_json(scenario: &scx_simulator::Scenario) -> Result<String, String> {
    use scx_simulator::Phase;
    use serde_json::{json, Map, Value};

    let duration_secs = (scenario.duration_ns / 1_000_000_000) as i64;

    let mut tasks = Map::new();

    // Build a map of PID -> task name for wake references
    let mut pid_to_name: std::collections::HashMap<i32, &str> = std::collections::HashMap::new();
    for task in &scenario.tasks {
        pid_to_name.insert(task.pid.0, &task.name);
    }

    for task in &scenario.tasks {
        refuse_unrepresentable_attributes(task)?;

        let mut task_obj = Map::new();

        // Set priority (nice value)
        task_obj.insert("priority".into(), json!(task.nice as i64));

        // Cgroup membership. rt-app has a `taskgroup` key and reproduces this
        // for real; emitting nothing here (as this did until the rt-app++
        // work) meant an exported spec ran with no cgroups at all, so every
        // cgroup match rule that fired in simulation missed on hardware.
        if let Some(ref cgroup) = task.cgroup_name {
            task_obj.insert("taskgroup".into(), json!(cgroup));
        }

        // Start delay. rt-app's `delay` is microseconds before the first loop.
        if task.start_time_ns > 0 {
            task_obj.insert("delay".into(), json!(task.start_time_ns / 1_000));
        }

        // Set loop count
        let loop_count: i64 = match task.behavior.repeat {
            scx_simulator::RepeatMode::Once => 1,
            scx_simulator::RepeatMode::Count(n) => n as i64,
            scx_simulator::RepeatMode::Forever => -1,
        };
        task_obj.insert("loop".into(), json!(loop_count));

        // Set CPU affinity if specified
        if let Some(ref cpus) = task.allowed_cpus {
            let cpu_list: Vec<u32> = cpus.iter().map(|c| c.0).collect();
            task_obj.insert("cpus".into(), json!(cpu_list));
        }

        // Convert phases to rt-app events
        // Use numbered suffixes for duplicate event types (run0, run1, etc.)
        let mut run_idx = 0;
        let mut sleep_idx = 0;
        let mut yield_idx = 0;

        for phase in &task.behavior.phases {
            match phase {
                Phase::Run(ns) => {
                    let usec = ns / 1_000;
                    let key = if run_idx == 0 {
                        "run".into()
                    } else {
                        format!("run{run_idx}")
                    };
                    task_obj.insert(key, json!(usec));
                    run_idx += 1;
                }
                Phase::SystemCpu(_) => {
                    return Err(format!(
                        "task {:?} contains calibrated SystemCpu, which rt-app can only replay as user CPU; refusing to relabel the estimand",
                        task.name
                    ));
                }
                Phase::Sleep(ns) => {
                    if *ns == u64::MAX {
                        // Suspend: self-suspend until woken
                        task_obj.insert("suspend".into(), json!(task.name.clone()));
                    } else {
                        let usec = ns / 1_000;
                        let key = if sleep_idx == 0 {
                            "sleep".into()
                        } else {
                            format!("sleep{sleep_idx}")
                        };
                        task_obj.insert(key, json!(usec));
                        sleep_idx += 1;
                    }
                }
                Phase::Park => {
                    task_obj.insert("suspend".into(), json!(task.name.clone()));
                }
                Phase::Wake(target_pid) => {
                    // Find the target task name
                    if let Some(target_name) = pid_to_name.get(&target_pid.0) {
                        task_obj.insert("resume".into(), json!(*target_name));
                    }
                }
                Phase::Yield => {
                    // rt-app's `yield` event calls sched_yield() once per loop
                    // iteration; its JSON value is parsed but ignored (verified
                    // by strace against ~/bin/rt-app: `"yield": 1` and
                    // `"yield": 7` both produce exactly one sched_yield per
                    // iteration, and omitting the key produces none).
                    let key = if yield_idx == 0 {
                        "yield".into()
                    } else {
                        format!("yield{yield_idx}")
                    };
                    task_obj.insert(key, json!(1));
                    yield_idx += 1;
                }
            }
        }

        tasks.insert(task.name.clone(), Value::Object(task_obj));
    }

    let root = json!({
        "global": {
            "duration": duration_secs,
            "default_policy": "SCHED_OTHER",
            "calibration": "CPU0"
        },
        "tasks": tasks
    });

    serde_json::to_string_pretty(&root).map_err(|e| format!("failed to serialize JSON: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use scx_simulator::*;

    #[test]
    fn test_scenario_to_rtapp_json_basic() {
        let scenario = Scenario::builder()
            .cpus(2)
            .add_task(
                "worker",
                0,
                TaskBehavior {
                    phases: vec![Phase::Run(5_000_000), Phase::Sleep(5_000_000)],
                    repeat: RepeatMode::Forever,
                },
            )
            .duration_ms(1000)
            .build();

        let json = scenario_to_rtapp_json(&scenario).unwrap();
        assert!(json.contains("\"worker\""));
        assert!(json.contains("\"run\""));
        assert!(json.contains("\"sleep\""));
        assert!(json.contains("\"loop\": -1"));
    }

    #[test]
    fn rtapp_refuses_to_relabel_calibrated_system_cpu_as_user_run() {
        let scenario = Scenario::builder()
            .add_task(
                "modelled-io",
                0,
                TaskBehavior {
                    phases: vec![Phase::SystemCpu(1_000_000), Phase::Park],
                    repeat: RepeatMode::Once,
                },
            )
            .duration_ms(10)
            .build();
        let err = scenario_to_rtapp_json(&scenario).expect_err("estimand must not be relabelled");
        assert!(err.contains("calibrated SystemCpu"));
        assert!(err.contains("refusing"));
    }

    /// Cgroup membership survives the round trip.
    ///
    /// It did not until the rt-app++ work: the exporter emitted
    /// priority/loop/cpus and the events, and nothing else. A scenario built
    /// from a `taskgroup` spec therefore exported to a spec with NO
    /// taskgroups, so every cgroup rule that fired in simulation missed on the
    /// real run — silently, because rt-app is perfectly happy to run tasks in
    /// the root cgroup. Cgroup path is the most-written match kind in the
    /// deployed corpus.
    #[test]
    fn export_preserves_cgroup_membership() {
        let scenario = Scenario::builder()
            .cpus(2)
            .cgroup("/prod/frontend", &[CpuId(0), CpuId(1)])
            .add_task_in_cgroup(
                "frontendd",
                0,
                TaskBehavior {
                    phases: vec![Phase::Run(1_000_000)],
                    repeat: RepeatMode::Once,
                },
                "/prod/frontend",
            )
            .duration_ms(10)
            .build();
        let json = scenario_to_rtapp_json(&scenario).expect("exports");
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed["tasks"]["frontendd"]["taskgroup"],
            serde_json::json!("/prod/frontend"),
            "exported spec must carry the taskgroup, or the real run has no cgroups: {json}"
        );
    }

    /// The exporter refuses every rt-app++ attribute rather than dropping it.
    ///
    /// Each of these drives a layer match in simulation and has no stock
    /// rt-app counterpart, so emitting a spec without it would hand someone a
    /// file that runs a measurably different workload under the same name.
    /// Same discipline as the `SystemCpu` refusal above.
    #[test]
    fn export_refuses_every_rtapp_pp_attribute() {
        let base = || TaskBehavior {
            phases: vec![Phase::Run(1_000_000)],
            repeat: RepeatMode::Once,
        };
        /// One case: the rt-app++ key, the mutation that sets it, and the
        /// phrase the refusal must contain.
        type Case = (&'static str, fn(&mut TaskDef), &'static str);
        let mutate: [Case; 5] = [
            (
                "thread_of",
                |t| t.thread_group_leader = Some(Pid(1)),
                "thread-group membership",
            ),
            ("uid", |t| t.uid = Uid(4711), "non-root uid/gid"),
            ("gid", |t| t.gid = Gid(1000), "non-root uid/gid"),
            (
                "kthread",
                |t| t.task_flags = 0x0020_0000,
                "task_struct flags",
            ),
            (
                "parent",
                |t| t.parent_pid = Some(Pid(1)),
                "an explicit parent",
            ),
        ];
        for (key, apply, needle) in mutate {
            let mut scenario = Scenario::builder()
                .cpus(2)
                .add_task("leader", 0, base())
                .add_task("worker", 0, base())
                .duration_ms(10)
                .build();
            let worker = scenario
                .tasks
                .iter_mut()
                .find(|t| t.name == "worker")
                .unwrap();
            apply(worker);

            let err = scenario_to_rtapp_json(&scenario).unwrap_err();
            assert!(
                err.contains(needle) && err.contains("worker"),
                "{key}: the refusal must name the attribute and the task; got: {err}"
            );
        }

        // And the control: the same scenario with none of them exports fine,
        // so the refusals above are not just "this builder never exports".
        let clean = Scenario::builder()
            .cpus(2)
            .add_task("leader", 0, base())
            .add_task("worker", 0, base())
            .duration_ms(10)
            .build();
        scenario_to_rtapp_json(&clean).expect("a plain scenario still exports");
    }

    #[test]
    fn test_scenario_to_rtapp_json_ping_pong() {
        let (ping_b, pong_b) = workloads::ping_pong(Pid(1), Pid(2), 500_000);
        let scenario = Scenario::builder()
            .cpus(2)
            .task(TaskDef {
                name: "ping".into(),
                pid: Pid(1),
                nice: 0,
                behavior: ping_b,
                start_time_ns: 0,
                mm_id: None,
                allowed_cpus: None,
                parent_pid: None,
                cgroup_name: None,
                task_flags: 0,
                migration_disabled: 0,
                thread_group_leader: None,
                uid: Uid(0),
                gid: Gid(0),
            })
            .task(TaskDef {
                name: "pong".into(),
                pid: Pid(2),
                nice: 0,
                behavior: pong_b,
                start_time_ns: 0,
                mm_id: None,
                allowed_cpus: None,
                parent_pid: None,
                cgroup_name: None,
                task_flags: 0,
                migration_disabled: 0,
                thread_group_leader: None,
                uid: Uid(0),
                gid: Gid(0),
            })
            .duration_ms(1000)
            .build();

        let json = scenario_to_rtapp_json(&scenario).unwrap();
        assert!(json.contains("\"ping\""));
        assert!(json.contains("\"pong\""));
        assert!(json.contains("\"resume\""));
        assert!(json.contains("\"suspend\""));
    }

    #[test]
    fn vm_inner_command_plumbs_scheduler_args_and_hooks() {
        // Pin the rt-app path so the assertion is deterministic and carries no
        // machine-specific path; also exercises the SCXSIM_RTAPP_BIN override.
        std::env::set_var("SCXSIM_RTAPP_BIN", "/tmp/test-rt-app");
        let cmd = build_inner_cmd(
            "lavd",
            Path::new("/tmp/scx_lavd"),
            Path::new("/tmp/r3_mimic.json"),
            4,
            TraceMode::None,
            Some("--enable-cpu-bw --verbose"),
            Some(Path::new("/tmp/pre hook.sh")),
            Some(Path::new("/tmp/post hook.sh")),
        );

        assert!(cmd.contains("export SCXSIM_SCHEDULER='lavd'"));
        assert!(cmd.contains("export SCXSIM_SCHED_BIN='/tmp/scx_lavd'"));
        assert!(cmd.contains("export SCXSIM_SCHEDULER_ARGS='--enable-cpu-bw --verbose'"));
        assert!(cmd.contains("export SCXSIM_WORKLOAD='/tmp/r3_mimic.json'"));
        assert!(cmd.contains("export SCXSIM_TRACE_MODE='none'"));
        assert!(cmd.contains("'/tmp/scx_lavd' --enable-cpu-bw --verbose &"));
        assert!(cmd.contains("echo '=== Running pre-hook ==='\n'/tmp/pre hook.sh'"));
        assert!(cmd.contains("echo '=== Running rt-app ==='"));
        assert!(cmd.contains("'/tmp/test-rt-app' '/tmp/r3_mimic.json'"));
        assert!(cmd.contains("echo '=== Running post-hook ==='\n'/tmp/post hook.sh'"));

        let pre_idx = cmd.find("=== Running pre-hook ===").unwrap();
        let workload_idx = cmd.find("=== Running rt-app ===").unwrap();
        let post_idx = cmd.find("=== Running post-hook ===").unwrap();
        assert!(pre_idx < workload_idx);
        assert!(workload_idx < post_idx);
    }
}
