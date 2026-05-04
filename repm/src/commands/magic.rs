// Copyright (c) Meta Platforms, Inc. and affiliates.
// SPDX-License-Identifier: GPL-2.0-only

//! `repm magic` — wizard-style workflow that chains the full pipeline.
//!
//! Interactive mode prompts for each parameter; headless mode uses
//! config defaults. Both modes chain: init → gen-config → run → analyze.

use std::io::{self, BufRead, Write as _};

use anyhow::{Context, Result};
use clap::Args;

use crate::commands::{analyze, gen_config, init, run};
use crate::config::RunMode;
use crate::workspace;

/// Top-level wizard: init + guided reproducer workflow.
///
/// Chains init → gen-config → run → analyze into one command.
/// In headless mode, uses config defaults for CI/automated runs.
#[derive(Debug, Args)]
pub struct MagicArgs {
    /// Run in headless mode (use defaults, no interactive prompts).
    #[arg(long)]
    pub headless: bool,

    /// Show the full workflow plan without executing.
    #[arg(long)]
    pub dry_run: bool,

    /// Project name (skips the interactive prompt).
    #[arg(long)]
    pub project_name: Option<String>,

    /// Phenomenon type (skips the interactive prompt).
    #[arg(long)]
    pub phenomenon: Option<String>,

    /// Schedulers to test (comma-separated, skips prompt).
    #[arg(long, value_delimiter = ',')]
    pub schedulers: Option<Vec<String>>,

    /// Number of repetitions per cell.
    #[arg(long)]
    pub reps: Option<u32>,

    /// Experiment duration in seconds.
    #[arg(long)]
    pub duration: Option<u32>,

    /// Experiment modes (comma-separated).
    #[arg(long, value_enum, value_delimiter = ',')]
    pub mode: Option<Vec<RunMode>>,

    /// Number of foreground threads.
    #[arg(long)]
    pub foreground: Option<u32>,

    /// Number of background threads.
    #[arg(long)]
    pub background: Option<u32>,

    /// Skip the init step (workspace already exists).
    #[arg(long)]
    pub skip_init: bool,

    /// Skip the gen-config step (config already generated).
    #[arg(long)]
    pub skip_gen_config: bool,

    /// Skip the run step (just analyze existing data).
    #[arg(long)]
    pub skip_run: bool,

    /// Skip the analyze step.
    #[arg(long)]
    pub skip_analyze: bool,
}

// ---------------------------------------------------------------------------
// Wizard plan — describes what will happen
// ---------------------------------------------------------------------------

/// A resolved plan for the wizard to execute.
#[derive(Debug)]
struct WizardPlan {
    project_name: String,
    phenomenon: String,
    reps: u32,
    duration: u32,
    modes: Vec<RunMode>,
    schedulers: Option<Vec<String>>,
    foreground: Option<u32>,
    background: Option<u32>,
    do_init: bool,
    do_gen_config: bool,
    do_run: bool,
    do_analyze: bool,
}

impl WizardPlan {
    fn print_summary(&self) {
        eprintln!();
        eprintln!("╔══════════════════════════════════════════════════╗");
        eprintln!("║           repm magic — workflow plan             ║");
        eprintln!("╚══════════════════════════════════════════════════╝");
        eprintln!();
        eprintln!("  Project:     {}", self.project_name);
        eprintln!("  Phenomenon:  {}", self.phenomenon);
        eprintln!("  Reps:        {}", self.reps);
        eprintln!("  Duration:    {}s", self.duration);
        eprintln!(
            "  Modes:       {}",
            self.modes
                .iter()
                .map(|m| m.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        if let Some(ref s) = self.schedulers {
            eprintln!("  Schedulers:  {}", s.join(", "));
        } else {
            eprintln!("  Schedulers:  (all from config)");
        }
        if let Some(fg) = self.foreground {
            eprintln!("  Foreground:  {} threads", fg);
        }
        if let Some(bg) = self.background {
            eprintln!("  Background:  {} threads", bg);
        }

        eprintln!();
        eprintln!("  Pipeline steps:");
        let steps = [
            (self.do_init, "1. repm init", "Scaffold workspace"),
            (
                self.do_gen_config,
                "2. repm gen-config",
                "Generate rt-app workload config",
            ),
            (self.do_run, "3. repm run", "Execute experiment matrix"),
            (
                self.do_analyze,
                "4. repm analyze",
                "Generate results tables",
            ),
        ];
        for (enabled, name, desc) in &steps {
            let marker = if *enabled { "[*]" } else { "[ ]" };
            eprintln!("    {} {} — {}", marker, name, desc);
        }
        eprintln!();
    }
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

pub fn execute(args: &MagicArgs) -> Result<()> {
    // Phase 1: Determine if workspace already exists
    let has_workspace = workspace::find_workspace_root(
        &std::env::current_dir().context("Failed to get current directory")?,
    )
    .is_some();

    // Phase 2: Build plan (interactive prompts or defaults)
    let plan = if args.headless {
        build_headless_plan(args, has_workspace)?
    } else {
        build_interactive_plan(args, has_workspace)?
    };

    // Phase 3: Show plan
    plan.print_summary();

    if args.dry_run {
        eprintln!("=== DRY RUN — no actions taken ===");
        eprintln!();
        if plan.do_run {
            eprintln!(
                "Estimated experiment time: ~{}s",
                plan.reps as usize
                    * plan.modes.len()
                    * plan.schedulers.as_ref().map(|s| s.len()).unwrap_or(1)
                    * plan.duration as usize
            );
        }
        eprintln!("Run without --dry-run to execute the full pipeline.");
        return Ok(());
    }

    // Phase 4: Execute each step
    let mut step_results = StepResults::default();

    // Step 1: Init
    if plan.do_init {
        eprintln!();
        eprintln!("━━━ Step 1/4: repm init ━━━");
        match execute_init(&plan) {
            Ok(()) => {
                step_results.init = StepStatus::Success;
                eprintln!("  ✓ Workspace initialized");
            }
            Err(e) => {
                step_results.init = StepStatus::Failed(format!("{}", e));
                eprintln!("  ✗ Init failed: {}", e);
                // Init failure is fatal — can't continue without a workspace
                print_final_summary(&step_results);
                return Err(e);
            }
        }
    } else {
        step_results.init = StepStatus::Skipped;
        eprintln!();
        eprintln!("━━━ Step 1/4: repm init (skipped — workspace exists) ━━━");
    }

    // Step 2: Gen-config
    if plan.do_gen_config {
        eprintln!();
        eprintln!("━━━ Step 2/4: repm gen-config ━━━");
        match execute_gen_config(&plan) {
            Ok(()) => {
                step_results.gen_config = StepStatus::Success;
                eprintln!("  ✓ Workload config generated");
            }
            Err(e) => {
                step_results.gen_config = StepStatus::Failed(format!("{}", e));
                eprintln!("  ✗ Gen-config failed: {}", e);
                eprintln!("  Continuing to next step...");
            }
        }
    } else {
        step_results.gen_config = StepStatus::Skipped;
        eprintln!();
        eprintln!("━━━ Step 2/4: repm gen-config (skipped) ━━━");
    }

    // Step 3: Run
    if plan.do_run {
        eprintln!();
        eprintln!("━━━ Step 3/4: repm run ━━━");
        // Gen-config must have succeeded for run to work
        if matches!(step_results.gen_config, StepStatus::Failed(_)) {
            step_results.run = StepStatus::Failed("skipped due to gen-config failure".into());
            eprintln!("  ✗ Run skipped — gen-config failed");
        } else {
            match execute_run(&plan) {
                Ok(()) => {
                    step_results.run = StepStatus::Success;
                    eprintln!("  ✓ Experiment matrix complete");
                }
                Err(e) => {
                    step_results.run = StepStatus::Failed(format!("{}", e));
                    eprintln!("  ✗ Run failed: {}", e);
                    eprintln!("  Continuing to analyze partial results...");
                }
            }
        }
    } else {
        step_results.run = StepStatus::Skipped;
        eprintln!();
        eprintln!("━━━ Step 3/4: repm run (skipped) ━━━");
    }

    // Step 4: Analyze
    if plan.do_analyze {
        eprintln!();
        eprintln!("━━━ Step 4/4: repm analyze ━━━");
        match execute_analyze() {
            Ok(()) => {
                step_results.analyze = StepStatus::Success;
                eprintln!("  ✓ Analysis complete");
            }
            Err(e) => {
                step_results.analyze = StepStatus::Failed(format!("{}", e));
                eprintln!("  ✗ Analyze failed: {}", e);
            }
        }
    } else {
        step_results.analyze = StepStatus::Skipped;
        eprintln!();
        eprintln!("━━━ Step 4/4: repm analyze (skipped) ━━━");
    }

    // Phase 5: Final summary
    print_final_summary(&step_results);

    Ok(())
}

// ---------------------------------------------------------------------------
// Plan builders
// ---------------------------------------------------------------------------

fn build_headless_plan(args: &MagicArgs, has_workspace: bool) -> Result<WizardPlan> {
    let project_name = args
        .project_name
        .clone()
        .unwrap_or_else(|| "experiment".to_string());
    let phenomenon = args
        .phenomenon
        .clone()
        .unwrap_or_else(|| "bad_tail_latency".to_string());

    // If workspace exists and we need defaults, try loading config
    let (reps, duration) = if has_workspace && (args.reps.is_none() || args.duration.is_none()) {
        match workspace::load_config_from_cwd() {
            Ok((_root, config)) => (
                args.reps.unwrap_or(config.defaults.reps),
                args.duration.unwrap_or(config.defaults.duration),
            ),
            Err(_) => (args.reps.unwrap_or(3), args.duration.unwrap_or(30)),
        }
    } else {
        (args.reps.unwrap_or(3), args.duration.unwrap_or(30))
    };

    Ok(WizardPlan {
        project_name,
        phenomenon,
        reps,
        duration,
        modes: args
            .mode
            .clone()
            .unwrap_or_else(|| vec![RunMode::RtappPinned]),
        schedulers: args.schedulers.clone(),
        foreground: args.foreground,
        background: args.background,
        do_init: !has_workspace && !args.skip_init,
        do_gen_config: !args.skip_gen_config,
        do_run: !args.skip_run,
        do_analyze: !args.skip_analyze,
    })
}

fn build_interactive_plan(args: &MagicArgs, has_workspace: bool) -> Result<WizardPlan> {
    eprintln!();
    eprintln!("╔══════════════════════════════════════════════════╗");
    eprintln!("║        repm magic — interactive wizard           ║");
    eprintln!("╚══════════════════════════════════════════════════╝");
    eprintln!();

    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();

    // Project name
    let project_name = if let Some(ref name) = args.project_name {
        name.clone()
    } else if has_workspace {
        let (_root, config) = workspace::load_config_from_cwd()?;
        eprintln!("  Existing workspace detected: {}", config.project.name);
        config.project.name.clone()
    } else {
        prompt_string(&mut lines, "Project name", "experiment")?
    };

    // Phenomenon
    let phenomenon = if let Some(ref p) = args.phenomenon {
        p.clone()
    } else if has_workspace {
        let (_root, config) = workspace::load_config_from_cwd()?;
        config.project.phenomenon.to_string()
    } else {
        eprintln!();
        eprintln!("  Phenomenon types:");
        eprintln!("    1. bad_tail_latency  — P99/P99.9 latency spikes");
        eprintln!("    2. bad_cpu_util      — unexpected CPU utilization");
        eprintln!("    3. bad_throughput    — throughput degradation");
        let choice = prompt_string(&mut lines, "Phenomenon (1/2/3 or name)", "1")?;
        match choice.as_str() {
            "1" => "bad_tail_latency".to_string(),
            "2" => "bad_cpu_util".to_string(),
            "3" => "bad_throughput".to_string(),
            other => other.to_string(),
        }
    };

    // Read config defaults if workspace exists
    let (default_reps, default_duration) = if has_workspace {
        let (_root, config) = workspace::load_config_from_cwd()?;
        (config.defaults.reps, config.defaults.duration)
    } else {
        (3, 30)
    };

    // Reps
    let reps = if let Some(r) = args.reps {
        r
    } else {
        let s = prompt_string(
            &mut lines,
            "Repetitions per cell",
            &default_reps.to_string(),
        )?;
        s.parse::<u32>().unwrap_or(default_reps)
    };

    // Duration
    let duration = if let Some(d) = args.duration {
        d
    } else {
        let s = prompt_string(
            &mut lines,
            "Experiment duration (seconds)",
            &default_duration.to_string(),
        )?;
        s.parse::<u32>().unwrap_or(default_duration)
    };

    // Modes
    let modes = if let Some(ref m) = args.mode {
        m.clone()
    } else {
        eprintln!();
        eprintln!("  Run modes:");
        eprintln!("    1. rtapp_pinned  — bare-metal with CPU pinning (recommended)");
        eprintln!("    2. rtapp_vm      — inside virtme-ng VM");
        eprintln!("    3. rtapp_sim     — scx-sim deterministic simulator");
        let choice = prompt_string(&mut lines, "Mode (1/2/3, comma-separated)", "1")?;
        parse_mode_choices(&choice)
    };

    // Schedulers
    let schedulers = if args.schedulers.is_some() {
        args.schedulers.clone()
    } else if has_workspace {
        let (_root, config) = workspace::load_config_from_cwd()?;
        let keys: Vec<String> = config.schedulers.keys().cloned().collect();
        if keys.is_empty() {
            None
        } else {
            eprintln!();
            eprintln!("  Available schedulers from config:");
            for (i, key) in keys.iter().enumerate() {
                let sched = &config.schedulers[key];
                eprintln!("    {}. {} [{}]", i + 1, key, sched.label);
            }
            let choice = prompt_string(
                &mut lines,
                "Schedulers (numbers or names, comma-separated, or 'all')",
                "all",
            )?;
            if choice.trim() == "all" {
                None // None means all
            } else {
                Some(resolve_scheduler_choices(&choice, &keys))
            }
        }
    } else {
        None
    };

    Ok(WizardPlan {
        project_name,
        phenomenon,
        reps,
        duration,
        modes,
        schedulers,
        foreground: args.foreground,
        background: args.background,
        do_init: !has_workspace && !args.skip_init,
        do_gen_config: !args.skip_gen_config,
        do_run: !args.skip_run,
        do_analyze: !args.skip_analyze,
    })
}

// ---------------------------------------------------------------------------
// Interactive prompt helpers
// ---------------------------------------------------------------------------

fn prompt_string(
    lines: &mut io::Lines<io::StdinLock>,
    prompt: &str,
    default: &str,
) -> Result<String> {
    eprint!("  {} [{}]: ", prompt, default);
    io::stderr().flush()?;
    match lines.next() {
        Some(Ok(line)) => {
            let trimmed = line.trim().to_string();
            if trimmed.is_empty() {
                Ok(default.to_string())
            } else {
                Ok(trimmed)
            }
        }
        Some(Err(e)) => Err(e.into()),
        None => Ok(default.to_string()),
    }
}

fn parse_mode_choices(input: &str) -> Vec<RunMode> {
    let mut modes = Vec::new();
    for part in input.split(',') {
        match part.trim() {
            "1" | "rtapp_pinned" | "rtapp-pinned" => modes.push(RunMode::RtappPinned),
            "2" | "rtapp_vm" | "rtapp-vm" => modes.push(RunMode::RtappVm),
            "3" | "rtapp_sim" | "rtapp-sim" => modes.push(RunMode::RtappSim),
            _ => {} // Skip unknown
        }
    }
    if modes.is_empty() {
        modes.push(RunMode::RtappPinned);
    }
    modes
}

fn resolve_scheduler_choices(input: &str, available: &[String]) -> Vec<String> {
    let mut result = Vec::new();
    for part in input.split(',') {
        let trimmed = part.trim();
        // Try as a number first
        if let Ok(idx) = trimmed.parse::<usize>() {
            if idx >= 1 && idx <= available.len() {
                result.push(available[idx - 1].clone());
                continue;
            }
        }
        // Try as a name
        if available.contains(&trimmed.to_string()) {
            result.push(trimmed.to_string());
        }
    }
    if result.is_empty() {
        available.to_vec()
    } else {
        result
    }
}

// ---------------------------------------------------------------------------
// Step execution — delegates to existing commands
// ---------------------------------------------------------------------------

fn execute_init(plan: &WizardPlan) -> Result<()> {
    let args = init::InitArgs {
        project_name: plan.project_name.clone(),
        new_git: false,
        use_git: None,
    };
    init::execute(&args)
}

fn execute_gen_config(plan: &WizardPlan) -> Result<()> {
    let args = gen_config::GenConfigArgs {
        foreground: plan.foreground,
        background: plan.background,
        cores: None,
        fg_run_us: 500,
        fg_sleep_us: 1500,
        bg_run_us: 130,
        bg_sleep_us: 950,
        bg_priority: 10,
        duration: Some(plan.duration),
        output: None,
        from_trace: None,
        log_basename: "repromagic".to_string(),
        with_irq: false,
        irq_run_us: 5000,
        irq_sleep_us: 5000,
        verbose: false,
        format: gen_config::ConfigFormat::Rtapp,
    };
    gen_config::execute(&args)
}

fn execute_run(plan: &WizardPlan) -> Result<()> {
    let args = run::RunArgs {
        mode: Some(plan.modes.clone()),
        schedulers: plan.schedulers.clone(),
        reps: Some(plan.reps),
        dry_run: false,
        new_version: None,
        experiment: None,
        purpose: Some(format!(
            "Wizard-generated experiment for {} ({})",
            plan.project_name, plan.phenomenon
        )),
        duration: Some(plan.duration),
    };
    run::execute(&args)
}

fn execute_analyze() -> Result<()> {
    let args = analyze::AnalyzeArgs {
        experiment: None, // Auto-discover
        compare: None,
        thread_type: "foreground".to_string(),
        format: analyze::OutputFormat::Markdown,
        citations: false,
        write: true,
        cross_check: true,
        score: None,
    };
    analyze::execute(&args)
}

// ---------------------------------------------------------------------------
// Step tracking and summary
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct StepResults {
    init: StepStatus,
    gen_config: StepStatus,
    run: StepStatus,
    analyze: StepStatus,
}

#[derive(Debug, Default)]
enum StepStatus {
    #[default]
    Pending,
    Success,
    Skipped,
    Failed(String),
}

impl std::fmt::Display for StepStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Success => write!(f, "OK"),
            Self::Skipped => write!(f, "skipped"),
            Self::Failed(reason) => write!(f, "FAILED: {}", reason),
        }
    }
}

impl StepStatus {
    fn marker(&self) -> &str {
        match self {
            Self::Success => "[OK]",
            Self::Skipped => "[--]",
            Self::Failed(_) => "[!!]",
            Self::Pending => "[..]",
        }
    }
}

fn print_final_summary(results: &StepResults) {
    eprintln!();
    eprintln!("╔══════════════════════════════════════════════════╗");
    eprintln!("║              repm magic — summary                ║");
    eprintln!("╚══════════════════════════════════════════════════╝");
    eprintln!();
    let steps = [
        ("init      ", &results.init),
        ("gen-config", &results.gen_config),
        ("run       ", &results.run),
        ("analyze   ", &results.analyze),
    ];
    for (name, status) in &steps {
        eprintln!("  {} {} — {}", status.marker(), name, status);
    }
    eprintln!();

    let any_failed = matches!(results.init, StepStatus::Failed(_))
        || matches!(results.gen_config, StepStatus::Failed(_))
        || matches!(results.run, StepStatus::Failed(_))
        || matches!(results.analyze, StepStatus::Failed(_));

    if any_failed {
        eprintln!("  Some steps failed. Fix the issues and re-run, or use");
        eprintln!("  --skip-init / --skip-gen-config / --skip-run to resume");
        eprintln!("  from a specific step.");
    } else {
        eprintln!("  All steps complete! Check experiments/ for results.");
    }
    eprintln!();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_mode_choices_numbers() {
        let modes = parse_mode_choices("1");
        assert_eq!(modes, vec![RunMode::RtappPinned]);

        let modes = parse_mode_choices("1,3");
        assert_eq!(modes, vec![RunMode::RtappPinned, RunMode::RtappSim]);
    }

    #[test]
    fn test_parse_mode_choices_names() {
        let modes = parse_mode_choices("rtapp_pinned,rtapp_vm");
        assert_eq!(modes, vec![RunMode::RtappPinned, RunMode::RtappVm]);
    }

    #[test]
    fn test_parse_mode_choices_mixed() {
        let modes = parse_mode_choices("1,rtapp_sim");
        assert_eq!(modes, vec![RunMode::RtappPinned, RunMode::RtappSim]);
    }

    #[test]
    fn test_parse_mode_choices_empty_defaults() {
        let modes = parse_mode_choices("invalid");
        assert_eq!(modes, vec![RunMode::RtappPinned]);
    }

    #[test]
    fn test_parse_mode_choices_clap_style() {
        let modes = parse_mode_choices("rtapp-pinned,rtapp-vm");
        assert_eq!(modes, vec![RunMode::RtappPinned, RunMode::RtappVm]);
    }

    #[test]
    fn test_resolve_scheduler_choices_by_number() {
        let available = vec!["eevdf".into(), "lavd_v1".into(), "lavd_v2".into()];
        let result = resolve_scheduler_choices("1,3", &available);
        assert_eq!(result, vec!["eevdf", "lavd_v2"]);
    }

    #[test]
    fn test_resolve_scheduler_choices_by_name() {
        let available = vec!["eevdf".into(), "lavd_v1".into()];
        let result = resolve_scheduler_choices("lavd_v1", &available);
        assert_eq!(result, vec!["lavd_v1"]);
    }

    #[test]
    fn test_resolve_scheduler_choices_mixed() {
        let available = vec!["eevdf".into(), "lavd_v1".into(), "lavd_v2".into()];
        let result = resolve_scheduler_choices("1, lavd_v2", &available);
        assert_eq!(result, vec!["eevdf", "lavd_v2"]);
    }

    #[test]
    fn test_resolve_scheduler_choices_invalid_falls_back() {
        let available = vec!["eevdf".into(), "lavd_v1".into()];
        let result = resolve_scheduler_choices("bogus", &available);
        assert_eq!(result, available); // Falls back to all
    }

    #[test]
    fn test_step_status_display() {
        assert_eq!(format!("{}", StepStatus::Success), "OK");
        assert_eq!(format!("{}", StepStatus::Skipped), "skipped");
        assert!(format!("{}", StepStatus::Failed("oops".into())).contains("oops"));
    }

    #[test]
    fn test_step_status_markers() {
        assert_eq!(StepStatus::Success.marker(), "[OK]");
        assert_eq!(StepStatus::Skipped.marker(), "[--]");
        assert_eq!(StepStatus::Failed("x".into()).marker(), "[!!]");
        assert_eq!(StepStatus::Pending.marker(), "[..]");
    }

    #[test]
    fn test_headless_plan_defaults() {
        let args = MagicArgs {
            headless: true,
            dry_run: false,
            project_name: None,
            phenomenon: None,
            schedulers: None,
            reps: None,
            duration: None,
            mode: None,
            foreground: None,
            background: None,
            skip_init: false,
            skip_gen_config: false,
            skip_run: false,
            skip_analyze: false,
        };
        let plan = build_headless_plan(&args, false).unwrap();
        assert_eq!(plan.project_name, "experiment");
        assert_eq!(plan.phenomenon, "bad_tail_latency");
        assert_eq!(plan.reps, 3);
        assert_eq!(plan.duration, 30);
        assert_eq!(plan.modes, vec![RunMode::RtappPinned]);
        assert!(plan.do_init);
        assert!(plan.do_gen_config);
        assert!(plan.do_run);
        assert!(plan.do_analyze);
    }

    #[test]
    fn test_headless_plan_with_overrides() {
        let args = MagicArgs {
            headless: true,
            dry_run: false,
            project_name: Some("my_test".into()),
            phenomenon: Some("bad_cpu_util".into()),
            schedulers: Some(vec!["eevdf".into()]),
            reps: Some(5),
            duration: Some(60),
            mode: Some(vec![RunMode::RtappPinned, RunMode::RtappVm]),
            foreground: Some(8),
            background: Some(32),
            skip_init: true,
            skip_gen_config: false,
            skip_run: false,
            skip_analyze: true,
        };
        let plan = build_headless_plan(&args, false).unwrap();
        assert_eq!(plan.project_name, "my_test");
        assert_eq!(plan.phenomenon, "bad_cpu_util");
        assert_eq!(plan.reps, 5);
        assert_eq!(plan.duration, 60);
        assert_eq!(plan.modes.len(), 2);
        assert_eq!(plan.schedulers, Some(vec!["eevdf".into()]));
        assert_eq!(plan.foreground, Some(8));
        assert_eq!(plan.background, Some(32));
        assert!(!plan.do_init); // skip_init
        assert!(plan.do_gen_config);
        assert!(plan.do_run);
        assert!(!plan.do_analyze); // skip_analyze
    }

    #[test]
    fn test_headless_plan_existing_workspace() {
        let args = MagicArgs {
            headless: true,
            dry_run: false,
            project_name: None,
            phenomenon: None,
            schedulers: None,
            reps: None,
            duration: None,
            mode: None,
            foreground: None,
            background: None,
            skip_init: false,
            skip_gen_config: false,
            skip_run: false,
            skip_analyze: false,
        };
        // has_workspace=true → skip init
        let plan = build_headless_plan(&args, true).unwrap();
        assert!(!plan.do_init);
    }

    #[test]
    fn test_wizard_plan_summary_does_not_panic() {
        let plan = WizardPlan {
            project_name: "test".into(),
            phenomenon: "bad_tail_latency".into(),
            reps: 3,
            duration: 30,
            modes: vec![RunMode::RtappPinned],
            schedulers: None,
            foreground: None,
            background: None,
            do_init: true,
            do_gen_config: true,
            do_run: true,
            do_analyze: true,
        };
        plan.print_summary(); // Should not panic
    }
}
