use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use clap::Args;

use crate::config::validate_project_name;
use crate::templates;
use crate::workspace;

/// Scaffold a workspace (git repo + directory structure + config).
#[derive(Debug, Args)]
pub struct InitArgs {
    /// Project name (required). Must match [a-zA-Z0-9_-]+.
    #[arg(long)]
    pub project_name: String,

    /// Create a new git repository for the workspace.
    #[arg(long, conflicts_with = "use_git")]
    pub new_git: bool,

    /// Use an existing git repository (specify path to repo root).
    #[arg(long, conflicts_with = "new_git")]
    pub use_git: Option<PathBuf>,
}

pub fn execute(args: &InitArgs) -> Result<()> {
    validate_project_name(&args.project_name)?;

    let workspace_root = std::env::current_dir().context("Failed to get current directory")?;

    // Check if workspace already exists
    let config_path = workspace_root.join(workspace::CONFIG_FILENAME);
    if config_path.exists() {
        anyhow::bail!(
            "Workspace already exists: {}\n\
             Remove {} to re-initialize.",
            workspace_root.display(),
            config_path.display()
        );
    }

    eprintln!(
        "Initializing repromagic workspace: {}",
        workspace_root.display()
    );
    eprintln!("  project: {}", args.project_name);

    // Step 1: Git setup
    if args.new_git {
        init_new_git(&workspace_root)?;
    } else if let Some(ref git_dir) = args.use_git {
        verify_git_repo(git_dir)?;
        eprintln!("  using existing git repo: {}", git_dir.display());
    } else {
        // No git flag — check if we're already in a git repo
        if is_inside_git_repo(&workspace_root) {
            eprintln!("  detected existing git repo");
        } else {
            eprintln!("  no git repo detected (use --new-git to create one)");
        }
    }

    // Step 2: Create directory structure
    workspace::create_workspace_dirs(&workspace_root)?;
    eprintln!("  created workspace directories");

    // Step 3: Write .gitkeep files so empty dirs are tracked
    for dir in workspace::WORKSPACE_DIRS {
        let gitkeep = workspace_root.join(dir).join(".gitkeep");
        if !gitkeep.exists() {
            std::fs::write(&gitkeep, "")?;
        }
    }

    // Step 4: Write repromagic_config.toml
    let config_content = workspace::default_config_toml(&args.project_name);
    std::fs::write(&config_path, &config_content)
        .with_context(|| format!("Failed to write {}", config_path.display()))?;
    eprintln!("  wrote {}", workspace::CONFIG_FILENAME);

    // Step 5: Write .gitignore
    workspace::write_gitignore(&workspace_root)?;
    eprintln!("  updated .gitignore");

    // Step 6: Install CLAUDE.md template and skill files
    let config = workspace::load_config(&workspace_root)?;
    templates::install_templates(&workspace_root, &config)?;

    // Step 7: Check prerequisites
    check_prerequisites();

    eprintln!();
    eprintln!("Workspace ready! Next steps:");
    eprintln!(
        "  1. Edit {} to configure schedulers and workload",
        workspace::CONFIG_FILENAME
    );
    eprintln!("  2. Run `repm gen-config` to generate rt-app workload config");
    eprintln!("  3. Run `repm run` to execute experiments");
    eprintln!("  4. Run `repm analyze` to process results");
    eprintln!();
    eprintln!("Or run `repm magic` for guided workflow.");

    Ok(())
}

fn init_new_git(dir: &std::path::Path) -> Result<()> {
    let status = Command::new("git")
        .args(["init"])
        .current_dir(dir)
        .status()
        .context("Failed to run `git init`")?;

    if !status.success() {
        anyhow::bail!("`git init` failed with exit code: {}", status);
    }
    eprintln!("  initialized new git repo");
    Ok(())
}

fn verify_git_repo(dir: &std::path::Path) -> Result<()> {
    if !dir.join(".git").exists() && !dir.join(".git").is_file() {
        // Check if it's a worktree (has a .git file pointing to the real repo)
        let status = Command::new("git")
            .args(["rev-parse", "--git-dir"])
            .current_dir(dir)
            .status()
            .context("Failed to check git repo")?;
        if !status.success() {
            anyhow::bail!("{} is not a git repository", dir.display());
        }
    }
    Ok(())
}

fn is_inside_git_repo(dir: &std::path::Path) -> bool {
    Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(dir)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn check_prerequisites() {
    let checks = [
        ("vng", "virtme-ng (required for VM testing)"),
        ("rt-app", "rt-app (workload generator)"),
    ];

    eprintln!();
    eprintln!("  prerequisites:");
    for (cmd, desc) in &checks {
        let found = Command::new("which")
            .arg(cmd)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if found {
            eprintln!("    [OK] {}", desc);
        } else {
            eprintln!("    [--] {} (not found in PATH)", desc);
        }
    }
}
