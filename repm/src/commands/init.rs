use anyhow::Result;
use clap::Args;

use crate::config::validate_project_name;

/// Scaffold a workspace (git repo + directory structure + config).
#[derive(Debug, Args)]
pub struct InitArgs {
    /// Project name (required). Must match [a-zA-Z0-9_-]+.
    #[arg(long)]
    pub project_name: String,
}

pub fn execute(args: &InitArgs) -> Result<()> {
    validate_project_name(&args.project_name)?;

    println!("repm init: project_name={:?}", args.project_name);
    println!();
    println!("TODO: Scaffold workspace:");
    println!("  - Detect if inside existing sched-test checkout");
    println!("  - Otherwise add sched-test as git submodule");
    println!("  - Fall back to black-box mode if git/build fails");
    println!("  - Check prerequisites: vng, rt-app-rs, scx-sim");
    println!("  - Create repromagic_config.toml");
    println!("  - Create directory structure: bin/schedulers/, configs/, traces/, experiments/, reports/");
    println!();
    println!("Next step: edit repromagic_config.toml, then run `repm gen-config`");

    Ok(())
}
