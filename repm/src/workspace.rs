//! Workspace discovery and management.
//!
//! A workspace is identified by `repromagic_config.toml` at its root,
//! analogous to how `git` finds `.git/`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::RepromagicConfig;

/// The config filename that identifies a workspace root.
pub const CONFIG_FILENAME: &str = "repromagic_config.toml";

/// Standard workspace subdirectories.
pub const WORKSPACE_DIRS: &[&str] = &[
    "bin/schedulers",
    "configs",
    "traces",
    "experiments",
    "reports",
];

/// Find the workspace root by walking up from `start_dir` looking for
/// `repromagic_config.toml`. Returns `None` if not found.
pub fn find_workspace_root(start_dir: &Path) -> Option<PathBuf> {
    let mut dir = start_dir.to_path_buf();
    loop {
        if dir.join(CONFIG_FILENAME).is_file() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Load the workspace config from a directory containing `repromagic_config.toml`.
pub fn load_config(workspace_root: &Path) -> Result<RepromagicConfig> {
    let config_path = workspace_root.join(CONFIG_FILENAME);
    let content = std::fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read {}", config_path.display()))?;
    let config: RepromagicConfig = toml::from_str(&content)
        .with_context(|| format!("Failed to parse {}", config_path.display()))?;
    Ok(config)
}

/// Load workspace config from CWD (walks up to find root).
/// Returns (workspace_root, config).
pub fn load_config_from_cwd() -> Result<(PathBuf, RepromagicConfig)> {
    let cwd = std::env::current_dir().context("Failed to get current directory")?;
    let root = find_workspace_root(&cwd)
        .with_context(|| {
            format!(
                "No {} found in {} or any parent directory.\n\
                 Run `repm init --project-name NAME` to create a workspace.",
                CONFIG_FILENAME,
                cwd.display()
            )
        })?;
    let config = load_config(&root)?;
    Ok((root, config))
}

/// Create workspace directory structure under `root`.
pub fn create_workspace_dirs(root: &Path) -> Result<()> {
    for dir in WORKSPACE_DIRS {
        let path = root.join(dir);
        std::fs::create_dir_all(&path)
            .with_context(|| format!("Failed to create directory: {}", path.display()))?;
    }
    Ok(())
}

/// Write a .gitignore suitable for a repromagic workspace.
pub fn write_gitignore(root: &Path) -> Result<()> {
    let gitignore_path = root.join(".gitignore");
    if gitignore_path.exists() {
        // Append to existing .gitignore
        let existing = std::fs::read_to_string(&gitignore_path)?;
        if existing.contains("# repromagic") {
            return Ok(()); // Already has our section
        }
        let mut content = existing;
        if !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(&gitignore_content());
        std::fs::write(&gitignore_path, content)?;
    } else {
        std::fs::write(&gitignore_path, gitignore_content())?;
    }
    Ok(())
}

fn gitignore_content() -> String {
    r#"
# repromagic — auto-generated entries
/target/
/traces/*.perfetto
/traces/*.pb
*.log
/bin/schedulers/*
!/bin/schedulers/.gitkeep
"#
    .to_string()
}

/// Generate the default repromagic_config.toml content for a new project.
pub fn default_config_toml(project_name: &str) -> String {
    format!(
        r##"# repromagic_config.toml — workspace configuration
# Created by: repm init --project-name {name}

[project]
name = "{name}"
phenomenon = "bad_tail_latency"
# description = "Describe the scheduling phenomenon under investigation"

[defaults]
cores = 8                  # ~8 cores in 2 CCXs (enough for parallelism + multi-LLC dynamics)
duration = 30              # Experiment duration in seconds
reps = 3                   # Repetitions per cell
warmup = 5                 # Warmup exclusion period in seconds

[topology]
workload_cpus = "0-7"
# IRQ exposure is OPTIONAL — uncomment for networking/timer-heavy apps:
# irq_cpus = [0, 2, 4, 6]
# generator_cpus = "8-11"

[schedulers.eevdf]
name = "eevdf"
label = "EEVDF"
# No binary — kernel default, no sched_ext

# [schedulers.lavd_v1]
# name = "lavd"
# label = "LAVD-v1"
# provenance = {{ repo = "https://github.com/sched-ext/scx", revision = "abc1234" }}
# binary = "bin/schedulers/scx_lavd_v1_abc1234"
# flags = ["--performance"]

[workload]
foreground_threads = 4     # Threads with defined scheduling characteristics
background_threads = 16    # CPU pressure / hog threads
"##,
        name = project_name
    )
}
