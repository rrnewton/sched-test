//! JSON spec parsing for the rt-app-rs format.
//!
//! The spec defines workloads with an optional `"cgroups"` map and per-task
//! `"cgroup"` fields. Specs without `"cgroups"` work unchanged.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::de;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

// ---------------------------------------------------------------------------
// cpu.max representation
// ---------------------------------------------------------------------------

/// CPU bandwidth limit corresponding to the cgroup v2 `cpu.max` file.
///
/// JSON format (structured):
/// ```json
/// { "quota": 200000, "period": 100000 }
/// { "quota": "max", "period": 100000 }
/// { "quota": "max" }                    // period defaults to 100000
/// ```
///
/// Kernel cgroupfs format (for writes): `"<quota> <period>"`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpuMax {
    /// Quota in microseconds per period, or `None` for "max" (unlimited).
    pub quota: Option<u64>,
    /// Period in microseconds (defaults to 100000 if not specified).
    pub period: u64,
}

impl CpuMax {
    /// The kernel default period (100ms = 100,000 µs).
    pub const DEFAULT_PERIOD: u64 = 100_000;

    /// Unlimited bandwidth ("max <period>").
    pub fn unlimited() -> Self {
        Self {
            quota: None,
            period: Self::DEFAULT_PERIOD,
        }
    }

    /// Format as the string written to `cpu.max` in cgroupfs.
    pub fn to_cgroupfs_string(&self) -> String {
        match self.quota {
            Some(q) => format!("{} {}", q, self.period),
            None => format!("max {}", self.period),
        }
    }

    /// Parse from the `"<quota> <period>"` format read from cgroupfs.
    pub fn parse(s: &str) -> Result<Self> {
        let parts: Vec<&str> = s.split_whitespace().collect();
        match parts.len() {
            1 => {
                let quota = Self::parse_quota_str(parts[0])?;
                Ok(Self {
                    quota,
                    period: Self::DEFAULT_PERIOD,
                })
            }
            2 => {
                let quota = Self::parse_quota_str(parts[0])?;
                let period: u64 = parts[1]
                    .parse()
                    .with_context(|| format!("invalid cpu.max period: {:?}", parts[1]))?;
                if period == 0 {
                    bail!("cpu.max period must be > 0");
                }
                Ok(Self { quota, period })
            }
            _ => bail!(
                "invalid cpu.max format: expected \"<quota> <period>\" or \"<quota>\", got {:?}",
                s
            ),
        }
    }

    fn parse_quota_str(s: &str) -> Result<Option<u64>> {
        if s == "max" {
            Ok(None)
        } else {
            let v: u64 = s
                .parse()
                .with_context(|| format!("invalid cpu.max quota: {:?}", s))?;
            Ok(Some(v))
        }
    }
}

impl fmt::Display for CpuMax {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_cgroupfs_string())
    }
}

/// Serde helper: the `quota` field is either a u64 or the string `"max"`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum QuotaValue {
    Limit(u64),
    Max(String),
}

/// Serde helper struct matching the JSON shape `{ "quota": ..., "period": ... }`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CpuMaxRepr {
    quota: QuotaValue,
    #[serde(default = "default_period")]
    period: u64,
}

fn default_period() -> u64 {
    CpuMax::DEFAULT_PERIOD
}

impl Serialize for CpuMax {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let repr = CpuMaxRepr {
            quota: match self.quota {
                Some(v) => QuotaValue::Limit(v),
                None => QuotaValue::Max("max".to_string()),
            },
            period: self.period,
        };
        repr.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CpuMax {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let repr = CpuMaxRepr::deserialize(deserializer)?;
        let quota = match repr.quota {
            QuotaValue::Limit(v) => Some(v),
            QuotaValue::Max(ref s) if s == "max" => None,
            QuotaValue::Max(ref s) => {
                return Err(de::Error::custom(format!(
                    "invalid quota value: expected a number or \"max\", got {:?}",
                    s
                )));
            }
        };
        Ok(CpuMax {
            quota,
            period: repr.period,
        })
    }
}

// ---------------------------------------------------------------------------
// Cgroup definition (one entry in the "cgroups" map)
// ---------------------------------------------------------------------------

/// Definition of a single cgroup's resource controls.
///
/// Currently supports only `cpu.max` (bandwidth limiting), matching the
/// production hierarchy data where 99.95% of entries use period=100000µs
/// and only `cpu.max` is set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CgroupDef {
    /// CPU bandwidth limit. Written to the cgroup's `cpu.max` file.
    #[serde(rename = "cpu.max")]
    pub cpu_max: Option<CpuMax>,
}

// ---------------------------------------------------------------------------
// Task definition (only the cgroup-relevant fields)
// ---------------------------------------------------------------------------

/// A task entry from the `"tasks"` section.
///
/// Captures the `"cgroup"` assignment along with workload definition fields
/// (phases, loops, priority, cpus, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskDef {
    /// Cgroup path this task should be placed in (must reference a key in `"cgroups"`).
    #[serde(default)]
    pub cgroup: Option<String>,

    /// Workload definition fields (phases, loops, priority, cpus, etc.).
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Top-level spec
// ---------------------------------------------------------------------------

/// The full rt-app-rs JSON specification.
///
/// Backward compatible: `cgroups` defaults to empty if absent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RtAppSpec {
    /// Global configuration (duration, policy, calibration, etc.).
    #[serde(default)]
    pub global: Option<serde_json::Value>,

    /// Cgroup hierarchy definitions. Keys are cgroup paths (e.g. "/workload/batch").
    /// Absent or empty means no cgroup setup.
    #[serde(default)]
    pub cgroups: BTreeMap<String, CgroupDef>,

    /// Task definitions. Each task may reference a cgroup path via `"cgroup"`.
    #[serde(default)]
    pub tasks: BTreeMap<String, TaskDef>,
}

impl RtAppSpec {
    /// Load a spec from a JSON file.
    pub fn load(path: &Path) -> Result<Self> {
        let contents =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::from_json(&contents)
    }

    /// Parse a spec from a JSON string.
    pub fn from_json(json: &str) -> Result<Self> {
        let spec: Self = serde_json::from_str(json).context("parsing rt-app JSON spec")?;
        spec.validate()?;
        Ok(spec)
    }

    /// Validate cross-references (task cgroup assignments point to defined cgroups).
    pub fn validate(&self) -> Result<()> {
        for (task_name, task_def) in &self.tasks {
            if let Some(ref cgroup_path) = task_def.cgroup {
                if !self.cgroups.contains_key(cgroup_path) {
                    bail!(
                        "task {:?} references cgroup {:?} which is not defined in \"cgroups\"",
                        task_name,
                        cgroup_path
                    );
                }
            }
        }

        // Validate cgroup paths look reasonable
        for path in self.cgroups.keys() {
            if !path.starts_with('/') {
                bail!(
                    "cgroup path {:?} must start with '/' (e.g. \"/workload\")",
                    path
                );
            }
            if path.contains("..") {
                bail!("cgroup path {:?} must not contain \"..\"", path);
            }
        }

        Ok(())
    }

    /// Returns true if this spec has cgroup definitions.
    pub fn has_cgroups(&self) -> bool {
        !self.cgroups.is_empty()
    }

    /// Export the global + tasks sections as plain JSON (without cgroup metadata).
    ///
    /// Useful for inspecting the workload definitions independently of the
    /// cgroup hierarchy.
    pub fn to_tasks_json(&self) -> Result<serde_json::Value> {
        let mut obj = serde_json::Map::new();

        if let Some(ref global) = self.global {
            obj.insert("global".to_string(), global.clone());
        }

        let mut tasks = serde_json::Map::new();
        for (name, def) in &self.tasks {
            tasks.insert(name.clone(), serde_json::Value::Object(def.extra.clone()));
        }
        obj.insert("tasks".to_string(), serde_json::Value::Object(tasks));

        Ok(serde_json::Value::Object(obj))
    }
}

// ---------------------------------------------------------------------------
// Production hierarchy JSON format (data/cgroup_hierarchies/)
// ---------------------------------------------------------------------------

/// A node in the production cgroup hierarchy JSON format.
///
/// Matches the schema from `data/cgroup_hierarchies/pattern_*.json`:
/// ```json
/// {
///   "name": "n0_c0_c2",
///   "cpu_max": { "quota": "200000", "period": "100000" },
///   "children": [...]
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductionNode {
    /// Encoded node name (e.g. "n0_c0_c2")
    pub name: String,

    /// CPU bandwidth limit, null at root
    pub cpu_max: Option<ProductionCpuMax>,

    /// Child nodes (absent on leaves)
    #[serde(default)]
    pub children: Vec<ProductionNode>,
}

/// cpu.max in the production JSON format (quota and period as separate string fields).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductionCpuMax {
    /// Quota: numeric string or "max"
    pub quota: String,
    /// Period: numeric string (typically "100000")
    pub period: String,
}

/// Top-level production hierarchy pattern file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductionPattern {
    /// Pattern hash ID
    pub pattern_id: String,
    /// Number of hosts with this pattern
    pub host_count: u64,
    /// Anonymized host IDs
    pub hosts: Vec<String>,
    /// The cgroup tree
    pub hierarchy: ProductionNode,
}

impl ProductionPattern {
    /// Load from a pattern JSON file.
    pub fn load(path: &Path) -> Result<Self> {
        let contents =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&contents)
            .with_context(|| format!("parsing production pattern {}", path.display()))
    }

    /// Convert this production pattern to an `RtAppSpec` cgroup map.
    ///
    /// Flattens the tree into path-keyed entries suitable for the `"cgroups"` section.
    pub fn to_cgroup_defs(&self) -> BTreeMap<String, CgroupDef> {
        let mut defs = BTreeMap::new();
        Self::flatten_node(&self.hierarchy, "", &mut defs);
        defs
    }

    fn flatten_node(
        node: &ProductionNode,
        parent_path: &str,
        defs: &mut BTreeMap<String, CgroupDef>,
    ) {
        let path = if parent_path.is_empty() {
            format!("/{}", node.name)
        } else {
            format!("{}/{}", parent_path, node.name)
        };

        let cpu_max = node.cpu_max.as_ref().map(|cm| {
            let quota = if cm.quota == "max" {
                None
            } else {
                cm.quota.parse::<u64>().ok()
            };
            let period = cm.period.parse::<u64>().unwrap_or(CpuMax::DEFAULT_PERIOD);
            CpuMax { quota, period }
        });

        defs.insert(path.clone(), CgroupDef { cpu_max });

        for child in &node.children {
            Self::flatten_node(child, &path, defs);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cpu_max_parse() {
        let cm = CpuMax::parse("200000 100000").unwrap();
        assert_eq!(cm.quota, Some(200000));
        assert_eq!(cm.period, 100000);

        let cm = CpuMax::parse("max 100000").unwrap();
        assert_eq!(cm.quota, None);
        assert_eq!(cm.period, 100000);

        let cm = CpuMax::parse("max").unwrap();
        assert_eq!(cm.quota, None);
        assert_eq!(cm.period, CpuMax::DEFAULT_PERIOD);
    }

    #[test]
    fn test_cpu_max_display() {
        let cm = CpuMax {
            quota: Some(200000),
            period: 100000,
        };
        assert_eq!(cm.to_string(), "200000 100000");
        assert_eq!(cm.to_cgroupfs_string(), "200000 100000");

        let cm = CpuMax::unlimited();
        assert_eq!(cm.to_string(), "max 100000");
    }

    #[test]
    fn test_cpu_max_serde_roundtrip() {
        let cm = CpuMax {
            quota: Some(3800000),
            period: 100000,
        };
        let json = serde_json::to_string(&cm).unwrap();
        assert_eq!(json, r#"{"quota":3800000,"period":100000}"#);

        let parsed: CpuMax = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cm);

        // "max" quota
        let cm_max = CpuMax::unlimited();
        let json_max = serde_json::to_string(&cm_max).unwrap();
        assert_eq!(json_max, r#"{"quota":"max","period":100000}"#);
        let parsed_max: CpuMax = serde_json::from_str(&json_max).unwrap();
        assert_eq!(parsed_max, cm_max);
    }

    #[test]
    fn test_cpu_max_period_defaults() {
        // Period should default to 100000 if omitted
        let json = r#"{"quota": 200000}"#;
        let cm: CpuMax = serde_json::from_str(json).unwrap();
        assert_eq!(cm.quota, Some(200000));
        assert_eq!(cm.period, CpuMax::DEFAULT_PERIOD);

        let json = r#"{"quota": "max"}"#;
        let cm: CpuMax = serde_json::from_str(json).unwrap();
        assert_eq!(cm.quota, None);
        assert_eq!(cm.period, CpuMax::DEFAULT_PERIOD);
    }

    #[test]
    fn test_spec_no_cgroups() {
        let json = r#"{
            "global": { "duration": 10 },
            "tasks": {
                "worker": { "loop": -1 }
            }
        }"#;
        let spec = RtAppSpec::from_json(json).unwrap();
        assert!(!spec.has_cgroups());
        assert_eq!(spec.tasks.len(), 1);
        assert!(spec.tasks["worker"].cgroup.is_none());
    }

    #[test]
    fn test_spec_with_cgroups() {
        let json = r#"{
            "global": { "duration": 10 },
            "cgroups": {
                "/workload": { "cpu.max": { "quota": "max", "period": 100000 } },
                "/workload/batch": { "cpu.max": { "quota": 200000, "period": 100000 } }
            },
            "tasks": {
                "worker": { "cgroup": "/workload/batch", "loop": -1 }
            }
        }"#;
        let spec = RtAppSpec::from_json(json).unwrap();
        assert!(spec.has_cgroups());
        assert_eq!(spec.cgroups.len(), 2);

        let batch = &spec.cgroups["/workload/batch"];
        assert_eq!(
            batch.cpu_max.as_ref().unwrap(),
            &CpuMax {
                quota: Some(200000),
                period: 100000,
            }
        );

        assert_eq!(
            spec.tasks["worker"].cgroup.as_deref(),
            Some("/workload/batch")
        );
    }

    #[test]
    fn test_spec_invalid_cgroup_ref() {
        let json = r#"{
            "cgroups": {
                "/workload": { "cpu.max": { "quota": "max" } }
            },
            "tasks": {
                "worker": { "cgroup": "/nonexistent", "loop": -1 }
            }
        }"#;
        let result = RtAppSpec::from_json(json);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("nonexistent"), "error was: {}", err);
    }

    #[test]
    fn test_spec_path_validation() {
        // Missing leading slash
        let json = r#"{
            "cgroups": {
                "workload": { "cpu.max": { "quota": "max" } }
            },
            "tasks": {}
        }"#;
        let result = RtAppSpec::from_json(json);
        assert!(result.is_err());

        // Path traversal
        let json = r#"{
            "cgroups": {
                "/workload/../escape": { "cpu.max": { "quota": "max" } }
            },
            "tasks": {}
        }"#;
        let result = RtAppSpec::from_json(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_tasks_json() {
        let json = r#"{
            "global": { "duration": 10 },
            "cgroups": {
                "/workload": { "cpu.max": { "quota": "max" } }
            },
            "tasks": {
                "worker": { "cgroup": "/workload", "loop": -1, "priority": 10 }
            }
        }"#;
        let spec = RtAppSpec::from_json(json).unwrap();
        let tasks_json = spec.to_tasks_json().unwrap();

        // "cgroups" section should not appear in tasks-only export
        assert!(tasks_json.get("cgroups").is_none());

        // Task should not have "cgroup" field (it's metadata, not workload)
        let task = &tasks_json["tasks"]["worker"];
        assert!(task.get("cgroup").is_none());

        // But should have the workload fields
        assert_eq!(task["loop"], -1);
        assert_eq!(task["priority"], 10);
    }

    #[test]
    fn test_production_pattern_parse() {
        let json = r#"{
            "pattern_id": "test123",
            "host_count": 1,
            "hosts": ["host_001"],
            "hierarchy": {
                "name": "n0",
                "cpu_max": null,
                "children": [
                    {
                        "name": "n0_c0",
                        "cpu_max": { "quota": "max", "period": "100000" },
                        "children": [
                            {
                                "name": "n0_c0_c0",
                                "cpu_max": { "quota": "200000", "period": "100000" }
                            }
                        ]
                    }
                ]
            }
        }"#;
        let pattern: ProductionPattern = serde_json::from_str(json).unwrap();
        assert_eq!(pattern.pattern_id, "test123");
        assert_eq!(pattern.hierarchy.children.len(), 1);

        let defs = pattern.to_cgroup_defs();
        assert_eq!(defs.len(), 3); // n0, n0_c0, n0_c0_c0

        // Root should have no cpu_max
        assert!(defs["/n0"].cpu_max.is_none());

        // n0_c0 should be unlimited
        let c0 = defs["/n0/n0_c0"].cpu_max.as_ref().unwrap();
        assert_eq!(c0.quota, None); // "max"

        // n0_c0_c0 should have quota 200000
        let c0_c0 = defs["/n0/n0_c0/n0_c0_c0"].cpu_max.as_ref().unwrap();
        assert_eq!(c0_c0.quota, Some(200000));
        assert_eq!(c0_c0.period, 100000);
    }

    #[test]
    fn test_cgroup_def_no_cpu_max() {
        let json = r#"{ }"#;
        let def: CgroupDef = serde_json::from_str(json).unwrap();
        assert!(def.cpu_max.is_none());
    }

    #[test]
    fn test_full_roundtrip_spec() {
        let json = r#"{
            "global": { "duration": 30, "calibration": "CPU0" },
            "cgroups": {
                "/app": { "cpu.max": { "quota": "max", "period": 100000 } },
                "/app/fg": { "cpu.max": { "quota": 3600000, "period": 100000 } },
                "/app/bg": { "cpu.max": { "quota": 200000, "period": 100000 } }
            },
            "tasks": {
                "fg_thread": { "cgroup": "/app/fg", "loop": -1 },
                "bg_worker": { "cgroup": "/app/bg", "loop": -1 }
            }
        }"#;
        let spec = RtAppSpec::from_json(json).unwrap();

        // Serialize back
        let reserialized = serde_json::to_string_pretty(&spec).unwrap();
        let reparsed = RtAppSpec::from_json(&reserialized).unwrap();

        assert_eq!(spec.cgroups.len(), reparsed.cgroups.len());
        assert_eq!(spec.tasks.len(), reparsed.tasks.len());
    }
}
