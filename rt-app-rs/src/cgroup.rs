//! Cgroup v2 filesystem operations.
//!
//! Creates a cgroup hierarchy under `/sys/fs/cgroup/`, applies `cpu.max`
//! limits, and places threads. Cleans up on drop.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::spec::{CpuMax, RtAppSpec};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default cgroup v2 mount point on modern Linux.
const CGROUPFS_ROOT: &str = "/sys/fs/cgroup";

/// Prefix for hierarchies created by rt-app-rs (for cleanup safety).
const RTAPP_PREFIX: &str = "rtapp";

// ---------------------------------------------------------------------------
// CgroupNode — a single actualized cgroup
// ---------------------------------------------------------------------------

/// A single cgroup that has been created on the filesystem.
#[derive(Debug)]
pub struct CgroupNode {
    /// Absolute path to this cgroup directory.
    path: PathBuf,
    /// The logical spec path (e.g. "/workload/batch").
    spec_path: String,
    /// Whether we created this directory (and should remove it on cleanup).
    owned: bool,
}

impl CgroupNode {
    /// The filesystem path of this cgroup.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The logical spec path.
    pub fn spec_path(&self) -> &str {
        &self.spec_path
    }

    /// Write `cpu.max` to this cgroup.
    fn write_cpu_max(&self, cpu_max: &CpuMax) -> Result<()> {
        let cpu_max_path = self.path.join("cpu.max");
        let value = cpu_max.to_cgroupfs_string();
        fs::write(&cpu_max_path, &value)
            .with_context(|| format!("writing cpu.max={:?} to {}", value, cpu_max_path.display()))
    }

    /// Move a thread (by TID) into this cgroup.
    ///
    /// Tries `cgroup.threads` first; falls back to `cgroup.procs` if the
    /// cgroup is in domain mode (where `cgroup.threads` is not supported).
    pub fn add_thread(&self, tid: u32) -> Result<()> {
        let threads_path = self.path.join("cgroup.threads");
        if let Ok(mut file) = fs::OpenOptions::new().write(true).open(&threads_path) {
            if write!(file, "{}", tid).is_ok() {
                return Ok(());
            }
        }
        // Fallback: use cgroup.procs (works for domain-type cgroups)
        self.add_process(tid)
    }

    /// Move a process (by PID) into this cgroup.
    pub fn add_process(&self, pid: u32) -> Result<()> {
        let procs_path = self.path.join("cgroup.procs");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .open(&procs_path)
            .with_context(|| format!("opening {}", procs_path.display()))?;
        write!(file, "{}", pid)
            .with_context(|| format!("writing pid {} to {}", pid, procs_path.display()))
    }
}

// ---------------------------------------------------------------------------
// CgroupHierarchy — the full actualized tree
// ---------------------------------------------------------------------------

/// A cgroup hierarchy created on the filesystem from an `RtAppSpec`.
///
/// Owns all created cgroup directories and removes them on drop (bottom-up).
///
/// # Example
///
/// ```no_run
/// use rt_app_rs::spec::RtAppSpec;
/// use rt_app_rs::cgroup::CgroupHierarchy;
///
/// let spec = RtAppSpec::from_json(r#"{
///     "cgroups": {
///         "/workload": { "cpu.max": { "quota": "max", "period": 100000 } },
///         "/workload/batch": { "cpu.max": { "quota": 200000, "period": 100000 } }
///     },
///     "tasks": {}
/// }"#).unwrap();
///
/// let hierarchy = CgroupHierarchy::create(&spec, None).unwrap();
/// // Cgroups exist on the filesystem now.
/// // hierarchy.place_thread("/workload/batch", tid).unwrap();
/// drop(hierarchy); // Cleans up all created cgroups.
/// ```
pub struct CgroupHierarchy {
    /// Map from spec path (e.g. "/workload/batch") to actualized cgroup.
    nodes: BTreeMap<String, CgroupNode>,
    /// The unique root prefix under cgroupfs (e.g. "/sys/fs/cgroup/rtapp_a1b2c3").
    root_dir: PathBuf,
}

impl CgroupHierarchy {
    /// Create the cgroup hierarchy on the filesystem.
    ///
    /// `namespace` is an optional unique prefix; if `None`, a random one
    /// is generated. The hierarchy is created under
    /// `/sys/fs/cgroup/rtapp_<namespace>/`.
    ///
    /// Requires the `cpu` controller to be available in the cgroup v2
    /// hierarchy (which is the default on modern kernels).
    pub fn create(spec: &RtAppSpec, namespace: Option<&str>) -> Result<Self> {
        if spec.cgroups.is_empty() {
            return Ok(Self {
                nodes: BTreeMap::new(),
                root_dir: PathBuf::new(),
            });
        }

        // Verify cgroupfs is mounted
        let cgroupfs = Path::new(CGROUPFS_ROOT);
        if !cgroupfs.exists() {
            bail!(
                "cgroup v2 filesystem not found at {}. Is cgroup2 mounted?",
                CGROUPFS_ROOT
            );
        }

        // Generate a unique namespace
        let ns = match namespace {
            Some(n) => n.to_string(),
            None => {
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos();
                let pid = std::process::id();
                format!("{}_{:x}_{}", RTAPP_PREFIX, ts, pid)
            }
        };

        let root_dir = cgroupfs.join(&ns);

        // Create the root
        fs::create_dir_all(&root_dir)
            .with_context(|| format!("creating cgroup root {}", root_dir.display()))?;

        // Enable CPU controller in the root's subtree_control
        Self::enable_cpu_controller(&root_dir)?;

        let mut nodes = BTreeMap::new();

        // Sort paths so parents are created before children
        let mut sorted_paths: Vec<&String> = spec.cgroups.keys().collect();
        sorted_paths.sort();

        // Identify which paths are internal (have children in the spec).
        // A path is internal if any other spec path starts with it + "/".
        let all_paths: Vec<&String> = sorted_paths.clone();
        let is_internal = |path: &str| -> bool {
            let prefix = format!("{}/", path);
            all_paths.iter().any(|other| other.starts_with(&prefix))
        };

        for spec_path in sorted_paths {
            let cgroup_def = &spec.cgroups[spec_path];

            // Convert spec path "/workload/batch" to filesystem subpath "workload/batch"
            let relative = spec_path.strip_prefix('/').unwrap_or(spec_path);
            let fs_path = root_dir.join(relative);

            // Create the directory
            fs::create_dir_all(&fs_path)
                .with_context(|| format!("creating cgroup dir {}", fs_path.display()))?;

            // Enable cpu controller in subtree_control ONLY for internal nodes.
            // Enabling it on leaf cgroups blocks thread placement (cgroup v2 constraint:
            // no processes in a cgroup that has controllers in subtree_control).
            if is_internal(spec_path) {
                Self::try_enable_cpu_controller(&fs_path);
            }

            let node = CgroupNode {
                path: fs_path,
                spec_path: spec_path.clone(),
                owned: true,
            };

            // Apply cpu.max if specified
            if let Some(ref cpu_max) = cgroup_def.cpu_max {
                node.write_cpu_max(cpu_max)?;
            }

            nodes.insert(spec_path.clone(), node);
        }

        Ok(Self { nodes, root_dir })
    }

    /// Enable the cpu controller in a cgroup's subtree_control.
    fn enable_cpu_controller(dir: &Path) -> Result<()> {
        let subtree_control = dir.join("cgroup.subtree_control");
        if subtree_control.exists() {
            fs::write(&subtree_control, "+cpu").with_context(|| {
                format!("enabling cpu controller in {}", subtree_control.display())
            })?;
        }
        Ok(())
    }

    /// Try to enable cpu controller; ignore errors (parent might not support it,
    /// or it might already be enabled).
    fn try_enable_cpu_controller(dir: &Path) {
        let subtree_control = dir.join("cgroup.subtree_control");
        if subtree_control.exists() {
            let _ = fs::write(&subtree_control, "+cpu");
        }
    }

    /// Place a thread into a cgroup by spec path.
    pub fn place_thread(&self, spec_path: &str, tid: u32) -> Result<()> {
        let node = self
            .nodes
            .get(spec_path)
            .with_context(|| format!("cgroup {:?} not found in hierarchy", spec_path))?;
        node.add_thread(tid)
    }

    /// Place a process into a cgroup by spec path.
    pub fn place_process(&self, spec_path: &str, pid: u32) -> Result<()> {
        let node = self
            .nodes
            .get(spec_path)
            .with_context(|| format!("cgroup {:?} not found in hierarchy", spec_path))?;
        node.add_process(pid)
    }

    /// Place all tasks from the spec into their assigned cgroups.
    ///
    /// `tid_map` maps task names to thread/process IDs.
    pub fn place_tasks(&self, spec: &RtAppSpec, tid_map: &BTreeMap<String, u32>) -> Result<()> {
        for (task_name, task_def) in &spec.tasks {
            if let Some(ref cgroup_path) = task_def.cgroup {
                if let Some(&tid) = tid_map.get(task_name) {
                    self.place_process(cgroup_path, tid)?;
                }
            }
        }
        Ok(())
    }

    /// Get a reference to a cgroup node by spec path.
    pub fn get(&self, spec_path: &str) -> Option<&CgroupNode> {
        self.nodes.get(spec_path)
    }

    /// Iterate over all (spec_path, CgroupNode) pairs.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &CgroupNode)> {
        self.nodes.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// The root filesystem directory for this hierarchy.
    pub fn root_dir(&self) -> &Path {
        &self.root_dir
    }

    /// Read the current `cpu.max` value from a cgroup on the filesystem.
    pub fn read_cpu_max(&self, spec_path: &str) -> Result<CpuMax> {
        let node = self
            .nodes
            .get(spec_path)
            .with_context(|| format!("cgroup {:?} not found in hierarchy", spec_path))?;
        let cpu_max_path = node.path.join("cpu.max");
        let contents = fs::read_to_string(&cpu_max_path)
            .with_context(|| format!("reading {}", cpu_max_path.display()))?;
        CpuMax::parse(contents.trim())
    }

    /// Number of cgroup nodes in the hierarchy.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the hierarchy is empty (no cgroups defined).
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Clean up: remove all cgroup directories in reverse order (leaves first).
    fn cleanup(&mut self) {
        if self.root_dir.as_os_str().is_empty() {
            return;
        }

        // First, migrate any remaining processes out of our cgroups
        // by moving them to the parent of our root.
        let parent = self.root_dir.parent().unwrap_or(Path::new(CGROUPFS_ROOT));
        for node in self.nodes.values().rev() {
            Self::migrate_processes_to_parent(&node.path, parent);
        }

        // Remove directories in reverse sorted order (deepest paths first)
        let mut paths: Vec<PathBuf> = self
            .nodes
            .values()
            .filter(|n| n.owned)
            .map(|n| n.path.clone())
            .collect();
        paths.sort();
        paths.reverse();

        for path in &paths {
            let _ = fs::remove_dir(path);
        }

        // Finally remove the root directory
        let _ = fs::remove_dir(&self.root_dir);
    }

    /// Move all processes from a cgroup to a parent cgroup.
    fn migrate_processes_to_parent(cgroup: &Path, parent: &Path) {
        let procs_path = cgroup.join("cgroup.procs");
        let parent_procs = parent.join("cgroup.procs");

        if let Ok(contents) = fs::read_to_string(&procs_path) {
            if let Ok(mut parent_file) = fs::OpenOptions::new().write(true).open(&parent_procs) {
                for line in contents.lines() {
                    let _ = writeln!(parent_file, "{}", line);
                }
            }
        }
    }
}

impl Drop for CgroupHierarchy {
    fn drop(&mut self) {
        self.cleanup();
    }
}

impl fmt::Debug for CgroupHierarchy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CgroupHierarchy")
            .field("root_dir", &self.root_dir)
            .field("num_nodes", &self.nodes.len())
            .field("paths", &self.nodes.keys().collect::<Vec<_>>())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_hierarchy() {
        let spec = RtAppSpec::from_json(r#"{ "tasks": {} }"#).unwrap();
        let hierarchy = CgroupHierarchy::create(&spec, None).unwrap();
        assert!(hierarchy.is_empty());
        assert_eq!(hierarchy.len(), 0);
    }

    /// Test that actually creates cgroups on the filesystem.
    /// Only runs as root.
    #[test]
    fn test_create_hierarchy_requires_root() {
        if !nix::unistd::geteuid().is_root() {
            eprintln!("skipping test_create_hierarchy_requires_root (not root)");
            return;
        }

        let spec = RtAppSpec::from_json(
            r#"{
            "cgroups": {
                "/app": { "cpu.max": { "quota": "max", "period": 100000 } },
                "/app/fg": { "cpu.max": { "quota": 3600000, "period": 100000 } },
                "/app/bg": { "cpu.max": { "quota": 200000, "period": 100000 } }
            },
            "tasks": {}
        }"#,
        )
        .unwrap();

        let hierarchy = CgroupHierarchy::create(&spec, Some("rtapp_test_unit")).unwrap();

        assert_eq!(hierarchy.len(), 3);
        assert!(hierarchy.get("/app").is_some());
        assert!(hierarchy.get("/app/fg").is_some());
        assert!(hierarchy.get("/app/bg").is_some());

        // Verify directories exist
        assert!(hierarchy.get("/app").unwrap().path().exists());
        assert!(hierarchy.get("/app/fg").unwrap().path().exists());
        assert!(hierarchy.get("/app/bg").unwrap().path().exists());

        // Read back cpu.max
        let bg_max = hierarchy.read_cpu_max("/app/bg").unwrap();
        assert_eq!(bg_max.quota, Some(200000));
        assert_eq!(bg_max.period, 100000);

        let fg_max = hierarchy.read_cpu_max("/app/fg").unwrap();
        assert_eq!(fg_max.quota, Some(3600000));

        // Drop should clean up
        let root = hierarchy.root_dir().to_path_buf();
        drop(hierarchy);
        assert!(!root.exists(), "root dir should be cleaned up on drop");
    }

    #[test]
    fn test_place_current_process_requires_root() {
        if !nix::unistd::geteuid().is_root() {
            eprintln!("skipping test_place_current_process_requires_root (not root)");
            return;
        }

        let spec = RtAppSpec::from_json(
            r#"{
            "cgroups": {
                "/test": { "cpu.max": { "quota": "max", "period": 100000 } }
            },
            "tasks": {}
        }"#,
        )
        .unwrap();

        let hierarchy = CgroupHierarchy::create(&spec, Some("rtapp_test_place")).unwrap();

        // Place our own process
        let pid = std::process::id();
        hierarchy.place_process("/test", pid).unwrap();

        // Verify we're in the right cgroup
        let procs_content =
            fs::read_to_string(hierarchy.get("/test").unwrap().path().join("cgroup.procs"))
                .unwrap();
        assert!(
            procs_content.contains(&pid.to_string()),
            "our pid should appear in cgroup.procs"
        );

        // Drop will migrate us back
    }
}
