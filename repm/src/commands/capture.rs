use anyhow::Result;
use clap::Args;

/// Capture scheduling trace (local or remote via SSH).
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
        println!("repm capture (SSH): host={}, duration={}s", host, args.duration);
        println!();
        println!("TODO (SSH mode):");
        println!("  1. Check remote prerequisites (wprof, etc.)");
        println!("  2. scp repm binary to remote (e.g., /tmp/repm)");
        println!("  3. Run `repm capture` remotely");
        println!("  4. Copy trace + metadata back to local traces/");
        println!("  5. Clean up remote binary");
    } else {
        println!("repm capture (local): duration={}s", args.duration);
        println!();
        println!("TODO (local mode):");
        println!("  1. Resolve scheduler binary");
        println!("  2. Start scheduler");
        println!("  3. Run Perfetto trace capture");
        println!("  4. Stop scheduler");
        println!("  5. Save trace + provenance.json to traces/");
    }

    if let Some(ref sched) = args.scheduler {
        println!("  scheduler: {}", sched);
    }

    println!();
    println!("Next step: run `repm gen-config` to create workload config from trace");

    Ok(())
}
