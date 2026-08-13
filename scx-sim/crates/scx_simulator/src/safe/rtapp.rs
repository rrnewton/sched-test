//! Parser for rt-app JSON workload descriptions.
//!
//! Converts rt-app JSON files into simulator [`Scenario`] objects, enabling
//! the same workload to run both for real (via rt-app) and in simulation.
//!
//! # Supported rt-app features
//!
//! - `run` / `runtime` — CPU-bound work (mapped to [`Phase::Run`])
//! - `sleep` — fixed-duration sleep (mapped to [`Phase::Sleep`])
//! - `suspend` — self-suspend until resumed (mapped to `Phase::Sleep(u64::MAX)`)
//! - `resume` — wake another task (mapped to [`Phase::Wake`])
//! - `yield` — call `sched_yield()` (mapped to [`Phase::Yield`])
//! - `timer` — periodic timer (approximated as `Phase::Sleep(period)`)
//! - `priority` — nice value
//! - `loop` — repetition control
//! - `phases` — multi-phase task definitions
//! - `instance` — multiple task instances
//! - `cpus` — CPU affinity mask (parsed into `TaskDef::allowed_cpus`)
//! - task-level `taskgroup` — mapped to simulator cgroups with all CPUs allowed.
//!   Both the legacy string form (`"taskgroup": "/tg1"`) and the cgroup v2
//!   object form (`"taskgroup": { "path": "/tg1", "cpu.max": "Q P", ... }`)
//!   are accepted. Implicit ancestor cgroups along the path are synthesized.
//! - `global.duration` — scenario duration
//!
//! # Limitations
//!
//! - JSON files with duplicate keys (common in rt-app) must be preprocessed
//!   with rt-app's `workgen` script or use suffixed keys (`"run0"`, `"run1"`).
//! - Unsupported events (`lock`, `unlock`, `wait`, `signal`, `broad`, `sync`,
//!   `mem`, `iorun`, `barrier`, `fork`) are skipped with a warning.
//! - Phase-level `taskgroup` migration is not modeled.

use std::collections::{BTreeSet, HashMap};

use serde_json::{Map, Value};
use tracing::{info, warn};

use crate::scenario::{
    sched_overhead_rbc_ns_from_env, seed_from_env, CgroupBandwidth, CgroupDef, IrqEvent, IrqType,
    NoiseConfig, OverheadConfig, Scenario, DEFAULT_WATCHDOG_TIMEOUT_NS,
};
use crate::task::{Phase, RepeatMode, TaskBehavior, TaskDef};
use crate::types::{CpuId, Pid};

/// Errors from parsing rt-app JSON.
#[derive(Debug)]
pub enum RtAppError {
    /// JSON parse error.
    Json(serde_json::Error),
    /// Missing required field.
    MissingField(&'static str),
    /// Invalid field value.
    InvalidValue(String),
    /// Unresolved task reference in `resume`.
    UnresolvedResume(String),
}

impl std::fmt::Display for RtAppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RtAppError::Json(e) => write!(f, "JSON parse error: {e}"),
            RtAppError::MissingField(field) => write!(f, "missing required field: {field}"),
            RtAppError::InvalidValue(msg) => write!(f, "invalid value: {msg}"),
            RtAppError::UnresolvedResume(name) => {
                write!(f, "unresolved resume target: {name:?}")
            }
        }
    }
}

impl From<serde_json::Error> for RtAppError {
    fn from(e: serde_json::Error) -> Self {
        RtAppError::Json(e)
    }
}

/// Strip C-style block comments (`/* ... */`) from input.
fn strip_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '/' {
            if chars.peek() == Some(&'*') {
                chars.next(); // consume '*'
                              // Skip until '*/'
                loop {
                    match chars.next() {
                        Some('*') if chars.peek() == Some(&'/') => {
                            chars.next(); // consume '/'
                            break;
                        }
                        Some(_) => continue,
                        None => break, // unterminated comment, just stop
                    }
                }
            } else {
                out.push(c);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Identify which rt-app event type a key represents, using prefix matching.
///
/// Returns `None` for non-event keys (like `loop`, `cpus`, `policy`, etc.).
fn classify_event(key: &str) -> Option<&'static str> {
    // Order matters: check longer prefixes first to avoid "run" matching "runtime"
    const EVENT_PREFIXES: &[&str] = &[
        "runtime", "run", "sleep", "timer", "suspend", "resume", "lock", "unlock", "wait",
        "signal", "broad", "sync", "mem", "iorun", "yield", "barrier", "fork",
    ];
    EVENT_PREFIXES
        .iter()
        .find(|&&prefix| key.len() >= prefix.len() && &key[..prefix.len()] == prefix)
        .copied()
}

/// Non-event keys that are valid at phase/task level.
const TASK_PHASE_KEYS: &[&str] = &[
    "loop",
    "phases",
    "instance",
    "delay",
    "policy",
    "priority",
    "cpus",
    "nodes_membind",
    "taskgroup",
    "dl-runtime",
    "dl-period",
    "dl-deadline",
    "util_min",
    "util_max",
];

/// Parse events from a phase/task object's key-value pairs (in insertion order).
fn parse_events(
    obj: &Map<String, Value>,
    name_to_pid: &HashMap<String, Pid>,
) -> Result<Vec<Phase>, RtAppError> {
    let mut phases = Vec::new();

    for (key, value) in obj.iter() {
        // Skip non-event keys
        if TASK_PHASE_KEYS.contains(&key.as_str()) {
            continue;
        }

        let event_type = match classify_event(key) {
            Some(t) => t,
            None => {
                // Unknown key — might be a phase-level property we don't know about
                continue;
            }
        };

        match event_type {
            "run" | "runtime" => {
                let usec = value
                    .as_u64()
                    .or_else(|| value.as_i64().map(|v| v as u64))
                    .ok_or_else(|| RtAppError::InvalidValue(format!("{key}: expected integer")))?;
                phases.push(Phase::Run(usec * 1_000)); // usec → ns
            }
            "sleep" => {
                let usec = value
                    .as_u64()
                    .or_else(|| value.as_i64().map(|v| v as u64))
                    .ok_or_else(|| RtAppError::InvalidValue(format!("{key}: expected integer")))?;
                phases.push(Phase::Sleep(usec * 1_000));
            }
            "timer" => {
                // Timer: approximate as sleep for the period duration
                let period_usec = if let Some(obj) = value.as_object() {
                    obj.get("period").and_then(|v| v.as_u64()).unwrap_or(0)
                } else {
                    value.as_u64().unwrap_or(0)
                };
                if period_usec > 0 {
                    phases.push(Phase::Sleep(period_usec * 1_000));
                }
            }
            "suspend" => {
                // Self-suspend: sleep indefinitely until woken by resume
                phases.push(Phase::Sleep(u64::MAX));
            }
            "resume" => {
                let target_name = value
                    .as_str()
                    .ok_or_else(|| RtAppError::InvalidValue(format!("{key}: expected string")))?;
                let target_pid = name_to_pid
                    .get(target_name)
                    .ok_or_else(|| RtAppError::UnresolvedResume(target_name.to_string()))?;
                phases.push(Phase::Wake(*target_pid));
            }
            "yield" => {
                // rt-app calls sched_yield() once per loop iteration for a
                // `yield` event; the JSON value is parsed but ignored (verified
                // by strace: `"yield": 1` and `"yield": 7` both yield once).
                phases.push(Phase::Yield);
            }
            unsupported => {
                warn!(
                    event = unsupported,
                    key = key.as_str(),
                    "skipping unsupported rt-app event"
                );
            }
        }
    }

    Ok(phases)
}

/// Parse an rt-app CPU affinity string into a list of CPU IDs.
///
/// rt-app supports several formats:
/// - Single CPU: `"0"` or `0`
/// - Comma-separated: `"0,2,4"`
/// - Range: `"0-3"` (expands to 0,1,2,3)
/// - Mixed: `"0,2-4,6"` (expands to 0,2,3,4,6)
/// - JSON array: `[0, 1, 2]`
fn parse_cpus(value: &Value) -> Result<Option<Vec<CpuId>>, RtAppError> {
    match value {
        Value::String(s) => {
            let mut cpus = Vec::new();
            for part in s.split(',') {
                let part = part.trim();
                if let Some((start, end)) = part.split_once('-') {
                    let start: u32 = start.trim().parse().map_err(|_| {
                        RtAppError::InvalidValue(format!("cpus: invalid range start {start:?}"))
                    })?;
                    let end: u32 = end.trim().parse().map_err(|_| {
                        RtAppError::InvalidValue(format!("cpus: invalid range end {end:?}"))
                    })?;
                    for cpu in start..=end {
                        cpus.push(CpuId(cpu));
                    }
                } else {
                    let cpu: u32 = part.parse().map_err(|_| {
                        RtAppError::InvalidValue(format!("cpus: invalid cpu {part:?}"))
                    })?;
                    cpus.push(CpuId(cpu));
                }
            }
            if cpus.is_empty() {
                Ok(None)
            } else {
                Ok(Some(cpus))
            }
        }
        Value::Number(n) => {
            let cpu = n.as_u64().ok_or_else(|| {
                RtAppError::InvalidValue(format!("cpus: expected unsigned integer, got {n}"))
            })? as u32;
            Ok(Some(vec![CpuId(cpu)]))
        }
        Value::Array(arr) => {
            let mut cpus = Vec::new();
            for v in arr {
                let cpu = v.as_u64().ok_or_else(|| {
                    RtAppError::InvalidValue(format!("cpus: array element not an integer: {v}"))
                })? as u32;
                cpus.push(CpuId(cpu));
            }
            if cpus.is_empty() {
                Ok(None)
            } else {
                Ok(Some(cpus))
            }
        }
        _ => {
            warn!("cpus: unexpected type, ignoring");
            Ok(None)
        }
    }
}

/// Normalize a taskgroup path into a leading-slash form with no trailing/empty
/// segments. Returns `None` for empty / root-only inputs.
fn normalize_taskgroup_name(raw: &str) -> Option<String> {
    let parts: Vec<_> = raw
        .trim()
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(format!("/{}", parts.join("/")))
    }
}

/// Parsed taskgroup spec: a normalized path plus optional `cpu.max` bandwidth.
#[derive(Debug, Clone)]
struct RtTaskgroupSpec {
    name: String,
    bandwidth: Option<CgroupBandwidth>,
}

/// Parse a `cpu.max`-style string ("`QUOTA PERIOD`" or "`max PERIOD`",
/// microseconds) into a [`CgroupBandwidth`]. Returns `Ok(None)` for the
/// unlimited (`max`) form; in that case the cgroup is created without a
/// bandwidth entry, matching the simulator's "no kernel CFS gate" semantics.
fn parse_cpu_max(value: &str) -> Result<Option<CgroupBandwidth>, RtAppError> {
    let parts: Vec<_> = value.split_whitespace().collect();
    if parts.len() != 2 {
        return Err(RtAppError::InvalidValue(format!(
            "taskgroup.cpu.max: expected 'max PERIOD' or 'QUOTA PERIOD', got {value:?}"
        )));
    }

    let period_us = parts[1].parse::<u64>().map_err(|_| {
        RtAppError::InvalidValue(format!("taskgroup.cpu.max: invalid period in {value:?}"))
    })?;
    if period_us == 0 {
        return Err(RtAppError::InvalidValue(format!(
            "taskgroup.cpu.max: period must be nonzero in {value:?}"
        )));
    }

    if parts[0] == "max" {
        return Ok(None);
    }

    let quota_us = parts[0].parse::<u64>().map_err(|_| {
        RtAppError::InvalidValue(format!("taskgroup.cpu.max: invalid quota in {value:?}"))
    })?;
    if quota_us == 0 {
        return Err(RtAppError::InvalidValue(format!(
            "taskgroup.cpu.max: quota must be nonzero in {value:?}"
        )));
    }

    Ok(Some(CgroupBandwidth {
        period_us,
        quota_us,
        burst_us: 0,
    }))
}

/// Parse a task's `taskgroup` field. Accepts:
///
/// - `"taskgroup": "/path"` (string form)
/// - `"taskgroup": { "path": "/path", "cpu.max": "QUOTA PERIOD", ... }`
///   (cgroup v2 object form, optional `cpu.weight` validated but ignored)
fn parse_taskgroup(obj: &Map<String, Value>) -> Result<Option<RtTaskgroupSpec>, RtAppError> {
    match obj.get("taskgroup") {
        Some(Value::String(name)) => {
            Ok(normalize_taskgroup_name(name).map(|name| RtTaskgroupSpec {
                name,
                bandwidth: None,
            }))
        }
        Some(Value::Object(spec)) => {
            let path = spec
                .get("path")
                .or_else(|| spec.get("name"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    RtAppError::InvalidValue(
                        "taskgroup: object requires string 'path' or 'name'".into(),
                    )
                })?;

            let Some(name) = normalize_taskgroup_name(path) else {
                return Ok(None);
            };

            if let Some(weight) = spec.get("cpu.weight").or_else(|| spec.get("cpu_weight")) {
                let weight = weight.as_u64().ok_or_else(|| {
                    RtAppError::InvalidValue(format!(
                        "taskgroup.cpu.weight: expected integer, got {weight}"
                    ))
                })?;
                if !(1..=10_000).contains(&weight) {
                    return Err(RtAppError::InvalidValue(format!(
                        "taskgroup.cpu.weight: expected 1..=10000, got {weight}"
                    )));
                }
            }

            let bandwidth = match spec.get("cpu.max").or_else(|| spec.get("cpu_max")) {
                Some(Value::String(cpu_max)) => parse_cpu_max(cpu_max)?,
                Some(v) => {
                    return Err(RtAppError::InvalidValue(format!(
                        "taskgroup.cpu.max: expected string, got {v}"
                    )));
                }
                None => None,
            };

            Ok(Some(RtTaskgroupSpec { name, bandwidth }))
        }
        Some(Value::Null) | None => Ok(None),
        Some(v) => Err(RtAppError::InvalidValue(format!(
            "taskgroup: expected string or object, got {v}"
        ))),
    }
}

fn same_bandwidth(a: &CgroupBandwidth, b: &CgroupBandwidth) -> bool {
    a.period_us == b.period_us && a.quota_us == b.quota_us && a.burst_us == b.burst_us
}

/// Stash the bandwidth values from a parsed `taskgroup` so they can be
/// attached to the synthesized `CgroupDef` after all tasks are parsed. If the
/// same taskgroup is referenced by multiple tasks with conflicting `cpu.max`
/// values, fail loudly rather than silently picking one.
fn record_taskgroup_bandwidth(
    cgroup_bandwidth: &mut HashMap<String, CgroupBandwidth>,
    spec: &RtTaskgroupSpec,
) -> Result<(), RtAppError> {
    let Some(bandwidth) = &spec.bandwidth else {
        return Ok(());
    };

    if let Some(existing) = cgroup_bandwidth.get(&spec.name) {
        if !same_bandwidth(existing, bandwidth) {
            return Err(RtAppError::InvalidValue(format!(
                "taskgroup {:?}: conflicting cpu.max values across tasks",
                spec.name
            )));
        }
        return Ok(());
    }

    cgroup_bandwidth.insert(spec.name.clone(), bandwidth.clone());
    Ok(())
}

/// Synthesize implicit `CgroupDef` entries for every taskgroup path referenced
/// by a task. Each path component (e.g. `/a/b/c`) materializes one cgroup at
/// each level (`/a`, `/a/b`, `/a/b/c`), parented to the previous level so the
/// engine's hierarchy walk finds the correct ancestors. All synthesized
/// cgroups inherit the full CPU set; bandwidth is attached only to the leaf
/// path that originally carried `cpu.max` data.
fn cgroup_defs_for_tasks(
    tasks: &[TaskDef],
    nr_cpus: u32,
    cgroup_bandwidth: &HashMap<String, CgroupBandwidth>,
) -> Vec<CgroupDef> {
    let mut names = BTreeSet::new();
    for task in tasks {
        let Some(name) = &task.cgroup_name else {
            continue;
        };

        let mut path = String::new();
        for part in name.split('/').filter(|part| !part.is_empty()) {
            path.push('/');
            path.push_str(part);
            names.insert(path.clone());
        }
    }

    let all_cpus: Vec<_> = (0..nr_cpus).map(CpuId).collect();
    names
        .into_iter()
        .map(|name| {
            let parent_name = name
                .rsplit_once('/')
                .and_then(|(parent, _)| (!parent.is_empty()).then(|| parent.to_string()));
            CgroupDef {
                bandwidth: cgroup_bandwidth.get(&name).cloned(),
                name,
                parent_name,
                cpuset: Some(all_cpus.clone()),
            }
        })
        .collect()
}

/// Parse a single rt-app task object into one or more `TaskDef`s.
///
/// Multiple `TaskDef`s are produced when `instance > 1`.
fn parse_task(
    name: &str,
    obj: &Map<String, Value>,
    pid_start: &mut i32,
    name_to_pid: &HashMap<String, Pid>,
    cgroup_bandwidth: &mut HashMap<String, CgroupBandwidth>,
) -> Result<Vec<TaskDef>, RtAppError> {
    let instance_count = obj.get("instance").and_then(|v| v.as_u64()).unwrap_or(1) as u32;

    let nice = obj
        .get("priority")
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
        .clamp(-20, 19) as i8;

    let loop_count = obj.get("loop").and_then(|v| v.as_i64()).unwrap_or(-1);

    // Parse CPU affinity
    let allowed_cpus = if let Some(cpus_val) = obj.get("cpus") {
        parse_cpus(cpus_val)?
    } else {
        None
    };

    let taskgroup = parse_taskgroup(obj)?;
    if let Some(spec) = &taskgroup {
        record_taskgroup_bandwidth(cgroup_bandwidth, spec)?;
    }
    let cgroup_name = taskgroup.map(|spec| spec.name);

    // Parse phases
    let all_phases = if let Some(phases_val) = obj.get("phases") {
        // Multi-phase task: each sub-object is a named phase
        let phases_obj = phases_val
            .as_object()
            .ok_or_else(|| RtAppError::InvalidValue("phases: expected object".into()))?;

        let mut all = Vec::new();
        for (_phase_name, phase_val) in phases_obj.iter() {
            let phase_obj = phase_val
                .as_object()
                .ok_or_else(|| RtAppError::InvalidValue("phase: expected object".into()))?;

            let phase_loop = phase_obj.get("loop").and_then(|v| v.as_i64()).unwrap_or(1);

            let events = parse_events(phase_obj, name_to_pid)?;

            if phase_loop <= 0 || phase_loop == 1 {
                all.extend(events);
            } else {
                for _ in 0..phase_loop {
                    all.extend(events.clone());
                }
            }
        }
        all
    } else {
        // Single-phase task: events are directly in the task object
        parse_events(obj, name_to_pid)?
    };

    if all_phases.is_empty() {
        warn!(task = name, "task has no events, skipping");
        return Ok(Vec::new());
    }

    // Handle loop: -1 means repeat forever, N>1 uses RepeatMode::Count
    let (final_phases, repeat) = if loop_count < 0 {
        (all_phases, RepeatMode::Forever)
    } else if loop_count <= 1 {
        (all_phases, RepeatMode::Once)
    } else {
        (all_phases, RepeatMode::Count(loop_count as u32))
    };

    // Create task instances
    let mut defs = Vec::new();
    for i in 0..instance_count {
        let task_name = if instance_count == 1 {
            name.to_string()
        } else {
            format!("{name}-{i}")
        };
        let pid = Pid(*pid_start);
        *pid_start += 1;

        defs.push(TaskDef {
            name: task_name,
            pid,
            nice,
            behavior: TaskBehavior {
                phases: final_phases.clone(),
                repeat,
            },
            start_time_ns: 0,
            mm_id: None,
            allowed_cpus: allowed_cpus.clone(),
            parent_pid: None,
            cgroup_name: cgroup_name.clone(),
            task_flags: 0,
            migration_disabled: 0,
        });
    }

    Ok(defs)
}

/// Extract run and sleep durations (in ns) from an irq_gen task definition.
///
/// Supports both flat (`"run": N, "sleep": M`) and phased layouts
/// (`"phases": { "irq_work": { "run": N }, "idle": { "sleep": M } }`).
fn extract_irq_gen_timing(obj: &Map<String, Value>) -> Result<(u64, u64), RtAppError> {
    // Try phased layout first (the v09 config uses this).
    if let Some(phases_val) = obj.get("phases") {
        if let Some(phases_obj) = phases_val.as_object() {
            let mut run_ns: Option<u64> = None;
            let mut sleep_ns: Option<u64> = None;

            for (_phase_name, phase_val) in phases_obj.iter() {
                if let Some(phase_obj) = phase_val.as_object() {
                    if let Some(r) = phase_obj
                        .get("run")
                        .and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|i| i as u64)))
                    {
                        run_ns = Some(r * 1_000); // usec → ns
                    }
                    if let Some(s) = phase_obj
                        .get("sleep")
                        .and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|i| i as u64)))
                    {
                        sleep_ns = Some(s * 1_000);
                    }
                }
            }

            if let (Some(r), Some(s)) = (run_ns, sleep_ns) {
                return Ok((r, s));
            }
        }
    }

    // Try flat layout.
    let run_us = obj
        .get("run")
        .and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|i| i as u64)));
    let sleep_us = obj
        .get("sleep")
        .and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|i| i as u64)));

    match (run_us, sleep_us) {
        (Some(r), Some(s)) => Ok((r * 1_000, s * 1_000)),
        _ => Err(RtAppError::InvalidValue(
            "irq_gen task missing run/sleep durations".into(),
        )),
    }
}

/// Load an rt-app JSON workload and convert it to a simulator [`Scenario`].
///
/// # Arguments
///
/// * `json_str` — Raw JSON string (may contain C-style `/* */` comments).
/// * `nr_cpus` — Number of simulated CPUs (rt-app doesn't specify this).
///
/// # Example
///
/// ```rust,no_run
/// use scx_simulator::load_rtapp;
///
/// let json = r#"{
///     "global": { "duration": 1 },
///     "tasks": {
///         "worker": {
///             "loop": -1,
///             "run": 5000,
///             "sleep": 5000
///         }
///     }
/// }"#;
///
/// let scenario = load_rtapp(json, 4).unwrap();
/// ```
pub fn load_rtapp(json_str: &str, nr_cpus: u32) -> Result<Scenario, RtAppError> {
    if nr_cpus == 0 {
        return Err(RtAppError::InvalidValue(
            "nr_cpus must be at least 1".into(),
        ));
    }
    let cleaned = strip_comments(json_str);
    let root: Value = serde_json::from_str(&cleaned)?;
    let root_obj = root
        .as_object()
        .ok_or(RtAppError::MissingField("root object"))?;

    // Parse global settings
    let duration_ns = if let Some(global) = root_obj.get("global") {
        let dur_secs = global
            .get("duration")
            .and_then(|v| v.as_i64())
            .unwrap_or(-1);
        if dur_secs > 0 {
            dur_secs as u64 * 1_000_000_000
        } else {
            // Default: 10 seconds if not specified or infinite
            10_000_000_000
        }
    } else {
        10_000_000_000
    };

    let tasks_obj = root_obj
        .get("tasks")
        .and_then(|v| v.as_object())
        .ok_or(RtAppError::MissingField("tasks"))?;

    // First pass: assign PIDs to build name→pid map
    let mut name_to_pid: HashMap<String, Pid> = HashMap::new();
    let mut pid_counter: i32 = 1;
    for (task_name, task_val) in tasks_obj.iter() {
        let instance_count = task_val
            .as_object()
            .and_then(|o| o.get("instance"))
            .and_then(|v| v.as_u64())
            .unwrap_or(1) as u32;

        if instance_count == 1 {
            name_to_pid.insert(task_name.clone(), Pid(pid_counter));
            pid_counter += 1;
        } else {
            // Map the base name to the first instance
            name_to_pid.insert(task_name.clone(), Pid(pid_counter));
            for i in 0..instance_count {
                let inst_name = format!("{task_name}-{i}");
                name_to_pid.insert(inst_name, Pid(pid_counter));
                pid_counter += 1;
            }
        }
    }

    // Second pass: parse tasks with name→pid resolution.
    // Detect irq_gen_* tasks and convert them to IrqEvent entries instead
    // of regular tasks.  In production, IRQs fire outside the scheduler's
    // control; the irq_gen tasks in rt-app specs are only meaningful for
    // VM/pinned modes where they generate real softirqs.  In simulation we
    // must model them as simulator-level IRQ injection so that
    // bpf_in_serving_softirq(), rq->clock_task, etc. behave correctly.
    let mut all_tasks: Vec<TaskDef> = Vec::new();
    let mut irq_events: Vec<IrqEvent> = Vec::new();
    let mut cgroup_bandwidth: HashMap<String, CgroupBandwidth> = HashMap::new();
    let mut pid_counter: i32 = 1;
    for (task_name, task_val) in tasks_obj.iter() {
        let task_obj = task_val.as_object().ok_or_else(|| {
            RtAppError::InvalidValue(format!("task {task_name}: expected object"))
        })?;

        if task_name.starts_with("irq_gen") {
            // Convert to periodic IrqEvents instead of a scheduled task.
            // Still consume a PID slot to keep PID numbering stable.
            let _pid = pid_counter;
            pid_counter += 1;

            // Extract the pinned CPU from the affinity mask.
            let cpu = if let Some(cpus_val) = task_obj.get("cpus") {
                let cpus = parse_cpus(cpus_val)?;
                match cpus {
                    Some(ref c) if c.len() == 1 => {
                        if c[0].0 >= nr_cpus {
                            return Err(RtAppError::InvalidValue(format!(
                                "irq_gen task {task_name:?} targets CPU {}, but only \
                                 {nr_cpus} CPUs are configured (valid range: 0..{})",
                                c[0].0,
                                nr_cpus - 1,
                            )));
                        }
                        c[0]
                    }
                    _ => {
                        warn!(
                            task = task_name.as_str(),
                            "irq_gen task without single-CPU pinning; using CPU 0"
                        );
                        CpuId(0)
                    }
                }
            } else {
                warn!(
                    task = task_name.as_str(),
                    "irq_gen task without cpus field; using CPU 0"
                );
                CpuId(0)
            };

            // Extract run and sleep durations from phases (or top-level).
            let (run_ns, sleep_ns) = extract_irq_gen_timing(task_obj)?;
            let period_ns = run_ns + sleep_ns;

            // Generate periodic softirq events for the full duration.
            let events_before = irq_events.len();
            let mut t: u64 = 0;
            while t <= duration_ns {
                irq_events.push(IrqEvent {
                    cpu,
                    at_ns: t,
                    duration_ns: run_ns,
                    irq_type: IrqType::SoftIrq,
                    wake_pids: Vec::new(),
                });
                t += period_ns;
            }
            let events_added = irq_events.len() - events_before;

            info!(
                task = task_name.as_str(),
                cpu = cpu.0,
                run_us = run_ns / 1_000,
                period_us = period_ns / 1_000,
                events = events_added,
                "converted irq_gen task to periodic softirq events"
            );
            continue;
        }

        let defs = parse_task(
            task_name,
            task_obj,
            &mut pid_counter,
            &name_to_pid,
            &mut cgroup_bandwidth,
        )?;
        all_tasks.extend(defs);
    }

    if all_tasks.is_empty() {
        return Err(RtAppError::InvalidValue(
            "no tasks with events found".into(),
        ));
    }

    // Validate CPU affinity entries against available CPUs.
    for def in &all_tasks {
        if let Some(ref cpus) = def.allowed_cpus {
            for cpu in cpus {
                if cpu.0 >= nr_cpus {
                    return Err(RtAppError::InvalidValue(format!(
                        "task {:?} has CPU affinity for CPU {}, but only {} CPUs \
                         are configured (valid range: 0..{})",
                        def.name,
                        cpu.0,
                        nr_cpus,
                        nr_cpus - 1,
                    )));
                }
            }
        }
    }

    let cgroups = cgroup_defs_for_tasks(&all_tasks, nr_cpus, &cgroup_bandwidth);

    Ok(Scenario {
        nr_cpus,
        smt_threads_per_core: 1,
        cpus_per_llc: 0,
        tasks: all_tasks,
        cgroups,
        duration_ns,
        noise: NoiseConfig::from_env(),
        overhead: OverheadConfig::from_env(),
        seed: seed_from_env(),
        fixed_priority: false,
        sched_overhead_rbc_ns: sched_overhead_rbc_ns_from_env(),
        // Explicit iff the operator actually set SCX_SIM_RBC_NS; the
        // unset case yields the Some(10) default, which must not hard-fail
        // on a PMU-less host.
        rbc_explicitly_requested: std::env::var_os("SCX_SIM_RBC_NS").is_some(),
        watchdog_timeout_ns: Some(DEFAULT_WATCHDOG_TIMEOUT_NS),
        ignore_bpf_errors: true,
        hotplug_events: Vec::new(),
        cpu_preempt_events: Vec::new(),
        cgroup_migrate_events: Vec::new(),
        task_rename_events: Vec::new(),
        cgroup_create_events: Vec::new(),
        cgroup_destroy_events: Vec::new(),
        cgroup_cpuset_change_events: Vec::new(),
        interleave: false,
        stochastic_timer_interleave: false,
        stochastic_timer_interleave_window_ns: 20_000_000,
        stochastic_timer_interleave_one_in: 4,
        targeted_cbw_yield_sites: false,
        targeted_cbw_yield_window_ns: 100_000_000,
        targeted_cbw_yield_limit: 1,
        preemptive: None,
        replay_trace: None,
        no_pmu_signal: false,
        max_cgroups: crate::cgroup::DEFAULT_MAX_CGROUPS,
        irq_events,
        futex_events: Vec::new(),
        native_concurrent: None,
        wait_debugger: false,
        warmup_ns: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_comments() {
        assert_eq!(strip_comments("hello /* world */ foo"), "hello  foo");
        assert_eq!(strip_comments("no comments"), "no comments");
        assert_eq!(strip_comments("a /* b /* nested */ c"), "a  c");
    }

    #[test]
    fn test_classify_event() {
        assert_eq!(classify_event("run"), Some("run"));
        assert_eq!(classify_event("run0"), Some("run"));
        assert_eq!(classify_event("runtime"), Some("runtime"));
        assert_eq!(classify_event("runtime1"), Some("runtime"));
        assert_eq!(classify_event("sleep"), Some("sleep"));
        assert_eq!(classify_event("sleep0"), Some("sleep"));
        assert_eq!(classify_event("timer"), Some("timer"));
        assert_eq!(classify_event("suspend"), Some("suspend"));
        assert_eq!(classify_event("resume"), Some("resume"));
        assert_eq!(classify_event("lock"), Some("lock"));
        assert_eq!(classify_event("loop"), None);
        assert_eq!(classify_event("cpus"), None);
        assert_eq!(classify_event("priority"), None);
    }

    #[test]
    fn test_simple_workload() {
        let json = r#"{
            "global": { "duration": 1 },
            "tasks": {
                "worker": {
                    "loop": -1,
                    "run": 5000,
                    "sleep": 5000
                }
            }
        }"#;

        let scenario = load_rtapp(json, 2).unwrap();
        assert_eq!(scenario.nr_cpus, 2);
        assert_eq!(scenario.duration_ns, 1_000_000_000);
        assert_eq!(scenario.tasks.len(), 1);

        let task = &scenario.tasks[0];
        assert_eq!(task.name, "worker");
        assert_eq!(task.nice, 0);
        assert_eq!(task.behavior.repeat, RepeatMode::Forever);
        assert_eq!(task.behavior.phases.len(), 2);
        assert!(matches!(task.behavior.phases[0], Phase::Run(5_000_000)));
        assert!(matches!(task.behavior.phases[1], Phase::Sleep(5_000_000)));
    }

    #[test]
    fn test_suspend_resume() {
        let json = r#"{
            "tasks": {
                "producer": {
                    "loop": -1,
                    "run": 10000,
                    "resume": "consumer",
                    "sleep": 20000
                },
                "consumer": {
                    "loop": -1,
                    "suspend": "consumer",
                    "run": 5000
                }
            }
        }"#;

        let scenario = load_rtapp(json, 2).unwrap();
        assert_eq!(scenario.tasks.len(), 2);

        let producer = &scenario.tasks[0];
        assert_eq!(producer.name, "producer");
        assert_eq!(producer.behavior.phases.len(), 3);
        assert!(matches!(
            producer.behavior.phases[0],
            Phase::Run(10_000_000)
        ));
        assert!(matches!(producer.behavior.phases[1], Phase::Wake(Pid(2)))); // consumer pid
        assert!(matches!(
            producer.behavior.phases[2],
            Phase::Sleep(20_000_000)
        ));

        let consumer = &scenario.tasks[1];
        assert_eq!(consumer.name, "consumer");
        assert_eq!(consumer.behavior.phases.len(), 2);
        assert!(matches!(
            consumer.behavior.phases[0],
            Phase::Sleep(u64::MAX)
        )); // suspend
        assert!(matches!(consumer.behavior.phases[1], Phase::Run(5_000_000)));
    }

    #[test]
    fn test_multi_phase() {
        let json = r#"{
            "tasks": {
                "task1": {
                    "loop": -1,
                    "phases": {
                        "active": {
                            "loop": 2,
                            "run": 1000
                        },
                        "idle": {
                            "sleep": 5000
                        }
                    }
                }
            }
        }"#;

        let scenario = load_rtapp(json, 1).unwrap();
        let task = &scenario.tasks[0];
        // phase "active" with loop=2 should expand to 2 runs, then 1 sleep
        assert_eq!(task.behavior.phases.len(), 3);
        assert!(matches!(task.behavior.phases[0], Phase::Run(1_000_000)));
        assert!(matches!(task.behavior.phases[1], Phase::Run(1_000_000)));
        assert!(matches!(task.behavior.phases[2], Phase::Sleep(5_000_000)));
    }

    #[test]
    fn test_instances() {
        let json = r#"{
            "tasks": {
                "worker": {
                    "instance": 3,
                    "loop": -1,
                    "run": 10000
                }
            }
        }"#;

        let scenario = load_rtapp(json, 4).unwrap();
        assert_eq!(scenario.tasks.len(), 3);
        assert_eq!(scenario.tasks[0].name, "worker-0");
        assert_eq!(scenario.tasks[1].name, "worker-1");
        assert_eq!(scenario.tasks[2].name, "worker-2");
        // Each should have unique PIDs
        assert_ne!(scenario.tasks[0].pid, scenario.tasks[1].pid);
    }

    #[test]
    fn test_nice_priority() {
        let json = r#"{
            "tasks": {
                "high": { "priority": -19, "loop": -1, "run": 1000 },
                "low":  { "priority": 10, "loop": -1, "run": 1000 }
            }
        }"#;

        let scenario = load_rtapp(json, 1).unwrap();
        assert_eq!(scenario.tasks[0].nice, -19);
        assert_eq!(scenario.tasks[1].nice, 10);
    }

    #[test]
    fn test_comments_and_timer() {
        let json = r#"{
            /* This is a comment */
            "tasks": {
                "periodic": {
                    "loop": -1,
                    "run": 2000,
                    "timer": { "ref": "tick", "period": 16667 }
                }
            }
        }"#;

        let scenario = load_rtapp(json, 1).unwrap();
        let task = &scenario.tasks[0];
        assert_eq!(task.behavior.phases.len(), 2);
        assert!(matches!(task.behavior.phases[0], Phase::Run(2_000_000)));
        // timer period 16667 usec = 16667000 ns
        assert!(matches!(task.behavior.phases[1], Phase::Sleep(16_667_000)));
    }

    #[test]
    fn test_parse_cpus_string_single() {
        let v = serde_json::json!("0");
        let cpus = parse_cpus(&v).unwrap();
        assert_eq!(cpus, Some(vec![CpuId(0)]));
    }

    #[test]
    fn test_parse_cpus_string_list() {
        let v = serde_json::json!("0,2,4");
        let cpus = parse_cpus(&v).unwrap();
        assert_eq!(cpus, Some(vec![CpuId(0), CpuId(2), CpuId(4)]));
    }

    #[test]
    fn test_parse_cpus_string_range() {
        let v = serde_json::json!("1-3");
        let cpus = parse_cpus(&v).unwrap();
        assert_eq!(cpus, Some(vec![CpuId(1), CpuId(2), CpuId(3)]));
    }

    #[test]
    fn test_parse_cpus_string_mixed() {
        let v = serde_json::json!("0,2-4,6");
        let cpus = parse_cpus(&v).unwrap();
        assert_eq!(
            cpus,
            Some(vec![CpuId(0), CpuId(2), CpuId(3), CpuId(4), CpuId(6)])
        );
    }

    #[test]
    fn test_parse_cpus_number() {
        let v = serde_json::json!(3);
        let cpus = parse_cpus(&v).unwrap();
        assert_eq!(cpus, Some(vec![CpuId(3)]));
    }

    #[test]
    fn test_parse_cpus_array() {
        let v = serde_json::json!([0, 1, 3]);
        let cpus = parse_cpus(&v).unwrap();
        assert_eq!(cpus, Some(vec![CpuId(0), CpuId(1), CpuId(3)]));
    }

    #[test]
    fn test_parse_cpus_in_workload() {
        let json = r#"{
            "global": { "duration": 1 },
            "tasks": {
                "pinned": {
                    "loop": -1,
                    "cpus": "0,1",
                    "run": 5000
                }
            }
        }"#;

        let scenario = load_rtapp(json, 4).unwrap();
        let task = &scenario.tasks[0];
        assert_eq!(task.allowed_cpus, Some(vec![CpuId(0), CpuId(1)]));
    }

    #[test]
    fn test_cpu_affinity_oob_rejected() {
        let json = r#"{
            "global": { "duration": 1 },
            "tasks": {
                "pinned": {
                    "loop": -1,
                    "cpus": [99, 100],
                    "run": 5000,
                    "sleep": 5000
                }
            }
        }"#;

        let err = load_rtapp(json, 4).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("CPU 99") && msg.contains("4 CPUs"),
            "expected clear OOB error, got: {msg}"
        );
    }

    #[test]
    fn test_zero_cpus_rejected() {
        let json = r#"{
            "global": { "duration": 1 },
            "tasks": {
                "t1": { "loop": -1, "run": 5000 }
            }
        }"#;

        let err = load_rtapp(json, 0).unwrap_err();
        assert!(
            err.to_string().contains("nr_cpus must be at least 1"),
            "expected nr_cpus validation error, got: {err}"
        );
    }

    // -- taskgroup → implicit cgroup synthesis (Diff 2/5) --

    #[test]
    fn test_normalize_taskgroup_name_simple() {
        assert_eq!(normalize_taskgroup_name("/tg1").as_deref(), Some("/tg1"));
        assert_eq!(normalize_taskgroup_name("tg1").as_deref(), Some("/tg1"));
        assert_eq!(
            normalize_taskgroup_name("/a/b/c").as_deref(),
            Some("/a/b/c")
        );
        assert_eq!(
            normalize_taskgroup_name("//a//b//").as_deref(),
            Some("/a/b")
        );
        assert_eq!(normalize_taskgroup_name("/"), None);
        assert_eq!(normalize_taskgroup_name(""), None);
    }

    #[test]
    fn test_taskgroup_string_form_synthesizes_cgroup() {
        let json = r#"{
            "global": { "duration": 1 },
            "tasks": {
                "runner": {
                    "loop": -1,
                    "run": 20000,
                    "sleep": 80000,
                    "taskgroup": "/tg_string"
                }
            }
        }"#;

        let scenario = load_rtapp(json, 4).unwrap();

        // Implicit cgroup synthesized from the taskgroup field.
        assert_eq!(scenario.cgroups.len(), 1);
        assert_eq!(scenario.cgroups[0].name, "/tg_string");
        assert_eq!(scenario.cgroups[0].parent_name, None);
        // No cpu.max in string form -> no bandwidth.
        assert!(scenario.cgroups[0].bandwidth.is_none());
        // All-CPUs cpuset.
        assert_eq!(
            scenario.cgroups[0].cpuset,
            Some(vec![CpuId(0), CpuId(1), CpuId(2), CpuId(3)])
        );
        // Task assignment.
        assert_eq!(scenario.tasks[0].cgroup_name.as_deref(), Some("/tg_string"));
    }

    #[test]
    fn test_taskgroup_object_form_with_cpu_max() {
        let json = r#"{
            "global": { "duration": 1 },
            "tasks": {
                "runner": {
                    "loop": -1,
                    "run": 20000,
                    "taskgroup": {
                        "path": "/tg_v2",
                        "cpu.weight": 250,
                        "cpu.max": "200000 100000"
                    }
                }
            }
        }"#;

        let scenario = load_rtapp(json, 2).unwrap();
        assert_eq!(scenario.cgroups.len(), 1);
        assert_eq!(scenario.cgroups[0].name, "/tg_v2");
        assert_eq!(scenario.tasks[0].cgroup_name.as_deref(), Some("/tg_v2"));

        let bw = scenario.cgroups[0]
            .bandwidth
            .as_ref()
            .expect("expected cpu.max bandwidth");
        assert_eq!(bw.period_us, 100_000);
        assert_eq!(bw.quota_us, 200_000);
        assert_eq!(bw.burst_us, 0);
    }

    #[test]
    fn test_taskgroup_object_form_cpu_max_max_is_unlimited() {
        let json = r#"{
            "global": { "duration": 1 },
            "tasks": {
                "runner": {
                    "loop": -1,
                    "run": 20000,
                    "taskgroup": {
                        "path": "/unlimited",
                        "cpu.max": "max 100000"
                    }
                }
            }
        }"#;

        let scenario = load_rtapp(json, 2).unwrap();
        assert_eq!(scenario.cgroups.len(), 1);
        assert_eq!(scenario.cgroups[0].name, "/unlimited");
        assert!(scenario.cgroups[0].bandwidth.is_none());
    }

    #[test]
    fn test_taskgroup_nested_path_synthesizes_ancestors() {
        let json = r#"{
            "global": { "duration": 1 },
            "tasks": {
                "leaf": {
                    "loop": -1,
                    "run": 20000,
                    "taskgroup": {
                        "path": "/a/b/c",
                        "cpu.max": "10000 100000"
                    }
                }
            }
        }"#;

        let scenario = load_rtapp(json, 2).unwrap();
        // /a, /a/b, /a/b/c — three implicit cgroups (BTreeSet -> sorted).
        let names: Vec<_> = scenario.cgroups.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["/a", "/a/b", "/a/b/c"]);

        // Hierarchy: each level parents the next.
        assert_eq!(scenario.cgroups[0].parent_name, None);
        assert_eq!(scenario.cgroups[1].parent_name.as_deref(), Some("/a"));
        assert_eq!(scenario.cgroups[2].parent_name.as_deref(), Some("/a/b"));

        // Bandwidth attached only to the leaf.
        assert!(scenario.cgroups[0].bandwidth.is_none());
        assert!(scenario.cgroups[1].bandwidth.is_none());
        let leaf_bw = scenario.cgroups[2]
            .bandwidth
            .as_ref()
            .expect("leaf cgroup should carry the cpu.max bandwidth");
        assert_eq!(leaf_bw.quota_us, 10_000);
        assert_eq!(leaf_bw.period_us, 100_000);
    }

    #[test]
    fn test_taskgroup_conflicting_cpu_max_rejected() {
        // Two tasks reference the same taskgroup path but with different
        // cpu.max values — must be rejected, not silently merged.
        let json = r#"{
            "global": { "duration": 1 },
            "tasks": {
                "a": {
                    "loop": -1,
                    "run": 5000,
                    "taskgroup": { "path": "/shared", "cpu.max": "10000 100000" }
                },
                "b": {
                    "loop": -1,
                    "run": 5000,
                    "taskgroup": { "path": "/shared", "cpu.max": "20000 100000" }
                }
            }
        }"#;

        let err = load_rtapp(json, 2).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("conflicting cpu.max"),
            "expected conflicting-cpu.max error, got: {msg}"
        );
    }

    #[test]
    fn test_taskgroup_invalid_cpu_max_rejected() {
        let json = r#"{
            "global": { "duration": 1 },
            "tasks": {
                "t": {
                    "loop": -1,
                    "run": 5000,
                    "taskgroup": { "path": "/x", "cpu.max": "10000" }
                }
            }
        }"#;
        let err = load_rtapp(json, 2).unwrap_err();
        assert!(err.to_string().contains("expected 'max PERIOD'"));
    }

    #[test]
    fn test_taskgroup_only_spec_synthesizes_cgroup_with_bandwidth() {
        // End-to-end parser test: a taskgroup-only rt-app spec (no top-level
        // `cgroup` block) is parsed into Scenario.cgroups with the per-cgroup
        // bandwidth fields populated from the inline `cpu.max` field.
        //
        // (Pre-shrink this test also exercised the engine-side
        // BandwidthManager bridge -- removed in `tg shrink-rust-
        // bandwidthmanager-518-to-30-lines-no-fake-approximation`. The
        // library's `scx_cgroup_bw_init` now configures per-cgroup
        // bandwidth state via the `cgroup_set_bandwidth` callback at
        // scenario load; the engine no longer maintains a parallel mirror.)

        let json = r#"{
            "global": { "duration": 1 },
            "tasks": {
                "stop_worker": {
                    "instance": 4,
                    "loop": -1,
                    "run": 100000,
                    "taskgroup": {
                        "path": "/test_bw_stop",
                        "cpu.max": "10000 100000"
                    }
                }
            }
        }"#;

        let scenario = load_rtapp(json, 4).unwrap();

        // Synthesized cgroup with bandwidth.
        assert_eq!(scenario.cgroups.len(), 1);
        assert_eq!(scenario.cgroups[0].name, "/test_bw_stop");
        let bw = scenario.cgroups[0]
            .bandwidth
            .as_ref()
            .expect("expected synthesized bandwidth");
        assert_eq!(bw.quota_us, 10_000);
        assert_eq!(bw.period_us, 100_000);

        // All four task instances assigned.
        assert_eq!(scenario.tasks.len(), 4);
        for t in &scenario.tasks {
            assert_eq!(t.cgroup_name.as_deref(), Some("/test_bw_stop"));
        }
    }
}
