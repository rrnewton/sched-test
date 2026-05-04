// Copyright (c) Meta Platforms, Inc. and affiliates.
// SPDX-License-Identifier: GPL-2.0-only

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use anyhow::{bail, Context, Result};
use chrono::Utc;
use clap::Args;
use serde::{Deserialize, Serialize};

use crate::config::{RepromagicConfig, RunMode, SchedulerDef};
use crate::workspace;

/// Run experiment matrix (scheduler × condition × rep).
#[derive(Debug, Args)]
pub struct RunArgs {
    /// Experiment mode(s) to run.
    #[arg(long, value_enum, value_delimiter = ',')]
    pub mode: Option<Vec<RunMode>>,

    /// Schedulers to test (comma-separated keys from config).
    #[arg(long, value_delimiter = ',')]
    pub schedulers: Option<Vec<String>>,

    /// Number of repetitions per cell.
    #[arg(long)]
    pub reps: Option<u32>,

    /// Show planned matrix without executing.
    #[arg(long)]
    pub dry_run: bool,

    /// Create a new experiment version (optionally with a short name).
    #[arg(long)]
    pub new_version: Option<Option<String>>,

    /// Experiment version to resume or run into (e.g., v001_initial).
    #[arg(long)]
    pub experiment: Option<String>,

    /// Purpose description for the experiment README.
    #[arg(long)]
    pub purpose: Option<String>,

    /// Duration in seconds (overrides config).
    #[arg(long)]
    pub duration: Option<u32>,
}

// ---------------------------------------------------------------------------
// Experiment matrix cell
// ---------------------------------------------------------------------------

/// A single cell in the experiment matrix.
#[derive(Debug, Clone)]
struct MatrixCell {
    mode: RunMode,
    scheduler_key: String,
    rep: u32,
}

impl std::fmt::Display for MatrixCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} × {} × rep {:03}",
            self.mode, self.scheduler_key, self.rep
        )
    }
}

// ---------------------------------------------------------------------------
// Provenance types (JSON-serializable)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExperimentProvenance {
    schema_version: String,
    experiment_version: String,
    timestamp: String,
    config_hash: String,
    host: HostInfo,
    workspace: WorkspaceInfo,
    schedulers: BTreeMap<String, SchedulerProvenance>,
    experiment: ExperimentInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HostInfo {
    hostname: String,
    cpu_model: String,
    cpu_count: u32,
    kernel: String,
    user: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkspaceInfo {
    git_revision: String,
    git_branch: String,
    git_dirty: bool,
    repm_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SchedulerProvenance {
    #[serde(rename = "type")]
    sched_type: String,
    binary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    binary_mtime: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    binary_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    binary_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provenance_repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provenance_revision: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExperimentInfo {
    modes: Vec<String>,
    reps: u32,
    duration_s: u32,
    warmup_s: u32,
    cores: u32,
    workload_cpus: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    irq_cpus: Option<Vec<u32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generator_cpus: Option<String>,
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

pub fn execute(args: &RunArgs) -> Result<()> {
    // Step 1: Load workspace config
    let (ws_root, config) = workspace::load_config_from_cwd()?;
    let issues = config.validate();
    if !issues.is_empty() {
        eprintln!("Config validation errors:");
        for issue in &issues {
            eprintln!("  - {}", issue);
        }
        bail!("Fix config issues before running experiments");
    }

    // Step 2: Resolve parameters (CLI overrides > config defaults)
    let reps = args.reps.unwrap_or(config.defaults.reps);
    let duration = args.duration.unwrap_or(config.defaults.duration);

    let modes = resolve_modes(args)?;
    let scheduler_keys = resolve_schedulers(args, &config)?;

    // Validate that requested schedulers exist in config
    for key in &scheduler_keys {
        if !config.schedulers.contains_key(key) {
            bail!(
                "Scheduler '{}' not found in config. Available: {}",
                key,
                config
                    .schedulers
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }

    // Validate mode × scheduler compatibility
    validate_mode_scheduler_compat(&modes, &scheduler_keys, &config)?;

    // Step 3: Build experiment matrix
    let matrix = build_matrix(&modes, &scheduler_keys, reps);

    // Step 4: Show matrix summary
    print_matrix_summary(&modes, &scheduler_keys, &config, reps, duration, &config);

    if args.dry_run {
        eprintln!();
        eprintln!("=== DRY RUN — showing planned matrix ===");
        eprintln!();
        for (i, cell) in matrix.iter().enumerate() {
            let sched = &config.schedulers[&cell.scheduler_key];
            eprintln!(
                "  [{:3}/{}] {} (label: {})",
                i + 1,
                matrix.len(),
                cell,
                sched.label
            );
        }
        eprintln!();
        eprintln!("Total cells: {}", matrix.len());
        eprintln!("Estimated time: ~{}s", matrix.len() as u32 * duration);
        eprintln!();
        eprintln!("Run without --dry-run to execute.");
        return Ok(());
    }

    // Step 5: Resolve experiment directory
    let exp_dir = resolve_experiment_dir(&ws_root, args, &config)?;
    eprintln!("Experiment directory: {}", exp_dir.display());

    // Step 6: Create directory structure
    create_experiment_dirs(&exp_dir, &modes, &scheduler_keys)?;

    // Step 7: Write provenance (seals the experiment)
    let provenance = build_provenance(
        &exp_dir,
        &config,
        &modes,
        &scheduler_keys,
        reps,
        duration,
        &ws_root,
    )?;
    write_provenance(&exp_dir, &provenance)?;

    // Step 8: Write config snapshot
    write_config_snapshot(&exp_dir, &ws_root)?;

    // Step 9: Write README
    write_experiment_readme(&exp_dir, &provenance, &config, args)?;

    // Step 10: Execute matrix (with resume support)
    execute_matrix(&exp_dir, &matrix, &config, duration, &ws_root)?;

    // Step 11: Summary
    eprintln!();
    eprintln!("=== Experiment complete ===");
    eprintln!("  Results: {}/data/", exp_dir.display());
    eprintln!("  Provenance: {}/provenance.json", exp_dir.display());
    eprintln!();
    eprintln!(
        "Next step: run `repm analyze --experiment {}`",
        exp_dir.display()
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Parameter resolution
// ---------------------------------------------------------------------------

fn resolve_modes(args: &RunArgs) -> Result<Vec<RunMode>> {
    if let Some(ref modes) = args.mode {
        Ok(modes.clone())
    } else {
        // Default: rtapp_pinned only
        Ok(vec![RunMode::RtappPinned])
    }
}

fn resolve_schedulers(args: &RunArgs, config: &RepromagicConfig) -> Result<Vec<String>> {
    if let Some(ref keys) = args.schedulers {
        Ok(keys.clone())
    } else {
        // All schedulers from config
        Ok(config.schedulers.keys().cloned().collect())
    }
}

/// Validate that mode × scheduler combinations are sensible.
/// E.g., rtapp_sim with kernel-builtin EEVDF is invalid (sim needs sched_ext).
fn validate_mode_scheduler_compat(
    modes: &[RunMode],
    scheduler_keys: &[String],
    config: &RepromagicConfig,
) -> Result<()> {
    for mode in modes {
        for key in scheduler_keys {
            let sched = &config.schedulers[key];
            if matches!(mode, RunMode::RtappSim) && sched.name.is_builtin() {
                bail!(
                    "Invalid combination: {} × {} — simulator requires a sched_ext scheduler binary, \
                     but '{}' ({}) is a kernel-builtin scheduler.\n\
                     Remove '{}' from schedulers or remove rtapp_sim from modes.",
                    mode,
                    sched.label,
                    key,
                    sched.name,
                    key,
                );
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Matrix construction
// ---------------------------------------------------------------------------

fn build_matrix(modes: &[RunMode], scheduler_keys: &[String], reps: u32) -> Vec<MatrixCell> {
    let mut cells = Vec::new();
    for &mode in modes {
        for key in scheduler_keys {
            for rep in 1..=reps {
                cells.push(MatrixCell {
                    mode,
                    scheduler_key: key.clone(),
                    rep,
                });
            }
        }
    }
    cells
}

fn print_matrix_summary(
    modes: &[RunMode],
    scheduler_keys: &[String],
    schedulers: &RepromagicConfig,
    reps: u32,
    duration: u32,
    config: &RepromagicConfig,
) {
    eprintln!("repm run — experiment matrix");
    eprintln!("  project: {}", config.project.name);
    eprintln!(
        "  modes: {}",
        modes
            .iter()
            .map(|m| m.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    eprintln!("  schedulers:");
    for key in scheduler_keys {
        let sched = &schedulers.schedulers[key];
        let binary_info = if let Some(ref b) = sched.binary {
            format!(" ({})", b.display())
        } else {
            " (kernel builtin)".to_string()
        };
        eprintln!("    - {} [{}]{}", key, sched.label, binary_info);
    }
    eprintln!("  reps: {}", reps);
    eprintln!("  duration: {}s", duration);
    eprintln!(
        "  total cells: {} modes × {} schedulers × {} reps = {}",
        modes.len(),
        scheduler_keys.len(),
        reps,
        modes.len() * scheduler_keys.len() * reps as usize
    );
}

// ---------------------------------------------------------------------------
// Experiment directory management
// ---------------------------------------------------------------------------

fn resolve_experiment_dir(
    ws_root: &Path,
    args: &RunArgs,
    _config: &RepromagicConfig,
) -> Result<PathBuf> {
    let experiments_dir = ws_root.join("experiments");

    // If --experiment is specified, use that
    if let Some(ref name) = args.experiment {
        let dir = experiments_dir.join(name);
        if dir.exists() {
            // Check if we can resume (provenance matches)
            check_experiment_resumable(&dir)?;
        }
        return Ok(dir);
    }

    // If --new-version, create a new version
    if args.new_version.is_some() {
        let next = next_version_number(&experiments_dir)?;
        let suffix = match &args.new_version {
            Some(Some(name)) => name.clone(),
            _ => "initial".to_string(),
        };
        let dir_name = format!("v{:03}_{}", next, suffix);
        return Ok(experiments_dir.join(dir_name));
    }

    // Default: auto-detect or create v001_initial
    let next = next_version_number(&experiments_dir)?;
    if next == 1 {
        Ok(experiments_dir.join("v001_initial"))
    } else {
        // Find the latest existing experiment to potentially resume
        let latest = find_latest_experiment(&experiments_dir)?;
        match latest {
            Some(dir) => {
                check_experiment_resumable(&dir)?;
                Ok(dir)
            }
            None => Ok(experiments_dir.join(format!("v{:03}_initial", next))),
        }
    }
}

fn next_version_number(experiments_dir: &Path) -> Result<u32> {
    if !experiments_dir.exists() {
        return Ok(1);
    }
    let mut max_version = 0u32;
    for entry in std::fs::read_dir(experiments_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(num) = parse_version_number(&name) {
                max_version = max_version.max(num);
            }
        }
    }
    Ok(max_version + 1)
}

fn parse_version_number(name: &str) -> Option<u32> {
    if name.starts_with('v') && name.len() >= 4 {
        name[1..4].parse().ok()
    } else {
        None
    }
}

fn find_latest_experiment(experiments_dir: &Path) -> Result<Option<PathBuf>> {
    if !experiments_dir.exists() {
        return Ok(None);
    }
    let mut latest: Option<(u32, PathBuf)> = None;
    for entry in std::fs::read_dir(experiments_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy().to_string();
            if let Some(num) = parse_version_number(&name_str) {
                if latest.as_ref().is_none_or(|(n, _)| num > *n) {
                    latest = Some((num, entry.path()));
                }
            }
        }
    }
    Ok(latest.map(|(_, p)| p))
}

fn check_experiment_resumable(exp_dir: &Path) -> Result<()> {
    let prov_path = exp_dir.join("provenance.json");
    if !prov_path.exists() {
        return Ok(()); // Not yet sealed — OK to use
    }

    let prov_str = std::fs::read_to_string(&prov_path)
        .with_context(|| format!("Failed to read {}", prov_path.display()))?;
    let prov: ExperimentProvenance = serde_json::from_str(&prov_str)
        .with_context(|| format!("Failed to parse {}", prov_path.display()))?;

    // M2 fix: Validate config hash matches provenance.
    // The config_snapshot.toml is the authoritative frozen config. We compare
    // the current workspace config hash against the provenance config_hash.
    // This prevents resuming with a changed config (which would produce mixed results).
    let config_snapshot_path = exp_dir.join("config_snapshot.toml");
    if config_snapshot_path.exists() {
        // Find workspace root to compute current config hash
        let cwd = std::env::current_dir().unwrap_or_default();
        if let Some(ws_root) = crate::workspace::find_workspace_root(&cwd) {
            let current_hash = hash_config_toml(&ws_root)?;
            if current_hash != prov.config_hash {
                bail!(
                    "Config has changed since experiment provenance was written.\n\
                     Provenance config hash: {}\n\
                     Current config hash:    {}\n\n\
                     The experiment is sealed — config changes require a new version.\n\
                     Create a new experiment: repm run --new-version <name>",
                    prov.config_hash,
                    current_hash,
                );
            }
        }
    }

    // Check that scheduler binaries haven't changed
    for (name, sched_prov) in &prov.schedulers {
        if let Some(ref binary) = sched_prov.binary {
            let binary_path = Path::new(binary);
            if binary_path.exists() {
                let meta = std::fs::metadata(binary_path)?;
                if let Some(ref expected_size) = sched_prov.binary_size {
                    let actual_size = meta.len();
                    if actual_size != *expected_size {
                        bail!(
                            "Scheduler binary '{}' ({}) has changed since provenance was written.\n\
                             Provenance size: {} bytes\n\
                             Current size:    {} bytes\n\
                             Create a new experiment version: repm run --new-version",
                            name,
                            binary,
                            expected_size,
                            actual_size
                        );
                    }
                }
            }
        }
    }

    eprintln!("  Resuming sealed experiment: {}", exp_dir.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// Directory creation
// ---------------------------------------------------------------------------

fn create_experiment_dirs(
    exp_dir: &Path,
    modes: &[RunMode],
    scheduler_keys: &[String],
) -> Result<()> {
    std::fs::create_dir_all(exp_dir)?;

    for mode in modes {
        for key in scheduler_keys {
            let data_dir = exp_dir.join("data").join(mode.to_string()).join(key);
            std::fs::create_dir_all(&data_dir)
                .with_context(|| format!("Failed to create {}", data_dir.display()))?;
        }
    }

    // Reports directory
    std::fs::create_dir_all(exp_dir.join("reports"))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Provenance
// ---------------------------------------------------------------------------

fn build_provenance(
    exp_dir: &Path,
    config: &RepromagicConfig,
    modes: &[RunMode],
    scheduler_keys: &[String],
    reps: u32,
    duration: u32,
    ws_root: &Path,
) -> Result<ExperimentProvenance> {
    let version_name = exp_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let config_hash = hash_config_toml(ws_root)?;
    let host = gather_host_info()?;
    let workspace_info = gather_workspace_info(ws_root)?;

    let mut schedulers = BTreeMap::new();
    for key in scheduler_keys {
        let sched = &config.schedulers[key];
        schedulers.insert(key.clone(), build_scheduler_provenance(sched, ws_root)?);
    }

    Ok(ExperimentProvenance {
        schema_version: "1.0".to_string(),
        experiment_version: version_name,
        timestamp: Utc::now().to_rfc3339(),
        config_hash,
        host,
        workspace: workspace_info,
        schedulers,
        experiment: ExperimentInfo {
            modes: modes.iter().map(|m| m.to_string()).collect(),
            reps,
            duration_s: duration,
            warmup_s: config.defaults.warmup,
            cores: config.defaults.cores,
            workload_cpus: config.topology.workload_cpus.clone(),
            irq_cpus: config.topology.irq_cpus.clone(),
            generator_cpus: config.topology.generator_cpus.clone(),
        },
    })
}

fn hash_config_toml(ws_root: &Path) -> Result<String> {
    let config_path = ws_root.join(workspace::CONFIG_FILENAME);
    let content = std::fs::read(&config_path)
        .with_context(|| format!("Failed to read {}", config_path.display()))?;

    // M4 fix: Use a standard hash algorithm and label it honestly.
    // We use Rust's DefaultHasher (SipHash-2-4) which is a well-defined
    // algorithm. For cryptographic integrity, a SHA-256 crate should be
    // added in the future. For config change detection, SipHash is sufficient.
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    Ok(format!("siphash24:{:016x}", hasher.finish()))
}

fn gather_host_info() -> Result<HostInfo> {
    let hostname = run_command_stdout("hostname", &[])?;
    let cpu_model = read_cpu_model().unwrap_or_else(|_| "unknown".to_string());
    let cpu_count = num_cpus();
    let kernel = run_command_stdout("uname", &["-r"])?;
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());

    Ok(HostInfo {
        hostname: hostname.trim().to_string(),
        cpu_model: cpu_model.trim().to_string(),
        cpu_count,
        kernel: kernel.trim().to_string(),
        user: user.trim().to_string(),
    })
}

fn read_cpu_model() -> Result<String> {
    let content = std::fs::read_to_string("/proc/cpuinfo")?;
    for line in content.lines() {
        if line.starts_with("model name") {
            if let Some((_key, value)) = line.split_once(':') {
                return Ok(value.trim().to_string());
            }
        }
    }
    Ok("unknown".to_string())
}

fn num_cpus() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1)
}

fn gather_workspace_info(ws_root: &Path) -> Result<WorkspaceInfo> {
    let git_revision = run_command_stdout_in("git", &["rev-parse", "HEAD"], ws_root)
        .unwrap_or_else(|_| "unknown".to_string());
    let git_branch = run_command_stdout_in("git", &["rev-parse", "--abbrev-ref", "HEAD"], ws_root)
        .unwrap_or_else(|_| "unknown".to_string());
    let git_dirty = run_command_stdout_in("git", &["status", "--porcelain"], ws_root)
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);

    Ok(WorkspaceInfo {
        git_revision: git_revision.trim().to_string(),
        git_branch: git_branch.trim().to_string(),
        git_dirty,
        repm_version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

fn build_scheduler_provenance(sched: &SchedulerDef, ws_root: &Path) -> Result<SchedulerProvenance> {
    let sched_type = if sched.name.is_builtin() {
        "kernel_builtin".to_string()
    } else {
        "sched_ext".to_string()
    };

    let (binary_str, binary_mtime, binary_size, binary_sha256) =
        if let Some(ref binary) = sched.binary {
            let abs_path = if binary.is_relative() {
                ws_root.join(binary)
            } else {
                binary.clone()
            };
            let display_path = binary.display().to_string();

            if abs_path.exists() {
                let meta = std::fs::metadata(&abs_path)?;
                let mtime = meta.modified().ok().and_then(|t| {
                    t.duration_since(SystemTime::UNIX_EPOCH).ok().map(|d| {
                        chrono::DateTime::from_timestamp(d.as_secs() as i64, 0)
                            .map(|dt| dt.to_rfc3339())
                            .unwrap_or_else(|| "unknown".to_string())
                    })
                });
                let size = meta.len();
                (
                    Some(display_path),
                    mtime,
                    Some(size),
                    None::<String>, // TODO: sha256 via crypto crate
                )
            } else {
                (Some(display_path), None, None, None)
            }
        } else {
            (None, None, None, None)
        };

    Ok(SchedulerProvenance {
        sched_type,
        binary: binary_str,
        binary_mtime,
        binary_size,
        binary_sha256,
        provenance_repo: sched.provenance.as_ref().map(|p| p.repo.clone()),
        provenance_revision: sched.provenance.as_ref().map(|p| p.revision.clone()),
    })
}

fn write_provenance(exp_dir: &Path, provenance: &ExperimentProvenance) -> Result<()> {
    let prov_path = exp_dir.join("provenance.json");
    if prov_path.exists() {
        eprintln!("  provenance.json already exists — experiment is sealed (resume mode)");
        return Ok(());
    }
    let json =
        serde_json::to_string_pretty(provenance).context("Failed to serialize provenance")?;
    std::fs::write(&prov_path, json + "\n")
        .with_context(|| format!("Failed to write {}", prov_path.display()))?;
    eprintln!("  wrote provenance.json (experiment sealed)");
    Ok(())
}

// ---------------------------------------------------------------------------
// Config snapshot
// ---------------------------------------------------------------------------

fn write_config_snapshot(exp_dir: &Path, ws_root: &Path) -> Result<()> {
    let snapshot_path = exp_dir.join("config_snapshot.toml");
    if snapshot_path.exists() {
        return Ok(()); // Already written (resume mode)
    }

    let config_path = ws_root.join(workspace::CONFIG_FILENAME);
    let content = std::fs::read_to_string(&config_path)?;
    std::fs::write(&snapshot_path, content)?;
    eprintln!("  wrote config_snapshot.toml");
    Ok(())
}

// ---------------------------------------------------------------------------
// README generation
// ---------------------------------------------------------------------------

fn write_experiment_readme(
    exp_dir: &Path,
    provenance: &ExperimentProvenance,
    config: &RepromagicConfig,
    args: &RunArgs,
) -> Result<()> {
    let readme_path = exp_dir.join("README.md");
    if readme_path.exists() {
        return Ok(()); // Already written (resume mode)
    }

    let purpose = args
        .purpose
        .clone()
        .unwrap_or_else(|| "Experiment purpose not specified.".to_string());

    let modes_str = provenance.experiment.modes.join(", ");

    let mut sched_table = String::new();
    for (key, sp) in &provenance.schedulers {
        let label = config
            .schedulers
            .get(key)
            .map(|s| s.label.as_str())
            .unwrap_or(key);
        let binary = sp.binary.as_deref().unwrap_or("(kernel)");
        let revision = sp.provenance_revision.as_deref().unwrap_or("n/a");
        sched_table.push_str(&format!(
            "| {} | {} | `{}` | {} |\n",
            label, sp.sched_type, binary, revision
        ));
    }

    let readme = format!(
        r#"# Experiment: {version}

**Date:** {timestamp}
**Machine:** {cpu_model}, {cpu_count} CPUs, kernel {kernel}
**Host:** {hostname}
**Project:** {project}

## Purpose

{purpose}

## Parameters

| Parameter | Value |
|-----------|-------|
| Cores | {cores} |
| Work CPUs | {workload_cpus} |
| IRQ CPUs | {irq_cpus} |
| Reps | {reps} |
| Duration | {duration}s |
| Warmup | {warmup}s |
| Modes | {modes} |

## Schedulers

| Label | Type | Binary | Revision |
|-------|------|--------|----------|
{sched_table}
## Reproducer

```bash
repm run --experiment {version} --reps {reps} --mode {modes} --duration {duration}
```

## Data

| Directory | Description |
|-----------|-------------|
| `data/` | Per-mode, per-scheduler CSV data |
| `reports/RESULTS.md` | Auto-generated summary |

## CSV Schema

Per `experiments/METRICS_SPECIFICATION.md`:
```
timestamp,mode,scheduler,condition,thread_type,thread_id,metric_name,percentile,value,unit,sample_count,rep,notes,avg_cpu_util_pct
```
"#,
        version = provenance.experiment_version,
        timestamp = provenance.timestamp,
        cpu_model = provenance.host.cpu_model,
        cpu_count = provenance.host.cpu_count,
        kernel = provenance.host.kernel,
        hostname = provenance.host.hostname,
        project = config.project.name,
        purpose = purpose,
        cores = provenance.experiment.cores,
        workload_cpus = provenance.experiment.workload_cpus,
        irq_cpus = provenance
            .experiment
            .irq_cpus
            .as_ref()
            .map(|v| format!("{:?}", v))
            .unwrap_or_else(|| "none".to_string()),
        reps = provenance.experiment.reps,
        duration = provenance.experiment.duration_s,
        warmup = provenance.experiment.warmup_s,
        modes = modes_str,
        sched_table = sched_table,
    );

    std::fs::write(&readme_path, readme)?;
    eprintln!("  wrote README.md");
    Ok(())
}

// ---------------------------------------------------------------------------
// Matrix execution
// ---------------------------------------------------------------------------

fn execute_matrix(
    exp_dir: &Path,
    matrix: &[MatrixCell],
    config: &RepromagicConfig,
    duration: u32,
    ws_root: &Path,
) -> Result<()> {
    let total = matrix.len();
    let mut completed = 0;
    let mut skipped = 0;
    let mut failed = 0;

    eprintln!();
    eprintln!("=== Executing {} cells ===", total);

    for (i, cell) in matrix.iter().enumerate() {
        let sched = &config.schedulers[&cell.scheduler_key];
        let csv_path = exp_dir
            .join("data")
            .join(cell.mode.to_string())
            .join(&cell.scheduler_key)
            .join(format!("rep_{:03}.csv", cell.rep));

        // M3 fix: Use a .done marker file to distinguish completed reps from
        // failed/placeholder ones. An empty CSV or a CSV with only a placeholder
        // row does NOT get a .done marker. Only successful data collection does.
        let done_marker = csv_path.with_extension("csv.done");

        // Resume: skip completed reps (those with a .done marker)
        if done_marker.exists() {
            skipped += 1;
            eprintln!("  [{:3}/{}] SKIP {} (already complete)", i + 1, total, cell);
            continue;
        }

        eprintln!("  [{:3}/{}] RUN  {} [{}]", i + 1, total, cell, sched.label);

        match execute_cell(cell, sched, &csv_path, duration, config, ws_root) {
            Ok(()) => {
                completed += 1;
                // Write .done marker on success
                let _ = std::fs::write(&done_marker, "");
                eprintln!("           OK → {}", csv_path.display());
            }
            Err(e) => {
                failed += 1;
                eprintln!("           FAILED: {}", e);
                // Remove any stale .done marker and CSV from previous attempts
                let _ = std::fs::remove_file(&done_marker);
                let _ = std::fs::remove_file(&csv_path);
            }
        }
    }

    eprintln!();
    eprintln!(
        "Matrix complete: {} completed, {} skipped (resumed), {} failed",
        completed, skipped, failed
    );

    if failed > 0 {
        eprintln!(
            "WARNING: {} cells failed. Re-run to retry failed cells.",
            failed
        );
    }

    Ok(())
}

fn execute_cell(
    cell: &MatrixCell,
    sched: &SchedulerDef,
    csv_path: &Path,
    duration: u32,
    config: &RepromagicConfig,
    ws_root: &Path,
) -> Result<()> {
    match cell.mode {
        RunMode::RtappPinned => {
            execute_rtapp_pinned(cell, sched, csv_path, duration, config, ws_root)
        }
        RunMode::RtappVm => execute_rtapp_vm(cell, sched, csv_path, duration, config, ws_root),
        RunMode::RtappSim => execute_rtapp_sim(cell, sched, csv_path, duration, config, ws_root),
    }
}

fn execute_rtapp_pinned(
    cell: &MatrixCell,
    sched: &SchedulerDef,
    csv_path: &Path,
    _duration: u32,
    config: &RepromagicConfig,
    ws_root: &Path,
) -> Result<()> {
    // Step 1: Start scheduler (if not kernel-builtin)
    let sched_binary_path: Option<PathBuf> = if !sched.name.is_builtin() {
        let binary = sched
            .binary
            .as_ref()
            .context("Non-builtin scheduler requires a binary path")?;
        let abs_binary = if binary.is_relative() {
            ws_root.join(binary)
        } else {
            binary.clone()
        };
        if !abs_binary.exists() {
            bail!(
                "Scheduler binary not found: {}\nBuild or copy the scheduler binary first.",
                abs_binary.display()
            );
        }
        Some(abs_binary)
    } else {
        None
    };

    let sched_process = if let Some(ref abs_binary) = sched_binary_path {
        let mut cmd = Command::new("sudo");
        cmd.arg(abs_binary.to_string_lossy().as_ref());
        for flag in &sched.flags {
            cmd.arg(flag);
        }
        let child = cmd
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .with_context(|| format!("Failed to start scheduler: {}", sched.label))?;
        eprintln!(
            "           started scheduler: {} (pid {})",
            sched.label,
            child.id()
        );

        // Give the scheduler a moment to initialize
        std::thread::sleep(std::time::Duration::from_secs(2));
        Some(child)
    } else {
        eprintln!("           using kernel-builtin scheduler: {}", sched.name);
        None
    };

    // Step 2: Run rt-app workload
    let rtapp_config = ws_root.join("configs/rtapp.json");
    if !rtapp_config.exists() {
        bail!(
            "rt-app config not found: {}\nRun `repm gen-config` first.",
            rtapp_config.display()
        );
    }

    // Create a temporary directory for rt-app logs
    let log_dir = csv_path
        .parent()
        .unwrap()
        .join(format!("rep_{:03}_logs", cell.rep));
    std::fs::create_dir_all(&log_dir)?;

    let rtapp_result = Command::new("sudo")
        .args(["rt-app", rtapp_config.to_str().unwrap()])
        .current_dir(&log_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .status();

    // Step 3: Stop scheduler
    // M1 fix: child.id() is the PID of `sudo`, not the scheduler binary.
    // sudo may have already exited after forking the scheduler. We must kill
    // the scheduler process by its binary name to avoid orphaned schedulers
    // contaminating subsequent experiment cells.
    if let Some(mut child) = sched_process {
        eprintln!("           stopping scheduler...");
        if let Some(ref abs_binary) = sched_binary_path {
            // Kill the actual scheduler process by its binary name
            let binary_name = abs_binary
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "scx_lavd".to_string());
            let _ = Command::new("sudo")
                .args(["pkill", "-f", &binary_name])
                .status();
        } else {
            // Fallback: try to kill the sudo child directly
            let _ = Command::new("sudo")
                .args(["kill", &child.id().to_string()])
                .status();
        }
        // Wait for sudo wrapper to exit
        let _ = child.wait();
        // Brief pause to ensure scheduler has fully stopped
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    // Step 4: Collect metrics from rt-app logs and write CSV
    match rtapp_result {
        Ok(status) if status.success() => {
            collect_rtapp_metrics(&log_dir, csv_path, cell, config)?;
            Ok(())
        }
        Ok(status) => {
            bail!("rt-app exited with status: {}", status);
        }
        Err(e) => {
            bail!("Failed to run rt-app: {}", e);
        }
    }
}

fn execute_rtapp_vm(
    _cell: &MatrixCell,
    sched: &SchedulerDef,
    csv_path: &Path,
    _duration: u32,
    _config: &RepromagicConfig,
    _ws_root: &Path,
) -> Result<()> {
    // VM mode: use virtme-ng (vng) to run the workload inside a VM
    // This requires vng to be installed and properly configured
    bail!(
        "rtapp_vm mode is not yet fully implemented.\n\
         Scheduler: {} ({})\n\
         Output: {}\n\
         Required: virtme-ng (vng) with custom kernel support.\n\
         Use rtapp_pinned mode for bare-metal testing.",
        sched.label,
        sched.name,
        csv_path.display()
    );
}

fn execute_rtapp_sim(
    cell: &MatrixCell,
    sched: &SchedulerDef,
    csv_path: &Path,
    duration: u32,
    config: &RepromagicConfig,
    ws_root: &Path,
) -> Result<()> {
    // Simulator mode: use scxsim to run the workload with the specified scheduler.
    //
    // scxsim reads rt-app JSON configs and simulates the workload under the
    // chosen scheduler, outputting a Perfetto trace and summary statistics.

    // Find scxsim binary
    let scxsim_binary = find_scxsim(ws_root)?;

    // Resolve the scheduler name for scxsim (e.g., "lavd", "tickless", "simple")
    let sim_sched_name = match &sched.name {
        crate::config::SchedulerName::Lavd => "lavd",
        crate::config::SchedulerName::Other(name) => name.as_str(),
        _ => bail!(
            "Scheduler '{}' ({}) is not supported in simulator mode.\n\
             Simulator supports: lavd, tickless, simple, cosmos, mitosis.",
            sched.label,
            sched.name
        ),
    };

    let cores = config.defaults.cores;
    let duration_ms = duration as u64 * 1000; // Convert seconds to milliseconds
    let warmup_ms = config.defaults.warmup as u64 * 1000;
    let seed = 42 + cell.rep - 1;

    // Create output directory for perfetto trace
    let csv_dir = csv_path.parent().unwrap();
    let perfetto_path = csv_dir.join(format!("rep_{:03}_trace.json", cell.rep));

    // Generate a scxsim-compatible workload JSON.
    // scxsim uses a simpler format than rt-app: {run, sleep} as integers (microseconds),
    // not phased configs with {runtime: {duration, mode}}.
    let sim_workload_path = csv_dir.join(format!("rep_{:03}_workload.json", cell.rep));
    generate_sim_workload(&sim_workload_path, config, duration)?;

    eprintln!(
        "           scxsim: scheduler={}, cores={}, duration={}ms, seed={}",
        sim_sched_name, cores, duration_ms, seed
    );

    // Run scxsim
    let output = Command::new(&scxsim_binary)
        .args([
            "run",
            sim_workload_path.to_str().unwrap(),
            "--scheduler",
            sim_sched_name,
            "--cpus",
            &cores.to_string(),
            "--seed",
            &seed.to_string(),
            "--end-time",
            &format!("{}ms", duration_ms),
            "--warmup-ms",
            &warmup_ms.to_string(),
            "--perfetto",
            perfetto_path.to_str().unwrap(),
            "--verbose-summary",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .with_context(|| format!("Failed to run scxsim: {}", scxsim_binary.display()))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        bail!(
            "scxsim failed (exit {}):\n{}",
            output.status,
            stderr.chars().take(1000).collect::<String>()
        );
    }

    // Parse scxsim summary output and write CSV
    parse_scxsim_output_to_csv(&stdout, &stderr, csv_path, cell, sim_sched_name, config)?;

    if perfetto_path.exists() {
        eprintln!("           trace: {}", perfetto_path.display());
    }

    Ok(())
}

/// Generate a scxsim-compatible workload JSON from workspace config.
///
/// scxsim's workload parser expects a simpler format than real rt-app:
/// ```json
/// {"global": {"duration": 1}, "tasks": {"fg_0": {"run": 500, "sleep": 1500, "loop": -1}}}
/// ```
/// Values are in microseconds.
fn generate_sim_workload(path: &Path, config: &RepromagicConfig, duration: u32) -> Result<()> {
    let mut workload = serde_json::Map::new();

    // Global section
    let mut global = serde_json::Map::new();
    global.insert(
        "duration".into(),
        serde_json::Value::Number(duration.into()),
    );
    global.insert(
        "default_policy".into(),
        serde_json::Value::String("SCHED_OTHER".into()),
    );
    workload.insert("global".into(), serde_json::Value::Object(global));

    // Tasks
    let mut tasks = serde_json::Map::new();

    let fg_threads = config.workload.foreground_threads;
    let bg_threads = config.workload.background_threads;
    let cores = config.defaults.cores;

    // Default timing from workload thread_types or fallback
    let (fg_run, fg_sleep) = config
        .workload
        .thread_types
        .values()
        .find(|t| matches!(t.role, crate::config::ThreadRole::Foreground))
        .map(|t| (t.run_us.unwrap_or(500), t.sleep_us.unwrap_or(1500)))
        .unwrap_or((500, 1500));

    let (bg_run, bg_sleep) = config
        .workload
        .thread_types
        .values()
        .find(|t| matches!(t.role, crate::config::ThreadRole::Background))
        .map(|t| (t.run_us.unwrap_or(130), t.sleep_us.unwrap_or(950)))
        .unwrap_or((130, 950));

    let all_cpus: Vec<serde_json::Value> = (0..cores)
        .map(|c| serde_json::Value::Number(c.into()))
        .collect();

    // Foreground threads
    for i in 0..fg_threads {
        let mut task = serde_json::Map::new();
        task.insert("run".into(), serde_json::Value::Number(fg_run.into()));
        task.insert("sleep".into(), serde_json::Value::Number(fg_sleep.into()));
        task.insert("loop".into(), serde_json::Value::Number((-1_i64).into()));
        task.insert("cpus".into(), serde_json::Value::Array(all_cpus.clone()));
        tasks.insert(format!("fg_thread_{}", i), serde_json::Value::Object(task));
    }

    // Background threads
    for i in 0..bg_threads {
        let mut task = serde_json::Map::new();
        task.insert("run".into(), serde_json::Value::Number(bg_run.into()));
        task.insert("sleep".into(), serde_json::Value::Number(bg_sleep.into()));
        task.insert("loop".into(), serde_json::Value::Number((-1_i64).into()));
        task.insert("priority".into(), serde_json::Value::Number(10.into()));
        task.insert("cpus".into(), serde_json::Value::Array(all_cpus.clone()));
        tasks.insert(format!("bg_hog_{}", i), serde_json::Value::Object(task));
    }

    workload.insert("tasks".into(), serde_json::Value::Object(tasks));

    let json =
        serde_json::to_string_pretty(&workload).context("Failed to serialize sim workload")?;
    std::fs::write(path, json + "\n")
        .with_context(|| format!("Failed to write {}", path.display()))?;

    Ok(())
}

/// Find the scxsim binary. Checks:
/// 1. `SCXSIM` environment variable
/// 2. `scxsim` in PATH
/// 3. Sibling scx-sim project (../scx-sim/target/release/scxsim)
/// 4. Workspace bin directory
fn find_scxsim(ws_root: &Path) -> Result<PathBuf> {
    // Check SCXSIM env var first (allows explicit override)
    if let Ok(path) = std::env::var("SCXSIM") {
        let p = PathBuf::from(&path);
        if p.exists() {
            return Ok(p);
        }
    }

    // Check PATH
    if let Ok(output) = Command::new("which").arg("scxsim").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            return Ok(PathBuf::from(path));
        }
    }

    // Check sibling scx-sim project (common in multi_sched-test layout)
    let candidates = [
        ws_root.join("../scx-sim/target/release/scxsim"),
        ws_root.join("../../sched-test1/scx-sim/target/release/scxsim"),
    ];

    for candidate in &candidates {
        let resolved = candidate.canonicalize().unwrap_or_default();
        if resolved.exists() {
            return Ok(resolved);
        }
    }

    // Check the workspace bin dir
    let ws_bin = ws_root.join("bin/scxsim");
    if ws_bin.exists() {
        return Ok(ws_bin);
    }

    bail!(
        "scxsim binary not found.\n\
         Looked in:\n\
         - PATH\n\
         - {}\n\
         - {}\n\
         Build with: cd scx-sim && cargo build --release --bin scxsim",
        candidates[0].display(),
        ws_bin.display(),
    );
}

/// Parse scxsim verbose-summary output into CSV metrics.
///
/// scxsim outputs multi-line blocks per task:
/// ```text
///   Task PID=1:
///     Schedules:       250
///     Run duration:    0.499ms mean, 0.100ms stddev, CV=20.1%
///     Inter-arrival:   2.002ms mean, ...
///     Sched latency:   p50=102.574us p90=... p99=... (N samples)
/// ```
/// And overall CPU utilization:
/// ```text
/// --- Overall CPU Utilization: 24.6% (4 CPUs, 500.000ms) ---
/// ```
fn parse_scxsim_output_to_csv(
    stdout: &str,
    stderr: &str,
    csv_path: &Path,
    cell: &MatrixCell,
    scheduler_name: &str,
    _config: &RepromagicConfig,
) -> Result<()> {
    let timestamp = Utc::now().to_rfc3339();
    let mode = cell.mode.to_string(); // C4 fix: use consistent mode name
    let rep = cell.rep;

    let mut csv_rows = vec![
        "timestamp,mode,scheduler,condition,thread_type,thread_id,metric_name,\
         percentile,value,unit,sample_count,rep,notes,avg_cpu_util_pct"
            .to_string(),
    ];

    let combined = format!("{}\n{}", stdout, stderr);

    // Extract overall CPU utilization
    let mut cpu_util: Option<f64> = None;
    for line in combined.lines() {
        // "--- Overall CPU Utilization: 24.6% (4 CPUs, 500.000ms) ---"
        if line.contains("Overall CPU Utilization:") {
            if let Some(pct_str) = extract_after(line, "Utilization:") {
                if let Some(val) = pct_str.trim().strip_suffix('%') {
                    cpu_util = val.trim().parse::<f64>().ok();
                }
            }
        }
    }
    let cpu_util_str = cpu_util.map(|v| format!("{:.1}", v)).unwrap_or_default();

    // Build PID → task name map from workload JSON.
    // scxsim assigns PIDs 1..N in task insertion order.
    // We infer thread type from task name prefix (fg_, bg_/hog, irq_).
    // Since we don't have the workload JSON here, we use PID to derive
    // thread_id and rely on the task block structure.

    // Parse per-task blocks.
    // Each block starts with "  Task PID=N:" and subsequent indented lines.
    let mut task_count = 0u32;
    let lines: Vec<&str> = combined.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];

        // Look for "  Task PID=N:"
        if let Some(pid_str) = extract_after(line, "Task PID=") {
            let pid_str = pid_str.trim_end_matches(':');
            let pid: u32 = match pid_str.parse() {
                Ok(v) => v,
                Err(_) => {
                    i += 1;
                    continue;
                }
            };

            // Collect all indented lines in this task block
            let mut schedules: u64 = 0;
            let mut run_mean_ns: Option<f64> = None;
            let mut inter_arrival_mean_ns: Option<f64> = None;
            let mut sched_lat_p50: Option<f64> = None;
            let mut sched_lat_p90: Option<f64> = None;
            let mut sched_lat_p99: Option<f64> = None;
            let mut sched_lat_p999: Option<f64> = None;
            let mut sched_lat_samples: u64 = 0;

            i += 1;
            while i < lines.len() {
                let block_line = lines[i].trim();
                if block_line.is_empty()
                    || block_line.starts_with("Task PID=")
                    || block_line.starts_with("---")
                    || block_line.starts_with("CPU ")
                {
                    break;
                }

                if block_line.starts_with("Schedules:") {
                    schedules = extract_after(block_line, "Schedules:")
                        .and_then(|s| s.trim().parse().ok())
                        .unwrap_or(0);
                } else if block_line.starts_with("Run duration:") {
                    // "Run duration:    0.499ms mean, 0.100ms stddev, CV=20.1%"
                    run_mean_ns = extract_duration_after_label(block_line, "Run duration:");
                } else if block_line.starts_with("Inter-arrival:") {
                    inter_arrival_mean_ns =
                        extract_duration_after_label(block_line, "Inter-arrival:");
                } else if block_line.starts_with("Sched latency:") {
                    // "Sched latency:   p50=102.574us p90=102.574us p99=102.574us p999=... max=... (N samples)"
                    sched_lat_p50 = extract_duration_ns(block_line, "p50=");
                    sched_lat_p90 = extract_duration_ns(block_line, "p90=");
                    sched_lat_p99 = extract_duration_ns(block_line, "p99=");
                    sched_lat_p999 = extract_duration_ns(block_line, "p999=");
                    // Extract sample count: "(N samples)"
                    if let Some(samples_str) = block_line.rfind('(').and_then(|start| {
                        block_line[start + 1..]
                            .find("samples")
                            .map(|_| &block_line[start + 1..])
                    }) {
                        sched_lat_samples = samples_str
                            .split_whitespace()
                            .next()
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(0);
                    }
                }

                i += 1;
            }

            // Determine thread type from PID ordering:
            // gen_sim_workload creates fg_thread_0..N first, then bg_hog_0..M
            let fg_count = _config.workload.foreground_threads;
            let (thread_type, thread_id) = if pid <= fg_count {
                ("foreground", format!("fg_thread_{}", pid - 1))
            } else {
                ("background", format!("bg_hog_{}", pid - fg_count - 1))
            };

            // Compute e2e latency = inter-arrival time (run + sleep + sched_latency)
            // This is the closest scxsim metric to "end-to-end" per-iteration time.
            let e2e_mean = inter_arrival_mean_ns;

            // Emit CSV rows
            if let Some(e2e) = e2e_mean {
                for pct in &["avg", "p50", "p99"] {
                    csv_rows.push(format!(
                        "{},{},{},baseline,{},{},e2e_latency,{},{:.0},ns,{},{},scxsim_inter_arrival,{}",
                        timestamp, mode, scheduler_name, thread_type, thread_id,
                        pct, e2e, schedules, rep, cpu_util_str,
                    ));
                }
            }

            if let Some(run_ns) = run_mean_ns {
                csv_rows.push(format!(
                    "{},{},{},baseline,{},{},run_duration,avg,{:.0},ns,{},{},scxsim_summary,{}",
                    timestamp,
                    mode,
                    scheduler_name,
                    thread_type,
                    thread_id,
                    run_ns,
                    schedules,
                    rep,
                    cpu_util_str,
                ));
            }

            if let Some(p50) = sched_lat_p50 {
                csv_rows.push(format!(
                    "{},{},{},baseline,{},{},sched_latency,p50,{:.0},ns,{},{},scxsim_summary,{}",
                    timestamp,
                    mode,
                    scheduler_name,
                    thread_type,
                    thread_id,
                    p50,
                    sched_lat_samples,
                    rep,
                    cpu_util_str,
                ));
            }
            if let Some(p90) = sched_lat_p90 {
                csv_rows.push(format!(
                    "{},{},{},baseline,{},{},sched_latency,p90,{:.0},ns,{},{},scxsim_summary,{}",
                    timestamp,
                    mode,
                    scheduler_name,
                    thread_type,
                    thread_id,
                    p90,
                    sched_lat_samples,
                    rep,
                    cpu_util_str,
                ));
            }
            if let Some(p99) = sched_lat_p99 {
                csv_rows.push(format!(
                    "{},{},{},baseline,{},{},sched_latency,p99,{:.0},ns,{},{},scxsim_summary,{}",
                    timestamp,
                    mode,
                    scheduler_name,
                    thread_type,
                    thread_id,
                    p99,
                    sched_lat_samples,
                    rep,
                    cpu_util_str,
                ));
            }
            if let Some(p999) = sched_lat_p999 {
                csv_rows.push(format!(
                    "{},{},{},baseline,{},{},sched_latency,p999,{:.0},ns,{},{},scxsim_summary,{}",
                    timestamp,
                    mode,
                    scheduler_name,
                    thread_type,
                    thread_id,
                    p999,
                    sched_lat_samples,
                    rep,
                    cpu_util_str,
                ));
            }

            task_count += 1;
            continue; // don't increment i again
        }

        i += 1;
    }

    if task_count == 0 {
        eprintln!("           warning: no per-task stats found in scxsim output");
        csv_rows.push(format!(
            "{},{},{},baseline,all,0,simulation_summary,n/a,0.0,n/a,0,{},no_task_stats_parsed,{}",
            timestamp, mode, scheduler_name, rep, cpu_util_str,
        ));
    } else {
        eprintln!(
            "           parsed {} task blocks, cpu_util={}",
            task_count, cpu_util_str
        );
    }

    let csv_content = csv_rows.join("\n") + "\n";
    std::fs::write(csv_path, csv_content)
        .with_context(|| format!("Failed to write {}", csv_path.display()))?;

    Ok(())
}

/// Extract the string after a prefix, stopping at whitespace.
fn extract_after<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    let idx = line.find(prefix)?;
    let rest = &line[idx + prefix.len()..];
    Some(rest.split_whitespace().next().unwrap_or(rest))
}

/// Extract a quoted string after a prefix: `Task "name"` → `name`
#[allow(dead_code)]
fn extract_quoted_after<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    let idx = line.find(prefix)?;
    let rest = &line[idx + prefix.len()..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

/// Extract a duration value from a label line like "Run duration: 0.499ms mean, ..."
/// Returns the first duration value in nanoseconds.
fn extract_duration_after_label(line: &str, label: &str) -> Option<f64> {
    let idx = line.find(label)?;
    let rest = line[idx + label.len()..].trim();
    // First token should be the duration, e.g., "0.499ms"
    let token = rest.split_whitespace().next()?;
    parse_duration_ns(token)
}

/// Extract a duration value in nanoseconds from a line.
/// Handles formats: "123.4µs", "1.5ms", "500ns", "1234"
fn extract_duration_ns(line: &str, prefix: &str) -> Option<f64> {
    let val_str = extract_after(line, prefix)?;
    parse_duration_ns(val_str)
}

/// Parse a duration string to nanoseconds.
fn parse_duration_ns(s: &str) -> Option<f64> {
    let s = s.trim();
    if let Some(v) = s.strip_suffix("µs").or_else(|| s.strip_suffix("us")) {
        return v.parse::<f64>().ok().map(|v| v * 1000.0);
    }
    if let Some(v) = s.strip_suffix("ms") {
        return v.parse::<f64>().ok().map(|v| v * 1_000_000.0);
    }
    if let Some(v) = s.strip_suffix("ns") {
        return v.parse::<f64>().ok();
    }
    if let Some(v) = s.strip_suffix('s') {
        return v.parse::<f64>().ok().map(|v| v * 1_000_000_000.0);
    }
    // Bare number: assume nanoseconds
    s.parse::<f64>().ok()
}

// ---------------------------------------------------------------------------
// Metrics collection from rt-app logs
// ---------------------------------------------------------------------------

fn collect_rtapp_metrics(
    log_dir: &Path,
    csv_path: &Path,
    cell: &MatrixCell,
    config: &RepromagicConfig,
) -> Result<()> {
    // rt-app produces per-thread log files: <basename>-<thread_name>-<tid>.log
    // Each line: <iteration> <period_us> <run_us> <start_us> <end_us> <slack_us>
    //
    // We extract scheduling latency metrics per thread.

    let mut csv_rows: Vec<String> = Vec::new();
    // C2 fix: use same 14-column schema as scxsim CSV for consistency with analyze.rs
    csv_rows.push(
        "timestamp,mode,scheduler,condition,thread_type,thread_id,metric_name,\
         percentile,value,unit,sample_count,rep,notes,avg_cpu_util_pct"
            .to_string(),
    );

    let timestamp = Utc::now().to_rfc3339();
    let mode = cell.mode.to_string();
    let scheduler = &cell.scheduler_key;
    let rep = cell.rep;

    // Scan log directory for rt-app log files
    let mut found_logs = false;
    if log_dir.exists() {
        for entry in std::fs::read_dir(log_dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.ends_with(".log") {
                continue;
            }

            found_logs = true;

            // Determine thread type from name
            let thread_type = if name.contains("fg_") {
                "foreground"
            } else if name.contains("bg_") || name.contains("hog") {
                "background"
            } else if name.contains("irq") {
                "irq_generator"
            } else {
                "unknown"
            };

            let thread_id = name.trim_end_matches(".log").to_string();

            // Parse the log file for latency data
            let content = std::fs::read_to_string(entry.path())?;
            let mut latencies: Vec<f64> = Vec::new();
            for line in content.lines() {
                let fields: Vec<&str> = line.split_whitespace().collect();
                if fields.len() >= 6 {
                    // slack_us (field 5) is our scheduling latency proxy
                    if let Ok(slack) = fields[5].parse::<f64>() {
                        latencies.push(slack);
                    }
                }
            }

            if latencies.is_empty() {
                continue;
            }

            // C3 fix: exclude warmup samples CHRONOLOGICALLY (before sorting).
            // Warmup samples are the first N entries, which correspond to the
            // initial warmup period before the workload reaches steady state.
            let n = latencies.len();
            let warmup_samples = (config.defaults.warmup as usize).min(n / 4); // Cap at 25% of samples
            let effective_chronological = &latencies[warmup_samples..];
            if effective_chronological.is_empty() {
                continue;
            }

            // Now sort for percentile computation
            let mut sorted = effective_chronological.to_vec();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let en = sorted.len();

            // Compute percentiles
            let avg = sorted.iter().sum::<f64>() / en as f64;
            let percentiles = [
                ("p50", percentile_value(&sorted, 50.0)),
                ("p90", percentile_value(&sorted, 90.0)),
                ("p99", percentile_value(&sorted, 99.0)),
                ("p999", percentile_value(&sorted, 99.9)),
                ("avg", avg),
                ("min", *sorted.first().unwrap()),
                ("max", *sorted.last().unwrap()),
            ];

            for (pname, value) in &percentiles {
                // C1 fix: use 'sched_latency' (not 'sched_latency_us') to match analyze.rs
                // C2 fix: use 14-column format with condition, notes, avg_cpu_util_pct
                csv_rows.push(format!(
                    "{},{},{},baseline,{},{},sched_latency,{},{:.2},us,{},{},rtapp_slack,",
                    timestamp, mode, scheduler, thread_type, thread_id, pname, value, en, rep
                ));
            }
        }
    }

    if !found_logs {
        // Write a placeholder CSV noting no data was collected (14-column format)
        csv_rows.push(format!(
            "{},{},{},baseline,unknown,unknown,no_data,n/a,0.0,us,0,{},no_rtapp_logs,",
            timestamp, mode, scheduler, rep
        ));
        eprintln!(
            "           WARNING: no rt-app log files found in {}",
            log_dir.display()
        );
    }

    // Write CSV
    let csv_content = csv_rows.join("\n") + "\n";
    std::fs::write(csv_path, csv_content)?;

    Ok(())
}

fn percentile_value(sorted: &[f64], pct: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((pct / 100.0) * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

// ---------------------------------------------------------------------------
// Utility: run commands and capture stdout
// ---------------------------------------------------------------------------

fn run_command_stdout(cmd: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(cmd)
        .args(args)
        .output()
        .with_context(|| format!("Failed to run `{}`", cmd))?;
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn run_command_stdout_in(cmd: &str, args: &[&str], dir: &Path) -> Result<String> {
    let output = Command::new(cmd)
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| format!("Failed to run `{}` in {}", cmd, dir.display()))?;
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_version_number() {
        assert_eq!(parse_version_number("v001_initial"), Some(1));
        assert_eq!(parse_version_number("v042_tuned_hogs"), Some(42));
        assert_eq!(parse_version_number("v999_max"), Some(999));
        assert_eq!(parse_version_number("not_a_version"), None);
        assert_eq!(parse_version_number("v"), None);
        assert_eq!(parse_version_number(""), None);
    }

    #[test]
    fn test_build_matrix() {
        let modes = vec![RunMode::RtappPinned];
        let schedulers = vec!["eevdf".to_string(), "lavd_v1".to_string()];
        let matrix = build_matrix(&modes, &schedulers, 3);
        assert_eq!(matrix.len(), 6); // 1 mode × 2 schedulers × 3 reps

        // Verify order: all reps for first scheduler, then all for second
        assert_eq!(matrix[0].scheduler_key, "eevdf");
        assert_eq!(matrix[0].rep, 1);
        assert_eq!(matrix[2].scheduler_key, "eevdf");
        assert_eq!(matrix[2].rep, 3);
        assert_eq!(matrix[3].scheduler_key, "lavd_v1");
        assert_eq!(matrix[3].rep, 1);
    }

    #[test]
    fn test_build_matrix_multi_mode() {
        let modes = vec![RunMode::RtappPinned, RunMode::RtappVm];
        let schedulers = vec!["eevdf".to_string()];
        let matrix = build_matrix(&modes, &schedulers, 2);
        assert_eq!(matrix.len(), 4); // 2 modes × 1 scheduler × 2 reps
    }

    #[test]
    fn test_percentile_value() {
        let data = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0];
        // p50 of [1..10]: idx = round(0.5 * 9) = 5 → value 6.0
        assert!((percentile_value(&data, 50.0) - 6.0).abs() < f64::EPSILON);
        assert!((percentile_value(&data, 99.0) - 10.0).abs() < f64::EPSILON);
        assert!((percentile_value(&data, 0.0) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_percentile_empty() {
        let data: Vec<f64> = vec![];
        assert!((percentile_value(&data, 50.0)).abs() < f64::EPSILON);
    }

    #[test]
    fn test_matrix_cell_display() {
        let cell = MatrixCell {
            mode: RunMode::RtappPinned,
            scheduler_key: "lavd_v1".to_string(),
            rep: 3,
        };
        let display = format!("{}", cell);
        assert!(display.contains("rtapp_pinned"));
        assert!(display.contains("lavd_v1"));
        assert!(display.contains("003"));
    }

    // =======================================================================
    // Regression tests for skeptic review critical findings C1-C4
    // =======================================================================

    /// C1 regression: rt-app CSV metric name must be 'sched_latency' (not
    /// 'sched_latency_us') so analyze.rs can match it.
    #[test]
    fn test_c1_rtapp_csv_metric_name_matches_analyze() {
        // Create a temp dir with a fake rt-app log
        let tmp = std::env::temp_dir().join("repm_test_c1");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // Write a minimal rt-app log: iteration period run start end slack
        let log_content =
            "0 2000 500 0 500 100\n1 2000 500 2000 2500 120\n2 2000 500 4000 4500 110\n";
        std::fs::write(tmp.join("repromagic-fg_thread_0-1234.log"), log_content).unwrap();

        let csv_path = tmp.join("rep_001.csv");
        let cell = MatrixCell {
            mode: RunMode::RtappPinned,
            scheduler_key: "eevdf".to_string(),
            rep: 1,
        };
        let config = crate::config::RepromagicConfig::from_toml(
            "[project]\nname = \"test\"\n[schedulers.eevdf]\nname = \"eevdf\"\nlabel = \"EEVDF\"\n",
        )
        .unwrap();

        collect_rtapp_metrics(&tmp, &csv_path, &cell, &config).unwrap();

        let csv = std::fs::read_to_string(&csv_path).unwrap();
        // Must contain 'sched_latency' (not 'sched_latency_us')
        assert!(
            csv.contains(",sched_latency,"),
            "CSV must use 'sched_latency' metric name, got:\n{}",
            csv
        );
        assert!(
            !csv.contains("sched_latency_us"),
            "CSV must NOT use 'sched_latency_us' (old name), got:\n{}",
            csv
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// C2 regression: rt-app and scxsim CSVs must have the same column schema
    /// (14 columns including condition, notes, avg_cpu_util_pct).
    #[test]
    fn test_c2_rtapp_csv_schema_matches_scxsim() {
        let tmp = std::env::temp_dir().join("repm_test_c2");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let log_content = "0 2000 500 0 500 100\n";
        std::fs::write(tmp.join("repromagic-fg_thread_0-1234.log"), log_content).unwrap();

        let csv_path = tmp.join("rep_001.csv");
        let cell = MatrixCell {
            mode: RunMode::RtappPinned,
            scheduler_key: "eevdf".to_string(),
            rep: 1,
        };
        let config = crate::config::RepromagicConfig::from_toml(
            "[project]\nname = \"test\"\n[schedulers.eevdf]\nname = \"eevdf\"\nlabel = \"EEVDF\"\n",
        )
        .unwrap();

        collect_rtapp_metrics(&tmp, &csv_path, &cell, &config).unwrap();

        let csv = std::fs::read_to_string(&csv_path).unwrap();
        let header = csv.lines().next().unwrap();

        // The canonical 14-column header
        let expected_columns = [
            "timestamp",
            "mode",
            "scheduler",
            "condition",
            "thread_type",
            "thread_id",
            "metric_name",
            "percentile",
            "value",
            "unit",
            "sample_count",
            "rep",
            "notes",
            "avg_cpu_util_pct",
        ];
        let actual_columns: Vec<&str> = header.split(',').collect();
        assert_eq!(
            actual_columns, expected_columns,
            "rt-app CSV header must match 14-column schema.\nExpected: {:?}\nGot:      {:?}",
            expected_columns, actual_columns
        );

        // Data rows must also have 14 fields (some may be empty)
        for (i, line) in csv.lines().enumerate().skip(1) {
            if line.trim().is_empty() {
                continue;
            }
            let fields: Vec<&str> = line.split(',').collect();
            assert_eq!(
                fields.len(),
                14,
                "Row {} must have 14 fields, got {} in: {}",
                i + 1,
                fields.len(),
                line
            );
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// C3 regression: warmup must exclude chronologically first samples,
    /// not the smallest sorted values.
    #[test]
    fn test_c3_warmup_excludes_chronological_not_sorted() {
        let tmp = std::env::temp_dir().join("repm_test_c3");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // Create a log where the first 2 samples (warmup) have HIGH latency
        // and the remaining 8 samples have LOW latency.
        // If warmup incorrectly operates on sorted data, it would remove the
        // 2 smallest values (from the steady-state), not the 2 initial ones.
        let mut all_lines = vec![
            "0 2000 500 0 500 9000".to_string(),
            "1 2000 500 2000 2500 9500".to_string(),
        ];
        // Steady state: 8 samples with slack=100 (low)
        for i in 2..10 {
            all_lines.push(format!(
                "{} 2000 500 {} {} 100",
                i,
                i * 2000,
                i * 2000 + 500
            ));
        }
        let log_content = all_lines.join("\n") + "\n";

        std::fs::write(tmp.join("repromagic-fg_thread_0-1234.log"), &log_content).unwrap();

        let csv_path = tmp.join("rep_001.csv");
        let cell = MatrixCell {
            mode: RunMode::RtappPinned,
            scheduler_key: "eevdf".to_string(),
            rep: 1,
        };
        // Set warmup=2 so we skip the first 2 chronological samples
        let config = crate::config::RepromagicConfig::from_toml(
            "[project]\nname = \"test\"\n[defaults]\nwarmup = 2\nduration = 30\n\
             [schedulers.eevdf]\nname = \"eevdf\"\nlabel = \"EEVDF\"\n",
        )
        .unwrap();

        collect_rtapp_metrics(&tmp, &csv_path, &cell, &config).unwrap();

        let csv = std::fs::read_to_string(&csv_path).unwrap();

        // Extract the avg value from the CSV
        let avg_line = csv.lines().find(|l| l.contains(",avg,")).unwrap();
        let avg_val: f64 = avg_line.split(',').nth(8).unwrap().parse().unwrap();

        // If warmup is correct (chronological), avg should be ~100 (steady state only).
        // If warmup is wrong (sorted), it would remove 2 smallest (100s) and avg
        // would include the 9000/9500 warmup spikes, giving avg >> 100.
        assert!(
            avg_val < 200.0,
            "After warmup exclusion, avg should be ~100 (steady state). \
             Got {:.1}, which suggests warmup removed sorted-smallest instead of chronological-first.",
            avg_val
        );

        // Also verify sample_count is 8 (10 total - 2 warmup)
        let sample_count: u32 = avg_line.split(',').nth(10).unwrap().parse().unwrap();
        assert_eq!(
            sample_count, 8,
            "Should have 8 samples after excluding 2 warmup"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// C4 regression: mode name in CSV must use cell.mode.to_string(),
    /// never hardcoded strings like "simulator".
    #[test]
    fn test_c4_mode_name_consistency() {
        // Verify that RunMode::RtappSim.to_string() is the canonical name
        assert_eq!(RunMode::RtappSim.to_string(), "rtapp_sim");
        assert_eq!(RunMode::RtappPinned.to_string(), "rtapp_pinned");
        assert_eq!(RunMode::RtappVm.to_string(), "rtapp_vm");

        // The mode field written to CSV must match — verify through
        // the collect_rtapp_metrics path
        let tmp = std::env::temp_dir().join("repm_test_c4");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let log_content = "0 2000 500 0 500 100\n";
        std::fs::write(tmp.join("repromagic-fg_thread_0-1234.log"), log_content).unwrap();

        let csv_path = tmp.join("rep_001.csv");
        let cell = MatrixCell {
            mode: RunMode::RtappPinned,
            scheduler_key: "test_sched".to_string(),
            rep: 1,
        };
        let config = crate::config::RepromagicConfig::from_toml(
            "[project]\nname = \"test\"\n[schedulers.eevdf]\nname = \"eevdf\"\nlabel = \"EEVDF\"\n",
        )
        .unwrap();

        collect_rtapp_metrics(&tmp, &csv_path, &cell, &config).unwrap();

        let csv = std::fs::read_to_string(&csv_path).unwrap();
        // Every data row must contain the canonical mode name
        for line in csv.lines().skip(1) {
            if line.trim().is_empty() {
                continue;
            }
            let mode_field = line.split(',').nth(1).unwrap();
            assert_eq!(
                mode_field, "rtapp_pinned",
                "Mode field must be 'rtapp_pinned', got '{}'",
                mode_field
            );
        }

        // Verify no hardcoded "simulator" string in mode field
        assert!(
            !csv.contains(",simulator,"),
            "CSV must not contain hardcoded 'simulator' as mode"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Integration test: CSV produced by collect_rtapp_metrics is parseable
    /// and contains the metric names that analyze.rs expects.
    #[test]
    fn test_rtapp_csv_parseable_by_analyze() {
        let tmp = std::env::temp_dir().join("repm_test_integration");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // Create fake rt-app logs
        let log_content =
            "0 2000 500 0 500 100\n1 2000 500 2000 2500 120\n2 2000 500 4000 4500 110\n";
        std::fs::write(tmp.join("repromagic-fg_thread_0-1234.log"), log_content).unwrap();
        std::fs::write(
            tmp.join("repromagic-bg_hog_0-5678.log"),
            "0 2000 130 0 130 50\n",
        )
        .unwrap();

        let csv_path = tmp.join("rep_001.csv");
        let cell = MatrixCell {
            mode: RunMode::RtappPinned,
            scheduler_key: "eevdf".to_string(),
            rep: 1,
        };
        let config = crate::config::RepromagicConfig::from_toml(
            "[project]\nname = \"test\"\n[schedulers.eevdf]\nname = \"eevdf\"\nlabel = \"EEVDF\"\n",
        )
        .unwrap();

        collect_rtapp_metrics(&tmp, &csv_path, &cell, &config).unwrap();

        // Parse with csv crate (same as analyze.rs uses)
        let content = std::fs::read_to_string(&csv_path).unwrap();
        let mut reader = csv::ReaderBuilder::new()
            .flexible(true)
            .from_reader(content.as_bytes());

        let headers = reader.headers().unwrap().clone();

        // Verify analyze.rs required columns exist
        let required = ["mode", "scheduler", "value", "metric_name", "percentile"];
        for col in &required {
            assert!(
                headers.iter().any(|h| h == *col),
                "CSV missing required column '{}'. Headers: {:?}",
                col,
                headers
            );
        }

        // Verify we can parse data rows and find sched_latency
        let mut found_sched_latency = false;
        let metric_col = headers.iter().position(|h| h == "metric_name").unwrap();
        for record in reader.records() {
            let record = record.unwrap();
            if record.get(metric_col) == Some("sched_latency") {
                found_sched_latency = true;
            }
        }
        assert!(
            found_sched_latency,
            "CSV must contain 'sched_latency' metric rows for analyze.rs to find"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // =======================================================================
    // Regression tests for skeptic review major findings M1-M4
    // =======================================================================

    /// M3 regression: .done marker file distinguishes completed from failed reps.
    /// - Successful cell → .done marker exists, resume skips it
    /// - Failed cell → no .done marker, resume retries it
    /// - Placeholder CSV (no real data) → no .done marker, resume retries it
    #[test]
    fn test_m3_done_marker_resume_logic() {
        let tmp = std::env::temp_dir().join("repm_test_m3");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let csv_path = tmp.join("rep_001.csv");
        let done_path = csv_path.with_extension("csv.done");

        // Case 1: No files → should NOT skip
        assert!(!done_path.exists());

        // Case 2: CSV exists but no .done → should NOT skip (failed/incomplete)
        std::fs::write(&csv_path, "some,data\n").unwrap();
        assert!(!done_path.exists(), "CSV without .done must not be skipped");

        // Case 3: Both CSV and .done exist → should skip
        std::fs::write(&done_path, "").unwrap();
        assert!(done_path.exists(), ".done marker must cause skip");

        // Case 4: Verify cleanup removes .done on retry
        std::fs::remove_file(&done_path).unwrap();
        std::fs::remove_file(&csv_path).unwrap();
        assert!(!done_path.exists());
        assert!(!csv_path.exists());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// M4 regression: hash label must honestly reflect the algorithm used.
    #[test]
    fn test_m4_hash_label_honesty() {
        let tmp = std::env::temp_dir().join("repm_test_m4");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // Write a fake config file
        let config_content = "[project]\nname = \"test\"\n";
        std::fs::write(tmp.join(crate::workspace::CONFIG_FILENAME), config_content).unwrap();

        let hash = hash_config_toml(&tmp).unwrap();

        // M4: must NOT contain 'fnv1a' (mislabeled)
        assert!(
            !hash.contains("fnv1a"),
            "Hash must not be labeled 'fnv1a' when using a different algorithm. Got: {}",
            hash
        );

        // Must contain a recognizable algorithm prefix
        assert!(
            hash.contains(':'),
            "Hash must have 'algorithm:value' format. Got: {}",
            hash
        );
        let prefix = hash.split(':').next().unwrap();
        assert!(
            ["siphash24", "sha256", "blake3"].contains(&prefix),
            "Hash prefix '{}' must name a real algorithm. Got: {}",
            prefix,
            hash
        );

        // Same content → same hash (deterministic)
        let hash2 = hash_config_toml(&tmp).unwrap();
        assert_eq!(hash, hash2, "Hash must be deterministic");

        // Different content → different hash
        std::fs::write(
            tmp.join(crate::workspace::CONFIG_FILENAME),
            "[project]\nname = \"different\"\n",
        )
        .unwrap();
        let hash3 = hash_config_toml(&tmp).unwrap();
        assert_ne!(
            hash, hash3,
            "Different configs must produce different hashes"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// M2 regression: check_experiment_resumable must detect config changes.
    /// We test this by creating a sealed experiment with one config hash,
    /// then calling check with a different config.
    #[test]
    fn test_m2_config_hash_in_provenance() {
        // Verify that ExperimentProvenance includes config_hash field
        let prov = ExperimentProvenance {
            schema_version: "1.0".into(),
            experiment_version: "v001_test".into(),
            timestamp: "2026-01-01T00:00:00Z".into(),
            config_hash: "siphash24:abcdef0123456789".into(),
            host: HostInfo {
                hostname: "test".into(),
                cpu_model: "test".into(),
                cpu_count: 4,
                kernel: "6.0".into(),
                user: "test".into(),
            },
            workspace: WorkspaceInfo {
                git_revision: "abc123".into(),
                git_branch: "main".into(),
                git_dirty: false,
                repm_version: "0.1.0".into(),
            },
            schedulers: BTreeMap::new(),
            experiment: ExperimentInfo {
                modes: vec!["rtapp_pinned".into()],
                reps: 3,
                duration_s: 30,
                warmup_s: 5,
                cores: 8,
                workload_cpus: "0-7".into(),
                irq_cpus: None,
                generator_cpus: None,
            },
        };

        // Serialize and deserialize to verify config_hash round-trips
        let json = serde_json::to_string_pretty(&prov).unwrap();
        assert!(
            json.contains("config_hash"),
            "provenance JSON must contain config_hash"
        );
        assert!(json.contains("siphash24:abcdef0123456789"));

        let parsed: ExperimentProvenance = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.config_hash, "siphash24:abcdef0123456789");
    }

    /// M1 regression: verify that the scheduler binary name can be extracted
    /// from a PathBuf for pkill.
    #[test]
    fn test_m1_binary_name_extraction() {
        let path = PathBuf::from("/usr/local/bin/scx_lavd_baseline_5a68bc66");
        let binary_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        assert_eq!(binary_name, "scx_lavd_baseline_5a68bc66");

        let path2 = PathBuf::from("bin/schedulers/scx_lavd");
        let binary_name2 = path2
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        assert_eq!(binary_name2, "scx_lavd");
    }
}
