use anyhow::Result;
use clap::Args;

use crate::config::RunMode;

/// Run experiment matrix (scheduler × condition × rep).
#[derive(Debug, Args)]
pub struct RunArgs {
    /// Experiment mode.
    #[arg(long, value_enum)]
    pub mode: Option<RunMode>,

    /// Schedulers to test (comma-separated keys from config).
    #[arg(long, value_delimiter = ',')]
    pub schedulers: Option<Vec<String>>,

    /// Number of repetitions per cell.
    #[arg(long)]
    pub reps: Option<u32>,
}

pub fn execute(args: &RunArgs) -> Result<()> {
    println!("repm run:");
    if let Some(ref mode) = args.mode {
        println!("  mode: {}", mode);
    }
    if let Some(ref scheds) = args.schedulers {
        println!("  schedulers: {}", scheds.join(", "));
    }
    if let Some(reps) = args.reps {
        println!("  reps: {}", reps);
    }

    println!();
    println!("TODO: Execute experiment matrix:");
    println!("  1. Read workspace config");
    println!("  2. Resolve scheduler binaries (bin/schedulers/)");
    println!("  3. Randomize run order");
    println!("  4. For each (scheduler, condition, rep):");
    println!("     a. Start scheduler");
    println!("     b. Run rt-app workload");
    println!("     c. Collect metrics");
    println!("     d. Stop scheduler");
    println!("  5. Write results CSV");
    println!("  6. Create versioned experiment directory with provenance");
    println!();
    println!("Next step: run `repm analyze` on the results");

    Ok(())
}
