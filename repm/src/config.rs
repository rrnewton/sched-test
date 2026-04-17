use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Top-level workspace configuration, deserialized from `repromagic_config.toml`.
#[derive(Debug, Serialize, Deserialize)]
pub struct RepromagicConfig {
    pub project: ProjectConfig,
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub topology: Option<TopologyConfig>,
    #[serde(default)]
    pub schedulers: std::collections::BTreeMap<String, SchedulerDef>,
    #[serde(default)]
    pub workload: Option<WorkloadConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProjectConfig {
    /// Project name: [a-zA-Z0-9_-]+
    pub name: String,
    /// What scheduling behavior is under investigation?
    #[serde(default)]
    pub phenomenon: Phenomenon,
    /// Free-form description for humans and report headers.
    #[serde(default)]
    pub description: Option<String>,
}

/// What scheduling behavior is under investigation?
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Phenomenon {
    BadTailLatency,
    BadCpuUtil,
    BadThroughput,
    #[default]
    #[serde(other)]
    Other,
}

impl std::fmt::Display for Phenomenon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Phenomenon::BadTailLatency => write!(f, "bad_tail_latency"),
            Phenomenon::BadCpuUtil => write!(f, "bad_cpu_util"),
            Phenomenon::BadThroughput => write!(f, "bad_throughput"),
            Phenomenon::Other => write!(f, "other"),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Defaults {
    /// Default CPU count (~8 cores in 2 CCXs).
    #[serde(default = "default_cores")]
    pub cores: u32,
    /// Default experiment duration in seconds.
    #[serde(default = "default_duration")]
    pub duration: u32,
    /// Default repetitions per cell.
    #[serde(default = "default_reps")]
    pub reps: u32,
    /// Warmup exclusion period in seconds.
    #[serde(default = "default_warmup")]
    pub warmup: u32,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            cores: default_cores(),
            duration: default_duration(),
            reps: default_reps(),
            warmup: default_warmup(),
        }
    }
}

fn default_cores() -> u32 { 8 }
fn default_duration() -> u32 { 30 }
fn default_reps() -> u32 { 3 }
fn default_warmup() -> u32 { 5 }

#[derive(Debug, Serialize, Deserialize)]
pub struct TopologyConfig {
    /// CPU set for workload threads (e.g., "0-7").
    pub workload_cpus: Option<String>,
    /// IRQ target CPUs (optional — for networking/timer-heavy apps).
    pub irq_cpus: Option<Vec<u32>>,
    /// CPU set for IRQ generator threads (optional).
    pub generator_cpus: Option<String>,
}

/// A scheduler definition for A vs B or A1→A2 comparison.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulerDef {
    /// Scheduler type identifier.
    pub name: SchedulerName,
    /// Human-readable label for reports (e.g., "LAVD-v2", "EEVDF").
    pub label: String,
    /// (repo_url, git_revision) for provenance tracking.
    #[serde(default)]
    pub provenance: Option<ProvenanceRef>,
    /// Path to scheduler binary (None for kernel-builtin like EEVDF).
    #[serde(default)]
    pub binary: Option<PathBuf>,
    /// Command-line flags passed when launching the scheduler.
    #[serde(default)]
    pub flags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvenanceRef {
    pub repo: String,
    pub revision: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchedulerName {
    Eevdf,
    Lavd,
    Cfs,
    #[serde(other)]
    Other,
}

impl std::fmt::Display for SchedulerName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SchedulerName::Eevdf => write!(f, "eevdf"),
            SchedulerName::Lavd => write!(f, "lavd"),
            SchedulerName::Cfs => write!(f, "cfs"),
            SchedulerName::Other => write!(f, "other"),
        }
    }
}

/// Workload configuration: N foreground + M background threads.
#[derive(Debug, Serialize, Deserialize)]
pub struct WorkloadConfig {
    /// Number of foreground threads (interesting scheduling behavior).
    #[serde(default = "default_foreground")]
    pub foreground_threads: u32,
    /// Number of background threads (CPU pressure / hog threads).
    #[serde(default = "default_background")]
    pub background_threads: u32,
}

fn default_foreground() -> u32 { 4 }
fn default_background() -> u32 { 16 }

/// Experiment run modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum RunMode {
    /// rt-app workload on bare metal with CPU pinning.
    RtappPinned,
    /// rt-app workload inside a virtme-ng VM.
    RtappVm,
    /// rt-app workload under scx-sim.
    RtappSim,
}

impl std::fmt::Display for RunMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunMode::RtappPinned => write!(f, "rtapp_pinned"),
            RunMode::RtappVm => write!(f, "rtapp_vm"),
            RunMode::RtappSim => write!(f, "rtapp_sim"),
        }
    }
}

/// Validate that a project name matches [a-zA-Z0-9_-]+
pub fn validate_project_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        anyhow::bail!("Project name cannot be empty");
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        anyhow::bail!(
            "Project name must match [a-zA-Z0-9_-]+, got: {:?}",
            name
        );
    }
    Ok(())
}
