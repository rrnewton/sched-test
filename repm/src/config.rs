//! Configuration types for `repromagic_config.toml`.
//!
//! These types define the workspace-level configuration schema. A workspace
//! is identified by the presence of `repromagic_config.toml` at its root.
//!
//! Design principles:
//! - TOML is for humans (comments, readable). JSON is for machines (rt-app configs).
//! - Every field has a sensible default where possible.
//! - Scheduler labels and thread types are project-defined strings, not fixed enums.
//! - Optional features (IRQ exposure, PMU calibration) are opt-in, never forced.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Top-level config
// ---------------------------------------------------------------------------

/// Root of `repromagic_config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RepromagicConfig {
    pub project: ProjectInfo,

    #[serde(default)]
    pub defaults: Defaults,

    #[serde(default)]
    pub topology: TopologyConfig,

    /// Keyed by a short identifier (e.g., `eevdf`, `lavd_v1`).
    /// Order-preserving via `BTreeMap` so TOML output is deterministic.
    #[serde(default)]
    pub schedulers: BTreeMap<String, SchedulerDef>,

    #[serde(default)]
    pub workload: WorkloadConfig,

    /// `[capture]` table — SSH trace collection settings.
    #[serde(default)]
    pub capture: CaptureConfig,
}

// ---------------------------------------------------------------------------
// Project info
// ---------------------------------------------------------------------------

/// `[project]` table — identifies what this workspace investigates.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectInfo {
    /// Project name: `[a-zA-Z0-9_-]+`, no spaces.
    pub name: String,

    /// What scheduling behavior is under investigation.
    #[serde(default)]
    pub phenomenon: Phenomenon,

    /// Free-form human description for report headers.
    #[serde(default)]
    pub description: String,
}

/// Scheduling phenomenon under investigation.
///
/// Drives `repm analyze` defaults: which metrics are primary, what thresholds
/// to flag, and how to structure the results table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Phenomenon {
    /// P99/P99.9 latency spikes.
    #[default]
    BadTailLatency,
    /// Unexpectedly high/low CPU utilization.
    BadCpuUtil,
    /// Throughput degradation under load.
    BadThroughput,
    /// Free-form description.
    Other(String),
}

impl std::fmt::Display for Phenomenon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadTailLatency => write!(f, "bad_tail_latency"),
            Self::BadCpuUtil => write!(f, "bad_cpu_util"),
            Self::BadThroughput => write!(f, "bad_throughput"),
            Self::Other(s) => write!(f, "other: {s}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

/// `[defaults]` table — global experiment defaults.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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

fn default_cores() -> u32 {
    8
}
fn default_duration() -> u32 {
    30
}
fn default_reps() -> u32 {
    3
}
fn default_warmup() -> u32 {
    5
}

// ---------------------------------------------------------------------------
// Topology
// ---------------------------------------------------------------------------

/// `[topology]` table — CPU layout for experiments.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TopologyConfig {
    /// CPU set for workload threads (e.g., `"0-7"`).
    #[serde(default = "default_workload_cpus")]
    pub workload_cpus: String,

    /// Optional: IRQ target CPUs. Only relevant for networking/timer-heavy apps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub irq_cpus: Option<Vec<u32>>,

    /// Optional: CPU set for IRQ generator threads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generator_cpus: Option<String>,
}

impl Default for TopologyConfig {
    fn default() -> Self {
        Self {
            workload_cpus: default_workload_cpus(),
            irq_cpus: None,
            generator_cpus: None,
        }
    }
}

fn default_workload_cpus() -> String {
    "0-7".to_string()
}

// ---------------------------------------------------------------------------
// Scheduler definitions
// ---------------------------------------------------------------------------

/// One entry in `[schedulers.<key>]`.
///
/// For **A1 → A2 comparisons** (evaluating a change to one scheduler),
/// define two entries with different labels and provenance pointing to
/// different revisions but the same [`SchedulerName`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SchedulerDef {
    /// Scheduler family identifier.
    pub name: SchedulerName,

    /// Human-readable label for reports and CSV `scheduler` column
    /// (e.g., `"EEVDF"`, `"LAVD-PostIRQ"`).
    pub label: String,

    /// Source provenance: `(repo_url, git_revision)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,

    /// Path to scheduler binary. `None` for kernel-builtin schedulers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<PathBuf>,

    /// Command-line flags passed when launching the scheduler.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<String>,
}

/// Scheduler family — the scheduler "kind," independent of version/revision.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SchedulerName {
    /// Linux kernel default (EEVDF on 6.6+). No binary needed.
    Eevdf,
    /// sched_ext LAVD.
    Lavd,
    /// Legacy CFS (pre-EEVDF kernels). No binary needed.
    Cfs,
    /// Custom / third-party scheduler.
    Other(String),
}

impl SchedulerName {
    /// Returns `true` for kernel-builtin schedulers that need no binary.
    pub fn is_builtin(&self) -> bool {
        matches!(self, Self::Eevdf | Self::Cfs)
    }
}

impl std::fmt::Display for SchedulerName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Eevdf => write!(f, "eevdf"),
            Self::Lavd => write!(f, "lavd"),
            Self::Cfs => write!(f, "cfs"),
            Self::Other(s) => write!(f, "{s}"),
        }
    }
}

/// Source provenance for a scheduler binary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Provenance {
    /// Git repository URL.
    pub repo: String,
    /// Git revision (commit hash, tag, or branch).
    pub revision: String,
}

// ---------------------------------------------------------------------------
// Workload
// ---------------------------------------------------------------------------

/// `[workload]` table — workload thread configuration.
///
/// Encodes the core repromagic abstraction:
/// - N **foreground** threads with defined characteristics and inter-thread
///   communication relationships to reproduce.
/// - M **background** threads for CPU utilization pressure.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkloadConfig {
    /// Number of foreground (latency-sensitive, critical-path) threads.
    #[serde(default = "default_foreground_threads")]
    pub foreground_threads: u32,

    /// Number of background (CPU-hog, pressure) threads.
    #[serde(default = "default_background_threads")]
    pub background_threads: u32,

    /// Optional per-thread type overrides for timing parameters.
    /// Keys are project-defined thread type names.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub thread_types: BTreeMap<String, ThreadTypeConfig>,
}

impl Default for WorkloadConfig {
    fn default() -> Self {
        Self {
            foreground_threads: default_foreground_threads(),
            background_threads: default_background_threads(),
            thread_types: BTreeMap::new(),
        }
    }
}

fn default_foreground_threads() -> u32 {
    4
}
fn default_background_threads() -> u32 {
    16
}

/// Per-thread-type timing configuration.
///
/// Controls rt-app JSON generation in `repm gen-config`.
/// All durations are in microseconds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ThreadTypeConfig {
    /// Thread role: foreground (critical path) or background (pressure).
    pub role: ThreadRole,

    /// How many threads of this type to create.
    #[serde(default = "default_thread_count")]
    pub count: u32,

    /// Run phase duration in microseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_us: Option<u32>,

    /// Sleep phase duration in microseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sleep_us: Option<u32>,
}

fn default_thread_count() -> u32 {
    1
}

/// Thread role classification — matches the Metrics Specification v3.0.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ThreadRole {
    Foreground,
    Background,
}

// ---------------------------------------------------------------------------
// Capture configuration
// ---------------------------------------------------------------------------

/// `[capture]` table — trace collection settings for `repm capture`.
///
/// Controls what data is collected from the target host, SSH connection
/// parameters, and trace command templates.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CaptureConfig {
    /// Default remote host (e.g., `root@prod-host`). CLI `--ssh` overrides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_host: Option<String>,

    /// SSH connection timeout in seconds.
    #[serde(default = "default_ssh_timeout")]
    pub ssh_timeout: u32,

    /// SSH identity file (private key path). Uses default SSH agent if omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_identity: Option<String>,

    /// Remote working directory for trace collection.
    #[serde(default = "default_remote_workdir")]
    pub remote_workdir: String,

    /// Trace commands to run on the target host. Each command is run
    /// sequentially. Use `{duration}` placeholder for capture duration.
    #[serde(default = "default_trace_commands")]
    pub trace_commands: Vec<TraceCommand>,

    /// Whether to sample /proc/interrupts during capture.
    #[serde(default = "default_true")]
    pub collect_interrupts: bool,

    /// /proc/interrupts sampling interval in seconds.
    #[serde(default = "default_interrupts_interval")]
    pub interrupts_interval: u32,

    /// Whether to collect LAVD stats (requires scx_lavd running).
    /// Skipped when `--scheduler-agnostic` is used.
    #[serde(default)]
    pub collect_lavd_stats: bool,

    /// Whether to sample /proc/stat (CPU time counters).
    /// Automatically enabled by `--scheduler-agnostic`.
    #[serde(default)]
    pub collect_proc_stat: bool,

    /// Whether to sample /proc/schedstat (scheduler run queue stats).
    /// Automatically enabled by `--scheduler-agnostic`.
    #[serde(default)]
    pub collect_schedstat: bool,

    /// Additional files to SCP back from the remote host after capture.
    /// Paths are relative to `remote_workdir`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_files: Vec<String>,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            default_host: None,
            ssh_timeout: default_ssh_timeout(),
            ssh_identity: None,
            remote_workdir: default_remote_workdir(),
            trace_commands: default_trace_commands(),
            collect_interrupts: true,
            interrupts_interval: default_interrupts_interval(),
            collect_lavd_stats: false,
            collect_proc_stat: false,
            collect_schedstat: false,
            extra_files: Vec::new(),
        }
    }
}

fn default_ssh_timeout() -> u32 {
    30
}

fn default_remote_workdir() -> String {
    "/tmp/repm_capture".to_string()
}

fn default_true() -> bool {
    true
}

fn default_interrupts_interval() -> u32 {
    1
}

fn default_trace_commands() -> Vec<TraceCommand> {
    vec![TraceCommand {
        name: "proc_interrupts".to_string(),
        command: "cat /proc/interrupts".to_string(),
        background: false,
    }]
}

/// A trace command to run during capture.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TraceCommand {
    /// Human-readable name for this trace step.
    pub name: String,
    /// Shell command to execute. Supports `{duration}` placeholder.
    pub command: String,
    /// If true, run in background (for duration-based traces like perfetto).
    #[serde(default)]
    pub background: bool,
}

// ---------------------------------------------------------------------------
// Experiment run modes
// ---------------------------------------------------------------------------

/// Experiment run modes (no PureRust — repromagic goes straight to rtapp).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[allow(clippy::enum_variant_names)]
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
            Self::RtappPinned => write!(f, "rtapp_pinned"),
            Self::RtappVm => write!(f, "rtapp_vm"),
            Self::RtappSim => write!(f, "rtapp_sim"),
        }
    }
}

// ---------------------------------------------------------------------------
// Config I/O
// ---------------------------------------------------------------------------

#[allow(dead_code)]
impl RepromagicConfig {
    /// Parse a `repromagic_config.toml` string.
    pub fn from_toml(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    /// Serialize to pretty-printed TOML.
    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }

    /// Validate the config, returning a list of human-readable issues.
    /// An empty list means the config is valid.
    pub fn validate(&self) -> Vec<String> {
        let mut issues = Vec::new();

        // --- project.name ---
        if self.project.name.is_empty() {
            issues.push("project.name is required".into());
        } else if !self
            .project
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            issues.push(format!(
                "project.name '{}' contains invalid characters (must match [a-zA-Z0-9_-]+)",
                self.project.name
            ));
        }

        // --- schedulers ---
        if self.schedulers.is_empty() {
            issues.push("at least one [schedulers.*] entry is required".into());
        }
        for (key, sched) in &self.schedulers {
            if sched.name.is_builtin() && sched.binary.is_some() {
                issues.push(format!(
                    "schedulers.{key}: kernel-builtin scheduler ({}) should not have a binary",
                    sched.name
                ));
            }
            if !sched.name.is_builtin() && sched.binary.is_none() {
                issues.push(format!(
                    "schedulers.{key}: non-builtin scheduler ({}) needs a binary path",
                    sched.name
                ));
            }
        }

        // --- defaults ---
        if self.defaults.cores == 0 {
            issues.push("defaults.cores must be > 0".into());
        }
        if self.defaults.duration <= self.defaults.warmup {
            issues.push(format!(
                "defaults.duration ({}) must be > defaults.warmup ({})",
                self.defaults.duration, self.defaults.warmup
            ));
        }

        issues
    }
}

/// Validate that a project name matches `[a-zA-Z0-9_-]+`.
pub fn validate_project_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        anyhow::bail!("Project name cannot be empty");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        anyhow::bail!("Project name must match [a-zA-Z0-9_-]+, got: {:?}", name);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The canonical example config from the repromagic plan.
    const EXAMPLE_CONFIG: &str = r#"
[project]
name = "cache_svc"
phenomenon = "bad_tail_latency"
description = "Evaluate scheduler impact on cache serving workload tail latency"

[defaults]
cores = 8
duration = 30
reps = 3
warmup = 5

[topology]
workload_cpus = "0-7"
irq_cpus = [0, 2, 4, 6]
generator_cpus = "8-11"

[schedulers.eevdf]
name = "eevdf"
label = "EEVDF"

[schedulers.lavd_v1]
name = "lavd"
label = "LAVD-v1"
binary = "bin/schedulers/scx_lavd_baseline_5a68bc66"
flags = ["--performance"]

[schedulers.lavd_v1.provenance]
repo = "https://github.com/sched-ext/scx"
revision = "5a68bc66"

[schedulers.lavd_v2]
name = "lavd"
label = "LAVD-v2"
binary = "bin/schedulers/scx_lavd_postirq_4ed537cd"
flags = ["--performance"]

[schedulers.lavd_v2.provenance]
repo = "https://github.com/sched-ext/scx"
revision = "4ed537cd"

[workload]
foreground_threads = 4
background_threads = 16
"#;

    #[test]
    fn parse_example_config() {
        let cfg = RepromagicConfig::from_toml(EXAMPLE_CONFIG).expect("parse failed");

        // Project
        assert_eq!(cfg.project.name, "cache_svc");
        assert_eq!(cfg.project.phenomenon, Phenomenon::BadTailLatency);
        assert!(cfg.project.description.contains("cache serving"));

        // Defaults
        assert_eq!(cfg.defaults.cores, 8);
        assert_eq!(cfg.defaults.duration, 30);
        assert_eq!(cfg.defaults.reps, 3);
        assert_eq!(cfg.defaults.warmup, 5);

        // Topology
        assert_eq!(cfg.topology.workload_cpus, "0-7");
        assert_eq!(cfg.topology.irq_cpus, Some(vec![0, 2, 4, 6]));
        assert_eq!(cfg.topology.generator_cpus, Some("8-11".into()));

        // Schedulers
        assert_eq!(cfg.schedulers.len(), 3);

        let eevdf = &cfg.schedulers["eevdf"];
        assert_eq!(eevdf.name, SchedulerName::Eevdf);
        assert_eq!(eevdf.label, "EEVDF");
        assert!(eevdf.binary.is_none());

        let v1 = &cfg.schedulers["lavd_v1"];
        assert_eq!(v1.name, SchedulerName::Lavd);
        assert_eq!(v1.label, "LAVD-v1");
        assert_eq!(
            v1.binary.as_deref().unwrap(),
            PathBuf::from("bin/schedulers/scx_lavd_baseline_5a68bc66")
        );
        assert_eq!(v1.flags, vec!["--performance"]);
        let prov = v1.provenance.as_ref().unwrap();
        assert_eq!(prov.repo, "https://github.com/sched-ext/scx");
        assert_eq!(prov.revision, "5a68bc66");

        // Workload
        assert_eq!(cfg.workload.foreground_threads, 4);
        assert_eq!(cfg.workload.background_threads, 16);
    }

    #[test]
    fn round_trip_serialize_deserialize() {
        let cfg = RepromagicConfig::from_toml(EXAMPLE_CONFIG).expect("parse failed");
        let toml_str = cfg.to_toml().expect("serialize failed");
        let cfg2 = RepromagicConfig::from_toml(&toml_str).expect("re-parse failed");
        assert_eq!(cfg, cfg2);
    }

    #[test]
    fn minimal_config_uses_defaults() {
        let toml = r#"
[project]
name = "quick-test"

[schedulers.eevdf]
name = "eevdf"
label = "EEVDF"
"#;
        let cfg = RepromagicConfig::from_toml(toml).expect("parse failed");
        assert_eq!(cfg.project.name, "quick-test");
        assert_eq!(cfg.project.phenomenon, Phenomenon::BadTailLatency);
        assert_eq!(cfg.defaults.cores, 8);
        assert_eq!(cfg.defaults.duration, 30);
        assert_eq!(cfg.defaults.reps, 3);
        assert_eq!(cfg.defaults.warmup, 5);
        assert_eq!(cfg.topology.workload_cpus, "0-7");
        assert!(cfg.topology.irq_cpus.is_none());
        assert_eq!(cfg.workload.foreground_threads, 4);
        assert_eq!(cfg.workload.background_threads, 16);
    }

    #[test]
    fn validate_good_config() {
        let cfg = RepromagicConfig::from_toml(EXAMPLE_CONFIG).expect("parse failed");
        let issues = cfg.validate();
        assert!(issues.is_empty(), "unexpected issues: {issues:?}");
    }

    #[test]
    fn validate_empty_name() {
        let toml = r#"
[project]
name = ""
[schedulers.eevdf]
name = "eevdf"
label = "EEVDF"
"#;
        let cfg = RepromagicConfig::from_toml(toml).unwrap();
        let issues = cfg.validate();
        assert!(issues.iter().any(|i| i.contains("name is required")));
    }

    #[test]
    fn validate_bad_name_chars() {
        let toml = r#"
[project]
name = "my project"
[schedulers.eevdf]
name = "eevdf"
label = "EEVDF"
"#;
        let cfg = RepromagicConfig::from_toml(toml).unwrap();
        let issues = cfg.validate();
        assert!(issues.iter().any(|i| i.contains("invalid characters")));
    }

    #[test]
    fn validate_no_schedulers() {
        let toml = r#"
[project]
name = "test"
"#;
        let cfg = RepromagicConfig::from_toml(toml).unwrap();
        let issues = cfg.validate();
        assert!(issues.iter().any(|i| i.contains("at least one")));
    }

    #[test]
    fn validate_builtin_with_binary() {
        let toml = r#"
[project]
name = "test"
[schedulers.eevdf]
name = "eevdf"
label = "EEVDF"
binary = "/usr/bin/something"
"#;
        let cfg = RepromagicConfig::from_toml(toml).unwrap();
        let issues = cfg.validate();
        assert!(issues
            .iter()
            .any(|i| i.contains("should not have a binary")));
    }

    #[test]
    fn validate_lavd_without_binary() {
        let toml = r#"
[project]
name = "test"
[schedulers.my_lavd]
name = "lavd"
label = "LAVD"
"#;
        let cfg = RepromagicConfig::from_toml(toml).unwrap();
        let issues = cfg.validate();
        assert!(issues.iter().any(|i| i.contains("needs a binary path")));
    }

    #[test]
    fn validate_duration_le_warmup() {
        let toml = r#"
[project]
name = "test"
[defaults]
duration = 3
warmup = 5
[schedulers.eevdf]
name = "eevdf"
label = "EEVDF"
"#;
        let cfg = RepromagicConfig::from_toml(toml).unwrap();
        let issues = cfg.validate();
        assert!(issues
            .iter()
            .any(|i| i.contains("must be > defaults.warmup")));
    }

    #[test]
    fn phenomenon_other_variant() {
        let toml = r#"
[project]
name = "custom"
phenomenon = { other = "mysterious scheduling anomaly" }
[schedulers.eevdf]
name = "eevdf"
label = "EEVDF"
"#;
        let cfg = RepromagicConfig::from_toml(toml).expect("parse failed");
        assert_eq!(
            cfg.project.phenomenon,
            Phenomenon::Other("mysterious scheduling anomaly".into())
        );
    }

    #[test]
    fn scheduler_name_other_variant() {
        let toml = r#"
[project]
name = "custom"
[schedulers.ghost]
name = { other = "ghost" }
label = "ghOSt"
binary = "bin/schedulers/ghost_agent"
"#;
        let cfg = RepromagicConfig::from_toml(toml).expect("parse failed");
        let ghost = &cfg.schedulers["ghost"];
        assert_eq!(ghost.name, SchedulerName::Other("ghost".into()));
        assert_eq!(ghost.label, "ghOSt");
    }

    #[test]
    fn thread_type_config_roundtrip() {
        let toml = r#"
[project]
name = "detailed"
[schedulers.eevdf]
name = "eevdf"
label = "EEVDF"

[workload]
foreground_threads = 2
background_threads = 8

[workload.thread_types.request_handler]
role = "foreground"
count = 2
run_us = 500
sleep_us = 1500

[workload.thread_types.cpu_hog]
role = "background"
count = 8
run_us = 130
sleep_us = 950
"#;
        let cfg = RepromagicConfig::from_toml(toml).expect("parse failed");

        let handler = &cfg.workload.thread_types["request_handler"];
        assert_eq!(handler.role, ThreadRole::Foreground);
        assert_eq!(handler.count, 2);
        assert_eq!(handler.run_us, Some(500));
        assert_eq!(handler.sleep_us, Some(1500));

        let hog = &cfg.workload.thread_types["cpu_hog"];
        assert_eq!(hog.role, ThreadRole::Background);
        assert_eq!(hog.count, 8);
        assert_eq!(hog.run_us, Some(130));

        // round-trip
        let s = cfg.to_toml().unwrap();
        let cfg2 = RepromagicConfig::from_toml(&s).unwrap();
        assert_eq!(cfg, cfg2);
    }

    #[test]
    fn display_impls() {
        assert_eq!(Phenomenon::BadTailLatency.to_string(), "bad_tail_latency");
        assert_eq!(Phenomenon::BadCpuUtil.to_string(), "bad_cpu_util");
        assert_eq!(
            Phenomenon::Other("custom".into()).to_string(),
            "other: custom"
        );
        assert_eq!(SchedulerName::Eevdf.to_string(), "eevdf");
        assert_eq!(SchedulerName::Other("ghost".into()).to_string(), "ghost");
        assert_eq!(RunMode::RtappPinned.to_string(), "rtapp_pinned");
        assert_eq!(RunMode::RtappVm.to_string(), "rtapp_vm");
        assert_eq!(RunMode::RtappSim.to_string(), "rtapp_sim");
    }

    #[test]
    fn validate_project_name_fn() {
        assert!(validate_project_name("cache_svc").is_ok());
        assert!(validate_project_name("lavd-irq-eval").is_ok());
        assert!(validate_project_name("").is_err());
        assert!(validate_project_name("my project").is_err());
        assert!(validate_project_name("foo/bar").is_err());
    }

    #[test]
    fn scheduler_is_builtin() {
        assert!(SchedulerName::Eevdf.is_builtin());
        assert!(SchedulerName::Cfs.is_builtin());
        assert!(!SchedulerName::Lavd.is_builtin());
        assert!(!SchedulerName::Other("ghost".into()).is_builtin());
    }
}
