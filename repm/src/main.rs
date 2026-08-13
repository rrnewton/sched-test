use anyhow::Result;
use clap::{Parser, Subcommand};

use repm::commands::{analyze, capture, gen_config, init, magic, run};

/// ReproMagic — capture and reproduce scheduling behavior.
///
/// A self-contained CLI tool for capturing production scheduling behavior
/// and replaying it as reproducible rt-app workloads on any Linux host.
#[derive(Parser)]
#[command(name = "repm", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Scaffold a workspace (git repo + directory structure + config).
    Init(init::InitArgs),

    /// Top-level wizard: init + guided reproducer workflow.
    Magic(magic::MagicArgs),

    /// Capture scheduling trace (local or remote via SSH).
    Capture(capture::CaptureArgs),

    /// Generate parameterized rt-app JSON configs.
    GenConfig(gen_config::GenConfigArgs),

    /// Run experiment matrix (scheduler × condition × rep).
    Run(run::RunArgs),

    /// Post-processing: results tables, cross-check metrics.
    Analyze(analyze::AnalyzeArgs),
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Commands::Init(args) => init::execute(args),
        Commands::Magic(args) => magic::execute(args),
        Commands::Capture(args) => capture::execute(args),
        Commands::GenConfig(args) => gen_config::execute(args),
        Commands::Run(args) => run::execute(args),
        Commands::Analyze(args) => analyze::execute(args),
    }
}
