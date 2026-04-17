use std::process::Command;

use anyhow::{Context, Result};
use clap::Args;

/// Capture scheduling trace (local or remote via SSH).
///
/// Local and remote share the same CLI arguments; SSH is just a flag.
/// In SSH mode, repm scps itself to the remote host and runs capture there.
#[derive(Debug, Args)]
pub struct CaptureArgs {
    /// Remote host for SSH capture (e.g., root@prod-host).
    /// If omitted, captures locally.
    #[arg(long)]
    pub ssh: Option<String>,

    /// Capture duration in seconds.
    #[arg(long, default_value = "30")]
    pub duration: u32,

    /// Scheduler to use during capture (key from config).
    #[arg(long)]
    pub scheduler: Option<String>,
}

pub fn execute(args: &CaptureArgs) -> Result<()> {
    if let Some(ref host) = args.ssh {
        execute_ssh_capture(host, args)
    } else {
        execute_local_capture(args)
    }
}

fn execute_ssh_capture(host: &str, args: &CaptureArgs) -> Result<()> {
    eprintln!("repm capture (SSH mode)");
    eprintln!("  host: {}", host);
    eprintln!("  duration: {}s", args.duration);

    // Step 1: Find our own binary to scp to remote
    let self_binary = std::env::current_exe()
        .context("Failed to determine repm binary path")?;
    let remote_path = "/tmp/repm";

    eprintln!("  scp {} → {}:{}", self_binary.display(), host, remote_path);
    let scp_status = Command::new("scp")
        .args([
            "-o", "StrictHostKeyChecking=no",
            self_binary.to_str().unwrap(),
            &format!("{}:{}", host, remote_path),
        ])
        .status()
        .context("Failed to run scp")?;

    if !scp_status.success() {
        anyhow::bail!("scp failed with exit code: {}", scp_status);
    }

    // Step 2: Run capture remotely
    let mut remote_cmd = format!("{} capture --duration {}", remote_path, args.duration);
    if let Some(ref sched) = args.scheduler {
        remote_cmd.push_str(&format!(" --scheduler {}", sched));
    }

    eprintln!("  ssh {} '{}'", host, remote_cmd);
    let ssh_status = Command::new("ssh")
        .args([
            "-o", "StrictHostKeyChecking=no",
            host,
            &remote_cmd,
        ])
        .status()
        .context("Failed to run ssh")?;

    if !ssh_status.success() {
        anyhow::bail!("Remote capture failed with exit code: {}", ssh_status);
    }

    // Step 3: Copy results back
    // TODO: Copy trace files from remote traces/ directory
    eprintln!("  TODO: copy trace results back from remote");

    // Step 4: Clean up remote binary
    eprintln!("  cleaning up remote binary");
    let _ = Command::new("ssh")
        .args([host, &format!("rm -f {}", remote_path)])
        .status();

    eprintln!("  SSH capture complete");
    Ok(())
}

fn execute_local_capture(args: &CaptureArgs) -> Result<()> {
    eprintln!("repm capture (local mode)");
    eprintln!("  duration: {}s", args.duration);

    if let Some(ref sched) = args.scheduler {
        eprintln!("  scheduler: {}", sched);
    }

    // TODO: Implement local capture pipeline
    // 1. Load workspace config
    // 2. Resolve scheduler binary
    // 3. Start scheduler (if not kernel-builtin)
    // 4. Run Perfetto trace capture
    // 5. Stop scheduler
    // 6. Save trace + provenance.json to traces/

    eprintln!();
    eprintln!("TODO: Local capture pipeline not yet implemented.");
    eprintln!("Requires: Perfetto trace tools, scheduler binary management.");
    eprintln!();
    eprintln!("Next step: run `repm gen-config` to create workload config");

    Ok(())
}
