//! `repm capture` — SSH trace collection from production hosts.
//!
//! This is the first step of the repromagic pipeline: getting production data.
//!
//! In SSH mode, repm connects to a remote host, runs configurable trace
//! commands (perfetto, /proc/interrupts sampling, LAVD stats), and SCPs
//! the results back to the local workspace.
//!
//! In local mode, traces are collected on the current machine.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use chrono::Utc;
use clap::Args;

use crate::workspace;

/// Capture scheduling trace (local or remote via SSH).
///
/// Connects to a target host, runs trace collection commands, and copies
/// results back to the local workspace under `traces/<timestamp>/`.
#[derive(Debug, Args)]
pub struct CaptureArgs {
    /// Remote host for SSH capture (e.g., root@prod-host).
    /// If omitted, uses [capture.default_host] from config, or captures locally.
    #[arg(long)]
    pub ssh: Option<String>,

    /// Capture duration in seconds.
    #[arg(long, default_value = "30")]
    pub duration: u32,

    /// Scheduler to use during capture (key from config).
    #[arg(long)]
    pub scheduler: Option<String>,

    /// Show commands that would be executed without running them.
    #[arg(long)]
    pub dry_run: bool,

    /// Experiment version tag (e.g., "v1", "baseline"). Used for output directory.
    #[arg(long, default_value = "default")]
    pub version: String,

    /// SSH connection timeout in seconds (overrides config).
    #[arg(long)]
    pub ssh_timeout: Option<u32>,

    /// SSH identity file (overrides config).
    #[arg(long, short = 'i')]
    pub identity: Option<String>,
}

pub fn execute(args: &CaptureArgs) -> Result<()> {
    // Try to load workspace config; if not in a workspace, use defaults
    let (ws_root, config) = match workspace::load_config_from_cwd() {
        Ok((root, cfg)) => (Some(root), Some(cfg)),
        Err(_) => {
            eprintln!("warning: not in a repromagic workspace, using defaults");
            (None, None)
        }
    };

    let capture_cfg = config.as_ref().map(|c| &c.capture);

    // Resolve SSH host: CLI > config > local mode
    let ssh_host = args
        .ssh
        .clone()
        .or_else(|| capture_cfg.and_then(|c| c.default_host.clone()));

    if let Some(ref host) = ssh_host {
        execute_ssh_capture(host, args, &ws_root, capture_cfg)
    } else {
        execute_local_capture(args, &ws_root, capture_cfg)
    }
}

// ---------------------------------------------------------------------------
// SSH capture
// ---------------------------------------------------------------------------

fn execute_ssh_capture(
    host: &str,
    args: &CaptureArgs,
    ws_root: &Option<PathBuf>,
    capture_cfg: Option<&crate::config::CaptureConfig>,
) -> Result<()> {
    let default_cfg = crate::config::CaptureConfig::default();
    let cfg = capture_cfg.unwrap_or(&default_cfg);

    let ssh_timeout = args.ssh_timeout.unwrap_or(cfg.ssh_timeout);
    let identity = args.identity.clone().or_else(|| cfg.ssh_identity.clone());
    let remote_workdir = &cfg.remote_workdir;

    let timestamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let capture_name = format!("capture_{}", timestamp);

    eprintln!("repm capture (SSH mode)");
    eprintln!("  host:       {}", host);
    eprintln!("  duration:   {}s", args.duration);
    eprintln!("  version:    {}", args.version);
    eprintln!("  remote dir: {}", remote_workdir);
    eprintln!("  timeout:    {}s", ssh_timeout);
    if let Some(ref sched) = args.scheduler {
        eprintln!("  scheduler:  {}", sched);
    }
    if args.dry_run {
        eprintln!("  [DRY RUN — no commands will be executed]");
    }
    eprintln!();

    // Build common SSH options
    let ssh_opts = build_ssh_opts(ssh_timeout, identity.as_deref());

    // Step 1: Test SSH connectivity
    eprintln!("step 1/6: testing SSH connectivity...");
    let test_cmd = ssh_command(host, "echo repm_ok", &ssh_opts);
    show_command("ssh-test", &test_cmd);
    if !args.dry_run {
        let output = run_command(&test_cmd)
            .context("SSH connectivity test failed — check host, key, and network")?;
        if !output.trim().contains("repm_ok") {
            bail!(
                "SSH connectivity test returned unexpected output: {}",
                output
            );
        }
        eprintln!("  connected OK");
    }
    eprintln!();

    // Step 2: Create remote working directory
    eprintln!("step 2/6: setting up remote workspace...");
    let remote_capture_dir = format!("{}/{}", remote_workdir, capture_name);
    let setup_cmd = ssh_command(host, &format!("mkdir -p {}", remote_capture_dir), &ssh_opts);
    show_command("ssh-mkdir", &setup_cmd);
    if !args.dry_run {
        run_command(&setup_cmd).context("Failed to create remote capture directory")?;
        eprintln!("  created {}", remote_capture_dir);
    }
    eprintln!();

    // Step 3: Collect host metadata (kernel, scheduler info, etc.)
    eprintln!("step 3/6: collecting host metadata...");
    let metadata_script = build_metadata_script(&remote_capture_dir, args);
    let meta_cmd = ssh_command(host, &metadata_script, &ssh_opts);
    show_command("ssh-metadata", &meta_cmd);
    if !args.dry_run {
        run_command(&meta_cmd).context("Failed to collect host metadata")?;
        eprintln!("  metadata collected");
    }
    eprintln!();

    // Step 4: Run trace commands
    eprintln!("step 4/6: running trace commands...");

    // 4a: /proc/interrupts sampling (if enabled)
    if cfg.collect_interrupts {
        let interval = cfg.interrupts_interval;
        let irq_script = format!(
            "for i in $(seq 0 {} {}); do \
                cat /proc/interrupts > {}/interrupts_${{i}}.txt; \
                sleep {}; \
            done",
            interval, args.duration, remote_capture_dir, interval,
        );
        let irq_cmd = ssh_command(host, &format!("nohup sh -c '{}' &", irq_script), &ssh_opts);
        show_command("ssh-interrupts", &irq_cmd);
        if !args.dry_run {
            run_command(&irq_cmd).context("Failed to start /proc/interrupts sampling")?;
            eprintln!("  started /proc/interrupts sampling (every {}s)", interval);
        }
    }

    // 4b: LAVD stats (if enabled)
    if cfg.collect_lavd_stats {
        let lavd_script = format!(
            "if command -v scx_lavd >/dev/null 2>&1; then \
                timeout {} scx_lavd --monitor 2>/dev/null > {}/lavd_stats.txt || true; \
            else \
                echo 'scx_lavd not found, skipping LAVD stats' >&2; \
            fi",
            args.duration + 2,
            remote_capture_dir,
        );
        let lavd_cmd = ssh_command(host, &format!("nohup sh -c '{}' &", lavd_script), &ssh_opts);
        show_command("ssh-lavd-stats", &lavd_cmd);
        if !args.dry_run {
            run_command(&lavd_cmd).context("Failed to start LAVD stats collection")?;
            eprintln!("  started LAVD stats collection");
        }
    }

    // 4c: User-configured trace commands
    for trace in &cfg.trace_commands {
        let expanded_cmd = trace
            .command
            .replace("{duration}", &args.duration.to_string());
        let full_cmd = if trace.background {
            format!(
                "nohup sh -c 'cd {} && {}' > {}/{}_stdout.txt 2>&1 &",
                remote_capture_dir, expanded_cmd, remote_capture_dir, trace.name,
            )
        } else {
            format!(
                "cd {} && {} > {}/{}_stdout.txt 2>&1",
                remote_capture_dir, expanded_cmd, remote_capture_dir, trace.name,
            )
        };
        let trace_cmd = ssh_command(host, &full_cmd, &ssh_opts);
        show_command(&format!("ssh-trace-{}", trace.name), &trace_cmd);
        if !args.dry_run {
            if trace.background {
                run_command(&trace_cmd)
                    .with_context(|| format!("Failed to start trace: {}", trace.name))?;
                eprintln!("  started background trace: {}", trace.name);
            } else {
                run_command(&trace_cmd)
                    .with_context(|| format!("Failed to run trace: {}", trace.name))?;
                eprintln!("  completed trace: {}", trace.name);
            }
        }
    }

    // If we started background processes, wait for the capture duration
    let has_background = cfg.collect_interrupts
        || cfg.collect_lavd_stats
        || cfg.trace_commands.iter().any(|t| t.background);

    if has_background && !args.dry_run {
        eprintln!();
        eprintln!("  waiting {}s for capture to complete...", args.duration);
        std::thread::sleep(std::time::Duration::from_secs(args.duration as u64));
        // Give a couple extra seconds for background processes to flush
        std::thread::sleep(std::time::Duration::from_secs(2));
        eprintln!("  capture period complete");
    }
    eprintln!();

    // Step 5: SCP results back to local workspace
    eprintln!("step 5/6: copying results to local workspace...");
    let local_data_dir = resolve_local_data_dir(ws_root.as_deref(), &args.version, &capture_name)?;

    if !args.dry_run {
        std::fs::create_dir_all(&local_data_dir)
            .with_context(|| format!("Failed to create local dir: {}", local_data_dir.display()))?;
    }

    let scp_src = format!("{}:{}/*", host, remote_capture_dir);
    let mut scp_args = vec!["-r".to_string()];
    scp_args.extend(ssh_opts_for_scp(&ssh_opts));
    scp_args.push(scp_src.clone());
    scp_args.push(local_data_dir.to_string_lossy().to_string());

    show_command_vec("scp-results", "scp", &scp_args);
    if !args.dry_run {
        let scp_status = Command::new("scp")
            .args(&scp_args)
            .status()
            .context("Failed to run scp")?;
        if !scp_status.success() {
            bail!("scp failed with exit code: {}", scp_status);
        }
        eprintln!("  copied to {}", local_data_dir.display());
    }
    eprintln!();

    // Step 6: Write local provenance.json and clean up remote
    eprintln!("step 6/6: writing provenance and cleaning up...");
    let provenance = build_provenance(host, args, &capture_name);
    let provenance_path = local_data_dir.join("provenance.json");

    show_command(
        "write-provenance",
        &[
            "write",
            provenance_path.to_str().unwrap_or("provenance.json"),
        ],
    );
    if !args.dry_run {
        let provenance_json =
            serde_json::to_string_pretty(&provenance).context("Failed to serialize provenance")?;
        std::fs::write(&provenance_path, &provenance_json)
            .with_context(|| format!("Failed to write {}", provenance_path.display()))?;
        eprintln!("  wrote {}", provenance_path.display());
    }

    // Clean up remote working directory
    let cleanup_cmd = ssh_command(host, &format!("rm -rf {}", remote_capture_dir), &ssh_opts);
    show_command("ssh-cleanup", &cleanup_cmd);
    if !args.dry_run {
        let _ = run_command(&cleanup_cmd); // Best-effort cleanup
        eprintln!("  cleaned up remote directory");
    }

    eprintln!();
    eprintln!("capture complete!");
    if !args.dry_run {
        eprintln!("  results: {}", local_data_dir.display());
        eprintln!("  provenance: {}", provenance_path.display());
    }
    eprintln!();
    eprintln!("next step: run `repm gen-config` to create workload config from trace data");

    Ok(())
}

// ---------------------------------------------------------------------------
// Local capture
// ---------------------------------------------------------------------------

fn execute_local_capture(
    args: &CaptureArgs,
    ws_root: &Option<PathBuf>,
    capture_cfg: Option<&crate::config::CaptureConfig>,
) -> Result<()> {
    let default_cfg = crate::config::CaptureConfig::default();
    let cfg = capture_cfg.unwrap_or(&default_cfg);

    let timestamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let capture_name = format!("capture_{}", timestamp);

    eprintln!("repm capture (local mode)");
    eprintln!("  duration: {}s", args.duration);
    eprintln!("  version:  {}", args.version);
    if let Some(ref sched) = args.scheduler {
        eprintln!("  scheduler: {}", sched);
    }
    if args.dry_run {
        eprintln!("  [DRY RUN — no commands will be executed]");
    }
    eprintln!();

    let local_data_dir = resolve_local_data_dir(ws_root.as_deref(), &args.version, &capture_name)?;

    if !args.dry_run {
        std::fs::create_dir_all(&local_data_dir)
            .with_context(|| format!("Failed to create dir: {}", local_data_dir.display()))?;
    }

    // Collect host metadata locally
    eprintln!("step 1/4: collecting host metadata...");
    let metadata = collect_local_metadata(args);
    if !args.dry_run {
        let meta_path = local_data_dir.join("host_metadata.json");
        let meta_json =
            serde_json::to_string_pretty(&metadata).context("Failed to serialize metadata")?;
        std::fs::write(&meta_path, &meta_json)?;
        eprintln!("  wrote {}", meta_path.display());
    }
    eprintln!();

    // /proc/interrupts sampling
    eprintln!("step 2/4: capturing /proc/interrupts...");
    if cfg.collect_interrupts && !args.dry_run {
        let interval = cfg.interrupts_interval;
        let samples = args.duration / interval;
        for i in 0..=samples {
            let irq_path = local_data_dir.join(format!("interrupts_{}.txt", i));
            match std::fs::read_to_string("/proc/interrupts") {
                Ok(content) => {
                    std::fs::write(&irq_path, &content)?;
                }
                Err(e) => eprintln!("  warning: failed to read /proc/interrupts: {}", e),
            }
            if i < samples {
                std::thread::sleep(std::time::Duration::from_secs(interval as u64));
            }
        }
        eprintln!("  collected {} samples", samples + 1);
    } else if args.dry_run {
        eprintln!(
            "  would sample /proc/interrupts every {}s for {}s",
            cfg.interrupts_interval, args.duration
        );
    }
    eprintln!();

    // Run configured trace commands locally
    eprintln!("step 3/4: running trace commands...");
    for trace in &cfg.trace_commands {
        let expanded_cmd = trace
            .command
            .replace("{duration}", &args.duration.to_string());
        let output_file = local_data_dir.join(format!("{}_stdout.txt", trace.name));

        show_command(
            &format!("trace-{}", trace.name),
            &["sh", "-c", &expanded_cmd],
        );

        if !args.dry_run {
            let output = Command::new("sh")
                .args(["-c", &expanded_cmd])
                .output()
                .with_context(|| format!("Failed to run trace: {}", trace.name))?;

            let mut f = std::fs::File::create(&output_file)?;
            f.write_all(&output.stdout)?;
            if !output.stderr.is_empty() {
                let stderr_file = local_data_dir.join(format!("{}_stderr.txt", trace.name));
                std::fs::write(&stderr_file, &output.stderr)?;
            }
            eprintln!("  completed: {} → {}", trace.name, output_file.display());
        }
    }
    eprintln!();

    // Write provenance
    eprintln!("step 4/4: writing provenance...");
    let provenance = build_provenance("localhost", args, &capture_name);
    let provenance_path = local_data_dir.join("provenance.json");

    if !args.dry_run {
        let provenance_json =
            serde_json::to_string_pretty(&provenance).context("Failed to serialize provenance")?;
        std::fs::write(&provenance_path, &provenance_json)?;
        eprintln!("  wrote {}", provenance_path.display());
    }

    eprintln!();
    eprintln!("capture complete!");
    if !args.dry_run {
        eprintln!("  results: {}", local_data_dir.display());
    }
    eprintln!();
    eprintln!("next step: run `repm gen-config` to create workload config from trace data");

    Ok(())
}

// ---------------------------------------------------------------------------
// SSH helpers
// ---------------------------------------------------------------------------

/// Build SSH option flags from config.
fn build_ssh_opts(timeout: u32, identity: Option<&str>) -> Vec<String> {
    let mut opts = vec![
        "-o".to_string(),
        "StrictHostKeyChecking=no".to_string(),
        "-o".to_string(),
        format!("ConnectTimeout={}", timeout),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
    ];
    if let Some(key) = identity {
        opts.push("-i".to_string());
        opts.push(key.to_string());
    }
    opts
}

/// Build an SSH command as a Vec<&str> for display/execution.
fn ssh_command(host: &str, remote_cmd: &str, ssh_opts: &[String]) -> Vec<String> {
    let mut cmd = vec!["ssh".to_string()];
    cmd.extend(ssh_opts.iter().cloned());
    cmd.push(host.to_string());
    cmd.push(remote_cmd.to_string());
    cmd
}

/// Convert SSH opts to SCP-compatible opts.
fn ssh_opts_for_scp(ssh_opts: &[String]) -> Vec<String> {
    // SCP accepts most SSH options via -o or directly
    ssh_opts.to_vec()
}

/// Run a command and return its stdout.
fn run_command(cmd: &[String]) -> Result<String> {
    if cmd.is_empty() {
        bail!("Empty command");
    }
    let output = Command::new(&cmd[0])
        .args(&cmd[1..])
        .output()
        .with_context(|| format!("Failed to execute: {}", cmd[0]))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "Command `{}` failed (exit {}): {}",
            cmd.join(" "),
            output.status,
            stderr.trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

// ---------------------------------------------------------------------------
// Metadata & provenance
// ---------------------------------------------------------------------------

/// Build the remote metadata collection script.
fn build_metadata_script(remote_dir: &str, args: &CaptureArgs) -> String {
    format!(
        r#"cat > {dir}/host_metadata.json << 'REPM_EOF'
{{
  "hostname": "$(hostname -f 2>/dev/null || hostname)",
  "kernel": "$(uname -r)",
  "kernel_full": "$(uname -a)",
  "arch": "$(uname -m)",
  "timestamp": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "capture_duration": {duration},
  "scheduler_requested": {scheduler},
  "cpu_count": $(nproc 2>/dev/null || echo 0),
  "sched_ext_enabled": $(cat /sys/kernel/sched_ext/root/ops 2>/dev/null && echo '"true"' || echo '"false"')
}}
REPM_EOF
# Now fix up with actual values using a helper script
python3 -c "
import json, subprocess, os
d = {{}}
d['hostname'] = subprocess.check_output(['hostname', '-f'], stderr=subprocess.DEVNULL).decode().strip() if os.path.exists('/bin/hostname') else 'unknown'
d['kernel'] = subprocess.check_output(['uname', '-r']).decode().strip()
d['kernel_full'] = subprocess.check_output(['uname', '-a']).decode().strip()
d['arch'] = subprocess.check_output(['uname', '-m']).decode().strip()
import datetime; d['timestamp'] = datetime.datetime.utcnow().strftime('%Y-%m-%dT%H:%M:%SZ')
d['capture_duration'] = {duration}
d['scheduler_requested'] = {scheduler}
d['cpu_count'] = int(subprocess.check_output(['nproc']).decode().strip())
try:
    d['sched_ext_ops'] = open('/sys/kernel/sched_ext/root/ops').read().strip()
except: d['sched_ext_ops'] = None
try:
    d['cmdline'] = open('/proc/cmdline').read().strip()
except: d['cmdline'] = None
with open('{dir}/host_metadata.json', 'w') as f: json.dump(d, f, indent=2)
" 2>/dev/null || echo "warning: python3 not available, using shell metadata" >&2"#,
        dir = remote_dir,
        duration = args.duration,
        scheduler = args
            .scheduler
            .as_ref()
            .map_or("null".to_string(), |s| format!("\"{}\"", s)),
    )
}

/// Collect metadata from the local machine.
fn collect_local_metadata(args: &CaptureArgs) -> BTreeMap<String, serde_json::Value> {
    use serde_json::Value;

    let mut meta = BTreeMap::new();

    let get_cmd = |cmd: &str, arg: &str| -> String {
        Command::new(cmd)
            .arg(arg)
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "unknown".to_string())
    };

    meta.insert("hostname".into(), Value::String(get_cmd("hostname", "-f")));
    meta.insert("kernel".into(), Value::String(get_cmd("uname", "-r")));
    meta.insert("kernel_full".into(), Value::String(get_cmd("uname", "-a")));
    meta.insert("arch".into(), Value::String(get_cmd("uname", "-m")));
    meta.insert(
        "timestamp".into(),
        Value::String(Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()),
    );
    meta.insert(
        "capture_duration".into(),
        Value::Number(args.duration.into()),
    );
    meta.insert(
        "scheduler_requested".into(),
        args.scheduler
            .as_ref()
            .map_or(Value::Null, |s| Value::String(s.clone())),
    );

    if let Ok(nproc) = Command::new("nproc").output() {
        if let Ok(n) = String::from_utf8_lossy(&nproc.stdout).trim().parse::<u64>() {
            meta.insert("cpu_count".into(), Value::Number(n.into()));
        }
    }

    // Check sched_ext
    if let Ok(ops) = std::fs::read_to_string("/sys/kernel/sched_ext/root/ops") {
        meta.insert("sched_ext_ops".into(), Value::String(ops.trim().into()));
    } else {
        meta.insert("sched_ext_ops".into(), Value::Null);
    }

    // Kernel command line
    if let Ok(cmdline) = std::fs::read_to_string("/proc/cmdline") {
        meta.insert("cmdline".into(), Value::String(cmdline.trim().into()));
    }

    meta
}

/// Build provenance record for a capture run.
fn build_provenance(
    host: &str,
    args: &CaptureArgs,
    capture_name: &str,
) -> BTreeMap<String, serde_json::Value> {
    use serde_json::Value;

    let mut prov = BTreeMap::new();
    prov.insert("capture_name".into(), Value::String(capture_name.into()));
    prov.insert("host".into(), Value::String(host.into()));
    prov.insert(
        "timestamp".into(),
        Value::String(Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()),
    );
    prov.insert("duration".into(), Value::Number(args.duration.into()));
    prov.insert("version".into(), Value::String(args.version.clone()));
    prov.insert(
        "scheduler".into(),
        args.scheduler
            .as_ref()
            .map_or(Value::Null, |s| Value::String(s.clone())),
    );

    // Capture the repm binary hash for reproducibility
    if let Ok(self_binary) = std::env::current_exe() {
        if let Ok(bytes) = std::fs::read(&self_binary) {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut hasher = DefaultHasher::new();
            bytes.hash(&mut hasher);
            prov.insert(
                "repm_binary_hash".into(),
                Value::String(format!("{:016x}", hasher.finish())),
            );
        }
    }

    prov
}

// ---------------------------------------------------------------------------
// Output directory resolution
// ---------------------------------------------------------------------------

/// Resolve the local data directory for this capture.
/// Layout: `<workspace>/experiments/<version>/data/<capture_name>/`
/// Falls back to `./traces/<capture_name>/` if not in a workspace.
fn resolve_local_data_dir(
    ws_root: Option<&Path>,
    version: &str,
    capture_name: &str,
) -> Result<PathBuf> {
    if let Some(root) = ws_root {
        Ok(root
            .join("experiments")
            .join(version)
            .join("data")
            .join(capture_name))
    } else {
        Ok(PathBuf::from("traces").join(capture_name))
    }
}

// ---------------------------------------------------------------------------
// Display helpers
// ---------------------------------------------------------------------------

fn show_command(label: &str, cmd: &[impl AsRef<str>]) {
    let parts: Vec<&str> = cmd.iter().map(|s| s.as_ref()).collect();
    eprintln!("  [{}] {}", label, shell_join(&parts));
}

fn show_command_vec(label: &str, program: &str, args: &[String]) {
    let mut parts = vec![program.to_string()];
    parts.extend(args.iter().cloned());
    let refs: Vec<&str> = parts.iter().map(|s| s.as_str()).collect();
    eprintln!("  [{}] {}", label, shell_join(&refs));
}

/// Join command parts into a shell-like display string, quoting as needed.
fn shell_join(parts: &[&str]) -> String {
    parts
        .iter()
        .map(|p| {
            if p.contains(' ') || p.contains('\'') || p.contains('"') || p.contains('$') {
                format!("'{}'", p.replace('\'', "'\\''"))
            } else {
                p.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_ssh_opts_basic() {
        let opts = build_ssh_opts(30, None);
        assert!(opts.contains(&"StrictHostKeyChecking=no".to_string()));
        assert!(opts.contains(&"ConnectTimeout=30".to_string()));
        assert!(opts.contains(&"BatchMode=yes".to_string()));
        assert!(!opts.contains(&"-i".to_string()));
    }

    #[test]
    fn test_build_ssh_opts_with_identity() {
        let opts = build_ssh_opts(10, Some("/home/user/.ssh/id_rsa"));
        assert!(opts.contains(&"-i".to_string()));
        assert!(opts.contains(&"/home/user/.ssh/id_rsa".to_string()));
    }

    #[test]
    fn test_ssh_command() {
        let opts = build_ssh_opts(30, None);
        let cmd = ssh_command("root@host", "ls -la", &opts);
        assert_eq!(cmd[0], "ssh");
        assert_eq!(cmd.last().unwrap(), "ls -la");
        assert!(cmd.contains(&"root@host".to_string()));
    }

    #[test]
    fn test_resolve_local_data_dir_with_workspace() {
        let dir = resolve_local_data_dir(
            Some(Path::new("/workspace")),
            "v1",
            "capture_20260417T120000Z",
        )
        .unwrap();
        assert_eq!(
            dir,
            PathBuf::from("/workspace/experiments/v1/data/capture_20260417T120000Z")
        );
    }

    #[test]
    fn test_resolve_local_data_dir_without_workspace() {
        let dir = resolve_local_data_dir(None, "v1", "capture_20260417T120000Z").unwrap();
        assert_eq!(dir, PathBuf::from("traces/capture_20260417T120000Z"));
    }

    #[test]
    fn test_shell_join() {
        assert_eq!(shell_join(&["ssh", "-o", "Foo=bar"]), "ssh -o Foo=bar");
        assert_eq!(
            shell_join(&["ssh", "echo hello world"]),
            "ssh 'echo hello world'"
        );
    }

    #[test]
    fn test_collect_local_metadata() {
        let args = CaptureArgs {
            ssh: None,
            duration: 10,
            scheduler: Some("eevdf".into()),
            dry_run: false,
            version: "test".into(),
            ssh_timeout: None,
            identity: None,
        };
        let meta = collect_local_metadata(&args);
        assert!(meta.contains_key("hostname"));
        assert!(meta.contains_key("kernel"));
        assert!(meta.contains_key("timestamp"));
        assert_eq!(meta["capture_duration"], serde_json::json!(10));
        assert_eq!(meta["scheduler_requested"], serde_json::json!("eevdf"));
    }

    #[test]
    fn test_build_provenance() {
        let args = CaptureArgs {
            ssh: Some("root@host".into()),
            duration: 30,
            scheduler: None,
            dry_run: false,
            version: "v1".into(),
            ssh_timeout: None,
            identity: None,
        };
        let prov = build_provenance("root@host", &args, "capture_test");
        assert_eq!(prov["host"], serde_json::json!("root@host"));
        assert_eq!(prov["duration"], serde_json::json!(30));
        assert_eq!(prov["version"], serde_json::json!("v1"));
        assert_eq!(prov["scheduler"], serde_json::Value::Null);
        assert!(prov.contains_key("repm_binary_hash"));
    }

    #[test]
    fn test_dry_run_args() {
        let args = CaptureArgs {
            ssh: Some("root@prod".into()),
            duration: 60,
            scheduler: Some("lavd_v1".into()),
            dry_run: true,
            version: "baseline".into(),
            ssh_timeout: Some(15),
            identity: Some("/root/.ssh/id_ed25519".into()),
        };
        assert!(args.dry_run);
        assert_eq!(args.ssh_timeout, Some(15));
        assert_eq!(args.identity.as_deref(), Some("/root/.ssh/id_ed25519"));
    }
}
